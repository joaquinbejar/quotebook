# quotebook: plan

Status: approved by Joaquin on 2026-10-29. Changes after approval are recorded as decisions.

## 1. Goal and non-goals

### Goal

Request-for-quote trading on Solana. A maker signs a firm quote off-chain with its ed25519
key; the taker submits one transaction that holds the ed25519 precompile instruction and the
quotebook `settle` instruction. The program proves, through the instructions sysvar, that the
precompile verified exactly this quote by this maker, checks expiry and replay state, and swaps
the two tokens atomically. No order book, no maker transaction per quote, no crank.

Deliverables:

- `quotebook-program`: Pinocchio program, `no_std`, no allocator.
- `quotebook-core`: `no_std` crate with the domain types, the canonical quote encoding and the
  replay bitmap, shared by the program and the clients (like `matchbook-engine`).
- `quotebook-sdk`: Rust taker SDK (build the precompile and settle instructions, decode
  accounts). TypeScript SDK after v1, as in matchbook.
- `quotebook-maker`: off-chain maker service that prices with market-maker-rs and signs quotes.
- `docs/benchmarks.md` with the CU of every instruction.

### Non-goals (v1)

- An off-chain RFQ network or relay with discovery and auth. v1 ships a minimal WebSocket
  protocol between one taker SDK and one maker service, enough for tests and a demo.
- Partial fills of one quote by several takers (see 4.3).
- Fees. Left as an open question; the account layout reserves space.
- Token-2022 mints with transfer fees or transfer hooks (rejected at series creation).
- Last-look. Quotes are firm: if the taker's transaction lands before expiry, it fills.

## 2. Domain model

`quotebook-core`, all integers, `bytemuck::Pod` where stored or signed.

```rust
pub struct Address(pub [u8; 32]);          // re-export of solana_address::Address
pub struct SeriesId(pub Address);          // the series account's address
pub struct BaseLots(pub u64);              // size in base lots, > 0
pub struct QuoteLotsPerBaseLot(pub u64);   // price in ticks: quote lots per base lot, > 0
pub struct QuoteAtoms(pub u64);            // a token amount in the quote mint's atoms
pub struct BaseAtoms(pub u64);
pub struct UnixTimestamp(pub i64);         // seconds, from the Clock sysvar
pub struct Nonce(pub u64);

#[repr(u8)]
pub enum Side { Bid = 0, Ask = 1 }         // the maker's side: Bid = maker buys base

/// What the maker signs. Fixed 208-byte little-endian encoding, defined in one place
/// (`Quote::encode` / `Quote::decode`), and the program never re-serializes it: it reads the
/// signed bytes straight out of the precompile instruction. The message may sit at an
/// unaligned offset in the precompile data, so `decode` uses `pod_read_unaligned` (or reads
/// field by field) and validates `side`; it never casts in place. All newtypes are
/// `#[repr(transparent)]`; padding is explicit so the struct can derive `Pod`.
#[repr(C)]
pub struct Quote {
    pub domain: [u8; 16],          // b"quotebook-v1\0\0\0\0": domain separation
    pub program_id: Address,       // binds the signature to this program
    pub deployment: [u8; 16],      // Config.deployment_id: binds it to one cluster
    pub maker: Address,
    pub series: SeriesId,
    pub taker: Address,            // all zeros = any taker; otherwise only this signer
    pub side: u8,                  // Side as u8; decode rejects anything but 0 or 1
    pub _pad: [u8; 7],
    pub price: QuoteLotsPerBaseLot,
    pub size: BaseLots,
    pub issued_at: UnixTimestamp,
    pub expires_at: UnixTimestamp,
    pub nonce: Nonce,
}   // 16 + 32 + 16 + 3×32 (maker, series, taker) + 8 + 5×8 = 208 bytes
```

Added beyond the brief's field list, and why (`issued_at` is explained in invariant 4):

- `domain` and `program_id`: a signature for another program that happens to sign a similar
  layout must not be valid here.
- `deployment`: devnet and mainnet can share the program address, mints and series PDAs, so
  `program_id` alone does not separate clusters. `Config.deployment_id: [u8; 16]` is the first 16
  bytes of the cluster's genesis hash, set once at `init_config` and never changed; it is
  copied into each `Series` at `create_series`, and `settle` compares `quote.deployment`
  against `series.deployment_id` (no extra account). The SDK and the maker service read it
  from chain and check it against `getGenesisHash`, never from a config file; a quote signed for
  devnet fails on mainnet. Tested with identical keys, program and series on two logical
  deployments.
- `taker`: an RFQ answer is meant for the requester. Without it, anyone who sees the quote
  (relay, logs, a leaked WebSocket frame) can fill it first. Zero means "open quote".

Invariants of a fillable quote, checked on-chain in this order:

1. The bytes come from a verified ed25519 instruction whose public key equals `maker` (5.1).
2. `domain` and `program_id` match; `series` equals the series account passed in.
3. `taker` is zero or equals the transaction's taker signer.
4. `now < expires_at`, `expires_at − issued_at <= series.max_quote_ttl` and
   `issued_at <= now + MAX_CLOCK_SKEW` (a program constant, proposed 10 s). Bounding `expires_at − now` alone would not bound a
   quote's lifetime (a 1-hour quote would simply become fillable in its last minute), and the
   bitmap argument in 4.2 depends on a real lifetime bound. The Clock's `unix_timestamp` is a
   stake-weighted estimate that can drift seconds from wall time; TTLs allow for that.
5. `window_base <= nonce` and not yet used (4.2).
6. `series.min_size <= fill_size <= size`, and `price` inside the series' limits. A dust fill
   still consumes the nonce; documented.
7. `price * fill_size * quote_lot_size` and `fill_size * base_lot_size` do not overflow `u64`.

```rust
#[repr(u32)]
pub enum QuotebookError {
    InvalidInstruction = 100,      // codes start at 100 like matchbook
    InvalidInstructionData,
    MissingSignature,
    InvalidAccountOwner,
    InvalidPda,
    InvalidMint,
    InvalidVaultOrTokenAccount,
    InvalidSysvar,
    // signature binding
    MissingSigVerify,              // no ed25519 instruction at the expected index
    MalformedSigVerify,            // count != 1, or offsets out of range
    SigVerifyNotSelfContained,     // an instruction index is not 0xFFFF
    SignerMismatch,                // signed pubkey != quote.maker
    QuoteLengthMismatch,           // message_data_size != Quote::LEN
    WrongDomain,
    WrongProgram,
    WrongSeries,
    WrongTaker,
    // quote state
    QuoteExpired,
    QuoteTtlTooLong,
    NonceCancelled,                // nonce below window_base after a cancel_all
    NonceUsed,
    NonceOutOfWindow,              // below the bitmap window base
    FillSizeInvalid,
    SizeNotOnLot,
    Overflow,
    SeriesPaused,
    NotAuthority,
    MintExtensionUnsupported,
    // appended in M1
    MalformedQuote,                // side not 0 or 1, non-zero padding, zero price or size
    WatermarkNotIncreasing,        // cancel_all did not raise window_base
}
```

M1 implementation notes (`quotebook-core`, decided while building it):

- Codes are dense from 100 (`InvalidInstruction`) to 129 (`WatermarkNotIncreasing`); new
  variants are only appended. `QuotebookError::from_code` lets clients decode them.
- `Quote` is a validated type, not the wire struct: `Quote::new(QuoteTerms)` rejects a zero
  price or size, and `QuoteTerms` (public fields, `Side` as the enum) is the input form. The
  208-byte layout is a private `QuoteWire` of byte arrays only (alignment 1, no padding, same
  layout on host and SBF), converted with `bytemuck::must_cast` (size checked at compile
  time). The domain tag is a constant, not a field: `decode` checks it (`WrongDomain`).
  `decode` accepts exactly the outputs of `encode` (property tested), so the encoding is
  canonical.
- `MAX_CLOCK_SKEW` (10 s) and `QUOTE_DOMAIN` live in `quotebook-core`.
- `derive(Pod)` on a multi-field struct emits a compile-time `const _: () = assert!(..)`,
  which `clippy::disallowed_macros` reports. Pod layouts therefore sit in small `layout`
  modules with a module-level `#[allow(clippy::disallowed_macros)]` and no logic; the same
  pattern applies to the M2 accounts.

### otc-rfq: what can be reused

`~/Repos/Local/otc-rfq` (crate 0.1.1) is a tokio/Postgres service with REST, WebSocket, FIX,
SBE and gRPC front ends and EVM settlement. Its domain types use `rust_decimal`, `uuid`,
`chrono` and `String`-carrying errors, and it has no signed quotes, nonces or ed25519 code. So:

- **On-chain: nothing is reusable as code.** The patterns are: `RfqState` /
  `SettlementState` as `repr(u8)` enums with `const fn can_transition_to`, `OrderSide`, and its
  checked-arithmetic trait (`value_objects/arithmetic.rs`), rewritten over integers.
- **Off-chain maker/taker service: worth reusing.** `application/services/price_bounds.rs`
  (sanity bounds before signing) and `ranking_strategy.rs` (taker picks among several
  makers' quotes), and the `Quote`/`Rfq` entities as the service-side model, with a conversion
  into `quotebook_core::Quote` at the signing boundary.
- **New in quotebook:** signed quote encoding, nonce and replay protection, ed25519
  introspection, all on-chain state.

Proposal: `quotebook-maker` takes a path dependency on nothing from otc-rfq in v1 (the useful
pieces are small) and copies `price_bounds` logic with attribution. Open question 5.

## 3. Accounts and instructions

### 3.1 Accounts

All program accounts start with an 8-byte header `{ tag: u8, version: u8, bump: u8, _pad: [u8;5] }`
and are `bytemuck::Pod`, loaded zero-copy.

| Account | Seeds | Owner | Size | Written by |
| --- | --- | --- | --- | --- |
| `Config` | `["config"]` | quotebook | 8 + 32 + 8 + 16 = 64 | `init_config`, `set_config` (admin; never `deployment_id`) |
| `Series` | `["series", base_mint, quote_mint, series_index_le]` | quotebook | 144 | `create_series`, `set_series_paused` |
| `MakerState` | `["maker", maker]` | quotebook | 8 + 32 + 8 + 8 + 2,064 = 2,120 | `init_maker`, `cancel_all`, `cancel_nonce`, `settle` |
| Delegate authority | `["delegate", maker]` | none (PDA, no data) | 0 | never; signs that maker's token transfers |

Field sketches:

```rust
struct Config { hdr: Hdr, admin: Address, series_count: u64, deployment_id: [u8; 16] }
struct Series {
    hdr: Hdr, series_index: u64, deployment_id: [u8; 16], base_mint: Address, quote_mint: Address,
    base_lot_size: u64, quote_lot_size: u64, max_quote_ttl: i64, min_size: u64,
    paused: u8, base_is_2022: u8, quote_is_2022: u8, _pad: [u8; 5],
    _reserved: [u8; 8],   // fee bps later
}
struct MakerState {
    hdr: Hdr, maker: Address,
    fills: u64,                 // counter, for clients
    delegate_bump: u8, _pad: [u8; 7],
    nonces: NonceWindow,        // quotebook-core, 2,064 bytes:
    //  window_base: u64,       // lowest fillable nonce; also the cancel-all watermark
    //  last_cancel_all: u64,   // tells NonceCancelled from NonceOutOfWindow
    //  used: [u64; 256],       // 16,384-bit ring: bit (n mod 16,384) for nonces in the window
}
```

Token flow: makers do not deposit. Each maker approves its own `["delegate", maker]` PDA as SPL
delegate on the token account it pays from (only the paying side needs approval), with the
allowance it is willing to trade; `settle` transfers maker ↔ taker directly with
`transfer_checked`, the maker's delegate PDA signing for the maker side and the taker signing
for its own side. A per-maker delegate means a `settle` bug can only reach the maker named in
the verified quote, never every maker's allowance. SPL allows one delegate per token account,
so a maker cannot approve quotebook and another protocol on the same account; documented.
`MakerState` is writable in every `settle`, so one maker's fills serialize within a block;
fine for RFQ. Rationale in 4.4.

### 3.2 Instructions

| # | Instruction | Signers | Accounts | Effect |
| --- | --- | --- | --- | --- |
| 0 | `init_config` | payer, admin | config (w), system | creates `Config` |
| 0b | `set_config` | admin | config (w) | changes `admin` only |
| 1 | `create_series` | admin, payer | config (w), series (w), base_mint, quote_mint, system | creates `Series` |
| 2 | `set_series_paused` | admin | config, series (w) | pause/unpause |
| 3 | `init_maker` | maker, payer | maker_state (w), system | creates `MakerState` |
| 4 | `cancel_all` | maker | maker_state (w) | slides `window_base` to `new_min` (must increase), clearing bits |
| 5 | `cancel_nonce` | maker | maker_state (w) | marks one nonce used; a nonce beyond the window slides it first |
| 6 | `settle` | taker | series, maker_state (w), maker (ro), delegate PDA, maker_base (w), maker_quote (w), taker_base (w), taker_quote (w), base_mint, quote_mint, base token program, quote token program, instructions sysvar | verifies and fills |
| 7 | (no `close_maker` in v1) | | | closing would destroy the replay bitmap: re-initializing the same PDA would make already-filled, unexpired quotes fillable again (codex review). `MakerState` is permanent; its rent (~0.015 SOL) is the price of a once-only guarantee. v2 option: a `created_at = now + MAX_CLOCK_SKEW` field checked as `quote.issued_at >= created_at` makes close and re-init safe |

`settle` data: `{ sig_ix_index: u8, fill_size: u64 }`. The quote itself is not in the settle
data: the program reads it from the precompile instruction, which avoids a 208-byte copy and
removes "the settle args and the signed bytes disagree" as a class of bug.

### 3.3 Security checks per instruction

Every rule below gets a LiteSVM negative test with a forged or swapped account.

- **All:** program accounts owned by quotebook and carrying the right `tag`; PDA addresses
  re-derived from seeds with the stored bump; writable flags as listed.
- `init_config`: config PDA; admin signs.
- `create_series`: admin signs and equals `config.admin`; series PDA from mints and
  `config.series_count`; mints owned by SPL Token or Token-2022; Token-2022 mints rejected if
  they carry transfer-fee, transfer-hook, confidential-transfer, permanent-delegate,
  non-transferable, pausable or `DefaultAccountState = Frozen` extensions; `base_mint !=
  quote_mint`; lot sizes non-zero.
- Token-2022 account-level extensions at `settle`: a taker account with CPI Guard cannot
  transfer via CPI, and a receiving account with MemoTransfer requires a memo. v1 rejects
  both with a clear error; tests cover each.
- `init_maker`, `cancel_*`: maker signs; `init_maker` fails if the PDA is already
  initialized (tag set). Pre-funding grief: anyone can send lamports to a PDA address, which
  makes a plain `create_account` fail, so every PDA creation (`init_config`, `create_series`,
  `init_maker`) uses transfer to rent-exempt + `allocate` + `assign` when lamports > 0 (check
  matchbook's helper); LiteSVM test with a pre-funded PDA; maker_state PDA from that maker;
  `cancel_all` rejects a non-increasing watermark.
- `settle`:
  - `taker` signs; it is not the maker (self-trade rejected).
  - `series` owned and tagged, not paused; mints equal the series mints; token programs equal
    the mints' owners.
  - `maker_state` PDA from `quote.maker`; `maker` account key equals `quote.maker`.
  - Token accounts: maker's are owned (SPL owner field) by `quote.maker`, taker's by the taker,
    each with the right mint; delegate PDA re-derived from `["delegate", quote.maker]` with
    `MakerState.delegate_bump`.
  - Instructions sysvar address equals the sysvar id (the Pinocchio `Instructions::try_from`
    check), then the ed25519 checks of 5.1.
  - Quote invariants 2 to 7 of section 2.
  - State update (`set bit`, `fills += 1`) before the CPIs, so a re-entrant path cannot reuse
    the nonce (quotebook makes no CPI that can call back, but the order costs nothing).

## 4. Key design decisions

### 4.1 Verifying the signature: precompile plus introspection

Options: (a) ed25519 precompile + instructions sysvar, (b) verify in-program (`curve25519`
syscalls cost far more CU; no ed25519 verify syscall exists), (c) maker co-signs the
transaction (needs the maker online per fill, defeats RFQ). **Recommended: (a).**

The check, in `quotebook-core::sigverify` so it is unit-testable off-chain:

1. Load the instruction at `sig_ix_index`; it must be before the current instruction.
2. Its program id equals `Ed25519SigVerify111111111111111111111111111`.
3. `data[0] == 1` (exactly one signature), `data.len() >= 16 + 32 + 64 + Quote::LEN`.
4. `signature_instruction_index`, `public_key_instruction_index` and
   `message_instruction_index` are all `0xFFFF`, so the precompile verified bytes inside its
   own data, which is the data we read. **This is the classic bug**: an attacker builds a
   precompile instruction that verifies a genuine signature over bytes in another
   instruction while the data we read holds something else. We require self-containment and
   then read the pubkey, signature and message exactly at the offsets the precompile used.
5. `message_data_size == Quote::LEN`; all three offset ranges are inside `data` and do not
   overflow `u16` arithmetic.
6. Pubkey bytes at `public_key_offset` equal `quote.maker`, where `quote` is decoded from the
   bytes at `message_data_offset`.

Negative tests: wrong program id, two signatures, each index set to 0 / another instruction,
offsets pointing at a second copy of a different quote, truncated data, pubkey of another key,
message one byte short, and the precompile placed after `settle`.

### 4.2 Replay protection for individual fills

| Option | Cost per fill | Concurrent quotes | Rent | Notes |
| --- | --- | --- | --- | --- |
| Monotonic nonce (`nonce > last_filled`) | tiny | no: filling quote 7 kills 1 to 6 | none | a maker answering five takers at once loses four quotes per fill |
| Used-nonce PDA per fill | create an account (~2,000+ CU and ~0.0009 SOL rent, refundable on close) | yes, unlimited | per fill | rent churn, account spam, a close flow |
| **Sliding bitmap in `MakerState`** | set one bit, sometimes clear words | yes, 16,384 in flight | 2 KiB once | nonces far ahead of the window slide it |

**Recommended: sliding bitmap.** Makers allocate nonces from a counter. One watermark,
`window_base`, is both the bitmap base and the brief's `min_valid_nonce`. For a nonce `n`:
reject if `n < window_base`; if `n >= window_base + 16,384`, slide first to `n − 16,383`;
then check and set bit `n mod 16,384` of the ring. Sliding from `B` to `B'` clears exactly the
slots of nonces `[B, B')` (`min(B' − B, 16,384)` bits), with masked clears on the edge words
because `B` and `B'` need not be multiples of 64; the model test includes unaligned
`cancel_all` values. `MakerState.last_cancel_all` tells them apart: a rejected
`n < window_base` reports `NonceCancelled` when `n < last_cancel_all` and
`NonceOutOfWindow` otherwise. `cancel_all(new_min)` is the single slide the brief asks for. Because a
quote's lifetime is bounded by `max_quote_ttl` (proposed 60 s), a maker issuing fewer than
16,384 quotes per TTL (about 270 per second) never loses a live quote to sliding; the maker
service enforces that rate. 2 KiB of bitmap costs about 0.015 SOL of rent once; milestone 4
measures the worst-case slide CU.

### 4.3 Partial fills

Options: none (fill exactly `size`); "fill up to `size` once" (taker chooses `fill_size <=
size`, nonce consumed, rest void); true partial fills (track remaining per nonce, needs per-quote
state, i.e. used-nonce accounts). **Recommended: fill up to size, once.** It costs nothing extra
in state, lets a taker take less than quoted (common in RFQ, the maker priced for at most that
size, so a smaller fill is no worse for it), and avoids per-quote accounts. True partial fills
are a v2 question tied to any shared or open quote use case.

### 4.4 Maker funds: delegate approval vs escrow

Escrow (maker deposits into a program vault) adds deposit/withdraw instructions and locks
capital per venue; delegate approval lets the maker trade from its wallet and cap exposure
with the approved amount, at the cost that a revoked approval makes a fill fail (which only
hurts the maker's fill rate, never the taker's funds, since the transaction is atomic).
**Recommended: delegate approval.** The delegate is a per-maker PDA (3.1), and an approval is
per token account, not per series.

### 4.5 Rounding

Amounts are integer: `base = fill_size × base_lot_size`, `quote = price × fill_size ×
quote_lot_size`, computed in `u128` and checked into `u64`. No rounding happens on-chain. The
maker service converts its `Decimal` price to ticks with explicit direction toward the maker:
bids round down, asks round up.

### 4.6 Off-chain maker service

`quotebook-maker` (std, tokio): reads a mid from Pyth Hermes or a configured feed, calls
`market_maker_rs::strategy::avellaneda_stoikov::calculate_optimal_quotes(mid, inventory,
risk_aversion, volatility, time_to_terminal_ms, order_intensity)` or the GLFT variant
(`GLFTStrategy::calculate_optimal_quotes` with a `GLFTConfig`), applies otc-rfq-style price
bounds, signs, and answers requests. For option series (metalbook) it can switch to
market-maker-rs's `options` feature (centre on Black-Scholes value, skew by greeks inventory).
Inventory is read from the maker's token balances and updated from `settle` logs.

### 4.7 Instructions sysvar index

The taker passes `sig_ix_index` explicitly instead of assuming `current − 1`, so a transaction
can hold compute-budget instructions or several fills. The program still requires
`sig_ix_index < current_index`.

## 5. Milestones

Branch per milestone `feat/m<N>-<name>`, one PR each.

1. **Core types and encoding.** `quotebook-core` with `Quote`, newtypes, errors, `encode`/
   `decode`, bitmap. *Done when* proptest shows `decode(encode(q)) == q` at aligned and
   unaligned offsets, `decode` rejects invalid `side` bytes, the bitmap model test
   (random fills, cancels, slides against a `BTreeSet` reference) passes, the lint-control
   failing control is in place, and `make pre-push` is clean. Lint control, from optfix
   (`afb9d4d`), when the Cargo workspace is created:
   - copy `lint-control/` (`Cargo.toml`, `src/lib.rs`, `check.sh`) from optfix;
   - add it to the workspace members;
   - add `cfg(lint_control)` to `unexpected_cfgs` `check-cfg`, next to Pinocchio's
     `cfg(target_os, values("solana"))`;
   - add `disallowed_macros = "deny"` to `[workspace.lints.clippy]` (list in `clippy.toml`);
   - allow `clippy::disallowed_macros` in tests and benches, and in the module that invokes
     the entrypoint macros (Pinocchio's `no_allocator!` expands to `assert!`), with a comment;
   - put `lint-control` in `make pre-push`.
2. **Program skeleton and accounts.** `init_config`, `create_series`, `init_maker`,
   `cancel_all`, `cancel_nonce`, with LiteSVM tests and negative tests for each
   validation rule. *Done when* every rule in 3.3 for these instructions has a passing forged-
   account test.
3. **`settle` with signature binding.** *Done when* happy-path fills (both sides, SPL Token and
   Token-2022) pass and every negative test listed in 4.1 and 3.3 is in `program/tests/sigverify.rs`
   and passes.
4. **Benchmarks.** *Done when* `docs/benchmarks.md` lists the CU of every instruction, `settle`
   including the precompile, and a comparison row against a fill on matchbook (IOC, 1 fill) on
   the same LiteSVM.
5. **Taker SDK.** *Done when* `quotebook-sdk` builds the two instructions from a `Quote` and a
   signature, and an end-to-end LiteSVM test uses only the SDK.
6. **Maker service.** *Done when* a local demo (maker service + taker CLI + LiteSVM or
   solana-test-validator) requests, receives and settles a quote priced by market-maker-rs,
   and the service refuses to exceed the 16,384-per-TTL nonce rate.
7. **Devnet and docs.** *Done when* the program is deployed to devnet, README and
   `docs/instructions.md` are written, and v0.1 crates are ready to publish (published after
   your go-ahead).

## Verified facts (checked 2026-10-28)

- Pinocchio 0.11.2 (and 0.12) entrypoint handlers take `&mut [AccountView]`; 0.11.2 resolves
  `solana-account-view` 2.1.0 and `solana-address` 2.9.0 (quotebook session, scratch crate with
  `unsafe_code = "deny"` and `bytemuck` 1.25 derives).

- `pinocchio` 0.11.2 (the version used) exposes `pinocchio::sysvars::instructions::Instructions`
  (`try_from(&AccountView)` checks the sysvar address; `load_current_index`,
  `load_instruction_at`, `get_instruction_relative`; `IntrospectedInstruction::
  get_instruction_data`), with the same API in 0.12.0. It has no ed25519 program id constant; it is
  `solana_sdk_ids::ed25519_program::ID` (`solana-sdk-ids` 3.1.0), or declared locally.
  Source: the pinocchio 0.12.0 crate source, `src/sysvars/instructions.rs`, re-checked on 0.11.2.
- ed25519 precompile data: `num_signatures: u8`, one padding byte, then 14-byte
  `Ed25519SignatureOffsets` records of seven little-endian `u16` (`signature_offset`,
  `signature_instruction_index`, `public_key_offset`, `public_key_instruction_index`,
  `message_data_offset`, `message_data_size`, `message_instruction_index`); `u16::MAX` means
  "this instruction"; one signature puts data at offset 16. Source: `solana-ed25519-program`
  3.0.0 `src/lib.rs` (also the crate the SDK uses to build the instruction).
- `solana-instructions-sysvar` 5.0.0, `litesvm` 0.18.1, `mollusk-svm` 0.16.4.
- sBPF v3 deployment is active on mainnet; local `cargo build-sbf` defaults to `--arch v3`.
- market-maker-rs 0.6.0 API names as above (`src/strategy/avellaneda_stoikov.rs`,
  `src/strategy/glft.rs`); it uses `rust_decimal::Decimal`.

## Dependencies (latest stable on crates.io, checked 2026-10-29)

Rule from `rules/global_rules.md`: every dependency at the latest stable version, written
`MAJOR.MINOR`, re-checked via `https://crates.io/api/v1/crates/<name>` (`max_stable_version`)
whenever it is added or touched.

| Crate | Latest stable | Requirement |
| --- | --- | --- |
| `pinocchio` | 0.12.0 (0.11.2 used) | `0.11`, decided exception below |
| `bytemuck` | 1.25.2 | `1.25`, features `derive`, `must_cast`, `min_const_generics` |
| `proptest` (dev) | 1.12.0 | `1.12` |
| `litesvm` (dev) | 0.18.1 | `0.18` |
| `mollusk-svm` (dev, optional) | 0.16.4 | `0.16` |
| `solana-address` | 2.9.0 | `2.9`, `default-features = false`, feature `bytemuck` |
| `pinocchio-system` | 0.7.0 (0.6.1 used) | `0.6`, decided exception below |
| `pinocchio-token` | 0.7.0 | `0.7`, see conflict below |
| `litesvm-token` (dev) | 0.18.1 | `0.18` |
| `solana-sdk-ids` | 3.1.0 | `3.1` |
| `solana-ed25519-program` (SDK, dev) | 3.0.0 | `3.0` |
| `solana-instructions-sysvar` (SDK) | 5.0.0 | `5.0` |
| `market-maker-rs` (maker) | 0.6.0 | `0.6` |
| `rust_decimal` (maker, SDK) | 1.43.0 | `1.43` |

**Version decision (Joaquin, 2026-10-29): option (b), the Pinocchio 0.11 generation.**
`pinocchio-token` 0.7.0 and `pinocchio-associated-token-account` 0.4.0 are built on
`solana-account-view` 2 (Pinocchio 0.11), while `pinocchio` 0.12.0 uses 3, so the latest
stable versions cannot be combined. All four repos and matchbook therefore use `pinocchio`
0.11 (0.11.2) and `pinocchio-system` 0.6 (0.6.1), the newest releases compatible with the
token crates. This is the documented exception to the latest-stable rule; move all four to
the 0.12 generation together once 0.12-compatible token crates are released.

## 6. Open questions

1. Fees: none in v1, or a taker fee to a treasury from day one?
2. Series creation: admin-only (proposed) or permissionless?
3. `max_quote_ttl`: 60 s proposed. Shorter for volatile underlyings?
4. Open quotes (`taker = 0`): the quotebook session suggests taker-bound by default with open
   quotes allowed. OK?
5. otc-rfq: copy the two service modules, or extract a shared crate both use?
6. Decided (2026-10-29): Pinocchio 0.11 generation (exception above).
7. Proposal (from review, pending your approval): no multi-fill instruction in v1; an explicit `sig_ix_index` already
   allows several `settle` instructions (one per maker or leg) in one atomic transaction.
