# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `quotebook-demo-maker` (`demo/`, D3): the hosted demo maker on devnet, D1's multi-series service
  over D2's marks on a loopback address, with a finalized RPC `ChainView` (one-slot snapshots with
  account checks, the fill history walked page by page, failing closed), each spot's age in
  `Status`, JSON log lines in a size-rotated file, and `status`, `drain`, `revoke` and `accounts`
  commands; `scripts/demo-agent.sh` installs it under launchd and runs halt and revoke.
- `quotebook-demo-watch` (`demo/`, D4): the demo maker's read-only watcher on devnet. It holds no
  key; it reads the chain at `finalized`, the maker's `Status` with a 2 s timeout, the halt file and
  the taker's `trades.jsonl`, and alerts on `balance`, `inventory`, `collateral`, `allowance`,
  `maker-down`, `stale-spot` (one `expected` line per listed closure instead of an alert, until 600 s
  after the reopening), `risk-unavailable`, `halted`, `writer-cap`, `no-series`, `fill-error` and
  `rpc`. Each alert is one line in `--alerts-log` and runs `QUOTEBOOK_WATCH_ALERT_CMD` if set, and
  comes back after 3,600 s while active; the JSON lines and the alert log rotate at 10 MB, 5 kept. `scripts/demo-watch-agent.sh` installs it under launchd.
- `quotebook-demo-roll` (`demo/`, D5): the demo maker's writer cycle on devnet. One run pairs each
  tradable expiry of the 1 oz XAG capped calls at the strike nearest spot, creates its quotebook
  series (the devnet admin's only signature), mints up to the band, the unlocked USDC and the writer
  cap, approves the delegate, and after settlement claims, redeems and revokes; the mints,
  approvals, claims, redeems and revokes run only between a halt and drain and a finalized
  confirmation, planned again after the drain. `plan` is the dry run. `scripts/demo-roll.sh` runs it
  under launchd every 6 hours with a lock, rewrites the pairs and restarts the maker and the
  watcher; a failed cycle leaves the halt file.
- `quotebook-demo-taker` (`demo/`, D5): one 1 lot fill against the demo maker per run, checked as
  `quotebook-taker` checks it, sides chosen to keep the inventory flat, one line per run in
  `trades.jsonl` (D4's contract plus the fill, read at `finalized`); it skips outside market hours,
  in the settlement window, while halted and with no inventory. `--dry-run` requests no quote.
  `scripts/demo-taker-agent.sh` runs it under launchd every 6 hours.
- `quotebook-demo`: `cycle` (the cycle's finalized view, plan and runner) and `taker`;
  `writer_cap::read_positions`, `exposure_of` and `Exposure::room`.
- `quotebook-cli`: `admin::create_associated_account`, the Associated Token Account program's
  `CreateIdempotent`.
- `quotebook-demo`: `writer_cap`, the cap on contracts written (2 contracts, 10.40 USDC committed)
  read from every metalbook `Writer` position at `finalized`, failing closed; and
  `schedule::closure_at`, `last_closure_before` and `utc_iso`.
- `quotebook-maker`: `client::request_status`.
- `jupiter-rfq` (`jupiter-rfq/`, not published; `docs/PLAN-phase2.md`, D6): an off-chain
  prototype of a Jupiter RFQ V1 webhook over quotebook's pricing. `POST /quote` prices in atoms
  with `quotebook_maker::pricing` within a deadline and declines with 404; `POST /swap` decodes the
  versioned transaction with a hand parser and applies the design's validator (allowlisted
  programs, one 37-byte `fill` matching the quote, the taker's signature) and fee budget
  (`u128` checked, caps per transaction and per hour, a SOL floor reserved under the lock);
  `GET /tokens`. Jupiter's fee on landed fills is debt held against the balance until a recorded
  collection releases it, kept in a durable ledger (`--debt-ledger`) that fails closed; a retried
  swap is answered the same only while its reservation is held and can still land, and gets the
  terminal answer once it is released. It holds no key and never signs or sends: a fill that
  passes is answered `rejected`. The `acceptance` feature answers the toolkit suite's magic
  request ids.
- `quotebook-cli`: `Rpc::finalized_accounts`, `finalized_signatures`, `finalized_transaction` and
  `finalized_program_accounts`, with `Cluster` implementations.

### Fixed

- `quotebook-demo`: the roll plan counts the rent metalbook charges the maker key for a new
  position, and logs the drain it waited for. The first live apply failed on a maker with no SOL.
- `quotebook-maker`: after a restart, `Status.newest_expires_at` is at least the end of the cold
  start, which bounds the quotes the previous run signed. A drain started right after a restart
  ended at once while one of those quotes could still fill.

## [0.2.0] - 2026-11-07

The on-chain program is unchanged from `3d0ec13`, the commit frozen for the audit and deployed on
devnet; these crates match it. 0.1.x clients cannot settle against it: `settle` now takes the fee
account.

### Added

- `quotebook-demo` (`demo/`, not published): metalbook series priced with one optfix 0.3
  `price_strip` per expiry, spot from the Pyth push feeds over RPC, failing closed on age, market
  hours and the settlement window (`docs/PLAN-phase2.md`, D2).
- Series fees (ADR 0001): `create_series` takes an optional `fee_bps` (at most
  `MAX_FEE_BPS` = 100), stored immutably in the series; the taker pays it, floor-rounded, into a
  Config-owned fee account; `withdraw_fees` (tag 8) moves collected fees out.
- The maker registry (ADR 0002): `set_profile` (tag 9) and `close_profile` (tag 10) over a
  `MakerProfile` PDA `["profile", maker]`; `quotebook-taker discover` and `--discover` /
  `--allow` on `quote` and `settle`; `quotebook-admin set-profile` and `close-profile`.
- `quotebook-maker`: the WebSocket client speaks TLS (`wss://`).
- `quotebook-maker`: `MakerService::multi` serves several series under one key, with a risk book
  (`risk`): one lock over limits, reservations and nonces; reservations released only by a
  finalized snapshot; `max_open_quotes`, `max_notional`, an inventory band, `max_fills_per_hour`
  (rebuilt from the chain after a restart), `allowed_takers` and a halt file; every failed or stale
  read fails closed. A `status` request reports the service's state without taking a nonce. The
  single-series constructor keeps its behaviour.

### Changed (breaking)

- `quotebook-maker`: `RejectReason` gains `UnknownSeries`, `RiskLimit`, `RiskUnavailable`, `Halted`
  and `TakerNotAllowed`; `ClientMessage` and `ServerMessage` gain `Status`; `ConfigError` gains
  `NoMarkets`, `DuplicateSeries`, `MixedDeployments` and `Issuer`.
- `quotebook-core`: `QuotebookError` gains `FeeTooHigh` (132), `InsufficientFees` (133) and
  `InvalidProfile` (134), and `ALL` has 35 entries; `Series._reserved` becomes `fee_bps` plus
  6 reserved bytes; `CreateSeriesArgs` gains `fee_bps` and `encode` returns
  `EncodedCreateSeriesArgs` (32 or 34 bytes); new `fee`, `profile` and `MakerProfile`.
- `quotebook-sdk`: `SettleAccounts` gains `fee_account: Option<Address>`, so struct literals
  must set it; new `withdraw_fees`, `create_fee_account`, `set_profile`, `close_profile` and
  `maker_profile`.
- `quotebook-maker` (I12, risk relative to the mid): `PricingParams` gains
  `min_half_spread_bps`, `max_half_spread_bps`, `max_skew_bps` and `model_units` (`ModelUnits`);
  `quote_prices` is replaced by `clamped_prices` and `quote`; `PricingError::OutOfBounds` is
  replaced by `NonPositiveBid`, `Crossed`, `TickTooCoarse` and `Tick`; new `PricingConfigError`
  and `market` (`MarketInfo`, `Rounding`, `TickError`).

## [0.1.1] - 2026-10-30

### Fixed

- Include the MIT license text in the packages: `quotebook-core`, `quotebook-sdk` and
  `quotebook-maker` each carry a copy of `LICENSE`. No code changes.

## [0.1.0] - 2026-10-30

First release. Published on crates.io: `quotebook-core`, `quotebook-sdk` and
`quotebook-maker`. The on-chain program (`quotebook-program`) and the command-line tools
(`quotebook-cli`) are built from this repository and not published. The program is unaudited
and deployed only on devnet, as `3Aq1AwFiK9ibNBLxvYQ18KLT9WbLwENQa3ZZViDW6mbo`
([docs/devnet.md](docs/devnet.md)).

### Added

- `quotebook-core`: `no_std`, allocation-free shared types. Quantity newtypes, the 208-byte
  signed `Quote` with a canonical encoding (decode accepts exactly what encode produces), the
  ed25519 precompile parser that requires a self-contained instruction (every index `0xFFFF`),
  the 16,384-nonce replay window and its issuance floor (`min_issued_at_beyond_window`),
  account layouts, instruction encodings and error codes 100 to 131 (#4, #7, #8).
- On-chain program on Pinocchio 0.11: `init_config` gated on the upgrade authority read from
  `ProgramData`, `set_config`, `create_series` with a Token-2022 mint extension allowlist,
  `set_series_paused`, `init_maker`, `cancel_all`, `cancel_nonce` and `settle`. `settle`
  reads the verified quote from the instructions sysvar, checks binding, time window, size and
  replay state, and moves both legs with `transfer_checked`; the maker leg goes through its
  delegate PDA, so makers keep their tokens until a fill (#7, #8).
- `quotebook-sdk`: PDAs, a builder for every instruction, the `[ed25519 precompile, settle]`
  pair, the delegate approval and account decoders (#9).
- Compute-unit benchmarks for every instruction in `docs/benchmarks.md`, produced by
  `make bench`; `settle` is 8,593 CU on SPL Token (#9).
- `quotebook-maker`: maker service that prices with market-maker-rs (Avellaneda-Stoikov),
  rounds ticks toward the maker with exact integer math, issues nonces under the window rule,
  resumes fail-closed after a restart (cold start of `max_quote_ttl + 10 s`, then a refresh
  from chain) and serves takers over WebSocket. Also the taker client: request, verify the
  signature and every on-chain rule, rank (#10).
- `quotebook-cli`: `quotebook-admin`, `quotebook-maker` and `quotebook-taker` over RPC. RPC
  URLs and maker-provided text are redacted from errors and `Debug`, and keys are never
  logged (#12).
- Devnet deployment with a settled demo fill, recorded in `docs/devnet.md` (#14).

[Unreleased]: https://github.com/joaquinbejar/quotebook/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/joaquinbejar/quotebook/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/joaquinbejar/quotebook/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/joaquinbejar/quotebook/releases/tag/v0.1.0
