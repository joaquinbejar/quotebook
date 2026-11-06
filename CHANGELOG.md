# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

### Changed (breaking, for 0.2.0)

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

[Unreleased]: https://github.com/joaquinbejar/quotebook/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/joaquinbejar/quotebook/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/joaquinbejar/quotebook/releases/tag/v0.1.0
