# v2 package quotes (design)

**Status:** approved design (Codex, #35), milestone L1 of
[`PLAN-improvements.md`](../PLAN-improvements.md). No code is planned in this phase; its
product questions are decided (see Decisions).

## Problem

A maker wants to quote several legs as one price: a spread, a basket, a roll. Today each leg
is a separate v1 quote with its own signature. A taker can already settle several v1 quotes
in one transaction, which is atomic, but nothing forces it to: it can take the legs it likes
and leave the rest. The maker then holds a position it priced only as part of a package.

v2 signs the package as a whole, so its legs can only be filled together, in the same ratio.

## Signed message

The ed25519 message stays 208 bytes, so the precompile instruction layout and its
self-contained checks are shared with v1; only the domain and the layout behind it differ.
The parser itself has to be split for that (see Signature parsing).

| Offset | Bytes | Field |
| ---: | ---: | --- |
| 0 | 16 | domain `quotebook-v2\0\0\0\0` |
| 16 | 32 | program id |
| 48 | 16 | deployment (first 16 bytes of the genesis hash) |
| 64 | 32 | maker |
| 96 | 32 | taker, or all zeros for an open package |
| 128 | 32 | `package_hash` |
| 160 | 8 | `units`: the package size; at most this many units fill |
| 168 | 8 | `issued_at` |
| 176 | 8 | `expires_at` |
| 184 | 8 | nonce |
| 192 | 16 | reserved, zero |

- **Legs:** each leg is `(series, side, price, lots_per_unit)`: the maker's side, the price in
  ticks of that series, and the base lots of that leg in one unit of the package.
- **Hash:** `package_hash = sha256(leg_count: u8 ‖ leg_0 ‖ … ‖ leg_{n-1})`, each leg encoded
  as `series (32) ‖ side (1) ‖ price (8, LE) ‖ lots_per_unit (8, LE)`: 49 bytes.
- **Canonical order:** legs are strictly increasing by series address. This makes the
  encoding unique and rules out a duplicated series; a package that needs two legs on one
  series is quoted as one leg with the net size.
- **Domain separation:** a v1 `settle` rejects the v2 domain (`WrongDomain`), and
  `settle_package` rejects v1. No leg of a package can be taken alone.
- **Ratio:** a fill of `k` units moves `k × lots_per_unit` base lots on every leg. Integer
  multiples avoid any rounding between legs.

## Signature parsing

Today `sigverify::verified_quote` does, in this order: exactly one signature record
(`MalformedSigVerify`), every instruction index `0xFFFF` (`SigVerifyNotSelfContained`), a
208-byte message (`QuoteLengthMismatch`), the signature, key and message ranges inside the
data (`MalformedSigVerify`), then `Quote::decode` on the message (its v1 domain and field
errors) and the signing key equal to `quote.maker` (`SignerMismatch`). The last two steps are
v1 specific, so v2 cannot call it as it is.

The code splits it in two layers:

- **`verified_message(data) -> (key, message)`:** the domain-free part, the first four steps
  above with the same errors in the same order. It returns the 32-byte key and the 208-byte
  message exactly as the precompile verified them.
- **`verified_quote`:** `verified_message`, then `Quote::decode`, then the maker binding. Same
  signature, same errors, same order as today: a pure refactor for v1.
- **`verified_package`:** `verified_message`, then the v2 header decode (`WrongDomain` for any
  other domain, `MalformedQuote` for nonzero reserved bytes or zero units), then the signing
  key equal to the header's maker (`SignerMismatch`).

The 208-byte length check stays in the shared layer because both headers are 208 bytes; a
future header of another size would move it to the domain layer.

## Canonical validation

A matching hash only proves that the args are the legs the maker signed; it does not prove
the maker signed a valid package. The decoder enforces every rule below before the hash is
computed, with the same code on chain, in `client::verify` and in the SDK encoder (which
refuses to build a package that breaks one).

| Rule | Error |
| --- | --- |
| args are exactly `10 + 17 · leg_count` bytes | `InvalidInstructionData` |
| `1 ≤ leg_count ≤ MAX_LEGS` (7, see Limits) | `MalformedQuote` |
| the account list is exactly the shared prefix plus each leg's accounts, fee account included when that series has `fee_bps > 0` | `NotEnoughAccountKeys` / `InvalidInstructionData` |
| header domain is `quotebook-v2` | `WrongDomain` |
| header reserved bytes are zero | `MalformedQuote` |
| header `units > 0` | `MalformedQuote` |
| every leg: `side` is 0 (bid) or 1 (ask), `price > 0`, `lots_per_unit > 0` | `MalformedQuote` |
| series addresses strictly increasing, by bytes: no duplicate, no other order | `MalformedQuote` |
| `1 ≤ fill_units ≤ units` | `FillSizeInvalid` |
| `fill_units × lots_per_unit` and each leg's quote amount fit in `u64` | `Overflow` |
| `sha256` of the legs equals `package_hash` | new `PackageMismatch` |

These are the v1 decoder's rules (`Quote::decode`: side 0 or 1, zero padding, nonzero price
and size) extended to the leg list.

## `settle_package`

Instruction `[ed25519 precompile, settle_package]`, like v1.

- **Args:** `sig_ix_index: u8`, `fill_units: u64`, `leg_count: u8`, then per leg `side (1) ‖
  price (8) ‖ lots_per_unit (8)`: 17 bytes. The series of each leg is the series account at
  that leg's position, so it is not repeated in the data.
- **Accounts:** shared first: taker (signer), the maker's state, the delegate PDA, the
  instructions sysvar, the token program(s). Then per leg: series, base mint, quote mint,
  maker base, maker quote, taker base, taker quote, and the fee account when that series has
  `fee_bps > 0`.
- **Checks, in order:**
  1. `verified_package` (Signature parsing): the self-contained precompile checks, the v2
     header and the maker binding;
  2. the canonical rules above, then `sha256` of the legs (from the args and the series
     accounts) equals `package_hash`; `sol_sha256` is a syscall, so this adds no dependency;
  3. header binding: program, deployment, taker (or open), clock window, `expires_at −
     issued_at` at most the smallest `max_quote_ttl` among the legs' series;
  4. `1 ≤ fill_units ≤ units`;
  5. per leg, every v1 rule: series not paused and of this deployment, mints and token
     accounts as in `settle`, `fill_units × lots_per_unit ≥ min_size`, no overflow;
  6. the nonce: one nonce for the whole package, consumed once, in the same window as v1.
- **Transfers:** per leg, the two `transfer_checked` legs of v1, with the fee rule of
  ADR 0001 per leg (each leg's own `fee_bps`, floor on that leg's quote atoms).
- **All or nothing:** any failing check or transfer fails the instruction, and the runtime
  rolls back every leg. There is no partial package.
- **Events:** one `FillEvent` per leg, unchanged (they share the nonce), then one package
  event `qb-pkg1`: maker, taker, nonce, `package_hash`, `fill_units`, `leg_count`. Indexers
  group legs by `(maker, nonce)`.

## Limits

Sizes are for a legacy transaction, with the taker as the only signer and fee payer. The
precompile instruction carries 320 bytes of data (16 bytes of offsets, the 64-byte signature,
the 32-byte key and the 208-byte message): 324 bytes encoded. The v2 rows are derived from the
account schema above, not yet measured.

### How a package is counted

- **Layout:** every leg keeps its account indices in the order of the schema (7, or 8 with a
  fee account), even when it repeats an account of another leg; there is no shared quote
  prefix. An index is 1 byte; each unique transaction key is 32.
- **Fixed part, 669 bytes:** 1 + 64 for the signature, 3 for the message header, 1 for the key
  count, 7 shared keys (taker, maker state, delegate PDA, instructions sysvar, one token
  program, quotebook, ed25519 program) at 224, 32 for the blockhash, 1 for the instruction
  count, 324 for the precompile instruction, then for `settle_package` 1 program index, 1
  account count, 5 shared indices, 1 data length, 1 tag and 10 bytes of fixed args.
- **A second token program** (an SPL leg next to a Token-2022 leg): one more shared key and
  index, +33.
- **Per leg:** 17 bytes of args, 1 byte per account index, 32 per key no earlier leg brought.
  - Distinct mints: 7 new keys, 248; with a fee account, 8 new keys, 281.
  - Shared quote mint, after the first leg: 4 new keys (series, base mint, maker base, taker
    base), 152; with a fee, the SDK passes the same canonical fee account for the shared mint,
    so only its index is new, 153.
- **Unique keys** (what the account-lock limit counts): `7 + T + (7 + F)·N` with distinct mints
  and `7 + T + 3 + F + 4·N` with a shared quote mint, where `T` is 1 with a second token
  program and `F` is 1 with a fee on every leg.
- **Compact lengths:** every compact-u16 above is 1 byte while its value is below 128; the
  data length (`11 + 17·N`) passes that at N=7, which only the lookup-table rows reach.

### Sizes and account counts

| Case (no lookup table) | Bytes | Fits in 1,232 | Unique keys |
| --- | ---: | --- | ---: |
| v1 `settle`, one quote (today, measured) | 900 | yes | 14 |
| distinct mints, no fee: `669 + 248·N` | N=2: 1,165; N=3: 1,413 | 2 legs | `7 + 7·N` |
| distinct mints, fee on every leg: `669 + 281·N` | N=2: 1,231; N=3: 1,512 | 2 legs (1 byte spare) | `7 + 8·N` |
| distinct mints, no fee, two token programs: `702 + 248·N` | N=2: 1,198 | 2 legs | `8 + 7·N` |
| distinct mints, fee, two token programs: `702 + 281·N` | N=1: 983; N=2: 1,264 | 1 leg | `8 + 8·N` |
| shared quote mint, no fee: `765 + 152·N` | N=3: 1,221; N=4: 1,373 | 3 legs | `10 + 4·N` |
| shared quote mint, fee: `797 + 153·N` | N=2: 1,103; N=3: 1,256 | 2 legs | `11 + 4·N` |
| shared quote mint, fee, two token programs: `830 + 153·N` | N=2: 1,136; N=3: 1,289 | 2 legs | `12 + 4·N` |

With an address lookup table (a v0 transaction, every key but the signer and the two programs
looked up), a leg costs its indices and args plus one lookup index per new key: about
`581 + 31·N` without fees and `581 + 33·N` with a fee on every leg (N=7: about 812). Size
stops binding; the unique-key count does not change, because looked-up accounts are locked
too.

### `MAX_LEGS` and admission

- **`MAX_LEGS` is 7.** The worst case for keys is distinct mints, a fee on every leg and two
  token programs: `8 + 8·N`, which is 64 at N=7 and 72 at N=8. With the account-lock limit at
  64 (at the time of writing; to recheck when this turns into code), 7 is the largest count
  every canonical package fits whatever its mints, fees and programs. The compute gate below
  may lower it, never raise it.
- **Size and keys are a separate admission constraint.** The program bounds only `leg_count`;
  the runtime already rejects a transaction above 1,232 bytes or the lock limit, so an
  oversized package cannot settle, it just fails. The SDK makes that failure early and
  explicit: `settle_package_instructions` computes the exact serialized size and unique-key
  count of the transaction it would build (with or without the lookup table) and returns a
  typed error (`PackageTooLarge { bytes }`, `TooManyAccounts { keys }`) instead of a
  transaction the cluster drops. The maker service runs the same check on a package request
  and refuses to sign a package the requester could not settle.
- **Boundary tests, with the code:** the real SDK serializer builds the boundary package of
  every table row, one that fits and the next that does not (for example 2 and 3 legs with
  distinct mints and fees, 1 and 2 with two token programs, 3 and 4 with a shared quote mint),
  and asserts the exact byte and key counts; and the worst-case 7-leg package (distinct mints,
  fees, two token programs, lookup table) settles in LiteSVM at exactly 64 keys. A schema
  change that moves a boundary fails the build.

### Compute

- **Projection, not a bound:** measured v1 `settle` costs (docs/benchmarks.md) are 9.2k CU on
  SPL Token without a fee, 12.2k with a 100 bps fee and 18.8k for the Token-2022 worst case.
  `N ×` those is only a projection: 7 legs would be about 85k and 132k CU. `settle_package`
  adds work v1 does not do (the canonical checks, `sol_sha256` on `1 + 49·N` bytes, the leg
  loop, per-leg account and extension checks, repeated accounts across legs), and saves some
  it shares (one signature parse, one nonce update), so the projection proves nothing.
- **Measurement gate:** the code PR adds `make bench` rows for `settle_package` at 2 legs and
  at `MAX_LEGS`, on SPL Token and on the Token-2022 worst case, with a fee on every leg and,
  for `MAX_LEGS`, two token programs and the lookup table. It does not merge until those rows
  are in docs/benchmarks.md and the boundary tests above pass; if the `MAX_LEGS` row exceeds
  the 200k default per instruction, `MAX_LEGS` drops to the largest measured count below it,
  and the SDK sets a compute-unit limit from the measured rows.
- **Rent and state:** none new. A package uses one nonce of the maker's existing window.

## Nonce window

v1 and v2 share `MakerState`'s window: one issuer, one counter, one replay state.

- The domain tag separates the signatures; the shared window separates the nonces. A nonce
  used by a v1 quote can never fill a package, and the reverse.
- `cancel_all` and `cancel_nonce` work for both domains with no change.
- The maker's issuance bound is unchanged: one package spends one nonce, however many legs it
  has.

A second window per domain was considered and rejected: it doubles `MakerState`, and the
maker would need two cancel paths for one intent ("cancel everything").

## Maker and taker tools

- **Maker:** a package request names the legs and the units; the service prices each leg as
  today and may add a package adjustment; it signs the header. Each paying leg's token
  account needs the delegate allowance for its share, summed when legs share the account.
- **Taker:** `client::verify` gains a package path: recompute the hash, run the v1 checks per
  leg against the chain, then the header checks.
- **SDK:** `settle_package_instructions`, with an optional lookup table; the TS SDK
  (I8) gains the header layout and hash vectors.

## Tests to write with the code

- Signature parsing: every existing `sigverify` test passes unchanged against the refactored
  `verified_quote` (same errors, same order). Cross domain: a v1 message given to
  `verified_package` and a v2 header given to `verified_quote` both fail `WrongDomain`, on chain
  through `settle` and `settle_package`. Key binding: a v2 header signed by a key other than
  its maker fails `SignerMismatch`. The self-contained and range cases run on both paths, and
  the `sigverify` fuzz target covers both.
- The hash: Rust and TS vectors, and a mutation of every leg field and the leg order.
- Canonical validation: one rejection per rule of the table, each from a valid package with
  one change: leg count 0 and `MAX_LEGS + 1`, a trailing or missing args byte, side 2, a zero
  price, zero lots, zero units, a nonzero reserved byte, two equal series and two descending
  series, `fill_units` 0 and `units + 1`, and an overflowing product. Each one is rejected
  before the hash, with the error of the table, on chain and in `client::verify`.
- Cherry-picking: v1 `settle` with a v2 header fails `WrongDomain`; `settle_package` with a
  leg removed, added, reordered or changed fails the hash check.
- Ratio and atomicity: exact balances for `k` units on every leg; a failing second or last
  leg (paused series, short allowance, frozen account) leaves every balance and the nonce
  unchanged.
- The shared window: a v1 nonce cannot fill a package and the reverse; `cancel_all` covers
  both.
- Limits: the boundary tests and the measured CU rows of Limits, and the SDK's
  `PackageTooLarge` and `TooManyAccounts` errors on the first package past each boundary.
- Fuzzing: the header decoder and the leg args join the I6 targets.

## Decisions

SOLANA-PROJECTS accepted the three drafts (2026-11-02, msg 46504):

1. **Fees:** no package-level fee override. Each leg pays its own series' fee (ADR 0001), so
   there is no new admin power and the fee stays immutable per series.
2. **Quote mints:** legs on different quote mints are allowed. It costs transaction space
   (see Limits) but adds no rule.
3. **Events:** a new `qb-pkg1` package event. `FillEvent` stays unchanged, so v1 indexers
   keep working.
