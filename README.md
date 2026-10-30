[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Stars](https://img.shields.io/github/stars/joaquinbejar/quotebook.svg)](https://github.com/joaquinbejar/quotebook/stargazers)
[![Issues](https://img.shields.io/github/issues/joaquinbejar/quotebook.svg)](https://github.com/joaquinbejar/quotebook/issues)
[![PRs](https://img.shields.io/github/issues-pr/joaquinbejar/quotebook.svg)](https://github.com/joaquinbejar/quotebook/pulls)

## QuoteBook v0.1.0: Firm RFQ Quotes on Solana

### Table of Contents
1. [Introduction](#introduction)
2. [Status](#status)
3. [Features](#features)
4. [Architecture](#architecture)
5. [Accounts](#accounts)
6. [Instructions](#instructions)
7. [Quote Format](#quote-format)
8. [Nonces, Cancels and Restarts](#nonces-cancels-and-restarts)
9. [Token-2022](#token-2022)
10. [Setup Instructions](#setup-instructions)
11. [Usage Examples](#usage-examples)
12. [Testing and Benchmarks](#testing-and-benchmarks)
13. [Compute Units](#compute-units)
14. [Error Codes](#error-codes)
15. [Security Notes](#security-notes)
16. [Contribution and Contact](#contribution-and-contact)

### Introduction

Request-for-quote trading on Solana with firm quotes signed off chain.

A maker signs a quote with its ed25519 key and hands it to a taker. The taker submits one
transaction: the ed25519 precompile instruction that verifies the signature, then
quotebook's `settle`, which proves through the instructions sysvar that the precompile
verified exactly this quote by this maker, checks its time window and replay state, and swaps
the two tokens atomically from both wallets. There is no order book, no deposit, no maker
transaction per quote and no crank. Withdrawing every outstanding quote is one `cancel_all`.

### Status

v1 is complete through milestone 6 of [docs/PLAN.md](docs/PLAN.md), and the milestone 7
tooling (admin CLI, maker binary, taker CLI over RPC) is merged. The program is **deployed on
devnet** as `3Aq1AwFiK9ibNBLxvYQ18KLT9WbLwENQa3ZZViDW6mbo`, with one test series and a settled
demo fill; addresses and signatures are in [docs/devnet.md](docs/devnet.md). It is not on
mainnet. The crates are not published on crates.io; use them from this repository.

### Features

- **One transaction per fill**: `[ed25519 precompile, settle]`; the maker sends nothing per quote.
- **No deposits**: makers approve a delegate PDA on their own token accounts; `settle` can only
  move funds of the maker named in the verified quote.
- **Signature binding**: the program reads the exact bytes the precompile verified, from a
  self-contained precompile instruction (every index `0xFFFF`), and decodes a canonical
  208-byte quote bound to program, series, deployment and optionally taker.
- **Replay protection**: a 16,384-nonce sliding window per maker, `cancel_all` and `cancel_nonce`.
- **Partial fills** between the series `min_size` and the quoted size (one fill per nonce).
- **SPL Token and Token-2022**, with an explicit extension allowlist.
- **Pinocchio 0.11**, `no_std`, no allocator, zero-copy `Pod` accounts; `settle` costs about
  8.6k CU on SPL Token.
- **Off-chain tooling**: a Rust SDK, a maker service (market-maker-rs Avellaneda-Stoikov
  pricing, price bounds, nonce issuance, WebSocket RFQ), a taker client, and three CLIs.
- **Strict lints**: `unsafe_code`, `panic`, `unwrap_used`, `indexing_slicing`,
  `arithmetic_side_effects` and `float_arithmetic` are denied workspace wide, with a lint
  control crate that proves the lints fire.

### Architecture

| Crate | Path | What it is |
| --- | --- | --- |
| `quotebook-core` | `quotebook-core/` | `no_std`, allocation free, shared by everything below: quantity newtypes, the signed `Quote` and its canonical encoding, the precompile parser (`sigverify`), the nonce window, account layouts, instruction encodings, error codes |
| `quotebook-program` | `program/` | The on-chain program: Pinocchio 0.11, `no_std`, no allocator, zero-copy `Pod` accounts |
| `quotebook-sdk` | `sdk/` | Rust client SDK: PDAs, a builder for every instruction, the precompile + `settle` pair, account decoders |
| `quotebook-maker` | `maker/` | Off-chain maker service (market-maker-rs pricing, price bounds, nonce issuance, signing, WebSocket RFQ) and the taker client (request, verify, rank) |
| `quotebook-cli` | `cli/` | Tools over RPC: `quotebook-admin`, `quotebook-maker` (the service as a binary), `quotebook-taker`; see [docs/devnet.md](docs/devnet.md) |

```text
 taker                               maker service
   |  QuoteRequest (WebSocket JSON)        |
   |-------------------------------------->|  price (Avellaneda-Stoikov), bounds check,
   |                                       |  ticks rounded toward the maker, nonce,
   |  Quote (208 bytes) + signature        |  ed25519 sign
   |<--------------------------------------|
   |
   |  verify (signature + every on-chain rule), rank by price
   |
   |  transaction: [ed25519 precompile(quote, signature), settle(sig_ix_index, fill_size)]
   v
 quotebook program: read the verified quote from the precompile instruction, check it,
 consume the nonce, transfer_checked base and quote legs (maker leg via its delegate PDA)
```

Makers do not deposit: each approves its own delegate PDA, `["delegate", maker]`, on the token
account it pays from, and `settle` moves the maker's leg with that delegate and the taker's
leg with the taker's signature. A `settle` bug can only reach the maker named in the verified
quote.

### Accounts

Every account starts with an 8-byte header `{ tag, version, bump, _pad[5] }` and is `Pod`.

| Account | Seeds | Size | Holds |
| --- | --- | ---: | --- |
| `Config` | `["config"]` | 64 | admin, series count, `deployment_id` (first 16 bytes of the cluster's genesis hash) |
| `Series` | `["series", base_mint, quote_mint, series_index_le]` | 144 | mints, lot sizes, `max_quote_ttl`, `min_size`, paused flag, Token-2022 flags, a copy of `deployment_id` |
| `MakerState` | `["maker", maker]` | 2,120 | fill count, delegate bump, the 16,384-nonce replay window. Permanent: there is no close in v1, because re-creating it would reset the replay window |
| delegate | `["delegate", maker]` | none | signs the maker's leg of `settle` |

### Instructions

| Tag | Instruction | Signers | Effect |
| ---: | --- | --- | --- |
| 0 | `init_config` | admin, payer | Creates `Config`. The admin must be the program's upgrade authority (read from its `ProgramData`), so the deployment cannot be front-run |
| 7 | `set_config` | admin | Hands the admin role to another key; `deployment_id` never changes |
| 1 | `create_series` | admin, payer | Creates a `Series` for two mints, checking both (Token-2022 allowlist below) |
| 2 | `set_series_paused` | admin | Pauses or resumes a series |
| 3 | `init_maker` | maker, payer | Creates the signer's `MakerState` |
| 4 | `cancel_all` | maker | Every nonce below `new_min` stops being fillable |
| 5 | `cancel_nonce` | maker | One nonce stops being fillable |
| 6 | `settle` | taker | Fills a signed quote read from the ed25519 precompile instruction at `sig_ix_index` |

Every PDA is created with its canonical bump and survives pre-funding (anyone can send
lamports to a PDA address before it exists). The program's account order for each instruction
is documented in `program/src/processor/`; the SDK builds them in that order.

### Quote Format

A quote is 208 bytes, little-endian, defined once in `quotebook_core::Quote`. The program never
re-serializes it: it decodes exactly the bytes the precompile verified.

| Offset | Size | Field |
| ---: | ---: | --- |
| 0 | 16 | domain tag `quotebook-v1\0\0\0\0` |
| 16 | 32 | program id |
| 48 | 16 | deployment id |
| 64 | 32 | maker (the signing key) |
| 96 | 32 | series |
| 128 | 32 | taker, or all zeros for an open quote |
| 160 | 1 | side: 0 = maker bids (buys base), 1 = maker asks |
| 161 | 7 | zero padding |
| 168 | 8 | price, ticks: quote lots per base lot |
| 176 | 8 | size, base lots (the most a taker may fill) |
| 184 | 8 | `issued_at`, Unix seconds |
| 192 | 8 | `expires_at`, Unix seconds |
| 200 | 8 | nonce |

Decoding accepts exactly the outputs of encoding (property tested), so the encoding is
canonical. The precompile instruction must hold exactly one signature with every instruction
index `0xFFFF` (signature, key and message inside the precompile instruction itself), which
closes the classic "verify one thing, read another" attack.

`settle` fills a quote when, in order: it is bound to this program, series and deployment; the
series is not paused; the taker is not the maker and is the bound taker (or the quote is open);
`issued_at − 10 s ≤ now < expires_at` and `expires_at − issued_at ≤ max_quote_ttl`;
`min_size ≤ fill_size ≤ size`; both amounts fit `u64`; and the nonce is fresh. A fill uses the
nonce whatever its size: the rest of the quote is void.

### Nonces, Cancels and Restarts

Each maker has a 16,384-bit replay window above a watermark. A nonce at or beyond the window's
top slides it forward and drops the oldest nonces. `cancel_all(new_min)` is the same slide,
driven by the maker. The window is exact; what makes sliding safe is a rule the maker follows
when issuing nonces:

> for every nonce `M`, `issued_at(M) ≥ expires_at(N) + MAX_CLOCK_SKEW` for every
> `N ≤ M − 16,384`

A fill of `M` lands no earlier than `issued_at(M) − MAX_CLOCK_SKEW`, when every quote it slides
out has already expired. As a rate: at most 16,384 nonces per `max_quote_ttl + 10 s`, about
234 quotes per second at 60 s. `quotebook_core::min_issued_at_beyond_window` computes the
floor, and the maker service's `NonceIssuer` enforces it.

Restarts are fail-closed. The issuer's history is in memory, and the chain only bounds each
quote's own lifetime, so a restarted maker must not sign a nonce that evicts a quote it signed
before. `NonceIssuer::resume` refuses every quote until `started_at + max_quote_ttl + 10 s`
(a 70 s cold start at 60 s), and then until it is refreshed with the maker's on-chain window
read from that time on, so it starts above every nonce used.

### Token-2022

Series mints may be SPL Token or Token-2022. A Token-2022 mint may carry only these
extensions: `MintCloseAuthority`, `DefaultAccountState` (not `Frozen`),
`InterestBearingConfig`, `MetadataPointer`, `TokenMetadata`, `GroupPointer`, `TokenGroup`,
`GroupMemberPointer`, `TokenGroupMember`, `ScaledUiAmount`. Everything else is rejected at
`create_series`, including transfer fees, transfer hooks, confidential transfers, permanent
delegates, non-transferable and pausable mints, and any type added after
spl-token-2022-interface 3.1.2. At `settle`, CPI Guard on the taker's paying account and
required memos on a receiving account are rejected up front.

### Setup Instructions

Requirements: the Rust toolchain pinned in `rust-toolchain.toml`, and the Agave CLI for
`cargo build-sbf` (needed by `make program`, `make test` and `make bench`).

```sh
git clone https://github.com/joaquinbejar/quotebook.git
cd quotebook
make program                              # builds target/deploy/quotebook_program.so
cargo build --release -p quotebook-cli    # quotebook-admin, quotebook-maker, quotebook-taker
```

### Usage Examples

#### Local demo

```sh
cargo run -p quotebook-maker --example demo   # two makers and a taker, settled in LiteSVM
```

Two maker services price from a fixed mid with different risk aversion; the taker requests a
quote from both, verifies each against every rule the program checks, ranks them and settles
the best.

#### Building a fill with the SDK

```rust
use quotebook_sdk::{settle_instructions, SettleAccounts};
use quotebook_sdk::core::{BaseLots, Quote};

// `quote` and `signature` come from the maker (208 bytes + 64 bytes);
// `accounts` names the taker, both mints, their token programs and the four token accounts.
let [precompile_ix, settle_ix] =
    settle_instructions(&program_id, &quote, &signature, &accounts, BaseLots::new(10), 0);
// Send [precompile_ix, settle_ix] in that order, signed by the taker.
```

`quotebook_sdk::ix` has a builder for every instruction (`init_config`, `create_series`,
`init_maker`, `cancel_all`, `cancel_nonce`, `set_series_paused`, `set_config`, `settle`,
`approve_delegate`), `quotebook_sdk::pda` derives every address and `quotebook_sdk::decode`
reads the accounts.

#### CLI over RPC

With `QB="--url <RPC_URL> --program-id <PROGRAM_ID>"` (keypair options take file paths):

```sh
quotebook-admin init-config $QB --admin upgrade-authority.json --payer payer.json
quotebook-admin create-series $QB --admin upgrade-authority.json --payer payer.json \
  --base <BASE_MINT> --quote <QUOTE_MINT> --base-lot <atoms> --quote-lot <atoms> \
  --max-ttl 60 --min-size 1
quotebook-admin init-maker $QB --maker maker.json --payer payer.json
quotebook-admin approve $QB --maker maker.json --payer payer.json \
  --account <MAKER_TOKEN_ACCOUNT> --amount <ATOMS>

quotebook-maker $QB --series <SERIES> --maker maker.json --maker-base <ACCOUNT> \
  --maker-quote <ACCOUNT> --listen 0.0.0.0:9000 --mid 25 --ttl 30

quotebook-taker settle $QB --maker ws://<host>:9000 --series <SERIES> --side buy \
  --size <lots> --taker taker.json --taker-base <ACCOUNT> --taker-quote <ACCOUNT>
```

`quotebook-admin` also has `show`, `show-series`, `cancel-all`, `cancel-nonce`, `pause` and
`resume`; `quotebook-taker quote` verifies and prints a quote without sending it. The full
runbook is [docs/devnet.md](docs/devnet.md).

### Testing and Benchmarks

```sh
make pre-push   # fmt check, clippy -D warnings, lint-control, program-control, SBF build, tests, docs
make test       # unit, property and LiteSVM integration tests (builds the program first)
make bench      # regenerate docs/benchmarks.md
```

The CLIs are tested end to end against the program in LiteSVM through their `Rpc` trait
(`cli/tests/e2e.rs`), never against a network.

### Compute Units

Measured in LiteSVM by `make bench`; full table and method in
[docs/benchmarks.md](docs/benchmarks.md).

| Instruction | CU |
| --- | ---: |
| `init_config` | 4,645 |
| `create_series` (SPL Token / Token-2022) | 7,892 / 8,460 |
| `init_maker` | 4,554 |
| `cancel_nonce`, `cancel_all` (worst case) | 1,676 to 1,788 |
| `set_series_paused` / `set_config` | 3,254 / 1,684 |
| `settle`, SPL Token (common / worst case) | 8,593 / 8,627 |
| `settle`, Token-2022 (common / worst case) | 11,998 / 18,188 |
| ed25519 precompile | 0 (charged as one signature in the fee) |

### Error Codes

`settle` and the other instructions fail with `ProgramError::Custom(code)`;
`QuotebookError::from_code` decodes them.

| Code | Error | Meaning |
| ---: | --- | --- |
| 100 | `InvalidInstruction` | unknown instruction |
| 101 | `InvalidInstructionData` | invalid instruction data |
| 102 | `MissingSignature` | a required signer did not sign |
| 103 | `InvalidAccountOwner` | account has the wrong owner program |
| 104 | `InvalidPda` | account address does not match its PDA seeds |
| 105 | `InvalidMint` | mint does not match the series, or is not an initialized token mint |
| 106 | `InvalidVaultOrTokenAccount` | token account has the wrong program, owner, mint or state |
| 107 | `InvalidSysvar` | sysvar account has the wrong address |
| 108 | `MissingSigVerify` | no ed25519 instruction at the expected index, before `settle` |
| 109 | `MalformedSigVerify` | not exactly one signature, or offsets out of range |
| 110 | `SigVerifyNotSelfContained` | an instruction index is not `0xFFFF` |
| 111 | `SignerMismatch` | signed public key is not the quote's maker |
| 112 | `QuoteLengthMismatch` | signed message is not 208 bytes |
| 113 | `WrongDomain` | quote domain tag mismatch |
| 114 | `WrongProgram` | quote signed for another program |
| 115 | `WrongSeries` | quote signed for another series or deployment |
| 116 | `WrongTaker` | quote bound to another taker |
| 117 | `QuoteExpired` | quote expired |
| 118 | `QuoteTtlTooLong` | quote lifetime too long, or issued more than 10 s ahead |
| 119 | `NonceCancelled` | nonce below the last `cancel_all` |
| 120 | `NonceUsed` | nonce already filled or cancelled |
| 121 | `NonceOutOfWindow` | nonce slid out of the replay window |
| 122 | `FillSizeInvalid` | fill size zero, above the quote, or below the series minimum |
| 123 | `SizeNotOnLot` | reserved; not returned in v1 |
| 124 | `Overflow` | an amount does not fit its integer type |
| 125 | `SeriesPaused` | series paused |
| 126 | `NotAuthority` | signer is not the configured authority |
| 127 | `MintExtensionUnsupported` | Token-2022 mint extension outside the allowlist |
| 128 | `MalformedQuote` | quote bytes not canonical (side, padding, zero price or size) |
| 129 | `WatermarkNotIncreasing` | `cancel_all` must raise the watermark |
| 130 | `SelfTrade` | the taker is the quote's maker |
| 131 | `AccountExtensionUnsupported` | CPI Guard on the taker's paying account, or required memos on a receiving account |

### Security Notes

- The program is **unaudited** and deployed only on devnet; do not use it with real funds.
- `init_config` can only be signed by the program's upgrade authority, so the deployment
  cannot be front-run. Deploy with the upgradeable loader, never loader-v4.
- Quotes are bound to program id, series, deployment id (genesis hash prefix) and
  optionally the taker, so a signed quote cannot be replayed on another cluster or series.
- A maker's exposure is capped by the delegate allowance it approves on each token account.
- A restarted maker service cold-starts for `max_quote_ttl + 10 s` and refreshes from the
  on-chain window before signing again (fail closed).
- Keep keypair files out of the repository (`.gitignore` ignores `keys/` and `*keypair*.json`).

Every change follows [rules/global_rules.md](rules/global_rules.md); design decisions and their
reasons are in [docs/PLAN.md](docs/PLAN.md).

## Contribution and Contact

We welcome contributions to this project! If you would like to contribute, please follow these steps:

1. Fork the repository.
2. Create a new branch for your feature or bug fix.
3. Follow [rules/global_rules.md](https://github.com/joaquinbejar/quotebook/blob/main/rules/global_rules.md) and run
   `make pre-push` before pushing.
4. Make your changes and ensure that the project still builds and all tests pass.
5. Commit your changes and push your branch to your forked repository.
6. Submit a pull request to the main repository.

If you have any questions, issues, or would like to provide feedback, please feel free to contact the project maintainer:


### **Contact Information**

- **Author**: Joaquín Béjar García <https://joaquinbejar.capitaldelta.co/>
- **Email**: jb@taunais.com
- **Telegram**: [@joaquin_bejar](https://t.me/joaquin_bejar)
- **Repository**: <https://github.com/joaquinbejar/quotebook>
- **Documentation**: <https://github.com/joaquinbejar/quotebook/tree/main/docs>

We appreciate your interest and look forward to your contributions!

## ✍️ License

Licensed under **MIT** license

<!-- related-projects:start -->
## Related projects

Repositories by the same author that this project depends on, and repositories that depend on it.

### Depends on

| Repository | Description |
|------------|-------------|
| [market-maker-rs](https://github.com/joaquinbejar/market-maker-rs) · [crates.io](https://crates.io/crates/market-maker-rs) | Quantitative market making strategies, starting with the Avellaneda-Stoikov model. |

### Reference

| Repository | Description |
|------------|-------------|
| [otc-rfq](https://github.com/joaquinbejar/otc-rfq) | Off-chain OTC RFQ service; its price bounds and best-price ranking patterns are reused (copied with attribution, no dependency). |

### Used by

| Repository | Description |
|------------|-------------|
| [metalbook](https://github.com/joaquinbejar/metalbook) | Cash-settled European options on gold and silver (XAU, XAG) on Solana, settled in USDC from Pyth. |

<!-- related-projects:end -->
