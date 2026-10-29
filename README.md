# QuoteBook

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

Request-for-quote trading on Solana with quotes signed off chain.

A maker signs a quote off chain; the taker submits it, and the program verifies the ed25519 signature through instruction introspection, checks expiry and nonce, and settles atomically. Cancelling every outstanding quote is one nonce bump.

**Status:** milestone 2 of [docs/PLAN.md](docs/PLAN.md).

- `quotebook-core`: the `no_std` crate shared by the program and the clients, with the
  quantity newtypes, the signed `Quote` and its canonical 208-byte encoding, the per-maker
  nonce replay window and the program error codes.
- `program`: the Pinocchio program with its accounts (`Config`, `Series`, `MakerState`) and
  the instructions `init_config`, `create_series`, `init_maker`, `cancel_all` and
  `cancel_nonce`, tested in LiteSVM. Compute-unit costs are in
  [docs/benchmarks.md](docs/benchmarks.md).

`settle`, the taker SDK and the maker service follow in later milestones.

## Development

```sh
make pre-push   # fmt check, clippy -D warnings, lint-control, SBF build, tests, docs
make bench      # regenerate docs/benchmarks.md
```

`make program`, `make test` and `make bench` need the Agave CLI (`cargo build-sbf`). The rules every change follows are in
[rules/global_rules.md](rules/global_rules.md).
