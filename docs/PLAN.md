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
| 0 | `init_config` | payer, admin | config (w), program_data, system | creates `Config`; admin must be the program's upgrade authority |
| 0b | `set_config` | admin | config (w) | changes `admin` only |
| 1 | `create_series` | admin, payer | config (w), series (w), base_mint, quote_mint, system | creates `Series` |
| 2 | `set_series_paused` | admin | config, series (w) | pause/unpause |
| 3 | `init_maker` | maker, payer | maker_state (w), system | creates `MakerState` |
| 4 | `cancel_all` | maker | maker_state (w) | slides `window_base` to `new_min` (must increase), clearing bits |
| 5 | `cancel_nonce` | maker | maker_state (w) | marks one nonce used; a nonce beyond the window slides it first |
| 6 | `settle` | taker | series, maker_state (w), delegate PDA, maker_base (w), maker_quote (w), taker_base (w), taker_quote (w), base_mint, quote_mint, base token program, quote token program, instructions sysvar | verifies and fills (M3: the `maker` account is dropped; every check uses `quote.maker`) |
| 7 | (no `close_maker` in v1) | | | closing would destroy the replay bitmap: re-initializing the same PDA would make already-filled, unexpired quotes fillable again (codex review). `MakerState` is permanent; its rent (~0.015 SOL) is the price of a once-only guarantee. v2 option: a `created_at = now + MAX_CLOCK_SKEW` field checked as `quote.issued_at >= created_at` makes close and re-init safe |

`settle` data: `{ sig_ix_index: u8, fill_size: u64 }`. The quote itself is not in the settle
data: the program reads it from the precompile instruction, which avoids a 208-byte copy and
removes "the settle args and the signed bytes disagree" as a class of bug.

Tags: 0 `init_config`, 1 `create_series`, 2 `set_series_paused` (`paused: u8`, 0 or 1),
3 `init_maker`, 4 `cancel_all`, 5 `cancel_nonce`, 6 `settle`, 7 `set_config` (`new_admin:
[u8; 32]`, non-zero; only the admin changes). Each handler module (`program/src/processor/`) documents
its account order. Measured CU per instruction: `docs/benchmarks.md` (`make bench`).

### 3.3 Security checks per instruction

Every rule below gets a LiteSVM negative test with a forged or swapped account.

- **All:** program accounts owned by quotebook and carrying the right `tag`; PDA addresses
  re-derived from seeds with the stored bump; writable flags as listed.
- `init_config`: config PDA; admin signs **and is the program's upgrade authority** (decided in
  M2: otherwise anyone could front-run `init_config` after deploy and make themselves admin).
  The instruction takes the program's `ProgramData` account, which must be owned by
  `BPFLoaderUpgradeab1e11111111111111111111111`, sit at the loader PDA `[program_id]`, and
  record `admin` as its upgrade authority; a program without an upgrade authority cannot be
  initialized, so `init_config` runs before any authority is dropped. This assumes deploys
  through the upgradeable loader; a move to loader-v4 changes where the authority lives and
  needs a new gate. `deployment_id` must be non-zero.
- `create_series`: admin signs and equals `config.admin`; series PDA from mints and
  `config.series_count`; mints owned by SPL Token or Token-2022; Token-2022 mints rejected if
  they carry transfer-fee, transfer-hook, confidential-transfer, permanent-delegate,
  non-transferable, pausable or `DefaultAccountState = Frozen` extensions; `base_mint !=
  quote_mint`; lot sizes non-zero.
  Implemented in M2 as an **allowlist**, stricter than the list above: a Token-2022 mint may
  carry only `MintCloseAuthority`, `DefaultAccountState` (not `Frozen`),
  `InterestBearingConfig`, `MetadataPointer`, `TokenMetadata`, `GroupPointer`, `TokenGroup`,
  `GroupMemberPointer`, `TokenGroupMember` and `ScaledUiAmount`; everything else
  (`MintExtensionUnsupported`) includes the listed ones, `ConfidentialTransferFeeConfig`,
  `ConfidentialMintBurn`, `PermissionedBurn`, account-only types and any type added after
  spl-token-2022-interface 3.1.2. Malformed extension data (non-zero padding, wrong account
  type, a TLV entry past the end), an uninitialized mint, an SPL Token mint longer than 82
  bytes and a Token-2022 multisig are `InvalidMint`. Arguments: `max_quote_ttl > 0` and
  `min_size >= 1`, else `InvalidInstructionData`.
- Token-2022 account-level extensions at `settle`: a taker account with CPI Guard cannot
  transfer via CPI, and a receiving account with MemoTransfer requires a memo. v1 rejects
  both with a clear error; tests cover each.
  M3: the error is `AccountExtensionUnsupported` (131, appended). CPI Guard is rejected only
  when enabled on the taker's *paying* account (the owner-signed CPI it forbids); on the
  maker's paying account (a delegate transfer, which CPI Guard allows) and on receiving
  accounts it passes. A required memo is rejected only on a receiving account. Both are
  confirmed against the real Token-2022 program in LiteSVM.
- `init_maker`, `cancel_*`: maker signs; `init_maker` fails if the PDA is already
  initialized (tag set). Pre-funding grief: anyone can send lamports to a PDA address, which
  makes a plain `create_account` fail, so every PDA creation (`init_config`, `create_series`,
  `init_maker`) uses transfer to rent-exempt + `allocate` + `assign` when lamports > 0 (check
  matchbook's helper); LiteSVM test with a pre-funded PDA; maker_state PDA from that maker;
  `cancel_all` rejects a non-increasing watermark.
  Implemented in M2 with `pinocchio_system::create_account_with_minimum_balance_signed`
  (matchbook has no Pinocchio helper), which does exactly that fallback; creation derives the
  canonical bump with `find_program_address`, so no second account can exist for the same
  seeds. `cancel_*` reject another maker's state (`NotAuthority`) and re-derive the PDA from
  the stored bump with `create_program_address`.
- `settle`:
  - `taker` signs; it is not the maker (self-trade rejected: `SelfTrade`, 130, appended in
    M3).
  - `series` owned and tagged, not paused; mints equal the series mints; token programs equal
    the mints' owners.
  - `maker_state` PDA from `quote.maker`; `maker` account key equals `quote.maker`.
  - Token accounts: maker's are owned (SPL owner field) by `quote.maker`, taker's by the taker,
    each with the right mint; delegate PDA re-derived from `["delegate", quote.maker]` with
    `MakerState.delegate_bump`.
  - Instructions sysvar address equals the sysvar id (the Pinocchio `Instructions::try_from`
    check), then the ed25519 checks of 5.1. M3: the parsing lives in
    `quotebook_core::sigverify::verified_quote` (unit-tested off-chain);
    `sigverify::instruction_data` builds the matching precompile data for clients. The
    precompile consumes no compute units (measured; it costs one signature in the fee).
  - "price inside the series' limits" (invariant 6): the series has no price limits in v1,
    so there is nothing to check beyond `price > 0`.
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
`NonceOutOfWindow` otherwise. `cancel_all(new_min)` is the single slide the brief asks for.
2 KiB of bitmap costs about 0.015 SOL of rent once; milestone 4 measures the worst-case slide
CU.

**Issuance bound (corrected in M1 review).** Invariant 4 lets a quote be filled only while
`issued_at − MAX_CLOCK_SKEW <= now < expires_at`, a span of up to `max_quote_ttl +
MAX_CLOCK_SKEW` (70 s at 60 s + 10 s), not `max_quote_ttl`. The maker therefore follows:
*for every nonce `M`, `issued_at(M) >= expires_at(N) + MAX_CLOCK_SKEW` for every `N <= M −
16,384`* (`quotebook_core::min_issued_at_beyond_window`). Proof sketch: the window drops `N`
only when some `M >= N + 16,384` is consumed; a `settle` of `M` lands at `t >= issued_at(M) −
MAX_CLOCK_SKEW >= expires_at(N)`, when `N` is already expired, and `cancel_nonce` /
`cancel_all` are the maker's own choice. With `issued_at` non-decreasing in the nonce, the rule
holds if `issued_at(N + 16,384) − issued_at(N) >= max_quote_ttl + MAX_CLOCK_SKEW`: at most
16,384 nonces per 70 s, about 234 quotes per second. A TTL-only horizon is unsound (codex's
counterexample: a quote accepted at `now = 0` with `issued_at = 10, expires_at = 70`, then a
nonce 16,384 stamped 61 s later is fillable at `t = 61` and evicts it); that case is a
regression test in `quotebook-core/tests/eviction_bound.rs`. The bound uses the maker's
largest `max_quote_ttl` across the series it quotes, since nonces are per maker.

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
   in its worst case for each token program and including the precompile (0 CU, charged as a
   signature in the fee), and a comparison with matchbook's IOC fill. Decided in M4: the
   matchbook figures are cited from matchbook's own LiteSVM bench (commit `72efa6d`, IOC 1
   fill 1,660 CU, wallet swap 1 fill 4,487 CU) rather than re-run here, marked as a
   cross-repo citation with the design differences.
5. **Taker SDK.** *Done when* `quotebook-sdk` builds the two instructions from a `Quote` and a
   signature, and an end-to-end LiteSVM test uses only the SDK.
   Built in M5: `sdk/` with `pda` (every PDA), `ix` (a builder per instruction, plus the
   token-program `Approve` a maker sends for its delegate), `settle_instructions` (precompile
   + settle), `decode` (program accounts, owner checked) and `deployment_id_from_genesis_hash`.
   The account layouts, instruction tags and argument encodings moved into `quotebook-core`
   (`accounts`, `instruction`), so the program and the SDK share one definition.
6. **Maker service.** *Done when* a local demo (maker service + taker CLI + LiteSVM or
   solana-test-validator) requests, receives and settles a quote priced by market-maker-rs,
   and the service refuses to stamp a nonce below `min_issued_at_beyond_window` of the quotes
   16,384 nonces back (section 4.2: at most 16,384 nonces per `max_quote_ttl +
   MAX_CLOCK_SKEW`).
   Built in M6: `maker/` (`quotebook-maker`). `pricing` prices with market-maker-rs 0.6's
   `avellaneda_stoikov::calculate_optimal_quotes` and checks `|price − mid| / mid` against a
   tolerance (otc-rfq's `price_bounds` pattern, copied with attribution, no dependency: open
   question 5 settled that way). `market` converts a decimal price to ticks rounding toward
   the maker (bids down, asks up). `issuer` keeps the running maximum expiry for each of the
   last 16,384 nonces and refuses a nonce below `min_issued_at_beyond_window` of the one
   16,384 back. `server` answers JSON requests over one WebSocket per connection; `client`
   requests, verifies the signature and every term the program checks, and ranks by price
   (otc-rfq's `BestPriceStrategy` pattern, on integer ticks). `keys` keeps the key out of
   `Debug` and logs. The demo (`examples/demo.rs`, also run as `tests/demo.rs`) is LiteSVM +
   two maker services + the taker flow, settling the best quote through the SDK. Not in v1,
   behind traits: a Pyth Hermes feed (`PriceFeed`) and live inventory (`Inventory`). GLFT and
   the options feature are not used yet.
   **Restarts are fail-closed (codex review of #10).** The issuer's history is in memory, and
   on-chain `check_time` bounds each quote's own lifetime, not whether a later nonce evicts
   it, so a restarted maker could sign a nonce that evicts a still-valid quote it signed
   before (reproduced against the program in `maker/tests/restart.rs`).
   `NonceIssuer::resume` refuses every quote until `started_at + max_quote_ttl +
   MAX_CLOCK_SKEW` (every pre-restart quote has then expired), and after that until
   `refresh` applies the on-chain window read from then on, so it starts above every nonce
   used, including pre-restart quotes filled during the wait. `max_quote_ttl` is the largest
   lifetime the key ever signed across its series. Cost: a cold start of 70 s at 60 s + 10 s
   with no quotes; persisting the issuer would remove it (not in v1). `NonceIssuer::fresh` is
   only for a key that has never signed. `refresh` is allowed only before an issuer's first
   quote (codex review): once it has quoted, the bound rests on contiguous nonces, and
   jumping the counter (for instance after the maker `cancel_nonce`s a nonce a window ahead)
   could evict one of its own live quotes. A live issuer that must move past the chain is
   replaced by a new `resume`, which pays the cold start.
   **Ticks are exact (codex review of #10).** A `Decimal` quotient rounds to 28 digits before
   any floor or ceiling, which floored 6.9999999999999999999999999999 / 7 to 1 tick.
   `price_to_ticks` now divides the exact integer `mantissa × base_lot × 10^qd` by
   `10^(scale + bd) × quote_lot` in 256-bit integers, rounding the same way at each step
   (exact for positive integers), with regressions at that scale and a property test against
   an exact `u128` rational reference.
7. **Devnet and docs.** *Done when* the program is deployed to devnet, README and
   `docs/instructions.md` are written, and v0.1 crates are ready to publish (published after
   your go-ahead).
   Prepared (not deployed): the README now documents the product, architecture, accounts,
   instructions, quote format, nonce and restart rules, the Token-2022 allowlist, CU and error
   codes (it takes the place of `docs/instructions.md`); `docs/devnet.md` is the deployment
   runbook, with what is needed from Joaquin and the RPC tooling still to build (admin CLI,
   maker binary, taker CLI; a new RPC client dependency to approve).
   Tooling built (`cli/`, `quotebook-cli`): `quotebook-admin` (init-config, create-series,
   pause/resume, set-admin, init-maker, approve, cancel-all, cancel-nonce, show),
   `quotebook-maker` (reads the series and the maker's window over RPC, resumes fail-closed,
   keeps its clock on the cluster's and refreshes after the cold start) and `quotebook-taker`
   (quote: request and check against chain; settle: also send). The quote answer now carries
   the maker's token accounts, which `settle` needs. Every chain call goes through an `Rpc`
   trait: `Cluster` on `solana-rpc-client`, LiteSVM in `cli/tests/e2e.rs`, so tests never
   touch a network.

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
| `solana-address` | 2.9.0 (2.7.0 used) | `2.7`, `default-features = false`, feature `bytemuck`; exception below |
| `pinocchio-system` | 0.7.0 (0.6.1 used) | `0.6`, decided exception below |
| `pinocchio-token` | 0.7.0 | `0.7`, see conflict below (not needed until M3) |
| `litesvm-token` (dev) | 0.18.1 | `0.18` (not needed in M2: test mints are written as raw account data) |
| `solana-account`, `solana-instruction`, `solana-keypair`, `solana-signer`, `solana-transaction` (dev) | 5.4.0, 4.0.0, 4.0.0, 4.0.0, 6.0.0 | `4.7`, `3.5`, `3.1`, `3.0`, `4.3`: the generations LiteSVM 0.18.1 builds against |
| `solana-sdk-ids` | 3.1.0 | `3.1` |
| `solana-ed25519-program` (SDK, dev) | 3.0.0 | `3.0` |
| `solana-instructions-sysvar` (SDK) | 5.0.0 | `5.0` |
| `market-maker-rs` (maker) | 0.6.0 | `0.6` |
| `rust_decimal` (maker) | 1.43.0 | `1.43` |
| `thiserror` (SDK, maker) | 2.0.21 | `2.0` |
| `market-maker-rs` (maker) | 0.6.0 | `0.6` |
| `tokio`, `tokio-tungstenite`, `futures-util` (maker) | 1.54.1, 0.30.0, 0.3.34 | `1.54`, `0.30`, `0.3` |
| `serde`, `serde_json` (maker) | 1.0.229, 1.0.154 | `1.0`, `1.0` |
| `tracing`, `tracing-subscriber` (maker; subscriber in the demo only) | 0.1.44, 0.3.23 | `0.1`, `0.3` |
| `solana-signature` (maker, `verify`) | 3.6.0 (3.5.2 used) | `3.5`: the LiteSVM generation, like `solana-keypair` |
| `solana-rpc-client` (CLI only) | 4.4.0 (4.3.2 used) | `4.3`: decided by Joaquin (official client, off-chain only, same in every repo). 4.4.0 pulls the agave 4.4 crates LiteSVM 0.18.1 does not build with; 4.3.2 is the newest that resolves with the LiteSVM pins (checked 2026-10-29) |
| `solana-commitment-config`, `solana-hash` (CLI) | 3.1.1, 4.7.0 (4.6.0 used) | `3.1`, `4.6`: what `solana-rpc-client` 4.3.2 uses |
| `solana-instruction` (SDK) | 4.0.0 | `3.5`: the SDK's `Instruction` must be LiteSVM's, so it stays on the LiteSVM generation (exception below) |

**LiteSVM pin (M2).** LiteSVM 0.18.1, the latest, requires `solana-address ~2.7` and the
Solana client crates of the 4.3 generation; newer releases of those crates (agave 4.4,
`solana-account` 5) do not compile with it, so the workspace uses `solana-address 2.7` and
the versions in the row above, and `Cargo.lock` was seeded from optfix's. Same exception as
optfix; move with LiteSVM.

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
5. otc-rfq: decided in M6: copy the two patterns (price bounds, best-price ranking) with
   attribution; no shared crate.
6. Decided (2026-10-29): Pinocchio 0.11 generation (exception above).
7. Proposal (from review, pending your approval): no multi-fill instruction in v1; an explicit `sig_ix_index` already
   allows several `settle` instructions (one per maker or leg) in one atomic transaction.
