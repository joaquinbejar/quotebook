# QuoteBook

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

Request-for-quote trading on Solana with quotes signed off chain.

A maker signs a quote off chain; the taker submits it, and the program verifies the ed25519 signature through instruction introspection, checks expiry and nonce, and settles atomically. Cancelling every outstanding quote is one nonce bump.

**Status:** milestone 6 of [docs/PLAN.md](docs/PLAN.md).

- `quotebook-core`: the `no_std` crate shared by the program and the clients, with the
  quantity newtypes, the signed `Quote` and its canonical 208-byte encoding, the parser that
  reads the verified quote out of the ed25519 precompile instruction, the per-maker nonce
  replay window and the program error codes.
- `program`: the Pinocchio program with its accounts (`Config`, `Series`, `MakerState`) and
  every v1 instruction: `init_config`, `set_config`, `create_series`, `set_series_paused`,
  `init_maker`, `cancel_all`, `cancel_nonce` and `settle`, tested in LiteSVM against the real
  SPL Token, Token-2022 and ed25519 programs. Compute-unit costs are in
  [docs/benchmarks.md](docs/benchmarks.md), with the worst case of `settle` per token
  program and a cited comparison with matchbook.
- `sdk` (`quotebook-sdk`): Rust client SDK with every PDA, a builder for every instruction,
  the precompile + `settle` pair for a signed quote, and account decoders; tested end to end
  in LiteSVM.

- `maker` (`quotebook-maker`): the maker service (market-maker-rs pricing, price bounds,
  the nonce issuance bound, ed25519 signing, a WebSocket RFQ protocol) and the taker client
  (request, verify, rank). `cargo run -p quotebook-maker --example demo` runs two makers and a
  taker against the program in LiteSVM and settles the best quote.

Devnet deployment follows in milestone 7.

## Development

```sh
make pre-push   # fmt check, clippy -D warnings, lint-control, SBF build, tests, docs
make bench      # regenerate docs/benchmarks.md
```

`make program`, `make test` and `make bench` need the Agave CLI (`cargo build-sbf`). The rules every change follows are in
[rules/global_rules.md](rules/global_rules.md).
