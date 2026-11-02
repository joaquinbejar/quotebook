# quotebook: improvement plan

Status: **draft r1, for Codex review**. GO from SOLANA-PROJECTS, msg 43338 (2026-11-02).
Baseline: `main` at `4b3751f` (v0.1.1 on crates.io; program `3Aq1Aw…6mbo` on devnet, built
from `898c173`). `docs/PLAN.md` stays the v1 design record; this file plans the work after v1.

Process: one branch and one PR per milestone (`feat/i<N>-<name>`), `make pre-push` clean
before each push, Codex reviews every PR before merge, and a report to SOLANA-PROJECTS at each
milestone. The global rules (`rules/global_rules.md`) apply unchanged; where a milestone needs
a rule exception or a new dependency, the item says so and this plan's approval is the
approval.

Effort: **S** up to a day, **M** two to four days, **L** more than that.

## Facts this plan relies on (checked 2026-11-02)

- **Version drift: none on `main`.** `Cargo.toml:6` is `version = "0.1.1"` and every
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
- **Done when:** CI is green on the PR, and a deliberate formatting error pushed to a
  throwaway branch fails it (failing control, run once and recorded in the PR).

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
  Squads itself is not used on devnet in this milestone. A Squads devnet rehearsal can follow
  if SOLANA-PROJECTS wants one.
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
  - Oracle for sigverify: any accepted input has exactly one signature, every offset index
    is `0xFFFF`, and the returned quote re-encodes to the bytes at the message offset.
    Decoders: accept implies `encode(decode(x)) == x`.
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
  - After every step it checks, against a pure Rust model:
    - conservation: the total of each mint across all accounts is constant;
    - each successful fill moves `fill × base_lot` and `price × fill × quote_lot`;
    - delegate allowances drop by exactly the maker's outflow;
    - a nonce fills at most once;
    - `fills` equals the number of successful settles;
    - every rejected transaction leaves every account byte-identical.
  - The case count is set by `QUOTEBOOK_PROPTEST_CASES`, low in pre-push and high in
    `make fuzz-long`.
- **Done when:**
  - each libFuzzer target has run 1 hour with no crash, recorded in the PR;
  - the property test passes 10,000 cases;
  - a deliberately broken `amounts` (off by one lot) and a disabled nonce check are each
    caught, as failing controls run once and recorded.

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
- **Known limit, documented:** logs can be truncated (10 KB per transaction) or dropped by
  RPC nodes. Indexers must treat the event as a hint and `MakerState.fills` plus the token
  balances as truth. A self-CPI event (as in Anchor's `emit_cpi`) is more robust, but it
  costs a CPI and a PDA. It was rejected for now, and the PR records the CU of both.
- **Done when:** LiteSVM tests decode the event for both sides and both token programs, and
  no event is emitted on any failed `settle`.

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

### I9. RFQ demo on a metalbook series (S, plus dependencies)

**Demo series:** a quotebook series with these parameters:

| Parameter | Value |
| --- | --- |
| base | a metalbook long-token mint (0 decimals) |
| `base_lot` | 1 |
| quote | Circle devnet USDC |
| `quote_lot` | 10,000 (a 0.01 USDC tick) |
| `min_size` | 1 |
| `max_quote_ttl` | 60 s |

**The fill:** the maker writes metalbook contracts, holds the long tokens and asks with the
CLI maker's fixed mid, taken from metalbook-maker's theoretical price for that series. The
taker buys one long token for USDC. Recorded in `docs/devnet.md`, with a cross-link for
metalbook's devnet log.

**Dependencies, to be resolved through SOLANA-PROJECTS before this milestone starts:**

- **A live faucet-sized series.** Every 1 oz series has expired, and 50 oz XAG needs 240+
  USDC. metalbook's window lists a 1 oz XAG series with a Nov 13 or later expiry, as it did
  for Oct 30.
- **Which key makes.** The metalbook devnet maker (`7tVP…`, 40 USDC) already writes. If it
  may also make on quotebook, it needs `init_maker` and a delegate approval of its long-token
  account. The alternative is the quotebook maker (`5btd…`), funded with USDC and
  writing its own contracts.
- **Taker USDC:** a few USDC to `3ZBj…` from the faucet or from the metalbook maker.

Not a hosted service: a scripted, reproducible run. A hosted maker priced from Pyth is out of
scope here.

### I10. Fees and maker registry: decisions, then implementation (S to decide, M to build)

Two short ADRs (`docs/adr/0001-series-fees.md`, `docs/adr/0002-maker-registry.md`) go to
Codex and SOLANA-PROJECTS before any code.

**Fees: recommended design.**

- **`fee_bps`:** `fee_bps: u16` takes the first 2 of `Series._reserved`'s 8 bytes.
  - It is set by `create_series` and never changed: there is no instruction that writes it.
  - It is capped at `MAX_FEE_BPS = 100`.
  - Existing series read 0, so they need no migration and their CU is unchanged. The account
    `VERSION` stays 1.
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
- **Cost:** about one more `transfer_checked`, measured in `make bench`.
- **Tests:** I6's conservation property test extends to the fee account.

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
| I9 | metalbook RFQ demo | S | external dependencies |
| I10 | Fee and registry ADRs, then code, then devnet upgrade | S + M | I6, I7 |
| I11 | Release 0.2.0 | S | I10, GO |
| L1 | v2 package quotes design | L (design only) | I10 |

## Open questions for SOLANA-PROJECTS

1. **Version drift.** There is none on `main` (see Facts). Did msg 43338 mean the devnet
   binary, which I2 covers, or something else?
2. **I9 dependencies:** the metalbook 1 oz series, which maker key, and taker USDC.
3. **I3:** is a devnet rehearsal with two plain keys enough, or should Squads also be
   rehearsed on devnet?
4. **I10 fees:** is `MAX_FEE_BPS = 100` right, and is a taker-paid fee the intent?
