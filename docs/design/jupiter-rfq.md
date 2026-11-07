# Jupiter RFQ adapter (design)

**Status:** proposed, the design doc of milestone D6 in [`PLAN-phase2.md`](../PLAN-phase2.md). The
prototype (`jupiter-rfq/`) follows D1. Nothing here registers with Jupiter, calls a Jupiter
endpoint, signs or sends. Mainnet and registration stay out of scope.

**Conclusion:** a Jupiter adapter cannot settle through quotebook's `settle`. It can only put
quotebook's pricing and part of its risk engine behind Jupiter's webhook, and co-sign fills of
Jupiter's own program. The commitment moves from a signed quote to a co-signed transaction, so D1's
risk book carries over in shape, but its nonce, delegate cap and taker allowlist do not.

## Sources

All retrieved on 2026-11-05, read as plain documents.

| Short name | Source |
| --- | --- |
| Overview | <https://developers.jup.ag/docs/swap/routing/rfq/v1/overview.md> |
| Webhook API | <https://developers.jup.ag/docs/swap/routing/rfq/v1/webhook-api.md> |
| Quoting | <https://developers.jup.ag/docs/swap/routing/rfq/v1/quoting.md> |
| Settlement | <https://developers.jup.ag/docs/swap/routing/rfq/v1/settlement.md> |
| V2 | <https://developers.jup.ag/docs/swap/routing/rfq/v2/streaming.md> |
| Old FAQ | <https://jupiterz.jup.ag/docs/faq> |
| Old breaker | <https://jupiterz.jup.ag/docs/circuit_breaker> |
| Toolkit | <https://github.com/jup-ag/rfq-webhook-toolkit> at `3aa4fce515d4b5827ee2d48f08041ea706d911cd`, the newest commit on `main` (2026-09-11), MIT |

Toolkit paths below are relative to that commit.

## What Jupiter's specs require

### V1: webhook, Order Engine

**Endpoints** (Webhook API), under a base URL given to Jupiter at registration:

| Endpoint | Purpose |
| --- | --- |
| `POST /quote` | Answer one quote request. 200 quotes; 404 declines, with no penalty |
| `POST /swap` | Receive a taker-signed transaction; verify, co-sign and land it |
| `GET /tokens` | The mints supported, polled every 10 minutes |
| `GET /permissioned-tokens` | Optional: mints minted or burned just in time. Not needed here |

Persistent 5xx responses suspend the webhook. An API key, if used, arrives as `X-API-KEY`.

**The flow** (Toolkit `README.md`, Overview):

1. Jupiter dispatches the request to all webhooks at once and compares values; speed only breaks
   ties. It simulates each quote and silently drops those that fail.
2. Jupiter builds the transaction and the taker signs it.
3. Jupiter checks it and forwards it to `POST /swap`.
4. The maker verifies, co-signs and lands it, then answers `accepted` or a rejection.

**Quote lifetime** (Quoting): fixed at 55 s, and not the maker's choice. The swap is forwarded only
while 20 s remain, so the taker has about the first 35 s.

**Settlement** (Settlement; `programs/order-engine/src/`):

- The program is `61DFfeTKM7trxYcPQCM78bJ794ddZprZpAwAnLiwTpYH` on mainnet, audited by Offside
  Labs in November 2024.
- `fill(input_amount, output_amount, expire_at)` requires `taker` and `maker` as signers and
  `expire_at >= Clock::unix_timestamp`.
- **The maker is the fee payer** and signs slot 0: the validator requires it, with 2 signers (maker,
  taker) or 3 (maker, ATA payer, taker). In V2 the maker signs index 1 instead.
- There is no off-chain signed quote. A quote is unsigned JSON (`openapi/openapi.json`), and the
  maker has last look at `/swap`.

**Inventory** (Webhook API, Toolkit `tests/README.md`): a hot wallet holding the tokens, with an ATA
for every advertised mint and SOL for network fees. A missing ATA fails simulation.

**Fees** (Quoting): forwarded as `feeBps` and applied by Jupiter while building the transaction. On a
quote of 1,000 USDC with 100 bps, only 990 USDC leaves the maker, and the 10 USDC is "accounted for
asynchronously". The maker quotes without accounting for it.

**Onboarding** (Overview): a request through Jupiter's support form, then Edge (pre-production) and
production. This plan does none of it.

**Network:** no page read mentions devnet. The toolkit's program has only a placeholder id without
the `production` feature. Edge is not a test network: its integration tests "perform real swaps on
mainnet" (Settlement).

### V2: gRPC streaming

From V2:

- Two persistent streams: `StreamQuotes` pushes order book snapshots, up to 5 levels a side, each
  with an expiry of at least 10 s. `StreamSwap` receives transactions to co-sign, with a 10 s
  deadline and no fallback.
- Fills go through the v6 aggregator (`JUP6…TaV4`) as part of a Metis route, which invokes `rfq_v2`
  (`fd3n…Br5`, one instruction, `fill_exact_in`). It rejects the fill if the maker's keys appear in
  any other instruction (error 6005).
- A returned message that differs from what was sent forces the maker offline at once.
- There is no self-serve onboarding; Jupiter issues the API key and maker id.
- V2 does not replace V1; they run concurrently.

**The prototype targets V1.** Its request and response shape can be exercised offline with the
toolkit's suite. V2 needs a live gRPC endpoint and credentials to do anything.

## The settlement gap

quotebook's model and Jupiter's differ at each step that matters:

| | quotebook | Jupiter V1 |
| --- | --- | --- |
| What commits the maker | A signed quote, fillable by its named taker | A co-signed transaction |
| Who builds the transaction | The taker | Jupiter |
| Program | quotebook `settle` | Order Engine `fill` |
| Maker's funds | Delegate on a token account, capped by the allowance | Direct signature from a hot wallet |
| Replay protection | On-chain nonce window | None in the program; a signature lands once |
| Who pays and lands | The taker | The maker |
| Withdraw | `cancel_all`, `cancel_nonce`, `revoke` | Nothing: a co-signed transaction lives to `expire_at` |

quotebook's signed quote and `settle` play no part. A `settle` cannot even be added to the
transaction: the toolkit's validator bails on any program outside its list (below), the maker must
not alter the message, and Jupiter builds it.

## What can be reused

- **Pricing:** `maker::pricing` and D2's marks, called with a stored spot, since a 250 or 500 ms
  deadline leaves no room for an RPC read per request. The fee stays out of the quote, as Quoting
  says.
- **The token list:** `GET /tokens` is the configured set of markets.
- **The shape of D1's risk book:** one lock over limits and reservations, finalized snapshots,
  failing closed, the halt file. The next section lists what changes.

**D2's spot is not reusable as configured.** D2 accepts a 600 s spot because it signs only for its
own taker. Webhook takers are the public, and `allowed_takers` has no equivalent, so a real
integration needs a fresh price source. The prototype can use D2's marks because it never fills.

## What D1's risk book carries over

D1 assumes a nonce and a signed quote. V1 has neither, so each mechanism needs an equivalent:

| D1 | V1 equivalent |
| --- | --- |
| Nonce issued under the lock | `quoteId`, single use: consumed at `/swap`, so a second transaction for one quote is refused. Keyed also by the message hash, so a retry of the same transaction is idempotent |
| Reservation when the quote is signed | Reservation when the transaction is **co-signed** (see below) |
| Released when `window.check` fails | Released when the signature is finalized, with its balances in the same snapshot |
| Released when Clock `>= expires_at` | Released when the signature is absent and finalized Clock `> expire_at`. Strictly greater: `fill` still passes when they are equal, unlike `settle` |
| `min(balance, delegated_amount)` | The balance alone. **There is no on-chain cap:** what the hot wallet holds is what can be lost, so it holds only that |
| `max_fills_per_hour` from nonces used | Order Engine `fill`s with this maker as signer, from the wallet's finalized signatures; rebuilt the same way after a restart, and refusing for 3,600 s of chain Clock if it cannot be |
| `allowed_takers` | None: takers are public |
| Cold start of `max_quote_ttl + 10 s` | No co-signing until a finalized Clock is past the last possible `expire_at`, 55 s from the last quote |
| Halt, drain | Halt declines `/quote` and rejects `/swap`. Drain waits for finalized Clock `>` the newest `expire_at` |
| `cancel-all`, `revoke` | **None.** The only emergency move is transferring funds out of the hot wallet |

**New limits**, with no D1 counterpart: a network fee budget, since the maker pays base and priority
fees on transactions it did not build (see the validator's fee budget); and the fee liability below.

**Reserve at `/swap`, not at `/quote`.** The frontend re-quotes every 3 s and a quote lives 55 s, so
one user can hold about 18 live quotes, some with no taker named. Reserving each would lock the
inventory many times over. So `/quote` records a bounded, unreserved entry by `quoteId` with a 55 s
life, and checks only against what `/swap` has reserved.

**Refuse at `/quote`, not at `/swap`.** This is the main change from D1. A 404 carries no penalty,
but a rejected swap counts as a failure (Settlement). Every limit therefore runs at `/quote`, and
again at `/swap` only as the last defence. Size per quote is capped low against available inventory,
because two winning quotes can still collide; the loser becomes a rejection.

**The fee is a liability until proven otherwise.** Quoting's "accounted for asynchronously" reads as
the maker owing the difference later. So the book reserves the gross amount and accrues the fee. The
sources do not say how or when it is collected, and that would need asking Jupiter.

## The fill-transaction validator

The maker signs a transaction it did not build, as fee payer. V1 has no on-chain exclusivity check
like `rfq_v2`'s, so this validator is what protects the wallet, and it has to be total. Jupiter
checks too, and "webhooks should verify as well" (Settlement).

The prototype reimplements `validate_fill_sanitized_message` (`order-engine-sdk/src/fill.rs`) from
its source, since that crate pulls in Anchor and `solana-sdk`. Beyond the plan's list:

- **Message:** fee payer is the maker; 2 or 3 signers, with the taker among them after the first.
- **Programs:** only these, anything else is refused:
  - Compute Budget: `SetComputeUnitLimit` and `SetComputeUnitPrice`, each exactly once, with values
    bounded as in the fee budget below; `RequestHeapFrame`, `SetLoadedAccountsDataSizeLimit` and
    any other Compute Budget instruction are refused;
  - Associated Token: idempotent create only, and the payer is not the maker;
  - System: `Transfer` only, with the taker as source;
  - Token and Token-2022: `TransferChecked`, with the taker as authority, and `SyncNative` only,
    for the expected receiver or integrator accounts;
  - Lighthouse: discriminators 5, 6, 9 and 10 only; the toolkit's comment warns that its memory
    write would let an attacker drain the signer;
  - Order Engine: exactly one `fill` with the right discriminator.
- **Fill:** taker, maker, mints, both amounts and `expire_at` equal the stored quote's. `expire_at` is
  still in the future with enough margin to land.
- **Trailing payload:** 37 bytes of instruction data (see the disagreements). Anything else is refused
  with its reason.
- **Refused outright in the prototype:** address lookup tables, which the toolkit's own decoder does
  not load (`order-engine-sdk/src/transaction.rs`), and Squads-wrapped transactions, whose flags
  are "not faithful" and whose maker slot differs (Settlement). A real integration needs the
  Squads path; the sample server does not have it.

### The fee budget

Requiring the two Compute Budget instructions protects nothing on its own: a taker can set any price,
and a SOL floor only says when the wallet is empty, not what one transaction may spend. The maker pays
the fee whether the transaction succeeds or fails, so the validator bounds it before co-signing:

- **Caps,** all in the maker's config, none with a built-in default: `max_cu_limit`,
  `max_cu_price` (micro-lamports per CU), `max_fee_lamports` per transaction, a rolling
  `max_fees_per_hour`, and `sol_floor`.
- **Cost of one transaction,** computed from the message itself:
  `signatures × lamports_per_signature + ceil(cu_limit × cu_price / 1,000,000)`. `signatures` is the
  header's `num_required_signatures`, not the 2 or 3 the validator expects, so any extra signer is
  priced (and then refused by the signer check). Precompile signatures cannot add to it, since the
  precompile programs are not on the allowed list. `lamports_per_signature` comes from config
  (5,000 today) and is refreshed in the background, never with an RPC call inside `/swap`.
- **Arithmetic:** the product is computed in `u128` and every step is checked; an overflow, a
  `cu_limit` above `max_cu_limit`, a `cu_price` above `max_cu_price`, or a total above
  `max_fee_lamports` refuses the transaction with its reason. No path panics.
- **Reservation,** under the same lock as the inventory reservation and before co-signing: the
  transaction is refused unless `sol_balance − reserved_fees − fee >= sol_floor` and the hour's
  spent plus reserved fees stay within `max_fees_per_hour`. Two concurrent `/swap`s therefore cannot
  both pass against the same lamports.
- **Release:** when the signature is finalized, the reservation becomes spent at the fee its metadata
  reports, success or failure, and counts against the hour. When the signature is absent and the
  finalized Clock is past `expire_at`, the reservation is released. After a restart, the hour's spend
  is rebuilt from the wallet's finalized signatures, as for `max_fills_per_hour`; if it cannot be,
  co-signing stays refused for 3,600 s of chain Clock.

Tests for the prototype: a `cu_price` of `u64::MAX`; a limit and price whose product overflows `u64`;
each cap exceeded by one; an extra required signature; a duplicate `SetComputeUnitPrice`; a
`RequestHeapFrame`; a missing Compute Budget instruction; two concurrent reservations that fit the
floor only one at a time; a failed transaction whose fee is kept as spent; and a restart whose hourly
spend cannot be rebuilt.

**Decoding:** `axum` and `base64`, the plan's dependencies, do not decode a versioned transaction.
The prototype either parses it by hand or asks for one `solana-*` crate in its PR.

**The sample server is not a model for this.** `server-example/src/server.rs` has a placeholder where
validation belongs, and its sending code is commented out.

## The fill-rate obligation

quotebook has no such thing today: a maker that declines a quote loses nothing. Webhook makers are
measured on two separate things, according to the current pages:

- **Monitor:** fill at least 95% of quotes won, over a rolling 24 h. It "only alerts" (Quoting).
- **Circuit breaker:** confirmed swaps record success; rejected, dropped and failed swaps record
  failure, unless the user held the quote for more than 15 s. Around 5 accumulated failures suspend
  the webhook, with a timeout growing from 2.5 to 15 minutes. Around 8 trips disable it, and recovery
  is manual (Settlement).

For the design, that means last look is not free, landing is the maker's job and its failures count,
and the conservative behaviour D1 prefers, refusing when unsure, has a cost here unless it happens at
`/quote`. The prototype cannot be measured: it never lands anything. It logs what it would have
rejected at `/swap` and why, since that rate is what would trip the breaker.

## Where the docs disagree

The newer site is the current one, though nothing states which wins. The prototype takes the stricter
reading where one exists, and any real integration has to confirm these with Jupiter.

| Item | Newer (developers.jup.ag) | Older | Prototype |
| --- | --- | --- | --- |
| Quote deadline | 500 ms (Webhook API, Quoting) | 250 ms (Toolkit `README.md`, Old FAQ) | 200 ms by default, and never beyond `x-request-timeout` |
| Swap deadline | 40 s (Webhook API) | 25 s (Old FAQ) | Bound by `expire_at`, at least 20 s away, and well under both |
| Fee payload | 5 bytes: `fee_bps`, `flags`, `integrator_fee_bps` (Quoting) | 3 bytes: fee, flags (Toolkit `README.md`) | 5 bytes. Quoting warns that a 3-byte read gives the wrong fee |
| Fill-rate window | Rolling 24 h, alerts only (Quoting) | 1 h, then suspension (Old FAQ, and Toolkit `README.md` under "Future considerations") | Not enforced; logged |
| Suspension timeout | 2.5 to 15 minutes (Settlement) | 5, 10, 20, 30 minutes (Toolkit `README.md`) | Not applicable |
| Slow-user exclusion | 15 s (Settlement) | 20 s (Toolkit `README.md`) | Not applicable |
| Re-quote interval | 3 s (Quoting) | 5 s (Toolkit `README.md`) | Not assumed |

Two more inside the current material:

- **Recovery:** Quoting says a suspended webhook "must be manually re-enabled"; Settlement says a
  successful fill after the timeout restores it, and that only the disabled state is manual.
- **Rejection shape:** `openapi/openapi.json` lists `state` as `accepted` or `rejected`, with
  `rejectionReason`. `webhook-api/src/enums.rs` adds `rejectedWithReason`, and the acceptance suite
  expects `{ "rejectedWithReason": "insufficientBalance" }`. **The types cannot come from the OpenAPI
  schema alone**, as the plan has it; they need `enums.rs` too.

## The acceptance suite

`tests/suites/acceptance/test.ts` calls only `WEBHOOK_URL`, so it can run offline as approved. Its
`/swap` tests are written for the sample server, though:

- it expects `accepted` for a fixed transaction whose `quoteId` was never quoted, which a real
  validator refuses;
- it selects rejections through magic `requestId`s (`00000000-0000-0000-0000-00000000000N`).

So the prototype passes those cases only behind a Cargo feature, `acceptance`, off by default, which
answers from those ids. It holds no key in any build, so even that path signs nothing. Its real
behaviour is covered by its own tests with forged fills.

`tests/` has both `package-lock.json` and `pnpm-lock.yaml`, and the Makefile uses `pnpm`. The plan's
`npm ci --ignore-scripts` has a lockfile to use, but whether it is in sync is not known until it runs.

## Whether a quotebook-settled path could exist

Only as a routing venue: Jupiter's router would have to know quotebook's program and build `settle`
into a route. That is a separate integration, on Jupiter's side, and out of scope. V2 is no shortcut,
since its fills go through Jupiter's own `rfq_v2`. No page read documents a way for a third-party RFQ
program to join.

## The prototype as built

`jupiter-rfq/` (D6's prototype) implements the sections above, with these choices where they
leave room:

- **Transactions** are parsed by hand (`tx.rs`), strictly: one packet, canonical lengths, no
  trailing bytes, the sanitize rules. Lookups are parsed, then refused.
- **`expire_at`** is not known when quoting, since Jupiter sets it. A fill is accepted when it lies
  in `[quoted_at, quoted_at + 55 s + 10 s of skew]` and at least `min_landing_secs` (20 s in the
  shipped config) after now.
- **Accounts:** the maker's two token accounts in `fill` must be the configured ones, and the taker's
  signature must verify with the maker's slot still empty; the toolkit's validator checks neither.
- **Not quoted:** native-SOL paths (an optional token account left out), a separate receiver and an
  integrator fee. `/quote` declines a receiver other than the taker; the validator refuses System
  and token transfers and a non-zero `integrator_fee_bps`.
- **Flags:** bit 0 must match the quote type; any other bit is refused.
- **Tracking:** until the maker would sign, a transaction is identified by the taker's signature.
- **No chain reads:** the binary reports a static wallet and static mids from its config; the desk's
  release and rebuild logic is exercised against a fake chain in tests.
- **At `/quote`** the per-transaction fee cap stands in for the unknown fee in the SOL floor and the
  hourly budget.
- **The OpenAPI example transaction** is not valid base64 (841 characters); the acceptance suite's
  transaction is the one that decodes.
- **The acceptance suite was not run:** `npm ci --ignore-scripts` refuses its out-of-sync
  `package-lock.json`, and the pinned alternative, `pnpm-lock.yaml`, needs `pnpm`, which is not
  installed. Its eight cases are replayed in `tests/acceptance.rs` under the `acceptance` feature.

## Not verified

- How the fee rounds, on which side it applies for exact-out, and how the accrued fee is collected.
- The Order Engine program's behaviour on mainnet: this reads its published source, and no build was
  compared with the deployed binary.
- The acceptance suite itself: its `package-lock.json` is out of sync, so `npm ci` refuses it.
- How Jupiter reads `x-request-timeout`; the prototype takes milliseconds.
- Anything in Jupiter's registration or Edge, by design.
