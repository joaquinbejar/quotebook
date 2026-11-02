# quotebook: improvement plan

Status: **approved r6** (Codex, PR #18 review at `a4f1e0b`). GO from SOLANA-PROJECTS, msg 43338 (2026-11-02).
Baseline: `main` at `4b3751f` (v0.1.1 on crates.io; program `3Aq1Aw…6mbo` on devnet, built
from `898c173`). `docs/PLAN.md` stays the v1 design record; this file plans the work after v1.

Process: one branch and one PR per milestone (`feat/i<N>-<name>`), `make pre-push` clean
before each push, Codex reviews every PR before merge, and a report to SOLANA-PROJECTS at each
milestone. The global rules (`rules/global_rules.md`) apply unchanged; where a milestone needs
a rule exception or a new dependency, the item says so and this plan's approval is the
approval.

Effort: **S** up to a day, **M** two to four days, **L** more than that.

## Facts this plan relies on (checked 2026-11-02)

- **Version drift: none on `main`** (confirmed by SOLANA-PROJECTS, msg 43378: its information
  was stale). `Cargo.toml:6` is `version = "0.1.1"` and every
  `quotebook-*` entry in `Cargo.lock` is 0.1.1. What does lag is the devnet binary: it was
  built from `898c173` (before the 0.1.0 bump). `git diff --stat 898c173 4b3751f -- program
  quotebook-core Cargo.lock` shows only the version lines of `Cargo.lock` and the new
  `quotebook-core/LICENSE`, so the program code is the same. I2 rebuilds it and compares the
  hash with the recorded `d7c2f1de…21e7`.
- No `.github/` directory. optfix's CI (`.github/workflows/ci.yml` and
  `.github/actions/toolchains/action.yml`, optfix milestone 9) is green on `main`
  (run 41295322810, 21 min 44 s).
- In audit scope today: `program/src` and `quotebook-core/src`, **3,458 lines** (`wc -l`).
- `settle` costs 8,593 CU on SPL Token and 18,188 in the Token-2022 worst case
  (`docs/benchmarks.md`).
- `Series` is 144 bytes. Its last field, `_reserved: [u8; 8]` (`quotebook-core/src/accounts.rs:104`),
  is zero in every series ever created, because `create_series` writes it as zero.
- Pinocchio 0.11.2 re-exports the raw syscalls as `pinocchio::syscalls`, `sol_log_data`
  included. It is an `unsafe extern` call and has no safe wrapper.
- One hot key holds three roles on devnet: upgrade authority, config admin and payer
  (`77ie…PPV`).
- `quotebook-admin` already has `pause`, `resume`, `set-admin`, `cancel-all` and
  `cancel-nonce`. It has no `revoke`.
- The CLI maker prices from a fixed mid only (`cli/src/maker.rs:32`).
- metalbook on devnet:
  - program `FPPu…suwg`;
  - USDC mint: Circle devnet USDC `4zMM…ncDU`, 6 decimals;
  - long tokens are 0-decimal SPL mints;
  - every 1 oz series is from Oct 30 and has expired;
  - the Nov 6 XAG series are 50 oz (240 USDC of collateral and up);
  - its maker holds 40 USDC (`metalbook/docs/devnet-log.md`, "Not done yet: Trading through
    QuoteBook").
- Tooling installed: `cargo-fuzz`, `nightly-2026-10-11`, Docker, Node v26.13.0.
  `solana-verify` is not installed.

## Phase 1: hardening

### I1. CI (S)

Copy optfix's workflow and composite action, adapted to quotebook's `make pre-push`
(`fmt-check lint lint-control program-control program test doc`):

- **Pins:** the same as optfix:
  - Agave CLI 4.4.0, with its tarball SHA-256;
  - platform-tools v1.57, with its SHA-256;
  - host Rust 1.99.1.
  Actions are pinned by commit SHA, with `permissions: contents: read`.
- **Makefile:** add `RUST_TOOLCHAIN` and `CARGO_FLAGS` (`--locked` in CI), as in optfix.
- **Jobs:**
  - `checks`: every pre-push target except `test`;
  - `test`;
  - `bench`: `make bench`, then `git diff --exit-code docs/benchmarks.md`.
  optfix splits `test` into shards because its pre-push takes 40 minutes. I time quotebook's
  first and shard only if `test` takes more than 20 minutes.
- **Guard step:** optfix's "the CI jobs cover make pre-push" step, so the workflow fails when
  `pre-push` changes and the jobs do not.
- **Merge gate, as a process rule:** no branch protection on quotebook. Required CI on
  `main` is for optfix only (the user's decision, relayed by SOLANA-PROJECTS in msg 43728).
  Every PR runs CI and is merged only when `checks`, `test` and `bench` (and `ts` after I8)
  are green. The merge comment links the green run.
- **Failing controls:** one scratch PR per job, each planting a single fault, each closed
  unmerged:
  - a formatting error fails `checks` only;
  - a failing test fails `test` only;
  - an edited row in `docs/benchmarks.md` fails `bench` only.
  The PR records the three run links and which jobs failed in each.
- **Done when:**
  - CI is green on the PR;
  - the three controls failed exactly their intended job.

### I2. Verifiable build (M)

- **Docker build:** build the program with `solana-verify build`, which uses the
  `solanafoundation/solana-verifiable-build` image pinned by digest.
  - Before choosing the image, check that it exists for Agave 4.4.0 / platform-tools v1.57.
  - If no 4.4 image exists, fall back to a Dockerfile in `scripts/` built from the same
    pinned archives CI uses, also pinned by digest. That fallback is a reproducible build,
    not one `solana-verify` can attest.
- **`make verifiable`:** a new target that writes `target/deploy/quotebook_program.so` and
  prints its SHA-256. CI runs it and compares the result with the hash committed in
  `docs/verifiable-build.md`.
- **Devnet:** compare the hash with `solana-verify get-program-hash 3Aq1Aw…6mbo --url devnet`.
  - If they differ (likely: the devnet binary was built on macOS outside Docker), upgrade
    devnet once with the verifiable artifact.
  - Record the commit, the hash and the signature in `docs/devnet.md`.
  - The program code is unchanged since `898c173`, so this upgrade changes no behaviour. It
    is the only program upgrade in Phase 1.
- **OtterSec:** submit to the OtterSec verification API (`verify-from-repo --remote`) only if
  it accepts devnet. Check that first and record the answer either way.
- **Done when:**
  - two clean Docker builds from the same commit produce the same hash;
  - CI reproduces it;
  - the devnet program hash equals it.

### I3. Separate upgrade authority (S, documents and a devnet rehearsal only)

`docs/governance.md` covers the following.

- **Role split.**
  - Upgrade authority: a Squads v4 multisig (2 of 3, hardware keys) on mainnet.
  - Config admin: a second Squads vault, or a separate key. It only pauses series, creates
    them and hands admin over.
  - Payer: a hot key with no authority.
  - `init_config` requires admin == upgrade authority at init (PLAN 3.3), so `set_config`
    moves admin away right after init.
- **Ordering.** The ordering on mainnet day:
  1. deploy from the verifiable build;
  2. `init_config`;
  3. `set_config` to the admin vault;
  4. `solana program set-upgrade-authority` to the multisig, with
     `--new-upgrade-authority` checked;
  5. `solana program show` as evidence.
  The program is never made `--final`.
- **Upgrade procedure.** A buffer written from the verifiable build, its hash compared with
  the build's, then the multisig approves and executes.
- **Devnet rehearsal** (reversible: we hold every key):
  - create `~/.config/solana/quotebook-devnet/{upgrade-authority,admin}.json` outside the
    repo;
  - move the upgrade authority and the admin to them;
  - check with `solana program show` and the config account;
  - leave the payer as it is.
  Plain keys are enough on devnet for this phase (SOLANA-PROJECTS, msg 43378). A Squads devnet
  rehearsal is a gated pre-mainnet item: listed in `docs/governance.md`, not done here.
- **No mainnet action.**

### I4. Runbooks (S)

`docs/runbooks.md`, each with the exact commands and what to check afterwards:

- **Incident: stop fills on a series.** `quotebook-admin pause`. There is no global pause, so
  a pause-all is one transaction per series. The runbook carries a loop over
  `series_count`. A global pause flag is an audit-scope question (I5), not a change here,
  because `Config` has no reserved bytes.
- **Maker key or service compromise:**
  1. revoke the delegate approval on the paying token accounts, which stops every fill at
     once;
  2. then `cancel-all` to the current nonce watermark;
  3. then move funds.
  This adds `quotebook-admin revoke` (Token and Token-2022 `Revoke`), which does not exist
  yet: a few lines in `sdk::ix` plus a test.
- **Maker service restart:** the cold start of `max_quote_ttl + 10 s` and what the logs show.
- **RPC key rotation:** URLs come only from arguments and environment, and are redacted.
- **Program upgrade and admin handover:** points to I3.
- **Devnet funding:** faucet limits, and Circle devnet USDC for the I9 demo.

### I5. Audit scope (S)

`docs/audit-scope.md`:

- **In scope:** `program/src` and `quotebook-core/src` (3,458 lines at `4b3751f`; recounted
  at the freeze).
- **Out of scope, with the reason:** the maker, CLI, SDK and TS SDK are off-chain clients;
  the program checks everything they send.
- **Trust assumptions:**
  - the ed25519 precompile;
  - the SPL Token and Token-2022 programs;
  - the upgrade authority and the admin;
  - the cluster clock within `MAX_CLOCK_SKEW`.
- **Invariants to attack**, numbered:
  1. a fill moves exactly the quoted amounts;
  2. each (maker, nonce) fills at most once;
  3. a quote binds the program, deployment, series and taker;
  4. the maker's leg only ever reaches `quote.maker`'s accounts through its own delegate;
  5. no instruction moves tokens without its signer;
  6. layout and encoding are canonical.
- **Accepted risks:**
  - one SPL delegate per token account;
  - no global pause;
  - `MakerState` is permanent;
  - open quotes (`taker = 0`) can be taken by whoever lands first.
- **The audit commit is frozen after I7 and I10**, which touch `settle`. Auditing before them
  would mean re-auditing `settle`.

### I6. Fuzzing: sigverify, settle and conservation (M)

Two layers. The fast one runs in `make pre-push`; the slow one runs on demand.

- **libFuzzer targets on core** (`fuzz/`, outside the workspace):
  - targets: `sigverify::verified_quote` on arbitrary precompile data, `Quote::decode` on
    arbitrary 208-byte inputs, and the instruction decoders (`SettleArgs` and the rest).
  - Oracle for sigverify: an independent reference parser written in the fuzz target
    (`quotebook-core/src/sigverify.rs:112-143` is the code under test). It predicts accept
    or the exact error, and the target asserts equality:
    - count byte 1;
    - every instruction index `0xFFFF`;
    - `message_data_size == 208`;
    - the signature, public key and message ranges inside the data;
    - `Quote::decode` of the message;
    - public key == `quote.maker`.
    On accept, the returned quote also re-encodes to the bytes at the message offset.
    Structured inputs are generated from a valid record with mutated fields, so that every
    rejection branch is reached, not only the first length check; coverage of each error
    variant is reported.
  - Decoders: accept implies `encode(decode(x)) == x`.
  - **Rule exception:** `cargo-fuzz` needs nightly. It is pinned to `nightly-2026-10-11` in
    `fuzz/rust-toolchain.toml`, and the workspace stays on stable.
  - The corpus and any crash reproducers are committed. A crash becomes a stable regression
    test in `quotebook-core/tests`.
  - **New dependencies for approval:** `libfuzzer-sys` and `arbitrary`, in `fuzz/` only.
- **Stateful LiteSVM property test of the program** (`program/tests/conservation.rs`, plain
  `proptest`, no new dependency):
  - Random sequences of `settle` (both sides, either token program), `cancel_all`,
    `cancel_nonce` and `set_series_paused` across 2 makers, 2 takers and 2 series.
  - Each sequence is a mix of valid quotes and mutated ones: a flipped byte, a wrong series,
    an expired quote, a replayed nonce, an over-size fill, a swapped account.
  - Outcomes are predicted, not just observed. A pure Rust model gets the step before it
    runs and predicts its outcome:
    - success, or the exact error code where the error is stable;
    - mutations that can only be rejected predict "rejected" plus the error set they may
      hit.
    The test asserts that the observed outcome equals the prediction. A regression that
    rejects every valid `settle` therefore fails at once.
  - Generation guarantees successful fills: every sequence carries at least one valid fill
    per side and token program. The run reports how many fills succeeded and fails if any
    (side, program) pair had none.
  - After every step it checks, against the model:
    - conservation: the total of each mint across all token accounts is constant;
    - each successful fill moves `fill × base_lot` and `price × fill × quote_lot`;
    - delegate allowances drop by exactly the maker's outflow;
    - a nonce fills at most once;
    - `fills` equals the model's count of successful settles;
    - a rejected transaction leaves the program and token accounts byte-identical.
      Lamports of the fee payer are excluded: a rejected transaction still pays its
      signature fee, and the model charges that fee to the payer explicitly.
  - The case count is set by `QUOTEBOOK_PROPTEST_CASES`, low in pre-push and high in
    `make fuzz-long`.
- **Done when:**
  - each libFuzzer target has run 1 hour with no crash, recorded in the PR;
  - the property test passes 10,000 cases;
  - each of these failing controls, run once and recorded, is caught:
    - a deliberately broken `amounts` (off by one lot);
    - a disabled nonce check;
    - a `settle` that always rejects;
    - a reference sigverify parser that skips the maker binding.

## Phase 2: features

### I7. Fill event (S)

- **Event:** `settle` emits one `sol_log_data` record after both transfers succeed.
  - Layout (`quotebook-core::event`, canonical like `Quote`, 168 bytes):
    - an 8-byte tag `qb-fill1`;
    - `series`, `maker` and `taker` (32 bytes each);
    - `nonce`, `side` (with 7 bytes of padding), `price` and `fill_size`;
    - `base_atoms`, `quote_atoms` and `fee` (0 until I10);
    - `unix_timestamp`.
- **unsafe:** the one `unsafe` call, `pinocchio::syscalls::sol_log_data`, gets a
  `// SAFETY:` comment and a module-local `allow`, as the rules require. Host builds get a
  no-op stub.
- **SDK and bench:**
  - `quotebook-sdk::decode::fill_event` parses it from the transaction's log lines
    (`Program data: <base64>`).
  - The new `settle` CU goes in `make bench`.
- **Provenance:** the raw payload decoder alone cannot tell a real event from a fake one.
  - Any program can log `Program data:` with the same tag.
  - A successful `settle` followed by a failing later instruction leaves the event in the
    logs, although the whole transaction rolled back.
  So `quotebook-sdk` has two layers:
  - `decode::fill_event_payload(bytes)`: context-free;
  - `events::fill_events(tx)`: what indexers use. It takes a confirmed transaction with its
    status and logs, and returns events only when all of these hold:
    - the transaction succeeded, with no error in its meta;
    - the commitment is `finalized` (or the level the caller asks for);
    - the record was emitted by the configured quotebook program itself. `fill_events`
      replays the invocation stack from the log lines:
      - `Program <id> invoke [n]` pushes a frame;
      - `Program <id> success` and `Program <id> failed: …` pop it.
      A `Program data:` line is attributed to the frame on top of the stack. It is accepted
      only when that frame's id is the configured program, at any depth: `settle` called by
      CPI from another program is supported. A foreign program's bytes are always rejected,
      even when that program is nested inside quotebook's own frame.
    - the emitting frame and every ancestor frame closed with their matching
      `Program <id> success`. Events are buffered per frame. When a frame fails, its own
      events and those of all its descendants are dropped. An event is yielded only once
      the whole ancestor chain, up to the top-level instruction, has succeeded. In today's
      runtime a failed CPI aborts the whole transaction, so a caller cannot catch it and
      the case of a successful `settle` under a failed but caught ancestor cannot occur. The
      rule costs nothing and keeps the API correct should that ever change.
    If the stack does not balance, a frame is left open, or the logs carry the runtime's
    `Log truncated` marker, `fill_events` returns `Incomplete` with the events it could
    prove so far. It fails closed and never guesses.
    Each event's identity is (signature, index of the event within the transaction), so it
    can be deduplicated.
- **Known limit, documented:**
  - logs can be truncated (10 KB per transaction) or dropped by RPC nodes;
  - so events are hints, not a complete trade history;
  - `MakerState.fills` and token balances show that fills happened, but cannot reconstruct
    a lost fill or its price: the signature and its instructions do.
  A self-CPI event (as in Anchor's `emit_cpi`) is more robust, but it costs a CPI and a PDA.
  It is rejected for now, and the PR records the CU of both.
- **Done when:** LiteSVM tests show all of these:
  - the event decodes for both sides and both token programs;
  - there is no event on any failed `settle`;
  - a spoofed emitter (a test program logging the same bytes) is rejected by
    `fill_events`;
  - a transaction of `settle` followed by a failing instruction yields no event from
    `fill_events`;
  - `settle` called by CPI from a test program at depth 2 yields its event;
  - a foreign program's same bytes, emitted inside a nested frame, are rejected;
  - logs cut before the success line, and logs carrying `Log truncated`, return
    `Incomplete`;
  - a caught ancestor failure, A calling B calling `settle`: `settle` succeeds, then B
    fails, and A and the transaction succeed. The runtime cannot produce this, so the test
    is a unit test over synthetic log lines; it must yield no event.

### I8. TypeScript SDK checked against Rust vectors (M)

- **Package:** a `ts/` package, not published (npm publishing is irreversible; it needs its
  own GO). Its contents:
  - PDAs;
  - quote encode and decode;
  - the precompile instruction (self-contained, every index `0xFFFF`);
  - the `settle` instruction and its account metas;
  - account and fill-event decoders;
  - signature verification over the 208 bytes.
- **Dependencies:** `@solana/kit` (latest stable, checked on npm when added); `typescript` as
  a dev dependency; tests on `node:test`, so no test framework. All are pinned exactly in
  `package-lock.json`.
- **Vectors:**
  - `cargo run -p quotebook-sdk --example vectors` writes `ts/vectors/vectors.json`:
    - quotes at edge values;
    - encodings;
    - precompile data;
    - `settle` data and metas;
    - PDAs with bumps;
    - account bytes;
    - fill events;
    - a valid and an invalid signature.
  - `make vectors-check` regenerates them and fails on a diff, like `bench`.
- **Tests and CI:**
  - The TS tests assert byte equality on every vector.
  - `make ts-check` (`npm ci`, `tsc --noEmit`, `node --test`) joins `pre-push`.
  - CI gets a `ts` job with Node pinned by `actions/setup-node` at a commit SHA.
- **Done when:**
  - every vector passes;
  - a mutated vector fails (control);
  - a TS-built `[precompile, settle]` transaction settles in LiteSVM, through a small Rust
    test that loads the TS output.

### I9. RFQ demo on a metalbook series (S; runs before Nov 13 21:00Z)

The demo needs no new code: it uses the existing `quotebook-admin`, `quotebook-maker` and
`quotebook-taker`. It is scheduled right after I1, because the metalbook series it trades
expires on Fri 2026-11-13 21:00Z.

**Accounts, provided by metalbook's window (msg 43382):**

| Item | Value |
| --- | --- |
| metalbook series | XAG capped call, 1 oz, strike 50, cap 55, expiry 1794603600: `B66MftMja6L6g2XFNLWHUWE2Rjq1yTHtToWQTvxsZx21` |
| long mint | `AF3A2Rt57QPYXnXg42nBNTN2TLP8GoMjp94ZVYVyjH4x` (SPL Token, 0 decimals) |
| maker | metalbook's devnet maker `7tVP…cACQ` (decided by SOLANA-PROJECTS, msg 43378). It holds the 3 long tokens and 15 USDC. |
| taker | quotebook's `3ZBj1qBpdoH5NrMTwAHq2fY6Hbo5M1GURxaP9y8bsqLs`, holding 10 USDC |

**Demo series:** a quotebook series with these parameters:

| Parameter | Value |
| --- | --- |
| base | the long mint |
| `base_lot` | 1 |
| quote | Circle devnet USDC `4zMM…ncDU` |
| `quote_lot` | 10,000 (a 0.01 USDC tick) |
| `min_size` | 1 |
| `max_quote_ttl` | 60 s |

**The run:**
1. `init_maker` for 7tVP….
2. 7tVP… approves its delegate PDA on its long-token account, for 3.
3. The CLI maker asks with a fixed mid from metalbook-maker's theoretical price for the series.
4. The taker buys 1 long token for USDC.

7tVP…'s key belongs to metalbook's window, so every step it signs is announced to that window
first. The run is recorded in `docs/devnet.md`, with a cross-link for metalbook's devnet log.

Not a hosted service: a scripted, reproducible run. A hosted maker priced from Pyth is out of
scope here.

### I10. Fees and maker registry: decisions, then implementation (S to decide, M to build)

Two short ADRs (`docs/adr/0001-series-fees.md`, `docs/adr/0002-maker-registry.md`) go to
Codex and SOLANA-PROJECTS before any code.

**Fees: recommended design.**

- **`fee_bps`:** `fee_bps: u16` takes the first 2 of `Series._reserved`'s 8 bytes.
  - It is set by `create_series` and never changed: there is no instruction that writes it.
  - It is capped at `MAX_FEE_BPS = 100`, and devnet series default to 0 (both agreed by
    SOLANA-PROJECTS, msg 43378, as is the taker-paid fee).
  - Existing series read 0, so they need no migration, and the account `VERSION` stays 1.
    With fee 0 there is no third transfer, but that does not prove the CU is unchanged: the
    compiled code changes. The new CU is measured, not assumed.
- **Why immutable:** a quote binds its series address, so an immutable fee means the signed
  quote fully determines what every party pays. A mutable fee would let the admin change a
  taker's cost after the maker signed.
- **Who pays:** the taker pays in quote atoms, `fee = ceil(quote_atoms × fee_bps / 10,000)`,
  rounded against the taker. The maker's flows are exactly the quoted ones on both sides:
  - Ask: the taker pays `quote_atoms` to the maker and `fee` to the fee account.
  - Bid: the maker's delegate pays `quote_atoms - fee` to the taker and `fee` to the fee
    account, so the maker's outflow and allowance use stay `quote_atoms`.
  - With `fee_bps = 0` there is no third transfer and no extra account.
- **Fee account:** any token account of the quote mint whose owner is the Config PDA. There
  is no stored address, because `Series` has no room for a 32-byte one. `settle` takes it as
  an optional account 13 when `fee_bps > 0`.
- **Withdrawals:** the new `withdraw_fees(amount)` (tag 8, admin) moves fees out, signed by
  the Config PDA's seeds.
- **Validation:** checked arithmetic on every amount and on `quote_atoms - fee`. When
  `fee_bps > 0`, the fee account must be:
  - of `series.quote_mint`;
  - under the quote mint's token program;
  - owned by the Config PDA, with the PDA re-derived;
  - free of any account extension outside the existing allowlist.
- **Cost:** `make bench` gets measured rows for fee 0 and fee > 0, on SPL Token and on
  Token-2022, both sides, plus `withdraw_fees`.
- **Tests:**
  - I6's conservation property test extends to the fee account.
  - `withdraw_fees` has negative tests:
    - signer not the admin;
    - wrong Config PDA;
    - a source account not owned by the Config PDA;
    - the wrong mint;
    - an amount above the balance;
    - a zero amount.

**Maker registry: recommended design.**

- **Account:** permissionless and on-chain. A `MakerProfile` PDA `["profile", maker]`,
  written only by the maker through the new `set_profile` and `close_profile`:
  - an endpoint of at most 128 bytes: printable ASCII, `wss://` only;
  - a display name of at most 32 bytes.
- **No contention:** `settle` never touches the profile, so it adds no write contention.
  Closing it is safe, unlike `MakerState`, because it holds no replay state.
- **Curation stays with the taker:** a local allowlist in the taker client. A permissioned,
  admin-approved registry is the alternative the ADR weighs. It adds an admin power and
  cannot stop a maker from quoting off-registry.
- **Untrusted input:** the endpoint is maker-provided text, so the existing redaction
  applies to it.
- **Taker CLI:** gains `--discover`, which lists the profiles with `getProgramAccounts` and a
  tag filter.

**After I10:** one devnet upgrade (verifiable build, via I3's authority) carrying I7 and I10.
I5's audit commit is frozen at that point.

### I11. Release 0.2.0 (S)

- **Why 0.2.0:** `quotebook-core` changes its public API (`Series._reserved` becomes
  `fee_bps`; new event and instruction types), so it is 0.2.0, not 0.1.2.
- **Steps:**
  - CHANGELOG;
  - a signed tag;
  - `cargo publish -p quotebook-core -p quotebook-sdk -p quotebook-maker`;
  - a GitHub release.
- **Before publishing:** publishing is irreversible. I read the inbox immediately before
  `cargo publish`, and it needs an explicit GO from SOLANA-PROJECTS at that moment.

### I12. Maker risk parameters relative to the mid (S)

Added after I9 (SOLANA-PROJECTS, msg 44092).

**Finding:** the maker service's Avellaneda-Stoikov parameters are absolute. Its spread term
`(2/γ) ln(1 + γ/k)` is in price units, and the defaults (`--risk-aversion 0.1`,
`--order-intensity 1.5`) were tuned for prices near 25. At the I9 mid of 1.20 USDC they put
the quote 54% away from the mid, so the maker's own 5% deviation guard rejected every
request. At a mid of 3,000 the same defaults would give a spread of about 0.04%, far too
tight.

**Change:**
- **Spread:** the half-spread and the inventory skew are expressed in basis points of the
  mid:
  - `min_half_spread_bps` (floor) and `max_half_spread_bps` (cap);
  - `max_skew_bps`, the cap on how far inventory may move the reservation price from the mid.
  The service derives the model's price-unit parameters from the mid at each quote, so one
  set of defaults works at any price level.
- **Precedence**, in order:
  1. the model computes a reservation price and a half-spread, from the defaults or from
     the absolute advanced overrides (the existing flags, documented as absolute);
  2. the skew is clamped to `max_skew_bps` and the half-spread to
     `[min_half_spread_bps, max_half_spread_bps]`. The clamps always apply, overrides
     included;
  3. the prices are rounded to the tick, the bid down and the ask up, with exact integer
     math, toward the maker;
  4. the deviation guard (5% of the mid by default) checks the rounded prices last. A failed
     guard is a refusal, never a silent re-price.
- **Infeasible quotes are typed refusals.** When no rounded pair passes every rule, the
  service refuses with an error that names the cause:
  - `TickTooCoarse`: the tick makes either side deviate beyond the guard, for example mid
    0.01 with a 0.01 tick;
  - `NonPositiveBid`: the rounded bid is 0 or below.
  The taker sees an error instead of a bad price.

**Domain:** a quote is guaranteed when `tick ≤ mid × guard / 2` and
`mid × (1 - guard) ≥ tick` (the bid stays positive). Outside it, the typed refusals above.

**Ordering:**
- At zero inventory: `bid < mid < ask`.
- At any inventory:
  - `bid < ask`;
  - `bid < reservation < ask`;
  - `|reservation - mid| ≤ max_skew_bps` of the mid;
  - both prices within the guard of the mid.
  With inventory, the mid may fall outside `[bid, ask]` by at most the skew cap. That is
  what skew is for.

**Tests:**
- At mid 1.20 (tick 0.01) and at mid 3,000 (tick 0.01): the default quote at zero inventory
  has a half-spread within the bps bounds, passes the guard, and has `bid < mid < ask`.
- A property test over (mid, tick, inventory), with mid from 0.01 to 1,000,000 and tick from
  1e-6 to 1:
  - inside the domain, a quote exists and satisfies the ordering rules for its inventory;
  - outside it, the refusal is exactly `TickTooCoarse` or `NonPositiveBid`.
- Overrides: absolute flags that would breach a bps clamp are clamped (tested at both caps).
- Regression: the I9 failure (absolute defaults at mid 1.20) now yields a valid quote.

## Later (L, design only for now)

### L1. v2 package quotes

`docs/design/v2-package-quotes.md` only; no code in this plan.

- **The problem:** a maker quotes N legs as one price. Today a taker could take the legs one
  by one, which is already atomic per transaction (PLAN open question 7). But each leg is
  signed alone, so a taker can cherry-pick them.
- **Signed message:** v2 signs a 208-byte header with domain `quotebook-v2` plus
  `package_hash = sha256(leg_count ‖ legs)`. Each leg is (series, side, price, size).
- **`settle_package`:**
  - recomputes the hash from its arguments;
  - consumes one nonce for the package;
  - fills every leg in the same ratio, or none.
- **Limits to quantify in the design:**
  - Transaction size is 1,232 bytes and each leg adds about 7 accounts, so 2 legs fit
    without a lookup table and 3 or more need one.
  - The CU estimate, from the measured per-leg `settle` cost.
  - `sha256` on-chain is a syscall (`sol_sha256`), so it adds no dependency.
- **Open for the design:** v1 and v2 quotes share `MakerState`'s nonce window; one window
  serves both domains.

## Out of scope

- **Jupiter RFQ adapter:** dropped (SOLANA-PROJECTS, msg 43338).
- **Sharding `MakerState` or the token accounts:** not until contention is measured. At
  8.6k CU per `settle`, the 12M CU per-account block limit allows about 1,390 settles per
  maker per block. The nonce rule caps a maker earlier: 16,384 per (60 + 10) s, about 94 per
  slot. So the window, not the account lock, is the first limit. To revisit only when a live
  maker hits it, with the measurement attached.
- Hosted maker service, npm publishing, mainnet deployment.

## Order and effort

| # | Milestone | Effort | Depends on |
| --- | --- | --- | --- |
| I1 | CI | S | |
| I2 | Verifiable build (+ devnet hash match) | M | I1 |
| I3 | Governance doc + devnet authority split | S | I2 |
| I4 | Runbooks + `revoke` | S | I3 |
| I5 | Audit scope (freeze after I10) | S | I4 |
| I6 | Fuzzing + conservation property test | M | I1 |
| I7 | Fill event | S | I6 |
| I8 | TS SDK + Rust vectors | M | I7 |
| I9 | metalbook RFQ demo (before 2026-11-13 21:00Z) | S | I1; metalbook accounts ready |
| I10 | Fee and registry ADRs, then code, then devnet upgrade | S + M | I6, I7 |
| I11 | Release 0.2.0 | S | I10, GO |
| I12 | Maker risk parameters relative to the mid | S | I9 finding |
| L1 | v2 package quotes design | L (design only) | I10 |

## Questions answered by SOLANA-PROJECTS (msg 43378)

1. Version drift: none; I2 keeps the hash comparison.
2. I9: the maker is 7tVP…; metalbook's window listed the series and funded the taker.
3. I3: plain keys on devnet; Squads is rehearsed before mainnet, not in this phase.
4. Fees: `MAX_FEE_BPS = 100`, taker-paid, devnet default 0, immutable per series.
