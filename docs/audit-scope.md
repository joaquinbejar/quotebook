# Audit scope

Milestone I5 of [`PLAN-improvements.md`](PLAN-improvements.md). This is the brief for an
external audit of the on-chain program.

**Status: frozen** after I7 (fill event) and I10 (fees and maker registry), which both change
`settle`. The audit commit is **`3d0ec1329923f565ce98722c3c3cd55bd9c0d0f6`** (`3d0ec13`), the
merge of #34. Its program is the I10 head `4e490ba` (no program change after it), with the
verifiable-build hashes
(`verifiable-build.md`):
- program SHA-256 `4748d02454e5045ec06f8dec1ea2e5575ce28c7f4ab56b64dda46c1a9626c87a`;
- executable hash `a5287763e1c93463d7f52b5850129ee5be870328e489d513c47d693182d47f4d`, which
  `solana-verify get-program-hash` must print for the deployed program.

Any later change to `program/` or `quotebook-core/` moves the audit commit and repeats the
freeze checklist below.

## In scope

The on-chain program and the `no_std` core it is built from, measured at the I10 head
`4e490ba` (`wc -l`, tests and comments included). The draft's baseline was 3,458 lines at
`7c46ef7`; the growth is I7's fill event and I10's fees and registry.

| Path | Lines | At `7c46ef7` | What it does |
| --- | ---: | ---: | --- |
| `program/src/processor/settle.rs` | 396 | 308 | the fill: precompile introspection, binding, time, size, replay, both transfers, the fee leg (ADR 0001), the fill event |
| `program/src/processor/{init_config,create_series,admin,init_maker,cancel}.rs` | 403 | 401 | config, series (with the immutable `fee_bps`), pause, admin handover, maker state, cancels |
| `program/src/processor/{withdraw_fees,profile}.rs` | 254 | new | `withdraw_fees` (ADR 0001); `set_profile`, `close_profile` (ADR 0002) |
| `program/src/{mint,token,tlv}.rs` | 290 | 290 | mint and token-account checks, the Token-2022 extension walker |
| `program/src/{pda,loader,state,lib,error,instruction,log}.rs`, `processor/mod.rs` | 387 | 323 | PDA checks, zero-copy loading, the entrypoint, dispatch, the event log |
| `quotebook-core/src/sigverify.rs` | 362 | 362 | the ed25519 precompile parser (self-contained, single signature) |
| `quotebook-core/src/quote.rs` | 406 | 406 | the 208-byte canonical quote and its time rules |
| `quotebook-core/src/nonce.rs` | 479 | 479 | the 16,384-bit replay window |
| `quotebook-core/src/{instruction,accounts,quantity,error,lib}.rs` | 1,008 | 889 | instruction encodings (tags 0 to 10), account layouts (with `MakerProfile`), quantity newtypes, error codes 100 to 134 |
| `quotebook-core/src/{fee,profile,event}.rs` | 504 | new | the floor fee and `MAX_FEE_BPS`, the profile argument and canonical rules, the fill event |
| **Total** | **4,489** | **3,458** | |

The dependencies the program links are pinocchio 0.11, pinocchio-system 0.6, pinocchio-token
0.7, bytemuck 1.25 and solana-address 2.7. They are in scope only for how quotebook uses them.

## Out of scope, and why

- **Off-chain clients:** `quotebook-sdk`, `quotebook-maker`, `quotebook-cli` and the TS SDK
  (I8). They only build, sign and send transactions, and the program checks everything they
  send. A bug there can cost the client's own user (a bad quote from a maker's own service, for
  example), but cannot break the program's guarantees for anyone else.
- **Test and tooling code:** `program/tests`, `fuzz/`, `scripts/`.

## Trust assumptions

1. **The ed25519 precompile** verifies every signature in its instruction before the program
   runs, and fails the transaction otherwise.
2. **SPL Token and Token-2022** implement `transfer_checked`, delegation and the extensions as
   specified.
3. **The upgrade authority** can replace the program and so can do anything; **the admin** can
   pause and create series, choose a new series' fee (at most `MAX_FEE_BPS`, 100 bps), and
   withdraw collected fees from Config-owned fee accounts ([`governance.md`](governance.md)).
   The admin cannot change an existing series' fee or touch makers' and takers' accounts.
4. **The cluster clock's** `unix_timestamp` is within `MAX_CLOCK_SKEW` (10 s) of the time
   makers use.
5. **Runtime atomicity:** a failed instruction, or a failed CPI, aborts the whole transaction
   and rolls back every account.

## Invariants to attack

Numbered for findings to cite.

1. **Exact amounts.** A successful `settle` moves exactly `fill × base_lot` base atoms and
   `quote = price × fill × quote_lot` quote atoms between the maker's and the taker's accounts
   named by the quote's series, in the direction of the quote's side, plus the fee
   `floor(quote × fee_bps / 10,000)` of that series into its fee account: on an ask the taker
   pays it on top of `quote`, on a bid it comes out of the taker's proceeds, and in both cases
   the maker's flows are exactly `quote` and the base. A series with `fee_bps` 0 takes no fee
   account and moves no fee. A successful `withdraw_fees` moves exactly its amount out of one
   fee account. Nothing else moves.
2. **Once only.** A (maker, nonce) pair fills at most once, across any sequence of `settle`,
   `cancel_all` and `cancel_nonce`, including slides of the window (PLAN 4.2 and the issuance
   bound).
3. **Binding.** A quote fills only on the program, deployment (genesis prefix) and series it
   names, only for its bound taker (or any taker if open), only inside its time window, and
   only up to its size.
4. **Signature.** The bytes `settle` acts on are exactly the bytes the precompile verified,
   signed by `quote.maker`: one signature, every index `0xFFFF`, and the message read from
   that same instruction (`sigverify.rs`).
5. **Maker isolation.** The maker's leg moves tokens only from accounts owned by `quote.maker`,
   only through that maker's own delegate PDA, and never beyond the allowance the maker set.
6. **No unsigned movement.** No instruction moves tokens without the owner's signature or that
   owner's delegate PDA. Only `settle` and `withdraw_fees` move tokens: `settle` as above, and
   `withdraw_fees` only out of a token account owned by the Config PDA, signed by the Config
   PDA and authorized by `config.admin`. The fee leg of `settle` pays only into a token account
   of the series' quote mint owned by the Config PDA. `set_profile` and `close_profile` move
   only lamports (rent in, rent out to the maker's chosen destination).
7. **Canonical encodings.** Every decoder (quote, instruction arguments, account layouts)
   accepts exactly what the matching encoder produces.
8. **Authority gates.** `init_config` accepts only the upgrade authority once; `create_series`,
   `set_series_paused`, `set_config` and `withdraw_fees` accept only `config.admin`; cancels,
   `set_profile` and `close_profile` accept only the maker, and only for its own
   `["profile", maker]` PDA with the stored bump. A series' `fee_bps` is written once, at
   `create_series`, and no instruction changes it.
9. **Account validation.** Every account passed in is checked for owner, tag, PDA seeds and
   bump, mint, token program and writability as listed in PLAN 3.3, ADR 0001 (the fee account)
   and ADR 0002 (the profile). A forged account fails with a specific error, and each rule has
   a negative test (`program/tests/{settle,fees,profile}.rs`).
10. **Registry isolation.** `settle` never reads a profile; a profile is a hint for takers.
    Clients accept a quote from a listed endpoint only if `quote.maker` is that profile's maker
    (and on the allowlist when one is given), so a profile cannot redirect a taker to another
    maker's quotes. The off-chain half of this is out of scope; the on-chain half is that only
    the maker can write its profile, every stored profile is canonical, and closing it
    returns only its own rent.

The repository's own evidence for these:
- the LiteSVM negative tests, one per rule in PLAN 3.3;
- the stateful conservation property test and the libFuzzer targets ([`fuzzing.md`](fuzzing.md));
- the compute-unit table ([`benchmarks.md`](benchmarks.md)).

## Accepted risks (by design, documented)

- **One delegate per token account.** A maker cannot approve quotebook and another protocol on
  the same account (PLAN 3.1).
- **`MakerState` is permanent.** There is no close, so the replay bitmap can never be reset
  (PLAN 3.2, item 7).
- **Open quotes** (`taker = 0`) can be filled by whoever lands first, including someone who saw
  the quote in transit.
- **A dust fill still consumes the nonce** (invariant 6 of PLAN 2).
- **Event logs are hints** (after I7): they can be truncated or dropped. Indexers must
  validate provenance as `quotebook-sdk::events` does.
- **Dust pays no fee** (ADR 0001): the floor makes the fee 0 below `10,000 / fee_bps` quote
  atoms. Splitting a fill into dust fills costs a nonce and a transaction each.
- **The registry certifies nothing** (ADR 0002): any maker can list itself, and its endpoint
  and name are untrusted text that clients must escape.

## Known items, fixed after the audit

Found after the freeze and left out of the audit commit on purpose: no program change before
the audit.

- **CU overhead of `find_program_address` on the fee path** (#39). On a fee series, `settle`
  finds the Config PDA by bump search, about 1,500 CU per failed attempt. The devnet program's
  Config bump is 253, so a fee fill costs 15,199 CU there against 12,207 in LiteSVM (bump 255).
  This is cost only: the derived address is the same, and so is every check. Candidate fix:
  verify against the stored Config bump (as `withdraw_fees` does) instead of searching.

## Questions for the auditors

1. **No global pause.** Pausing everything takes one transaction per series
   ([`runbooks.md`](runbooks.md)). `Config` has no reserved bytes, so a global flag would need a
   layout change. Is per-series pausing enough for an RFQ venue?
2. **Admin recovery.** `set_config` is signed by the current admin, and nothing forces a
   recovery. If the admin key is lost, or an attacker has already rotated it away, only a program
   upgrade restores control ([`governance.md`](governance.md)). Should the upgrade authority be
   able to reset the admin?
3. **Clock skew.** `MAX_CLOCK_SKEW` is 10 s. Is that the right bound against devnet and mainnet
   clock drift, given that it lengthens every quote's on-chain life?
4. **Token-2022.** Is the mint extension allowlist (PLAN 3.3) complete and safe as the token
   program evolves? An unknown future extension type is rejected, so the allowlist fails
   closed.
5. **The fee path:** the fee rounds down (floor), in the taker's favour (ADR 0001), so dust
   trades pay no fee; is that acceptable? Is the fee-account check (any token account of the
   quote mint owned by the Config PDA, writable, initialized, not frozen) complete, and is it
   right that the fee account does not have to be the canonical associated token account?
6. **Profile rent:** a profile costs its maker about 0.0017 SOL of rent, and `close_profile`
   returns it to a destination of the maker's choice. Is a maker-chosen destination safe?

## Freeze checklist

- [x] I7 (#32) and I10 (#34) merged; main at `3d0ec13`.
- [x] The devnet upgrade to `3d0ec13` done (2026-11-03, slot 518208830): `get-program-hash`
  prints the executable hash above and the smoke tests pass (`devnet.md`, #38).
- [x] `wc -l` recounted (the table above, at `4e490ba`).
- [x] `scripts/verifiable/build.sh` at the program's commit: both hashes above, equal to CI's
  `verifiable` job and to `make verifiable-check`.
- [x] `make pre-push` and `make fuzz-check` green at `4e490ba`; 10,000 conservation
  sequences (314,300 steps, fees and `withdraw_fees` included) passed on the I10 program
  (`fuzzing.md`); hour runs of every fuzz target, the I10-extended ones included, found no
  crash.
- [x] The CU table regenerated (`make bench`, `benchmarks.md`).
- [x] The merge commit written in Status above.
- [ ] This file's PR merged.
