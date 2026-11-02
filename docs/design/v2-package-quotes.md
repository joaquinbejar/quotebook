# v2 package quotes (design)

**Status:** draft design, milestone L1 of [`PLAN-improvements.md`](../PLAN-improvements.md).
No code is planned in this phase. For review by Codex and SOLANA-PROJECTS.

## Problem

A maker wants to quote several legs as one price: a spread, a basket, a roll. Today each leg
is a separate v1 quote with its own signature. A taker can already settle several v1 quotes
in one transaction, which is atomic, but nothing forces it to: it can take the legs it likes
and leave the rest. The maker then holds a position it priced only as part of a package.

v2 signs the package as a whole, so its legs can only be filled together, in the same ratio.

## Signed message

The ed25519 message stays 208 bytes, so the precompile instruction and its self-contained
parser (`sigverify`) are reused unchanged; only the domain and the layout behind it differ.

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

## Canonical validation

A matching hash only proves that the args are the legs the maker signed; it does not prove
the maker signed a valid package. The decoder enforces every rule below before the hash is
computed, with the same code on chain, in `client::verify` and in the SDK encoder (which
refuses to build a package that breaks one).

| Rule | Error |
| --- | --- |
| args are exactly `10 + 17 · leg_count` bytes | `InvalidInstructionData` |
| `1 ≤ leg_count ≤ MAX_LEGS` (8, see Limits) | `MalformedQuote` |
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
  1. the precompile instruction verifies as today, with the v2 domain;
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

Sizes are for a legacy transaction, with the taker as the only signer and fee payer, one token
program and no fee accounts. The precompile instruction carries 320 bytes of data (16 bytes of
offsets, the 64-byte signature, the 32-byte key and the 208-byte message): 324 bytes encoded.

| Case | Bytes | Fits in 1,232? |
| --- | ---: | --- |
| v1 `settle`, one quote (today, measured) | 900 | yes |
| v2, distinct quote mints per leg: `669 + 248·N` | N=2: 1,165; N=3: 1,413 | 2 legs |
| v2, one shared quote mint (USDC legs): `765 + 152·N` | N=3: 1,221; N=4: 1,373 | 3 legs |
| v2 with an address lookup table, all but the signer and the two programs looked up: about `581 + 31·N` | N=8: about 829 | size is not the limit |

The v2 rows are derived from the account schema above, not yet measured:

- **Layout:** every leg keeps its 7 account indices, in the order of the schema, even when it
  repeats an account of another leg; there is no shared quote prefix. An index is 1 byte; a
  new transaction key is 32.
- **Fixed part, 669 bytes:** 1 + 64 for the signature, 3 for the message header, 1 for the key
  count, 7 shared keys (taker, maker state, delegate PDA, instructions sysvar, token program,
  quotebook, ed25519 program) at 224, 32 for the blockhash, 1 for the instruction count, 324
  for the precompile instruction, then for `settle_package` 1 program index, 1 account count,
  5 shared indices, 1 data length, 1 tag and 10 bytes of fixed args.
- **Distinct mints, 248 per leg:** 7 new keys (224), 7 indices and 17 bytes of args.
- **Shared quote mint:** the first leg costs 248; each later leg adds only series, base mint,
  maker base and taker base as new keys (128) plus 7 indices and 17 bytes of args: 152. So
  `669 + 248 + 152·(N − 1) = 765 + 152·N`.
- **Compact lengths:** every compact-u16 above is 1 byte while its value is below 128; the
  data length (`11 + 17·N`) passes that at N=7, which only the lookup-table row reaches.

Each table row becomes a test with the code: the real SDK serializer builds the boundary
packages (2 legs distinct and 3 shared must fit; 3 distinct and 4 shared must not) and the
test asserts the exact sizes, so a schema change that moves a boundary fails the build.

- **Accounts:** a package with distinct mints references `7 + 7·N` accounts; with a shared
  quote mint, `10 + 4·N` (the quote mint, maker quote and taker quote counted once). The
  per-transaction account-lock limit (64 at the time of writing; to recheck when this turns
  into code) caps them at 8 and 13 legs. `MAX_LEGS` is 8 so that every valid package fits the
  limit whatever its mints; the compute row below may lower it.
- **Compute (projection, not a bound):** measured v1 `settle` costs (docs/benchmarks.md) are
  9.2k CU on SPL Token without a fee, 12.2k with a 100 bps fee and 18.8k for the Token-2022
  worst case. `N ×` those is only a projection: 8 legs would be about 98k and 150k CU.
  `settle_package` adds work v1 does not do (the canonical checks, `sol_sha256` on
  `1 + 49·N` bytes, the leg loop, per-leg account and extension checks, repeated accounts
  across legs), and saves some it shares (one signature parse, one nonce update), so the
  projection proves nothing.
- **Measurement gate:** the code PR adds `make bench` rows for `settle_package` at 2 legs and
  at `MAX_LEGS`, on SPL Token and on the Token-2022 worst case, with a fee on every leg. It
  does not merge until those rows are in docs/benchmarks.md; if the `MAX_LEGS` row exceeds
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
- Limits: the serialized boundary packages of the size table, with and without a lookup
  table, and the measured CU rows of the gate above.
- Fuzzing: the header decoder and the leg args join the I6 targets.

## Open questions

1. Should the header carry a package-level fee override, or only the per-leg series fees?
   The draft uses per-leg fees only: no new admin power, and the fee stays immutable per
   series.
2. Should legs on different quote mints be allowed? It costs transaction space (above) but
   no new rule. The draft allows them.
3. The package event: a new `qb-pkg1` tag, or a version bump of the fill event with a
   package field? The draft keeps `FillEvent` unchanged so v1 indexers keep working.
