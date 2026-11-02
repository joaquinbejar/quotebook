# Audit scope

Milestone I5 of [`PLAN-improvements.md`](PLAN-improvements.md). This is the brief for an
external audit of the on-chain program.

**Status: draft.** The audit commit is frozen after I7 (fill event) and I10 (fees and maker
registry), because both change `settle` and auditing before them would mean re-auditing it. At
the freeze this file gets:
- the commit;
- the recounted lines;
- the verifiable-build hashes of that commit (`verifiable-build.md`).

## In scope

The on-chain program and the `no_std` core it is built from, measured at `7c46ef7` (`wc -l`,
tests and comments included):

| Path | Lines | What it does |
| --- | ---: | --- |
| `program/src/processor/settle.rs` | 308 | the fill: precompile introspection, binding, time, size, replay, both transfers |
| `program/src/processor/{init_config,create_series,admin,init_maker,cancel}.rs` | 401 | config, series, pause, admin handover, maker state, cancels |
| `program/src/{mint,token,tlv}.rs` | 290 | mint and token-account checks, the Token-2022 extension walker |
| `program/src/{pda,loader,state,lib,error,instruction}.rs`, `processor/mod.rs` | 323 | PDA checks, zero-copy loading, the entrypoint, dispatch |
| `quotebook-core/src/sigverify.rs` | 362 | the ed25519 precompile parser (self-contained, single signature) |
| `quotebook-core/src/quote.rs` | 406 | the 208-byte canonical quote and its time rules |
| `quotebook-core/src/nonce.rs` | 479 | the 16,384-bit replay window |
| `quotebook-core/src/{instruction,accounts,quantity,error,lib}.rs` | 889 | instruction encodings, account layouts, quantity newtypes, error codes |
| **Total** | **3,458** | |

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
   pause and create series ([`governance.md`](governance.md)).
4. **The cluster clock's** `unix_timestamp` is within `MAX_CLOCK_SKEW` (10 s) of the time
   makers use.
5. **Runtime atomicity:** a failed instruction, or a failed CPI, aborts the whole transaction
   and rolls back every account.

## Invariants to attack

Numbered for findings to cite.

1. **Exact amounts.** A successful `settle` moves exactly `fill × base_lot` base atoms and
   `price × fill × quote_lot` quote atoms between the maker's and the taker's accounts named
   by the quote's series, in the direction of the quote's side. Nothing else moves.
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
   owner's delegate PDA, and no instruction other than `settle` moves tokens at all.
7. **Canonical encodings.** Every decoder (quote, instruction arguments, account layouts)
   accepts exactly what the matching encoder produces.
8. **Authority gates.** `init_config` accepts only the upgrade authority once; `create_series`,
   `set_series_paused` and `set_config` accept only `config.admin`; cancels accept only the
   maker.
9. **Account validation.** Every account passed in is checked for owner, tag, PDA seeds and
   bump, mint, token program and writability as listed in PLAN 3.3. A forged account fails
   with a specific error, and each rule has a negative test.

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
5. **The fee path** (I10, once frozen): the fee rounds down (floor), in the taker's favour
   (ADR 0001), so dust trades pay no fee; is that acceptable, and is the Config-PDA-owned fee
   account check complete?

## Freeze checklist

- [ ] I7 and I10 merged, and the devnet upgrade carrying them done.
- [ ] `wc -l` recounted at the freeze commit, and the table above updated.
- [ ] `make verifiable` at the freeze commit, with both hashes recorded here.
- [ ] `make pre-push`, `make fuzz-check` and 10,000 conservation sequences green at that commit.
- [ ] The CU table regenerated (`make bench`).
