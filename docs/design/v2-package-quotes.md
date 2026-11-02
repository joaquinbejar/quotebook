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
  2. `sha256` of the legs (from the args and the series accounts) equals `package_hash`;
     `sol_sha256` is a syscall, so this adds no dependency;
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
| v1 `settle`, one quote (today) | 900 | yes |
| v2, distinct quote mints per leg: `669 + 248·N` | N=2: 1,165; N=3: 1,413 | 2 legs |
| v2, one shared quote mint (USDC legs): `768 + 149·N` | N=3: 1,215; N=4: 1,364 | 3 legs |
| v2 with an address lookup table, all but the signer and the two programs looked up: about `581 + 31·N` | N=8: 829 | size is not the limit |

- **Accounts:** a package with distinct mints locks `7 + 7·N` accounts. The per-transaction
  account-lock limit (64 at the time of writing; to recheck when this turns into code) caps it
  at 8 legs; a shared quote mint allows 13.
- **Compute:** measured v1 `settle` costs (docs/benchmarks.md) bound a leg: 9.2k CU on SPL
  Token without a fee, 12.2k with a 100 bps fee, 18.8k for the Token-2022 worst case. Part of
  that is per transaction (sysvar read, signature parse, nonce update), so a package costs
  less than `N ×` those. Upper bounds: 8 legs at 12.2k is 98k CU, at 18.8k is 150k CU, below
  the 200k default per instruction; `sol_sha256` on 1 + 49·8 bytes adds a few hundred CU.
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
- Cherry-picking: v1 `settle` with a v2 header fails `WrongDomain`; `settle_package` with a
  leg removed, added, reordered or changed fails the hash check.
- Ratio and atomicity: exact balances for `k` units on every leg; a failing second or last
  leg (paused series, short allowance, frozen account) leaves every balance and the nonce
  unchanged.
- The shared window: a v1 nonce cannot fill a package and the reverse; `cancel_all` covers
  both.
- Limits: the largest package that fits, with and without a lookup table, and its CU row in
  `make bench`.
- Fuzzing: the header decoder and the leg args join the I6 targets.

## Open questions

1. Should the header carry a package-level fee override, or only the per-leg series fees?
   The draft uses per-leg fees only: no new admin power, and the fee stays immutable per
   series.
2. Should legs on different quote mints be allowed? It costs transaction space (above) but
   no new rule. The draft allows them.
3. The package event: a new `qb-pkg1` tag, or a version bump of the fill event with a
   package field? The draft keeps `FillEvent` unchanged so v1 indexers keep working.
