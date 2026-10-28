# quotebook: plan

Status: draft for review. Nothing here is implemented yet.

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

/// What the maker signs. Fixed 160-byte little-endian encoding, defined in one place
/// (`Quote::encode` / `Quote::decode`), and the program never re-serializes it: it reads the
/// signed bytes straight out of the precompile instruction.
#[repr(C)]
pub struct Quote {
    pub domain: [u8; 16],          // b"quotebook-v1\0\0\0\0": domain separation
    pub program_id: Address,       // binds the signature to this deployment
    pub maker: Address,
    pub series: SeriesId,
    pub taker: Address,            // all zeros = any taker; otherwise only this signer
    pub side: Side,                // + 7 bytes padding
    pub price: QuoteLotsPerBaseLot,
    pub size: BaseLots,
    pub expires_at: UnixTimestamp,
    pub nonce: Nonce,
}
```

Added beyond the brief's field list, and why:

- `domain` and `program_id`: a signature for quotebook on devnet, or for another program that
  happens to sign a similar layout, must not be valid here.
- `taker`: an RFQ answer is meant for the requester. Without it, anyone who sees the quote
  (relay, logs, a leaked WebSocket frame) can fill it first. Zero means "open quote".

Invariants of a fillable quote, checked on-chain in this order:

1. The bytes come from a verified ed25519 instruction whose public key equals `maker` (5.1).
2. `domain` and `program_id` match; `series` equals the series account passed in.
3. `taker` is zero or equals the transaction's taker signer.
4. `now < expires_at` and `expires_at - now <= series.max_quote_ttl`.
5. `nonce >= maker_state.min_valid_nonce` and not yet used (4.2).
6. `0 < fill_size <= size`, and `size`, `price` are multiples/inside the series' limits.
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
    NonceCancelled,                // nonce < min_valid_nonce
    NonceUsed,
    NonceOutOfWindow,              // below the bitmap window base
    FillSizeInvalid,
    SizeNotOnLot,
    Overflow,
    SeriesPaused,
    NotAuthority,
    MintExtensionUnsupported,
}
```

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
| `Config` | `["config"]` | quotebook | 8 + 32 + 8 = 48 | `init_config`, `set_config` (admin) |
| `Series` | `["series", base_mint, quote_mint, series_index_le]` | quotebook | 8 + 32×2 + 8×4 + 8 + 8 = 120 | `create_series`, `set_series_paused` |
| `MakerState` | `["maker", maker]` | quotebook | 8 + 32 + 8×3 + 512 = 576 | `init_maker`, `cancel_all`, `cancel_nonce`, `settle` |
| Delegate authority | `["delegate"]` | none (PDA, no data) | 0 | never; signs token transfers |

Field sketches:

```rust
struct Config { hdr: Hdr, admin: Address, series_count: u64 }
struct Series {
    hdr: Hdr, base_mint: Address, quote_mint: Address,
    base_lot_size: u64, quote_lot_size: u64, max_quote_ttl: i64, min_size: u64,
    paused: u8, base_is_2022: u8, quote_is_2022: u8, _pad: [u8; 5],
    _reserved: [u8; 8],   // fee bps later
}
struct MakerState {
    hdr: Hdr, maker: Address,
    min_valid_nonce: u64,       // cancel-all watermark
    window_base: u64,           // lowest nonce tracked by the bitmap
    fills: u64,                 // counter, for clients
    used: [u64; 64],            // 4,096-bit bitmap for nonces window_base .. window_base + 4096
}
```

Token flow: makers do not deposit. Each maker approves the `delegate` PDA as SPL delegate on
its own base and quote token accounts (with the allowance it is willing to trade); `settle`
transfers maker ↔ taker directly with `transfer_checked`, the delegate PDA signing for the
maker side and the taker signing for its own side. Rationale in 4.4.

### 3.2 Instructions

| # | Instruction | Signers | Accounts | Effect |
| --- | --- | --- | --- | --- |
| 0 | `init_config` | payer, admin | config (w), system | creates `Config` |
| 1 | `create_series` | admin, payer | config (w), series (w), base_mint, quote_mint, system | creates `Series` |
| 2 | `set_series_paused` | admin | config, series (w) | pause/unpause |
| 3 | `init_maker` | maker, payer | maker_state (w), system | creates `MakerState` |
| 4 | `cancel_all` | maker | maker_state (w) | `min_valid_nonce = new_min` (must increase) |
| 5 | `cancel_nonce` | maker | maker_state (w) | marks one nonce used |
| 6 | `settle` | taker | series, maker_state (w), maker (ro), delegate PDA, maker_base (w), maker_quote (w), taker_base (w), taker_quote (w), base_mint, quote_mint, base token program, quote token program, instructions sysvar | verifies and fills |
| 7 | `close_maker` | maker | maker_state (w), rent receiver (w) | closes the account |

`settle` data: `{ sig_ix_index: u8, fill_size: u64 }`. The quote itself is not in the settle
data: the program reads it from the precompile instruction, which avoids a 160-byte copy and
removes "the settle args and the signed bytes disagree" as a class of bug.

### 3.3 Security checks per instruction

Every rule below gets a LiteSVM negative test with a forged or swapped account.

- **All:** program accounts owned by quotebook and carrying the right `tag`; PDA addresses
  re-derived from seeds with the stored bump; writable flags as listed.
- `init_config`: config PDA; admin signs.
- `create_series`: admin signs and equals `config.admin`; series PDA from mints and
  `config.series_count`; mints owned by SPL Token or Token-2022; Token-2022 mints rejected if
  they carry transfer-fee, transfer-hook, confidential-transfer, permanent-delegate or
  non-transferable extensions; `base_mint != quote_mint`; lot sizes non-zero.
- `init_maker`, `cancel_*`, `close_maker`: maker signs; maker_state PDA from that maker;
  `cancel_all` rejects a non-increasing watermark.
- `settle`:
  - `taker` signs; it is not the maker (self-trade rejected).
  - `series` owned and tagged, not paused; mints equal the series mints; token programs equal
    the mints' owners.
  - `maker_state` PDA from `quote.maker`; `maker` account key equals `quote.maker`.
  - Token accounts: maker's are owned (SPL owner field) by `quote.maker`, taker's by the taker,
    each with the right mint; delegate PDA from its seed.
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
| **Sliding bitmap in `MakerState`** | set one bit, sometimes shift | yes, 4,096 in flight | 576 bytes once | nonces far ahead of the window slide it |

**Recommended: sliding bitmap.** Makers allocate nonces from a counter. A nonce `n` is fillable
iff `n >= min_valid_nonce`, `n >= window_base` and bit `n − window_base` is clear. If
`n >= window_base + 4096`, the window slides forward to `n − 4095` and the bits that fall off
are dropped, which marks those older nonces unfillable (they are below `window_base`). Because
quotes have a short TTL (`max_quote_ttl`, proposed 60 s), a maker issuing fewer than 4,096
quotes per TTL never loses a live quote to sliding; the maker service enforces that rate.
`cancel_all` is the single watermark bump the brief asks for; `cancel_nonce` sets one bit.

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
**Recommended: delegate approval.** The delegate is one program-wide PDA, so an approval is
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
   `decode`, bitmap. *Done when* proptest shows `decode(encode(q)) == q`, the bitmap model test
   (random fills, cancels, slides against a `BTreeSet` reference) passes, and `make pre-push`
   is clean.
2. **Program skeleton and accounts.** `init_config`, `create_series`, `init_maker`,
   `cancel_all`, `cancel_nonce`, `close_maker`, with LiteSVM tests and negative tests for each
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
   and the service refuses to exceed the 4,096-per-TTL nonce rate.
7. **Devnet and docs.** *Done when* the program is deployed to devnet, README and
   `docs/instructions.md` are written, and v0.1 crates are ready to publish (published after
   your go-ahead).

## Verified facts (checked 2026-10-28)

- `pinocchio` 0.12.0 exposes `pinocchio::sysvars::instructions::Instructions`
  (`try_from(&AccountView)` checks the sysvar address; `load_current_index`,
  `load_instruction_at`, `get_instruction_relative`; `IntrospectedInstruction::
  get_instruction_data`). It has no ed25519 program id constant; it is
  `solana_sdk_ids::ed25519_program::ID` (`solana-sdk-ids` 3.1.0), or declared locally.
  Source: the pinocchio 0.12.0 crate source, `src/sysvars/instructions.rs`.
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

## 6. Open questions

1. Fees: none in v1, or a taker fee to a treasury from day one?
2. Series creation: admin-only (proposed) or permissionless?
3. `max_quote_ttl`: 60 s proposed. Shorter for volatile underlyings?
4. Are open quotes (`taker = 0`) wanted at all in v1, or always taker-bound?
5. otc-rfq: copy the two service modules, or extract a shared crate both use?
6. Pinocchio 0.12 (released today) or 0.11 like matchbook? Proposal: 0.12.
7. Should `settle` support several fills (several makers) in one instruction for multi-leg
   RFQs, as metalbook spreads might want?
