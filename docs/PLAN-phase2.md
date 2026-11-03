# quotebook: phase 2 plan (devnet operations)

Status: **draft r1**, for Codex review. Brief from SOLANA-PROJECTS, msg 51619 (2026-11-03).
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
- The Nov 6 expiry has 26 series at 50 oz, none written. The devnet log does not show Nov 13
  as listed yet.
- metalbook's keeper settles each series on a 30-minute Pyth average before expiry.
- `metalbook-maker::mark` prices a capped call as `C(K) − C(cap)` × multiplier, and refuses
  when the market is closed or the settlement window has begun. That crate is
  `publish = false` and still on optfix 0.2.
- `metalbook-core` and `metalbook-sdk` 0.1.0 are on crates.io. Their dependencies are bytemuck,
  `solana-address` 2.7 and `solana-instruction`, the generation quotebook already uses.
- Its devnet log lists "Trading through QuoteBook, once the maker has more USDC" as not done.

**Pricing inputs**
- **optfix 0.3.0** (crates.io, 2026-11-02) is `no_std` with Q64.64 fixed point.
  - `bsm::price_strip(expiry, &MarketInputs, &[StripLeg], &mut [Price])` prices many strikes
    of one expiry in one call. Each leg is bit-identical to a single `price`.
  - There is no vol surface: the caller passes one `Volatility`.
- **Spot:** metalbook and varswap read Pyth Hermes over `ureq` 3.5 with a shared key. metalbook
  is capped at 7 requests a minute.

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
- **Spot:** a Hermes poller on XAU/USD and XAG/USD, one request per 15 s for both feeds
  (4 a minute), cached, stale after 60 s.
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
  - `ureq` 3.5 (3.5.1), rustls only, as in metalbook. Only `demo/` uses them.

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
  - the Hermes key through `zsh -c 'source ~/.zshrc; exec …'`;
  - logs under `~/.local/share/quotebook-demo/<name>/`;
  - the binary copied out of the checkout, so a later `git pull` does not change what runs.
- **Restarts** pay the cold start (70 s at 60 s TTL). That is accepted and documented.
- **Endpoint:** listens on `127.0.0.1` by default. A public `wss://` endpoint and a registry
  profile (ADR 0002) need a tunnel or host decision (question 2). Without it, `set-profile`
  is not done.
- **Runbook:** `docs/runbooks.md` gets start, stop (by PID or `launchctl bootout`), key
  rotation, refilling, the kill switch and recovery from a crash loop.

### D4. Watcher and alerts (S)

`quotebook-demo-watch` (in `demo/`), mirroring `varswap-watch`. It holds no key and only reads
the chain and the maker's local endpoint.

- **Alert kinds:**
  - `balance` (payer SOL below `--min-balance`, default 0.3 SOL on devnet);
  - `inventory` (USDC or long tokens below a floor);
  - `allowance` (a delegate allowance below 20% of its approval);
  - `maker-down` (no answer to a probe request within 2 s; the probe asks for a quote and never
    settles it);
  - `stale-spot` (the maker rejects with stale feed for 5 minutes);
  - `no-series` (no quotable series for the next 24 h while the market is open, which means
    metalbook has not listed);
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

- **Coordination with metalbook:** settled with its window through SOLANA-PROJECTS before
  D5 starts, and written into the PR.
  - Which series: a listed expiry with an open market. Preferably a 1 oz series: a 50 oz XAG
    capped call locks 250 USDC per contract.
  - Who writes the contracts, and how the maker gets long tokens and USDC (question 1).
- **Taker:** `quotebook-demo-taker` (in `demo/`), its own key
  (`~/.config/solana/quotebook-demo/taker.json`). One run does this:
  1. reads the quotable series;
  2. requests a quote from the demo maker for 1 lot;
  3. checks it as `quotebook-taker` does;
  4. settles;
  5. reads both sides' balances;
  6. appends one JSON line (time, series, side, price, size, fee, signature, slot, CU,
     balances before and after) to `trades.jsonl`.
  - **Sides alternate** (buy, then sell back) so inventory stays near flat, inside D1's band.
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
  - **Optional:** running the toolkit's offline Vitest acceptance suite against `localhost`.
    It needs `npm install` of a third-party repo in an isolated directory, so it only runs if
    SOLANA-PROJECTS agrees (question 4).
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
| D5 | Recurring trade with metalbook | M | D3, D4, metalbook series and funding |
| D6 | Jupiter RFQ design and prototype | M | D1 (limits), D2 (pricing) |
| D7 | Phase report | S | D5, D6 |

D1, D2 and D6's design doc start in parallel. D6's prototype follows D1. Only D5 needs
metalbook, so its coordination request goes out when this plan is approved, not when D5
starts.

## Out of scope

- Any change to `program/` or `quotebook-core/` (audit freeze). Issue #39 waits for the audit.
- Release 0.2.0 (I11): it still needs its own GO. The demo builds from the workspace, so it is
  not blocked.
- Mainnet, Jupiter registration or any call to a Jupiter endpoint, npm or crates.io
  publishing.
- A volatility surface or an implied-vol feed.
- Writing metalbook contracts from quotebook code: the maker's long tokens come from the
  metalbook flow (question 1).

## Questions for SOLANA-PROJECTS

1. **Funding and series for D5.** A 50 oz XAG capped call at strike 50 locks 250 USDC per
   contract, and Circle's devnet faucet is slow. Should metalbook list a 1 oz expiry for the
   demo, or supply long tokens or USDC? Who writes the contracts the maker sells: the demo
   maker through `metalbook-admin mint`, or metalbook's maker?
2. **Public endpoint for D3.** Is localhost enough (the taker runs on the same host), or should
   the demo maker be reachable over `wss://` (a tunnel such as Tailscale Funnel or
   cloudflared), so it can be listed in the registry?
3. **Hermes for D2.** Should it share metalbook's and varswap's key (D2 adds 4 requests a
   minute), use a separate key, or use the public endpoint?
4. **D6 acceptance suite.** May the toolkit's offline Vitest suite be run locally against
   `localhost`, after an `npm install` in an isolated directory?
