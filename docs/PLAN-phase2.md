# quotebook: phase 2 plan (devnet operations)

Status: **approved** (Codex, msg 55429, at `d35979b`; merged in #40). Brief from SOLANA-PROJECTS, msg 51619 (2026-11-03);
decisions from msgs 51662, 53465 and 53489 and metalbook's facts from msg 52647 are folded in
(see "Decisions"). No item is open: the spot source is decided and its sample passed ("Spot
source").
Baseline: `main` at `cf423b2`. The program is **frozen at `3d0ec13`** for the external audit
(`docs/audit-scope.md`): no milestone here touches `program/` or `quotebook-core/`. Devnet runs
that binary (`docs/devnet.md`, "I10 upgrade and smoke tests"). All work is off-chain and
devnet only.

Process: as in [`PLAN-improvements.md`](PLAN-improvements.md). One branch and one PR per
milestone (`feat/d<N>-<name>`), `make pre-push` and `make lint` clean before each push, Codex
reviews every PR before merge, and a report to SOLANA-PROJECTS at each milestone.
`rules/global_rules.md` applies unchanged. New dependencies are listed per milestone; this
plan's approval is their approval. Effort: **S** up to a day, **M** two to four days, **L** more.

Scope (msg 51619):
1. A hosted demo maker on devnet: launchd, its own funded payer, quoting metalbook series
   priced with optfix 0.3 strips, risk limits, logs and alerts like `varswap-watch`.
2. A Jupiter RFQ adapter: a design doc and an off-chain prototype against Jupiter's published
   spec, with no external integration or sign-up.
3. A real, recurring quotebook × metalbook trade (a taker script), recorded in
   `docs/devnet.md` and coordinated with metalbook for the series.

## Facts this plan relies on (checked 2026-11-03 to 2026-11-05)

**quotebook maker** (`maker/`, not in audit scope)
- It prices through two traits, `server::PriceFeed` (`mid() -> Option<Decimal>`, `None` when
  stale) and `server::Inventory`.
- A `MakerService` serves **one** series and owns its `NonceIssuer`
  (`maker/src/server.rs:88`, `:117`).
- Nonces are per maker, not per series (`MakerState`). Two services under one key would
  therefore race the same nonce space.
- `QuoteRequest` already carries `series` (`maker/src/protocol.rs:22`), so one service can
  route requests to several markets.
- The cold start is `max_ttl_any_series + 10 s` on every restart (`maker/src/lib.rs`).
- `respond` checks size and price, then locks the issuer only to take a nonce
  (`maker/src/server.rs:209`), so a limit checked before that lock would not be atomic with it.
- On chain a quote fills only while `issued_at − 10 s <= now < expires_at`
  (`quotebook-core/src/quote.rs:137`). `NonceWindow::check` is public and fails for a nonce that
  is used, cancelled or below the window, so reconciling fills needs no change to `core`.
- `quotebook-admin` already has `cancel-all`, `approve` and `revoke`, with keypair paths
  (`cli/src/bin/quotebook-admin.rs`).

**metalbook** (origin/main `275525d`)
- Program: `FPPu…suwg`, with Circle devnet USDC (`4zMM…ncDU`, 6 decimals) as collateral.
- Long tokens are 0-decimal mints at `["long", series]`. A writer mints `n` of them by locking
  `n × max_payout`.
- A capped call pays `min(S − K, cap − K)` with the cap 10% above the strike. At strike 50 on a
  50 oz XAG series, that locks (55 − 50) × 50 = 250 USDC per contract.
- Expiries are weekly, Fridays 16:00 New York. The calendar holds Nov 6, 13, 20 and Dec 4, 11,
  18.
- Listing is done by hand (`metalbook-admin list-week`); there is no roller and no launchd
  agent.
- **1 oz series already exist** (metalbook, msg 52647: `Config.multiplier` is 1000 for both
  metals, read on devnet 2026-11-04; only the Nov 6 series are 50 oz). Nov 13 (expiry
  1794603600) has 24 live 1 oz series, 14 XAU and 10 XAG. Its XAG capped calls are:

  | Strike | Locks | Series |
  | --- | --- | --- |
  | 48 | 4.8 USDC | `ZEmgA2iSdz2ruWcX6ARi8jZR6MEiiCpiDjzBGiVZAdW` |
  | 49 | 4.9 USDC | `R1KQh6RoAbymFvXZ3uxxCzqKshDdNRJzoVar2KgiAsW` |
  | 50 | 5.0 USDC | `B66MftMja6L6g2XFNLWHUWE2Rjq1yTHtToWQTvxsZx21` (paired with quotebook series 1 since I9) |
  | 51 | 5.1 USDC | `6NuEV8mbELdVAehtszXJpsHP11mmwaMDkoqWgwMXuZ6r` |
  | 52 | 5.2 USDC | `HzUsyCk2AAZ1Zy7DS6KLYLB1bprBjsSjmVmgLc1CpWxS` |
- **Roller** (metalbook's phase 2, from about Nov 9): every 6 h, skipping 2 h around each expiry,
  it lists the first calendar expiry at least 3 days ahead, a put and a capped call at every $1
  XAG and $50 XAU strike within ±5% of spot. There is no Nov 27 expiry, so after Nov 20 nothing
  is listed until about Dec 1. Keepers run by hand on Nov 13 and Nov 20, under launchd after
  that; a writer's collateral stays locked until settlement.
- `metalbook-core` and `metalbook-sdk` 0.1.0 as published are byte-identical to main's `src/`
  (`diff -r`, 2026-11-04), and `Series` is 504 bytes in the crate and on chain (`B66M…`).
- metalbook's keeper settles each series on a 30-minute Pyth average before expiry.
- `metalbook-maker::mark` prices a capped call as `C(K) − C(cap)` × multiplier, and refuses
  when the market is closed or the settlement window has begun. That crate is
  `publish = false` and still on optfix 0.2.
- Their dependencies are bytemuck, `solana-address` 2.7 and `solana-instruction`, the generation
  quotebook already uses.
- A writer's position is the 80-byte PDA `["writer", series, owner]` with `written` (contracts
  written and not closed) and `claimed` (`metalbook-core/src/accounts.rs`). That is the on-chain
  source for D5's writer cap.
- Its devnet log lists "Trading through QuoteBook, once the maker has more USDC" as not done.
- USDC on devnet (read 2026-11-04): metalbook's maker `7tVP…cACQ` holds 14.23 and can spare 14.00,
  with 10 to 15 more on request after Nov 13's redeem and claim (msg 52647). The I9 taker
  `3ZBj…sqLs` holds 8.77.

**Registry and the maker's transport**
- A profile endpoint must start with `wss://`: `quotebook-core/src/profile.rs:19` enforces it
  on chain, and `core` is frozen. `ws://127.0.0.1:9000` cannot be listed.
- The maker service accepts plain WebSocket only (`accept_async`, `maker/src/server.rs:313`);
  TLS exists only on the client side.

**Pricing inputs**
- **optfix 0.3.0** (crates.io, 2026-11-02) is `no_std` with Q64.64 fixed point.
  - `bsm::price_strip(expiry, &MarketInputs, &[StripLeg], &mut [Price])` prices many strikes
    of one expiry in one call. Each leg is bit-identical to a single `price`.
  - There is no vol surface: the caller passes one `Volatility`.
- **Spot, Hermes:** metalbook and varswap read Pyth Hermes with a shared key, whose quota is
  theirs (metalbook 7 requests a minute, varswap 3). The public endpoint is no alternative:
  `hermes.pyth.network/v2/updates/price/latest` without a key answered HTTP 401 `unauthorized`
  twice on 2026-11-04 (09:11Z and 09:18Z).
- **Spot, on chain:** devnet has Pyth push-feed accounts for both metals, `PriceUpdateV2` of 134
  bytes owned by the receiver `rec5…tFJ` at level Full. They are the shard-0 PDAs of
  `pythWSns…iRsT`: XAU `2uPQ…uEWW` and XAG `H9Jx…Znot` (51.9415 at 09:12Z). Neither metalbook's
  nor varswap's keeper is their writer: `write_authority` is the account itself. Sampled every
  15 s for 5 minutes on 2026-11-04, one update arrived 306 s after the last, the age peaked at
  295 s. **That is a single gap:** the cadence is not documented or guaranteed.
- **These feeds have failed before:** metalbook stopped using sponsored push feeds because the
  XAG account was stale when checked on 2026-10-29 (`metalbook/docs/devnet.md`, "Facts to rely
  on"; whether it was this same account is not known).
- **24-hour sample, 2026-11-04 09:21Z to 2026-11-05 09:21Z** (Wednesday to Thursday), one read
  a minute per feed, 1,441 reads each, no read errors:

  | | XAU | XAG |
  | --- | --- | --- |
  | Largest gap between updates, market open | 316 s | 316 s |
  | Typical gap (p50, p90) | 306 s, 307 s | 306 s, 307 s |
  | Gaps above 600 s | 1 (3,629 s) | 1 (3,629 s) |
  | Age at read, market open: maximum | under 320 s | under 320 s |
  | `conf / price`: p50, maximum | 0.51, 7.34 bps | 1.87, 9.42 bps |
  | Price move between consecutive updates: p50, p99, maximum | 5.0, 28.5, 37.2 bps | 9.3, 61.0, 61.7 bps |

  - The one gap above 600 s is 21:59:59Z to 23:00:28Z, the metals market's daily break (17:00 to
    18:00 New York). Every read older than 320 s falls inside it.
  - **Limits of this sample:** it is one day and has no weekend. Both feeds publish at the same
    instants, so they are one source and can fail together. It says nothing about tomorrow.
- `pyth-lite` 0.1.0 (crates.io, no dependencies) decodes and checks that account: owner,
  discriminator, length, verification level, feed id, positive price, staleness, confidence.
  It treats a `publish_time` ahead of `now` as fresh and does not check the exponent
  (`src/update.rs`, `PriceUpdate::verify`), so D2 adds both checks.

**varswap ops pattern** (origin/main `f5d5c7c`)
- `scripts/{keeper,watch,window-roll}-agent.sh install|uninstall|status <name>` write plists
  from heredocs, `plutil -lint` them and reload with `launchctl bootout` and `bootstrap`.
- Keys and URLs reach the agents as paths and environment variables. Secrets come from
  `zsh -c 'source ~/.zshrc; exec …'`, never from the plist.
- The watcher holds no key. It checks lag, balances (`--min-balance`), stale feeds and
  degraded state, and re-alerts every 3,600 s. Its alerts go to `VARSWAP_WATCH_ALERT_CMD`,
  which on devnet appends to `alerts.log`, plus JSON lines on stdout.
- There is no log rotation; `watch.jsonl` reached 830 KB in a day.

**Jupiter RFQ** (sources: developers.jup.ag `/docs/swap/routing/rfq/v1/*`, `v2/streaming`;
`github.com/jup-ag/rfq-webhook-toolkit`, MIT, pushed 2026-10-15)
- **V1 webhook.** Jupiter calls the maker:
  - `POST /quote` and `POST /swap`, with `GET /tokens` polled every 10 minutes;
  - the quote deadline is 250 ms or 500 ms (the docs disagree), sent as `x-request-timeout`;
  - 404 declines without penalty.
- **Jupiter builds the swap transaction.** The taker signs it first, then the maker verifies it,
  co-signs and sends it.
- **Settlement is Jupiter's Order Engine** (mainnet `61DF…TpYH`), not a third party's program.
  - Its `fill` requires taker and maker as signers.
  - The toolkit's validator rejects unexpected programs and duplicate fills.
  - There is no off-chain signed quote.
- **V2 (gRPC)** streams price levels and settles through `rfq_v2` behind the v6 aggregator.
- **No devnet support** is documented. Inventory sits in a hot maker wallet with ATAs.
- **Fill rate:** a 95% fill rate is required, enforced by a circuit breaker.
- **Onboarding** is by registration with Jupiter, which this plan does not do.
- **Consequence:** a Jupiter adapter **cannot settle through quotebook's `settle`**. It can
  only put quotebook's pricing and risk engine behind Jupiter's webhook and co-sign Order Engine
  fills.

## Milestones

### D1. Multi-series maker service, risk book (M)

The demo quotes every listed metalbook series under one maker key, so the service has to
route several markets through one nonce issuer, and every firm quote it has signed has to hold
its funds until it can no longer fill.

- **Routing:** `maker::server` serves a set of markets keyed by series address, all sharing one
  `MakerKey`. A request for a series outside the set is refused with `UnknownSeries`.
- **Per market:**
  - each market has its own `PriceFeed`, `Inventory`, `PricingParams`, TTL and `max_size`;
  - the cold start uses the longest `max_quote_ttl` across the set.
- **Compatibility:** the single-series constructor stays as a one-market wrapper, so
  `quotebook-maker` and its tests do not change behaviour. The limits are optional there.
- **One lock:** a `Mutex<RiskBook>` holds the `NonceIssuer` and the reservations together.
  Pricing runs before the lock. Under it, in one critical section: the halt check, every limit,
  the reservation, then the nonce. The quote is signed after the lock is released. If building
  it fails, the reservation is dropped, since nothing signed left the service.
- **Reservations:** each signed quote reserves what a fill would take from the maker:
  - a taker buy reserves `size` of the market's long token account;
  - a taker sell reserves `price × size` of the USDC account, which **all markets share**, so a
    quote on one series reduces what every other series can quote;
  - available = `min(balance, delegated_amount)` from the latest snapshot, minus all reservations
    on that account. A quote that does not fit is refused with `RiskLimit`.
- **Snapshots:** a `ChainView` trait returns one `getMultipleAccounts` at `finalized`, so all of it
  is from one slot: the Clock sysvar, the maker's `MakerState` and every maker token account. The
  RPC implementation is in `demo/`, using the client `cli/` already has; `maker/` gains no RPC
  dependency. It polls every 10 s.
- **Releases, only from a finalized snapshot.** A reservation ends when either holds in the same
  snapshot:
  - `window.check(nonce)` fails: the nonce was used or cancelled, and the balances read in that
    snapshot already include the fill;
  - Clock `>= expires_at`: the chain can never fill it again, and its balances include any fill
    that happened.
  - Nothing is released on wall-clock time or on `confirmed` data, so a dropped fork cannot
    release funds. What a fill brings in counts only once it is finalized, on the safe side.
- **Fail closed:** when the snapshot is older than `max_snapshot_age` (60 s), an RPC call fails or
  a snapshot's slot goes backwards, the service refuses everything with `RiskUnavailable` and
  releases nothing.
- **Restart:** reservations live in memory. After a restart no quote is issued until the cold
  start has passed **and** a snapshot's Clock is past its end, so every quote from before the
  restart is dead and its fills are in the balances.
- **`max_fills_per_hour` survives a restart.** A counter held only in memory would allow 4 fills,
  a 70 s restart and 4 more in the same hour. So the count is rebuilt from the chain, after that
  snapshot and before the first quote:
  - **Source:** `getSignaturesForAddress` on the maker's `MakerState` at `finalized`, then
    `getTransaction` on each successful one, walking back until `blockTime` is more than an hour
    behind the snapshot's Clock.
  - **Count:** every `settle` instruction, inner ones included, so a transaction holding several
    counts for all of them. A transaction with no `blockTime` counts as inside the hour.
  - **Fails closed:** if the history cannot be read in full (an RPC error, a page limit, a
    transaction that does not decode), the service assumes the limit is used up and refuses with
    `RiskLimit` until a finalized snapshot's Clock is 3,600 s past the restart.
- **Limits**, each with a `RejectReason`:
  - `max_open_quotes` across all markets: the number of reservations, so a quote stays open until
    it is released, not until its wall-clock expiry;
  - per-market `max_notional` in quote atoms, summed over the market's open quotes;
  - a per-market inventory band (`max_long`, `max_short` in base lots), projected: the
    snapshot's position with every open quote on that side assumed filled;
  - a global `max_fills_per_hour`: fills are nonces seen used in finalized snapshots, by that
    snapshot's Clock, never quotes issued. The check is `fills in the last hour + open quotes <
    limit`, so each firm quote counts as a possible fill. A nonce the maker cancelled itself also
    counts, on the safe side;
  - `allowed_takers`: a request for any other taker is refused with `TakerNotAllowed`, so the demo
    signs only for its own taker (msg 53465).
- **Halt and drain.** A halt file stops new quotes. **It does not revoke quotes already signed:**
  they stay fillable until they expire.
  - **The file:** when it exists, or when reading it fails with anything but not found,
    everything is refused with `Halted`. It is checked inside the lock on each request.
  - **Drain:** wait until a finalized snapshot's Clock reaches `newest_expires_at`.
  - **Revoke, in an emergency:** `quotebook-admin cancel-all --new-min <next nonce>`, then
    `revoke` on every maker account, then confirm both at `finalized`. A service that has
    cancelled restarts, paying the cold start, because a live issuer cannot jump its counter
    (`maker/src/issuer.rs`).
  - **Reuse:** no allowance is changed and no maker key is rotated or reused until a drain or a
    revoke is confirmed. If the service is down, there is nothing to ask: wait out
    `max_quote_ttl + 10 s` on the chain's Clock from when it stopped.
- **Status request:** a new `ClientMessage::Status` answers with `halted`, `open_quotes`,
  `next_nonce`, `newest_expires_at`, the snapshot's age and each spot's age. It takes no nonce and
  no reservation. The watcher (D4) and the drain use it; a quote is no longer needed as a probe.
- **Hard cap on chain:** the delegate allowance on each maker account. The service is
  configured below it and the watcher (D4) alerts as it drains.
- **Tests:**
  - unit tests for routing, every limit and the halt file, including an unreadable one;
  - concurrent requests on several series racing for the last USDC: exactly what fits is signed;
  - quote is not fill: expired and unfilled releases, filled counts toward the hour;
  - lag and rollback, with a fake `ChainView`: a stale snapshot, a slot going backwards, a fill
    seen and then gone before finality, an RPC error. Each refuses and releases nothing;
  - restart: no quote before a snapshot's Clock passes the cold start;
  - fill, restart, limit: 4 fills, a restart, and the fifth is refused until the hour has passed;
    a transaction holding several `settle`s counts for all of them; unreadable or truncated history
    refuses until 3,600 s of the chain's Clock have passed, not of wall-clock time;
  - a LiteSVM test with two series under one key: no nonce issued twice, and `cancel-all` kills a
    signed quote.

### D2. metalbook pricing with optfix 0.3 strips (M)

A new workspace crate `demo/` (`quotebook-demo`, `publish = false`), holding the
metalbook-specific parts so `maker/` stays generic.

- **Series:** read from chain with `metalbook-sdk`/`metalbook-core` 0.1 (decode `Series`;
  `long_mint`), filtered to capped calls and puts with an open market.
  - A static config gives the quotebook series that pairs each metalbook series (its long mint
    as base, USDC as quote), and the vol and rate per underlying.
  - **First step of D2:** check that the published 0.1.0 layout decodes the live devnet
    accounts. If it does not, pin a git `rev` of metalbook instead, per the rules.
- **Pricing:**
  - one `price_strip` call per expiry, with legs `Call K` and `Call cap` for every capped call
    and `Put K` for every put;
  - mark = `C(K) − C(cap)` (or `P(K)`) × multiplier, clamped to `[0, max_payout]`, the
    definition of `metalbook-maker::mark`;
  - the feed returns `None` (no quote) when spot is stale, the market is closed, or `now ≥
    expiry − settlement_window`.
- **Spot (decided, see "Spot source"):** read the two devnet push-feed accounts over RPC, once a
  minute. No Hermes and no key.
  - **Checks on each read:** `PriceUpdate::load` (owner, discriminator, length) and `verify` (level
    Full, feed id, positive price, age, confidence).
  - **Added, because `pyth-lite` does not check them:** `publish_time` at most 30 s ahead of `now`;
    the exponent inside `-12..=0`; `publish_time` never lower than the last one accepted.
  - **Fails closed at run time:** age is computed again on every `mid()` from the stored
    `publish_time`, so if the reader stops, quotes stop too. Older than 600 s means no quote, plus
    the `stale-spot` alert (D4). A failed read keeps the old value only until that limit.
- **The sample is an entry gate, not a guarantee.** Two separate conditions had to hold before
  quotes could be enabled: the coordinator's GO (msg 53489) and the 24-hour sample's pass. Both
  hold. Neither says the feed will keep working; the run-time rule above is what protects the
  maker.
- **600 s and 50 bps are demo settings, not a statement about price quality.** In open hours, a
  quote is priced on a spot about 160 s old at the median and up to about 380 s (the 320 s measured
  plus the one-minute read interval). The largest XAG move between updates in
  the sample, 62 bps, is about 0.16 USDC on a 1 oz at-the-money capped call, against an I9
  half-spread near 0.03 USDC, so a taker who knew the fresh price could pick the maker off. That is
  accepted only because the service signs for its own taker (`allowed_takers`), on localhost, at 1
  lot per quote under D5's real cap on written contracts. It is not acceptable for outside takers.
- **Confidence guard:** no quote when `conf / price` exceeds 50 bps. In the sample it would never
  have fired (9.42 bps at most), so it catches a broken feed, not a stale one.
- **Expected downtime:** no quotes from about 22:10Z to 23:00Z each day (the break, plus 600 s) and
  over the weekend. Both are correct behaviour, independent of the market-hours check.
- **Weekend behaviour is unmeasured:** the sample covers one Wednesday to Thursday. How the feeds
  behave across a weekend, and how soon they resume on Sunday, are assumptions until the watcher
  has seen one. D8 records what the first weekend showed.
- **Vol:** one number per underlying from config (the I9 demo used 0.30 for XAG), recorded in
  `docs/devnet.md` with the date; carry 0, rate 4%. A vol source is out of scope (optfix has no
  surface).
- **Units:** quotebook ticks are quote atoms per lot. The mark is converted at the series'
  `quote_lot_size` and rounded toward the maker on each side (`maker::market`).
- **Tests:** against fixtures produced by `metalbook-maker::mark` on the same inputs; strip
  marks must equal them exactly, since the legs are bit-identical. Also: a stale feed, the
  closed market, the settlement window, a future `publish_time`, an out-of-range exponent, a
  `publish_time` going backwards, and the reader stopping with a value in hand.
- **New dependencies** (versions checked on crates.io 2026-11-03):
  - `optfix` 0.3 (0.3.0);
  - `metalbook-core` and `metalbook-sdk` 0.1 (0.1.0);
  - `pyth-lite` 0.1 (0.1.0). Only `demo/` uses them; `ureq` is not needed.

### D3. Hosted demo maker on devnet (M)

- **Binary:** `quotebook-demo-maker` (in `demo/`): D1's service over D2's markets, with
  `tracing` JSON logs.
- **Keys:** a new maker key and a new payer, `~/.config/solana/quotebook-demo/{maker,payer}.json`,
  never in the repo or a plist. The payer is funded with SOL from `77ie…CPPV`.
  `init_maker` and delegate approvals are done once, recorded in `docs/devnet.md`.
- **launchd:** `scripts/demo-agent.sh install|uninstall|status <name>`, copied from varswap's
  `keeper-agent.sh`:
  - label `com.quotebook.demo-maker.<name>`;
  - `KeepAlive{SuccessfulExit: false}`, `ThrottleInterval` 120;
  - no secret in the environment: the spot source needs no key (D2), and the keypairs are file
    paths;
  - logs under `~/.local/share/quotebook-demo/<name>/`;
  - the binary copied out of the checkout, so a later `git pull` does not change what runs.
- **Restarts** pay the cold start (70 s at 60 s TTL). That is accepted and documented.
- **Endpoint:** listens on `127.0.0.1` only; no tunnel and no public endpoint (msg 51662). The
  taker runs on the same host with `--maker ws://127.0.0.1:9000`.
- **Registry profile:** listing only. The program requires `wss://` and the service speaks plain
  `ws://`, so the profile carries a documented `wss://localhost:9000` and the name `quotebook
  demo (localhost, not reachable)`. `discover` lists it, but `--discover` cannot connect to it,
  and the demo taker never uses that mode. Making it connectable needs TLS on the server, which
  is out of scope here.
- **Halt commands:** `scripts/demo-agent.sh halt <name>` (halt file and drain) and `revoke <name>`
  (D1's emergency path: `cancel-all`, `revoke`, confirmed at `finalized`).
- **Runbook:** `docs/runbooks.md` gets start, stop (by PID or `launchctl bootout`), halt, drain and
  revoke with the order D1 requires, key rotation, refilling and recovery from a crash loop.

### D4. Watcher and alerts (S)

`quotebook-demo-watch` (in `demo/`), mirroring `varswap-watch`. It holds no key and only reads
the chain and the maker's local status (D1).

- **Alert kinds:**
  - `balance` (payer SOL below `--min-balance`, default 0.3 SOL on devnet);
  - `inventory` (USDC or long tokens below a floor);
  - `collateral` (the writer's USDC below one contract's `max_payout` when the next cycle is
    due);
  - `allowance` (a delegate allowance below 20% of its approval);
  - `maker-down` (no answer to a `Status` request within 2 s; it takes no nonce or reservation);
  - `stale-spot` (a spot older than 600 s, read from the feed accounts by the watcher itself, so
    it does not depend on the maker). During a scheduled closure, it writes one line marked
    `expected` per closure and does not re-alert, since the daily break would otherwise alert every
    day. It is a normal alert from 600 s after the scheduled reopening (msg 55249).
    - **Only the listed windows count**, so the schedule cannot hide an unscheduled outage: the
      daily break, 17:00 to 18:00 New York, and the weekend, Friday 17:00 to Sunday 18:00 New York,
      both computed in New York time so daylight saving moves them. They are constants with tests,
      not inferred from the feed going quiet. A holiday or any other stop alerts.
    - An alert already firing when a window starts keeps re-alerting; a window never silences it.
    - The longest a real outage can stay quiet is one that starts just before a window, until 600 s
      after that window ends. Quotes are stopped throughout either way;
  - `risk-unavailable` (the maker's snapshot is older than `max_snapshot_age`);
  - `halted` (the halt file exists, re-alerted as any other);
  - `writer-cap` (open written contracts or committed collateral, read from chain, above D5's cap;
    this should never fire, so it means a bug or a key used elsewhere);
  - `no-series` (no quotable series while the market is open and the calendar says one should
    be listed). It reads the calendar so the known gaps (after each expiry, until the roller's
    next listing; Nov 20 to about Dec 1) stay silent, as does a series the writer cap is blocking
    (D5);
  - `fill-error` (the taker script's last run failed, read from its log).
- **Delivery:**
  - each alert runs `QUOTEBOOK_WATCH_ALERT_CMD` with `KIND`, `SUBJECT` and `MESSAGE`, which on
    devnet appends to `~/.local/share/quotebook-demo/<name>/alerts.log`;
  - JSON lines go on stdout;
  - re-alert after 3,600 s;
  - nothing goes on the bus, as varswap's devnet watcher (coordinator ruling).
- **Logs:** rotation by size (10 MB, keep 5) in the binary, fixing varswap's unbounded
  `watch.jsonl`.
- **launchd:** `com.quotebook.demo-watch.<name>`, `KeepAlive` true, `--interval 60`.
- **Tests:** each alert kind against a fake chain, re-alert timing, rotation.

### D5. Recurring quotebook × metalbook trade (M; needs metalbook)

- **Series:** the 1 oz XAG capped calls of Nov 13 are live, so D5 does not wait for the roller.
  From about Nov 9 the roller lists each next expiry at least 3 days ahead, with strikes around
  spot. That is enough for 1 lot every 6 h (metalbook asked; answered in msg 52647's thread).
- **The demo maker is the writer:** it locks its own devnet USDC in metalbook and sells the long
  tokens through quotebook. This is the only milestone that writes metalbook state.
- **What 14 USDC can write** (one contract, 1 oz; the cap is 10% above the strike):

  | Series | Locks per contract | Writable with ~14 USDC |
  | --- | --- | --- |
  | XAG capped call, strike 48 to 52 | 4.8 to 5.2 USDC | yes, 2, leaving about 3.6 to 4.4 USDC to buy back |
  | XAG put, strike 50 | 50 USDC | no |
  | XAU capped call, strike 4,000 | 400 USDC | no |
  | XAU put, strike 4,000 | 4,000 USDC | no |

  So the demo **trades XAG capped calls only** and quotes nothing it cannot write. D2 still
  prices XAU and puts, and tests cover them. Trading XAU needs at least 400 USDC per contract,
  which is a funding request to SOLANA-PROJECTS, not a code change.
- **Funding, in this order:** D5 starts by sending metalbook the demo maker's USDC account;
  metalbook signs a 14.00 USDC transfer from `7tVP…cACQ` to it; the first cycle runs only after
  that lands. 10 to 15 more are available on request after Nov 13.
- **Limits sized to that funding:**
  - `max_size` 1 lot;
  - inventory band 0 to 2 long tokens, so the maker never sells what it does not hold;
  - delegate allowances of 2 long tokens and 4 USDC, the hard cap on chain;
  - `max_fills_per_hour` 4;
  - `allowed_takers` holds only `3ZBj…sqLs`.
- **A real cap on contracts written (msg 53489).** The band limits long tokens held, not contracts
  written: after selling 2, inventory is 0 and the band would allow a third mint, paid for from
  premiums and cash. So the cycle enforces, per maker and across **every series and expiry**:
  - `max_written_contracts` 2;
  - `max_committed_collateral` 10.40 USDC.
  - **Source:** the chain alone, at `finalized`: every `Writer` account of this owner
    (`getProgramAccounts`, filtered by tag and owner; D5's first step checks that the RPC serves
    it). Open contracts are `written` while `claimed` is 0; committed is that times each series'
    `max_payout`.
  - **Release:** capacity comes back only when a `close` or `claim_writer` is finalized. A mint, a
    roll or a new expiry releases nothing.
  - **Fails closed:** a failed or partial read means no mint.
  - **Consequence:** with 2 contracts open on Nov 13, the Nov 20 cycle writes nothing until Nov
    13's claim is finalized. The maker may have nothing to sell for a few days; that is accepted,
    and the taker logs `no inventory` rather than failing.
- **Worst case:** settlement in the money pays out of the collateral, so the writer can lose all
  of what it locked, at most 10.40 USDC by the cap above. When its USDC cannot cover one contract,
  the cycle does not write, `collateral` fires, and the maker quotes only what it still holds.
- **Weekly cycle:** `scripts/demo-roll.sh` under launchd
  (`com.quotebook.demo-roll.<name>`, as varswap's `window-roll.sh`). Idempotent, and it acts only
  on what is missing. It holds a lock file, so only one cycle runs. Minting spends the USDC that
  bid quotes reserve, and approving replaces an allowance, so steps 3 to 5 run **between a D1 halt
  and drain and a finalized confirmation**, a pause of about 70 s a week:
  1. reads metalbook's listed 1 oz XAG capped calls for the next expiry, picking the strike
     nearest spot;
  2. creates the matching quotebook series if there is none (admin `GZhm…tVnL`, `base_lot` 1,
     `quote_lot` 10,000, `fee_bps` 0; `B66M…` already has series 1);
  3. mints contracts with `metalbook-sdk`, up to the band, the funding and the writer cap;
  4. creates the token accounts and approves the delegate on them;
  5. after settlement, which a keeper runs by hand on Nov 13 and Nov 20, redeems and claims
     through metalbook's flow, and revokes the old approvals. The USDC stays locked until then,
     so a cycle writes only from what is unlocked.
  - **The admin key in a scheduled job is devnet only.** `GZhm…tVnL` signs only `create_series`,
    logged to `roll.log`. Its plist is dedicated to this job and holds the key's file path, no key
    material (msg 53465). **This is not allowed on mainnet:** a scheduled job never holds a mainnet
    admin key. `docs/devnet.md` and `docs/runbooks.md` carry the same line.
- **Tests for the cap:** sell then roll (2 sold, inventory 0, cash available, third mint refused);
  two expiries coexisting (Nov 13 open blocks Nov 20's write; its finalized claim allows it; a claim
  not yet finalized does not); a failed `Writer` read refuses; a mint with a live quote waits for
  the drain.
- **Taker:** `quotebook-demo-taker` (in `demo/`), reusing the I9 taker `3ZBj…sqLs` (approved, msg
  53465), which already holds 8.77 USDC, so no new funding is needed. Its plist holds the keypair's
  file path only. One run does this:
  1. reads the quotable series;
  2. requests a quote from the demo maker for 1 lot;
  3. checks it as `quotebook-taker` does;
  4. settles;
  5. reads both sides' balances;
  6. appends one JSON line (time, series, side, price, size, fee, signature, slot, CU,
     balances before and after) to `trades.jsonl`.
  - **Sides alternate** (buy, then sell back) so inventory stays near flat, inside D1's band.
    Each round trip costs the taker the spread, about 0.06 USDC at I9's 2.5% half-spread on a
    1.20 mid, so its balance lasts about 140 round trips (10 weeks at two a day). The watcher's
    `inventory` floor reports when it runs low.
- **launchd:** `com.quotebook.demo-taker.<name>`, `StartInterval` 21,600 (every 6 h), skipped
  outside market hours and inside the settlement window.
- **Recording:**
  - the first trade is recorded in `docs/devnet.md`, in a new section with accounts,
    signatures and checks, like I9's;
  - after that, a weekly docs PR summarizes `trades.jsonl` (count, volume, failures and their
    reasons, CU) and does not list every signature.
- **After expiry:** redeeming the taker's long tokens goes through metalbook's flow and is
  recorded in metalbook's devnet log, as in I9.

### D6. Jupiter RFQ adapter: design and prototype (M)

- **Design doc:** `docs/design/jupiter-rfq.md`.
  - What Jupiter's V1 and V2 specs require, cited by URL and retrieval date.
  - The settlement gap: the Order Engine is Jupiter's, and the maker co-signs a transaction it
    did not build. quotebook's signed quote and `settle` play no part.
  - What can be reused:
    - pricing (`maker::pricing`, D2's marks);
    - the risk limits (D1);
    - the token list.
  - What is new:
    - a hot wallet with ATAs instead of the delegate model;
    - the fill-transaction validator;
    - the 95% fill-rate obligation.
  - Where the docs disagree: the deadlines (250 or 500 ms; 25 or 40 s), the fee bytes (3 or
    5) and the fill-rate window.
  - Whether a quotebook-settled path could exist: only as a Jupiter routing venue, a separate
    integration that is out of scope.
- **Prototype:** a crate `jupiter-rfq/` (`publish = false`), off-chain only.
  - **Server:** a V1 webhook (`POST /quote`, `POST /swap`, `GET /tokens`) built on axum, with
    request and response types written from the published OpenAPI schema. Its JSON fixtures
    are copied into the repo with their source and licence (MIT).
  - **`/quote`:** prices with the maker's pricing, applies D1's limits, answers within a
    configured deadline (default 200 ms), and returns 404 to decline.
  - **`/swap`:** decodes the base64 versioned transaction and applies Jupiter's published
    checks:
    - one Order Engine `fill`;
    - the expected mints, amounts, taker and maker;
    - `expire_at` in the future;
    - only allowlisted programs.
  - **Never signs or sends.** The prototype answers `rejected` with the reason, or, in tests
    only, `accepted` with the transaction it would have signed. There is no mainnet RPC, no
    Jupiter endpoint and no registration.
  - **Tests:**
    - the schema fixtures;
    - a valid fill built locally against the Order Engine's published account layout;
    - forged variants (extra program, second fill, wrong mint or amount, expired, maker not
      signer), each rejected with its reason;
    - deadline handling.
  - **Jupiter's acceptance suite** (approved, msg 51662): the toolkit's offline Vitest suite
    runs against `localhost` under these conditions:
    - an isolated directory outside the repo, at a pinned commit SHA recorded in the PR;
    - `npm ci --ignore-scripts`;
    - `env -i` with only `PATH` and `WEBHOOK_URL`, so no credentials are in the environment;
    - only the offline suite: the integration tests go through Jupiter's Edge and perform real
      mainnet swaps, and they are never run.
- **New dependencies:** `axum` 0.8 (0.8.9) and its `tower`/`hyper` stack; `base64`. They are
  checked and cited in the PR, and only `jupiter-rfq/` uses them.

### D7. Dashboard section (S)

A small quotebook section in the shared dashboard (msg 53465), the last milestone that builds
anything. The format is `section.json` version 1, from varswap's own `docs/PLAN-phase2.md`, P3a,
and its `docs/dashboard.md` and schema once they merge.

- **Generator:** `scripts/dashboard.sh <out-dir>` writes `<out-dir>/quotebook/section.json`. It
  reads public chain data only: the demo maker's signatures and its `settle` transactions, over
  devnet's public RPC. It holds no key, reads no local file and makes no Hermes request.
- **Content:**
  - tiles: fills, volume in USDC and the last fill's time;
  - one series: cumulative fills;
  - one table of the latest fills, at most 100 rows (time, series, side, price, size, signature),
    with `href_columns` to the devnet explorer;
  - `status` is `warn` when no fill has landed in 24 h of open market, `stale` after 48 h.
- **Nothing private:** only public addresses, amounts and signatures of landed transactions. The
  RPC URL is not written to `source` or to logs.
- **Publication:** as in varswap's P4: `.github/workflows/dashboard.yml`, daily at 23:00Z and on
  `workflow_dispatch`, on hosted runners with no secrets, `pages: write`, `id-token: write`,
  `contents: read`, the Pages actions pinned by SHA, and no new required check. Pages is not enabled
  on this repo today (API 404 on 2026-11-05), so this milestone turns it on (approved, msg 55249).
- **URL:** `https://joaquinbejar.github.io/quotebook/quotebook/section.json`. It is confirmed to
  varswap (`joaquin/s-094edf5fc368`) only after the first deploy, because the shell marks a section
  stale after 48 h and an unconfirmed entry stays pending.
- **Tests:** the output validates against the schema in CI, with the `serde_json` already in the
  workspace; hrefs follow varswap's allowlist; fixture transactions give the expected tiles.
- **Dependency:** the schema file has to be merged in varswap first; until then D7 builds against
  the plan's example.

### D8. Phase report (S)

- `docs/devnet.md`: the demo maker's accounts, its parameters (vol, rate, limits), the
  trades so far, the alerts seen, the CU per fill and how the feeds behaved over the first weekend.
- `docs/runbooks.md` updated for the demo agents.
- The final report to SOLANA-PROJECTS, with the open items.

## Order and effort

| # | Milestone | Effort | Depends on |
| --- | --- | --- | --- |
| D1 | Multi-series maker service, risk book, halt and drain | M | |
| D2 | metalbook pricing with optfix 0.3 strips | M | |
| D3 | Hosted demo maker (launchd, own payer and key) | M | D1, D2 |
| D4 | Watcher and alerts | S | D3 |
| D5 | Writer cycle and recurring trade with metalbook | M | D3, D4, metalbook's USDC transfer |
| D6 | Jupiter RFQ design and prototype | M | D1 (limits), D2 (pricing) |
| D7 | Dashboard section | S | D5, varswap's schema |
| D8 | Phase report | S | D6, D7 |

D1, D2 and D6's design doc start in parallel. D6's prototype follows D1. Only D5 needs
metalbook: the Nov 13 series are live, and it signs the USDC transfer when D5 starts. To trade
before Nov 13, D5 has to land by Nov 12; otherwise it starts on Nov 20's series, listed about
Nov 15. Nothing depends on that date.

## Out of scope

- Any change to `program/` or `quotebook-core/` (audit freeze). Issue #39 waits for the audit.
- Release 0.2.0 (I11): it still needs its own GO. The demo builds from the workspace, so it is
  not blocked.
- Mainnet, Jupiter registration or any call to a Jupiter endpoint, npm or crates.io
  publishing.
- A volatility surface or an implied-vol feed.
- XAU and put trading until there is funding for them (D5), and TLS on the maker service (D3).

## Decisions (SOLANA-PROJECTS, msg 51662)

1. **Series and funding.** metalbook lists 1 oz XAG and XAU demo series and rolls them weekly,
   after Nov 6. The demo maker is the writer. Funding starts with what `7tVP…cACQ` can spare
   (about 14 USDC), with limits to match; more on request. Consequence: XAG capped calls only
   (D5).
2. **Endpoint.** Localhost is enough, with no tunnel. The profile can carry a documented
   localhost endpoint. Consequence: a listing that cannot be connected to, because the program
   requires `wss://` and the service speaks `ws://` (D3).
3. **Hermes.** The public endpoint, at most one request a minute, with backoff; the shared key's
   quota is not touched. **Replaced:** that endpoint answers 401 without a key (see "Spot source").
4. **Jupiter's suite.** Allowed in an isolated directory, at a pinned version, with `npm ci
   --ignore-scripts` where possible and no credentials in the environment (D6).

## Decisions (SOLANA-PROJECTS, msgs 53465, 53489 and 55249)

1. **Spot.** Push feeds over RPC, maximum age 600 s, conditional on the 24-hour sample; the sample
   is only the entry gate; at run time the maker fails closed, with an alert. Devnet, own taker
   only.
2. **Writer cap.** A real cap on contracts written (D5).
3. **Taker.** Reusing `3ZBj…sqLs` is approved.
4. **Admin key in a scheduled job.** `GZhm…tVnL` is approved on devnet only, in a dedicated plist
   with the file path and no key material, with an explicit line that this is not allowed on
   mainnet (D5).
5. **Dashboard.** A small section with trades and fills, as the last milestone (D7), enabling
   GitHub Pages with no secrets.
6. **`stale-spot` during closures.** One `expected` line per closure, a normal alert from 600 s
   after reopening, and only the listed windows count (D4). Weekend behaviour is noted as
   unmeasured (D2).

## Spot source

Decided: the devnet push-feed accounts over RPC (D2). The pass condition was no gap above 600 s
on either feed during open market hours. **It passed:** the largest was 316 s on both, and the only
longer gap is the daily break (the numbers are in "Facts", with the sample's limits). The fallback,
a separate Hermes key that the coordinator would ask the user for, was not needed. If the feeds
stop later, the maker stops quoting and alerts; returning to this question is then the coordinator's
decision.
