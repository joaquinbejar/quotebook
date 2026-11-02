# Fuzzing and the conservation property test

Milestone I6 of [`PLAN-improvements.md`](PLAN-improvements.md). Two layers:

1. **The stateful program test** (`program/tests/conservation.rs`) runs in `make pre-push` and
   in CI's `test` job, on stable. Its default is 24 sequences; set `QUOTEBOOK_PROPTEST_CASES`
   to run more.
2. **libFuzzer targets on `quotebook-core`** (`fuzz/`) run on demand. CI's `fuzz-corpus` job
   replays the committed corpus on every PR.

## 1. Stateful LiteSVM property test

### Setup

- 2 makers and 2 takers.
- 4 series (I10, ADR 0001): per token program one without a fee and one with a fee. Series 0
  is SPL Token with the 32-byte v1 arguments (no fee), series 1 SPL Token at 100 bps, series 2
  Token-2022 with the 34-byte arguments and `fee_bps` 0, series 3 Token-2022 at 37 bps.
- One token account per (participant, series, mint), so 32 accounts. Each starts with
  10^12 atoms, and the makers' accounts are approved to their delegate PDAs.
- For each fee series, its Config-owned fee account (account 13 of `settle`) and a treasury
  account of another owner that `withdraw_fees` pays into, both starting empty: 36 token
  accounts in all.

### Steps

A sequence is 8 to 40 random operations:
- `settle` on either side and either series, with an open or taker-bound quote and a fresh,
  reused, cancelled or out-of-window nonce;
- `cancel_all`;
- `cancel_nonce`;
- `set_series_paused`;
- `withdraw_fees` from either fee account, for part of its balance, all of it, one atom more,
  or zero.

A `settle` either is valid or carries one mutation:
- a flipped signature byte;
- a quote for the other series;
- an expired quote;
- a fill above the quote's size;
- a zero fill;
- a quote bound to the other taker;
- the taker's base account from the other series.

Eight valid fills, one per (side, series), are inserted at random positions in every sequence.

### Oracle

A pure Rust model receives each step **before** it runs and predicts the outcome:
- success;
- the exact quotebook error and instruction index;
- or a precompile rejection.

It predicts in `settle`'s own check order:
1. the signature;
2. the series binding;
3. pause;
4. the taker;
5. time;
6. fill size;
7. the nonce, using the window rules of docs/PLAN.md 4.2;
8. the token accounts.

For `withdraw_fees` it predicts `InvalidInstructionData` for a zero amount, `InsufficientFees`
above the fee account's balance, and otherwise success.

The test asserts that the observed outcome equals the prediction. A regression that rejects
every valid `settle` therefore fails at the first step.

After every step it also checks:

| Check | What must hold |
| --- | --- |
| Conservation | The total of each mint across the 36 accounts (fee and treasury accounts included) is constant. |
| Exact movement | Each token account's amount and delegated amount equal the model's. A fill moves `fill × base_lot` and `price × fill × quote_lot` on the four accounts of that fill, plus the fee `floor(quote × fee_bps / 10,000)`, computed in the test in `u128` rather than through `quotebook_core::fee`: on an ask the taker pays it on top of the quote, on a bid it comes out of the taker's proceeds, and in both cases it lands in the series' fee account. A withdrawal moves exactly its amount from the fee account to the treasury. Nothing else changes: every byte outside amount and allowance is identical. |
| Allowances | The maker's delegated amount drops by exactly its outflow. |
| Replay state | For each maker, `MakerState.fills`, `window_base`, `last_cancel_all` and the used count equal the model's, and every nonce the model marks as used reads as `NonceUsed`. |
| Rollback | A rejected transaction leaves the config, the four series, both maker states and all 36 token accounts byte-identical. |
| Lamports | The fee payer pays exactly 5,000 lamports per signature, precompile signatures included (15,000 for a `settle`), success or not. No other account's lamports move. |
| Pause flags | Each series' `paused` flag equals the model's. |

The run fails if any (side, series) pair never filled, if no fill ever credited a fee
account, if no withdrawal ever succeeded, or if `withdraw_fees` never failed with each of
`InsufficientFees` and `InvalidInstructionData`. It prints the count of every outcome.

### Results (2026-11-02, Studio)

24 cases (pre-push) take about 2 s. 500 cases took 47 s, with this spread:

| Outcome | Count |
| --- | ---: |
| Success | 6,969 |
| `NonceUsed` | 1,441 |
| `SeriesPaused` | 1,276 |
| `FillSizeInvalid` | 898 |
| Precompile rejection | 552 |
| `WrongSeries` | 547 |
| `WrongTaker` | 485 |
| `WatermarkNotIncreasing` | 482 |
| `QuoteExpired` | 411 |
| `InvalidVaultOrTokenAccount` | 341 |
| `NonceCancelled` | 108 |
| `NonceOutOfWindow` | 70 |

Successful fills per (side, series): Bid on SPL 1,128; Ask on SPL 1,144; Bid on Token-2022
1,117; Ask on Token-2022 1,091.

10,000 cases (`make fuzz-long`): **passed** in 19 min 31 s (1,170.6 s of test time). There were
276,075 steps, every one matching the model's prediction:

| Outcome | Count |
| --- | ---: |
| Success | 139,000 |
| `NonceUsed` | 28,393 |
| `SeriesPaused` | 27,095 |
| `FillSizeInvalid` | 18,879 |
| Precompile rejection | 11,256 |
| `WrongSeries` | 11,067 |
| `WatermarkNotIncreasing` | 10,265 |
| `WrongTaker` | 9,567 |
| `QuoteExpired` | 9,349 |
| `InvalidVaultOrTokenAccount` | 6,758 |
| `NonceCancelled` | 2,921 |
| `NonceOutOfWindow` | 1,525 |

Successful fills per (side, series): Bid on SPL 22,083; Ask on SPL 22,144; Bid on Token-2022
21,983; Ask on Token-2022 21,920.

The results above are from I6, with two fee-free series. The next section is the I10 model.

### Results with fees and `withdraw_fees` (I10, 2026-11-02, Studio)

24 cases (pre-push) take about 3 s in a debug build. 10,000 cases in a release build
(`QUOTEBOOK_PROPTEST_CASES=10000 cargo test --release -p quotebook-program --test
conservation`) **passed** in 108 s of test time: 314,300 steps, every one matching the model's
prediction. The run was repeated on the program with the I10-02 fix (only an Ask forms
`quote_atoms + fee`) with identical counts; the fixed seed makes the sequences the same.

| Outcome | Count |
| --- | ---: |
| Success | 183,437 |
| `NonceUsed` | 28,265 |
| `FillSizeInvalid` | 18,222 |
| `SeriesPaused` | 15,881 |
| `WrongSeries` | 9,841 |
| Precompile rejection | 9,824 |
| `WrongTaker` | 9,177 |
| `QuoteExpired` | 9,136 |
| `InvalidInstructionData` (`withdraw_fees` of zero) | 8,725 |
| `WatermarkNotIncreasing` | 8,063 |
| `InvalidVaultOrTokenAccount` | 6,386 |
| `InsufficientFees` | 3,197 |
| `NonceCancelled` | 2,782 |
| `NonceOutOfWindow` | 1,364 |

Successful fills per (side, series), Bid then Ask: series 0 (SPL, no fee) 15,530 and 15,521;
series 1 (SPL, 100 bps) 15,531 and 15,493; series 2 (Token-2022, `fee_bps` 0) 15,665 and
15,404; series 3 (Token-2022, 37 bps) 15,644 and 15,666. 62,119 steps credited a fee account
and 12,708 withdrew from one.

### Failing controls

Each control was a temporary edit to `program/src/processor/settle.rs`, rebuilt and run at
24 cases, then reverted:

| Control | Caught by |
| --- | --- |
| `amounts` off by one lot (`(fill + 1) × base_lot`) | step 2, the amount of a token account: 1,000,000,016,000 observed vs 1,000,000,015,000 modelled |
| Nonce check disabled (`consume` removed) | step 2, the used count of the maker's window: 1 observed vs 2 modelled |
| `settle` always rejects | step 0, the outcome: `Overflow` observed vs the predicted error |
| I10: the fee rounded up instead of down (`quotebook-core/src/fee.rs`, `(product + 9,999) / 10,000`) | step 2, the amount of a token account of a fee fill: 999,999,935,352 modelled |

## 2. libFuzzer targets (`fuzz/`)

`fuzz/` is its own Cargo workspace (`exclude = ["fuzz"]` at the root) on `nightly-2026-10-11`,
pinned in `fuzz/rust-toolchain.toml`. cargo-fuzz needs nightly, and the main workspace stays on
stable: this is the rule exception the plan approved. The dependencies are `libfuzzer-sys` 0.4,
`arbitrary` 1.5 and `bytemuck` 1.25, and only `fuzz/` uses them.

| Target | Input | Oracle |
| --- | --- | --- |
| `sigverify` | raw bytes, or a valid record built by `sigverify::instruction_data` from arbitrary terms and then edited: the count, any of the 7 offset fields, any byte, a message byte, the public key, a truncation, extra bytes | `fuzz/src/lib.rs::reference_verified_quote` is an independent parser written from docs/PLAN.md 4.1. It predicts accept or the exact error: the count, the padding and record length, the three `0xFFFF` indexes, `message_data_size == 208`, the three ranges, the quote decode, and public key == `quote.maker`. The target asserts equality, and on accept that the quote re-encodes to the message bytes and that its maker is the verified key. |
| `quote_decode` | arbitrary bytes | a reference decode predicts the exact error; accept implies `encode(decode(x)) == x` |
| `instruction_args` | arbitrary bytes | `Tag::split` accepts exactly tags 0 to 10 (I10 adds 8 `withdraw_fees`, 9 `set_profile`, 10 `close_profile`). `CreateSeriesArgs::decode` takes 32 bytes, or 34 with a fee, and the reference predicts the exact error: `FeeTooHigh` above 100 bps, otherwise `InvalidInstructionData`. `ProfileArgs::decode` matches `reference_profile_args` (exact lengths, then the ADR 0002 text rules) error for error. Every other decoder accepts exactly the documented inputs, and every encoder round-trips. |
| `accounts_decode` | raw bytes, or a layout with the right length and header for one of the four account types plus edits; a `MakerProfile` body is first built canonical (a `wss://` endpoint of 7 to 128 bytes, a name of 0 to 32, zero tails) | `accounts::decode::<T>` accepts exactly the right length, tag and version for `Config`, `Series`, `MakerState` and `MakerProfile`, and re-encodes to the same bytes. For a decoded profile, `profile::check_canonical` must agree with `reference_profile_canonical`, written from ADR 0002: header and profile padding zero, the text rules, zero tails. |

`fuzz/tests/corpus_coverage.rs` (`make fuzz-check`) replays every committed corpus through
its oracle and fails unless each accepting path and rejection occurs. For `accounts_decode`
that is each type, a canonical and a non-canonical profile, and a rejected layout; for
`instruction_args`, every decoder accepting at least once, an I10 tag, `create_series` with a
fee and with `FeeTooHigh`, and `ProfileArgs` both accepted and `InvalidProfile`. For
`sigverify`, every outcome of `verified_quote`:

- `Ok`;
- `MalformedSigVerify`;
- `SigVerifyNotSelfContained`;
- `QuoteLengthMismatch`;
- `WrongDomain`;
- `MalformedQuote`;
- `SignerMismatch`.

So a corpus that only ever reaches the first length check cannot pass.

### How to run

```sh
make fuzz                 # each target for FUZZ_TIME seconds (default 60), grows fuzz/corpus
make fuzz-check           # replay the corpus (-runs=0) and the sigverify branch coverage
make fuzz-long            # one hour per target, then 10,000 conservation sequences
```

A crash leaves its input in `fuzz/artifacts/<target>/`. The fix comes with a stable regression
test in `quotebook-core/tests`.

### Results (2026-11-02, Studio, one hour per target)

Each target ran for 3,601 s: no crash, no artifact. `quote_decode` and `accounts_decode` were
rerun for an hour after they became structured targets (Codex review); the first table row
of each is that rerun. The edge counts are not comparable across targets: the structured
targets reach far more of the decoders than raw bytes did (28 and 29 edges before).

| Target | Executions | Coverage (edges) | Corpus after `cargo fuzz cmin` |
| --- | ---: | ---: | ---: |
| `sigverify` | 312,643,553 | 433 | 213 inputs |
| `quote_decode` (structured) | 387,346,157 | 345 | 178 inputs |
| `instruction_args` | 3,336,695,269 | 137 | 43 inputs |
| `accounts_decode` (structured) | 460,252,239 | 276 | 218 inputs |
| `instruction_args`, I10 (tags 8 to 10, fee args, `ProfileArgs`) | 1,685,936,597 | 209 | 56 inputs |
| `accounts_decode`, I10 (with `MakerProfile` and the canonical rule) | 411,005,968 | 542 | 301 inputs |

The two I10 rows are hour reruns (3,601 s each, in parallel, no crash, no artifact) of the
targets extended for fees and profiles, started from their earlier corpora plus seeds for the
new decoders; each corpus was minimized again after the run.

`make fuzz-check` replays every corpus and requires each accepting path and rejection:
- `sigverify`: `Ok` 51, `MalformedSigVerify` 123, `MalformedQuote` 12, `SignerMismatch` 10,
  `SigVerifyNotSelfContained` 9, `QuoteLengthMismatch` 4, `WrongDomain` 4;
- `quote_decode`: `Ok` 56, `QuoteLengthMismatch` 92, `WrongDomain` 19, `MalformedQuote` 11;
- `accounts_decode` (after I10): `MakerProfile` 73, `MakerProfileNonCanonical` 35,
  `MakerState` 15, `Config` 12, `Series` 4, rejected 162;
- `instruction_args` (after I10): every decoder accepts at least once (`Tag` 46, of which an
  I10 tag 8; `ProfileArgs` 8 and `InvalidProfile` 17; `deployment_id` 5; `new_admin` 4;
  `SettleArgs` 3; `u64` 3; `CreateSeriesArgs` 2, `FeeTooHigh` 2 and with a fee 1; `paused`
  2).

### Failing control

The reference parser's maker binding (public key == `quote.maker`) was removed temporarily,
and `sigverify` was fuzzed from an empty corpus. Within about 40,500 executions (under a
second) the target panicked: `core Err(SignerMismatch) but reference Ok { … }`. A reference
that skips a rule is caught at once. The change was reverted and the target rebuilt.
