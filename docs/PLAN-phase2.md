# quotebook: phase 2 plan (devnet operations)

Status: **draft r3**, for Codex review. Brief from SOLANA-PROJECTS, msg 51619 (2026-11-03);
decisions from msg 51662 and metalbook's facts from msg 52647 are folded in (see "Decisions").
One item is open: the spot source ("Open question"), because the public Hermes endpoint refuses
keyless requests.
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

## Facts this plan relies on (checked 2026-11-03)

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
- `pyth-lite` 0.1.0 (crates.io, no dependencies) decodes and checks that account: owner,
  discriminator, length, feed id, staleness.

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

### D1. Multi-series maker service (M)

The demo quotes every listed metalbook series under one maker key, so the service has to
route several markets through one nonce issuer.

- **Routing:** `maker::server` serves a set of markets keyed by series address, all sharing one
  `Mutex<NonceIssuer>` and one `MakerKey`. A request for a series outside the set is refused
  with `UnknownSeries`.
- **Per market:**
  - each market has its own `PriceFeed`, `Inventory`, `PricingParams`, TTL and `max_size`;
  - the cold start uses the longest `max_quote_ttl` across the set.
- **Compatibility:** the single-series constructor stays as a one-market wrapper, so
  `quotebook-maker` and its tests do not change behaviour.
- **Risk limits** are new, enforced before a nonce is issued, each with a `RejectReason`:
  - `max_open_quotes` across all markets (live, unexpired, unsettled quotes);
  - per-market `max_notional` in quote atoms;
  - a per-market inventory band (`max_long`, `max_short` in base lots), read from
    `Inventory`;
  - a global `max_fills_per_hour`;
  - a **kill switch**: when a file (path in config) exists, everything is refused with
    `Halted`.
- **Hard cap on chain:** the delegate allowance on each maker account. The service is
  configured below it and the watcher (D4) alerts as it drains.
- **Tests:** unit tests for routing, every limit and the kill switch. A LiteSVM test runs two
  series under one key and checks that no nonce is issued twice.

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
- **Spot (proposed, see "Open question"):** read the two devnet push-feed accounts over RPC,
  once a minute, and check them with `pyth-lite` (owner, feed id, level Full). No Hermes and no
  key. The value is stale after 600 s, twice the observed gap, so a quote can be priced on a spot
  up to 10 minutes old. A 1% XAG move in that time is about 0.25 USDC on a 1 oz at-the-money
  capped call, against an I9 half-spread near 0.03 USDC. That is accepted for a demo at 1 lot per
  quote and 2 contracts of exposure, where the only taker is our own script. It would not be
  acceptable for outside takers.
- **Confidence guard:** no quote when `conf / price` exceeds 50 bps.
- **Vol:** one number per underlying from config (the I9 demo used 0.30 for XAG), recorded in
  `docs/devnet.md` with the date; carry 0, rate 4%. A vol source is out of scope (optfix has no
  surface).
- **Units:** quotebook ticks are quote atoms per lot. The mark is converted at the series'
  `quote_lot_size` and rounded toward the maker on each side (`maker::market`).
- **Tests:** against fixtures produced by `metalbook-maker::mark` on the same inputs; strip
  marks must equal them exactly, since the legs are bit-identical. Also: a stale feed, the
  closed market and the settlement window.
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
  - no secret in the environment: the spot source needs no key (D2, option A), and the keypairs are file paths;
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
- **Runbook:** `docs/runbooks.md` gets start, stop (by PID or `launchctl bootout`), key
  rotation, refilling, the kill switch and recovery from a crash loop.

### D4. Watcher and alerts (S)

`quotebook-demo-watch` (in `demo/`), mirroring `varswap-watch`. It holds no key and only reads
the chain and the maker's local endpoint.

- **Alert kinds:**
  - `balance` (payer SOL below `--min-balance`, default 0.3 SOL on devnet);
  - `inventory` (USDC or long tokens below a floor);
  - `collateral` (the writer's USDC below one contract's `max_payout` when the next cycle is
    due);
  - `allowance` (a delegate allowance below 20% of its approval);
  - `maker-down` (no answer to a probe request within 2 s; the probe asks for a quote and never
    settles it);
  - `stale-spot` (the maker rejects with stale feed for 5 minutes);
  - `no-series` (no quotable series while the market is open and the calendar says one should
    be listed). It reads the calendar so the known gaps (after each expiry, until the roller's
    next listing; Nov 20 to about Dec 1) stay silent;
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
- **Funding:** metalbook signs a 14.00 USDC transfer from `7tVP…cACQ` to the demo maker's USDC
  account when D5 starts; 10 to 15 more are available on request after Nov 13.
- **Limits sized to that funding:**
  - `max_size` 1 lot;
  - inventory band 0 to 2 long tokens, so the maker never sells what it does not hold;
  - delegate allowances of 2 long tokens and 4 USDC, the hard cap on chain;
  - `max_fills_per_hour` 4.
- **Worst case:** settlement in the money pays out of the collateral, so the writer can lose all
  of what it locked (at most 10.4 USDC at 2 contracts). When its USDC cannot cover one contract, the
  cycle does not write, `collateral` fires, and the maker quotes only what it still holds.
- **Weekly cycle:** `scripts/demo-roll.sh` under launchd
  (`com.quotebook.demo-roll.<name>`, as varswap's `window-roll.sh`). Idempotent, and it acts only
  on what is missing:
  1. reads metalbook's listed 1 oz XAG capped calls for the next expiry, picking the strike
     nearest spot;
  2. creates the matching quotebook series if there is none (admin `GZhm…tVnL`, `base_lot` 1,
     `quote_lot` 10,000, `fee_bps` 0; `B66M…` already has series 1);
  3. mints contracts with `metalbook-sdk`, up to the band and the funding;
  4. creates the token accounts and approves the delegate on them;
  5. after settlement, which a keeper runs by hand on Nov 13 and Nov 20, redeems and claims
     through metalbook's flow, and revokes the old approvals. The USDC stays locked until then,
     so a cycle writes only from what is unlocked.
  - The admin key signs here, so a cycle only ever calls `create_series`; it logs what it did
    to `roll.log`.
- **Taker:** `quotebook-demo-taker` (in `demo/`), reusing the I9 taker `3ZBj…sqLs`, which already
  holds 8.77 USDC, so no new funding is needed. One run does this:
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

### D7. Phase report (S)

- `docs/devnet.md`: the demo maker's accounts, its parameters (vol, rate, limits), the
  trades so far, the alerts seen and the CU per fill.
- `docs/runbooks.md` updated for the demo agents.
- The final report to SOLANA-PROJECTS, with the open items.

## Order and effort

| # | Milestone | Effort | Depends on |
| --- | --- | --- | --- |
| D1 | Multi-series maker service, risk limits, kill switch | M | |
| D2 | metalbook pricing with optfix 0.3 strips | M | |
| D3 | Hosted demo maker (launchd, own payer and key) | M | D1, D2 |
| D4 | Watcher and alerts | S | D3 |
| D5 | Writer cycle and recurring trade with metalbook | M | D3, D4, metalbook's USDC transfer |
| D6 | Jupiter RFQ design and prototype | M | D1 (limits), D2 (pricing) |
| D7 | Phase report | S | D5, D6 |

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
   quota is not touched. **It cannot be followed as written:** that endpoint answers 401 without
   a key (see "Open question").
4. **Jupiter's suite.** Allowed in an isolated directory, at a pinned version, with `npm ci
   --ignore-scripts` where possible and no credentials in the environment (D6).

## Open question (for SOLANA-PROJECTS)

The public Hermes endpoint answers 401 without a key, so decision 3 needs replacing. D2 is
written for option A; the plan changes only in D2's spot lines and its dependency.

- **A (proposed): the devnet push-feed accounts over RPC.** No key and no Hermes quota, and
  `pyth-lite` already handles the format. The cost is a staler spot, about 5
  minutes in the one gap measured and with no guarantee of cadence. The watcher's `stale-spot`
  alert reports it if the feeds stop.
- **B: a separate Hermes key for the demo.** A fresher spot, but it needs Joaquin to obtain a key
  and adds a secret to the launchd job.
