//! The multi-series service and its risk book, with a fake chain: routing, every limit, halt,
//! quote against fill, stale and rolled-back reads, restarts and the status answer.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    missing_docs
)]

use std::collections::HashSet;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use quotebook_core::{BaseLots, Nonce, NonceWindow, Quote, Seconds, Side, UnixTimestamp};
use quotebook_maker::keys::MakerKey;
use quotebook_maker::market::MarketInfo;
use quotebook_maker::pricing::{ModelUnits, PricingParams};
use quotebook_maker::protocol::{
    from_hex, ClientMessage, QuoteRequest, RejectReason, ServerMessage, StatusMessage,
    StatusRequest, TakerSide,
};
use quotebook_maker::risk::{
    settle_count, ChainError, ChainSnapshot, ChainView, MarketLimits, RiskConfig, TokenAccountState,
};
use quotebook_maker::server::{
    ConfigError, FixedInventory, FixedMid, MakerService, MarketEntry, ServiceConfig, Start,
    TimeSource,
};
use rust_decimal::Decimal;
use solana_address::Address;
use solana_keypair::Keypair;

const NOW: i64 = 1_700_000_000;
const TTL: i64 = 30;
const PLENTY: u64 = 1_000_000_000_000;

fn addr(byte: u8) -> Address {
    Address::new_from_array([byte; 32])
}

const SERIES_A: u8 = 9;
const SERIES_B: u8 = 10;
const BASE_A: u8 = 21;
const BASE_B: u8 = 23;
const USDC: u8 = 22;
const TAKER: u8 = 40;

#[derive(Clone)]
struct Clock(Arc<AtomicI64>);

impl Clock {
    fn new() -> Self {
        Self(Arc::new(AtomicI64::new(NOW)))
    }
    fn set(&self, t: i64) {
        self.0.store(t, Ordering::SeqCst);
    }
}

impl TimeSource for Clock {
    fn now(&self) -> UnixTimestamp {
        UnixTimestamp::new(self.0.load(Ordering::SeqCst))
    }
}

fn config(series: u8, base: u8, max_quote_ttl: i64) -> ServiceConfig {
    let d = |v: &str| Decimal::from_str(v).unwrap();
    ServiceConfig {
        market: MarketInfo {
            program_id: addr(0x51),
            deployment: [7; 16],
            series: addr(series),
            base_decimals: 6,
            quote_decimals: 6,
            base_lot_size: 10_000,
            quote_lot_size: 100,
            max_quote_ttl: Seconds::new(max_quote_ttl),
            min_size: BaseLots::new(1),
        },
        pricing: PricingParams {
            risk_aversion: d("0.1"),
            volatility: d("0.6"),
            time_to_terminal_ms: 3_600_000,
            order_intensity: d("1.5"),
            max_deviation: d("0.05"),
            min_half_spread_bps: 5,
            max_half_spread_bps: 100,
            max_skew_bps: 100,
            model_units: ModelUnits::Relative,
        },
        ttl: Seconds::new(TTL),
        max_size: BaseLots::new(1_000),
        maker_base: addr(base),
        maker_quote: addr(USDC),
    }
}

fn wide_limits() -> MarketLimits {
    MarketLimits {
        max_notional: PLENTY,
        max_long: u64::MAX,
        max_short: u64::MAX,
    }
}

fn entry(series: u8, base: u8, limits: MarketLimits, max_quote_ttl: i64) -> MarketEntry {
    MarketEntry {
        config: config(series, base, max_quote_ttl),
        limits,
        feed: Box::new(FixedMid(Decimal::from_str("1.2").unwrap())),
        inventory: Box::new(FixedInventory(Decimal::ZERO)),
    }
}

/// A halt file path that does not exist, unique to `name`.
fn no_halt(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qb-risk-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("halt")
}

fn risk(name: &str) -> RiskConfig {
    RiskConfig {
        max_open_quotes: 1_000,
        max_fills_per_hour: 1_000,
        max_snapshot_age: Seconds::new(60),
        allowed_takers: vec![addr(TAKER)],
        halt_file: no_halt(name),
    }
}

/// A fake chain: what the next reads return.
struct Fake {
    snapshot: Mutex<Result<ChainSnapshot, ChainError>>,
    history: Mutex<Result<Vec<Option<UnixTimestamp>>, ChainError>>,
}

fn snapshot(slot: u64, clock: i64, window: NonceWindow) -> ChainSnapshot {
    let plenty = TokenAccountState {
        amount: PLENTY,
        delegated_amount: PLENTY,
    };
    ChainSnapshot {
        slot,
        clock: UnixTimestamp::new(clock),
        window,
        accounts: vec![
            (addr(BASE_A), plenty),
            (addr(BASE_B), plenty),
            (addr(USDC), plenty),
        ],
    }
}

impl Fake {
    fn new(s: ChainSnapshot) -> Self {
        Self {
            snapshot: Mutex::new(Ok(s)),
            history: Mutex::new(Ok(Vec::new())),
        }
    }
    fn set(&self, s: ChainSnapshot) {
        *self.snapshot.lock().unwrap() = Ok(s);
    }
    fn fail(&self) {
        *self.snapshot.lock().unwrap() = Err(ChainError("rpc down".into()));
    }
}

impl ChainView for Fake {
    fn snapshot(&self) -> Result<ChainSnapshot, ChainError> {
        self.snapshot.lock().unwrap().clone()
    }
    fn fill_times_since(
        &self,
        _since: UnixTimestamp,
    ) -> Result<Vec<Option<UnixTimestamp>>, ChainError> {
        self.history.lock().unwrap().clone()
    }
}

fn service(entries: Vec<MarketEntry>, cfg: RiskConfig, clock: &Clock) -> MakerService {
    MakerService::multi(
        MakerKey::new(Keypair::new()),
        entries,
        Start::Fresh(Nonce::new(0)),
        cfg,
        Box::new(clock.clone()),
    )
    .unwrap()
}

fn two_markets() -> Vec<MarketEntry> {
    vec![
        entry(SERIES_A, BASE_A, wide_limits(), 60),
        entry(SERIES_B, BASE_B, wide_limits(), 60),
    ]
}

/// A service with two markets and a first snapshot applied.
fn ready(name: &str, clock: &Clock) -> (MakerService, Fake) {
    let s = service(two_markets(), risk(name), clock);
    let fake = Fake::new(snapshot(100, NOW, NonceWindow::new()));
    s.reconcile(&fake).unwrap();
    (s, fake)
}

fn request(series: u8, side: TakerSide, lots: u64) -> QuoteRequest {
    QuoteRequest {
        request_id: 1,
        series: addr(series).to_string(),
        taker: addr(TAKER).to_string(),
        taker_side: side,
        size_lots: lots,
    }
}

fn quoted(answer: ServerMessage) -> Quote {
    match answer {
        ServerMessage::Quote(m) => Quote::decode(&from_hex(&m.quote).unwrap()).unwrap(),
        other => panic!("not a quote: {other:?}"),
    }
}

fn reason(answer: ServerMessage) -> RejectReason {
    match answer {
        ServerMessage::Reject(r) => r.reason,
        other => panic!("not a refusal: {other:?}"),
    }
}

fn status(s: &MakerService) -> StatusMessage {
    match s.status(5) {
        ServerMessage::Status(m) => m,
        other => panic!("not a status: {other:?}"),
    }
}

fn used(nonces: &[u64]) -> NonceWindow {
    let mut w = NonceWindow::new();
    for n in nonces {
        w.consume(Nonce::new(*n)).unwrap();
    }
    w
}

// Routing.

#[test]
fn test_two_series_share_one_nonce_space() {
    let clock = Clock::new();
    let (s, _fake) = ready("routing", &clock);
    let a = quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    let b = quoted(s.respond(&request(SERIES_B, TakerSide::Buy, 1)));
    assert_eq!(a.terms().series.address(), addr(SERIES_A));
    assert_eq!(b.terms().series.address(), addr(SERIES_B));
    assert_eq!(a.terms().nonce, Nonce::new(0));
    assert_eq!(b.terms().nonce, Nonce::new(1));
}

#[test]
fn test_each_market_sends_its_own_base_account() {
    let clock = Clock::new();
    let (s, _fake) = ready("accounts", &clock);
    let ServerMessage::Quote(m) = s.respond(&request(SERIES_B, TakerSide::Buy, 1)) else {
        panic!("refused");
    };
    assert_eq!(m.maker_base, addr(BASE_B).to_string());
    assert_eq!(m.maker_quote, addr(USDC).to_string());
}

#[test]
fn test_unknown_series_is_refused() {
    let clock = Clock::new();
    let (s, _fake) = ready("unknown", &clock);
    assert_eq!(
        reason(s.respond(&request(77, TakerSide::Buy, 1))),
        RejectReason::UnknownSeries
    );
}

#[test]
fn test_bad_market_sets_are_refused() {
    let clock = Clock::new();
    let build = |entries| {
        MakerService::multi(
            MakerKey::new(Keypair::new()),
            entries,
            Start::Fresh(Nonce::new(0)),
            risk("bad-sets"),
            Box::new(clock.clone()),
        )
        .map(|_| ())
    };
    assert_eq!(build(vec![]), Err(ConfigError::NoMarkets));
    assert_eq!(
        build(vec![
            entry(SERIES_A, BASE_A, wide_limits(), 60),
            entry(SERIES_A, BASE_B, wide_limits(), 60),
        ]),
        Err(ConfigError::DuplicateSeries)
    );
    let mut other = entry(SERIES_B, BASE_B, wide_limits(), 60);
    other.config.market.deployment = [8; 16];
    assert_eq!(
        build(vec![entry(SERIES_A, BASE_A, wide_limits(), 60), other]),
        Err(ConfigError::MixedDeployments)
    );
}

// Limits.

#[test]
fn test_no_quote_before_the_first_snapshot() {
    let clock = Clock::new();
    let s = service(two_markets(), risk("no-snapshot"), &clock);
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskUnavailable
    );
}

#[test]
fn test_only_allowed_takers_get_quotes() {
    let clock = Clock::new();
    let (s, _fake) = ready("takers", &clock);
    let mut r = request(SERIES_A, TakerSide::Buy, 1);
    r.taker = addr(41).to_string();
    assert_eq!(reason(s.respond(&r)), RejectReason::TakerNotAllowed);

    let mut none = risk("takers-none");
    none.allowed_takers.clear();
    let s = service(two_markets(), none, &clock);
    s.reconcile(&Fake::new(snapshot(1, NOW, NonceWindow::new())))
        .unwrap();
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::TakerNotAllowed
    );
}

#[test]
fn test_max_open_quotes_spans_markets() {
    let clock = Clock::new();
    let mut cfg = risk("open");
    cfg.max_open_quotes = 2;
    let s = service(two_markets(), cfg, &clock);
    s.reconcile(&Fake::new(snapshot(1, NOW, NonceWindow::new())))
        .unwrap();
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    quoted(s.respond(&request(SERIES_B, TakerSide::Buy, 1)));
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskLimit
    );
}

#[test]
fn test_max_notional_is_per_market_and_holds_open_quotes() {
    let clock = Clock::new();
    let (first_ask, ask_notional) = {
        let (s, _fake) = ready("notional-price", &clock);
        let q = quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
        (q.terms().price.get(), q.terms().price.get() * 100)
    };
    assert!(first_ask > 0);
    let limited = MarketLimits {
        max_notional: ask_notional,
        ..wide_limits()
    };
    let s = service(
        vec![
            entry(SERIES_A, BASE_A, limited, 60),
            entry(SERIES_B, BASE_B, wide_limits(), 60),
        ],
        risk("notional"),
        &clock,
    );
    s.reconcile(&Fake::new(snapshot(1, NOW, NonceWindow::new())))
        .unwrap();
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskLimit
    );
    quoted(s.respond(&request(SERIES_B, TakerSide::Buy, 1)));
}

#[test]
fn test_inventory_band_projects_open_quotes() {
    let clock = Clock::new();
    let band = MarketLimits {
        max_notional: PLENTY,
        max_long: 2,
        max_short: 0,
    };
    let s = service(
        vec![entry(SERIES_A, BASE_A, band, 60)],
        risk("band"),
        &clock,
    );
    // Holds 1 lot of base, with plenty of USDC.
    let mut snap = snapshot(1, NOW, NonceWindow::new());
    snap.accounts = vec![
        (
            addr(BASE_A),
            TokenAccountState {
                amount: 10_000,
                delegated_amount: PLENTY,
            },
        ),
        (
            addr(USDC),
            TokenAccountState {
                amount: PLENTY,
                delegated_amount: PLENTY,
            },
        ),
    ];
    s.reconcile(&Fake::new(snap)).unwrap();
    // One bid brings it to 2 if filled; a second would pass max_long.
    quoted(s.respond(&request(SERIES_A, TakerSide::Sell, 1)));
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Sell, 1))),
        RejectReason::RiskLimit
    );
    // One ask brings it to 0 if filled; a second would pass max_short.
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskLimit
    );
}

#[test]
fn test_shared_usdc_reserves_across_markets() {
    let clock = Clock::new();
    let probe = {
        let (s, _fake) = ready("usdc-price", &clock);
        quoted(s.respond(&request(SERIES_A, TakerSide::Sell, 1)))
            .terms()
            .price
            .get()
            * 100
    };
    let s = service(two_markets(), risk("usdc"), &clock);
    let mut snap = snapshot(1, NOW, NonceWindow::new());
    snap.accounts[2].1 = TokenAccountState {
        amount: probe * 2,
        delegated_amount: PLENTY,
    };
    s.reconcile(&Fake::new(snap)).unwrap();
    quoted(s.respond(&request(SERIES_A, TakerSide::Sell, 1)));
    quoted(s.respond(&request(SERIES_B, TakerSide::Sell, 1)));
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Sell, 1))),
        RejectReason::RiskLimit
    );
    // Long tokens are a separate account, so asks are still quoted.
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
}

#[test]
fn test_allowance_below_balance_is_the_limit() {
    let clock = Clock::new();
    let s = service(two_markets(), risk("allowance"), &clock);
    let mut snap = snapshot(1, NOW, NonceWindow::new());
    snap.accounts[0].1 = TokenAccountState {
        amount: PLENTY,
        delegated_amount: 10_000,
    };
    s.reconcile(&Fake::new(snap)).unwrap();
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskLimit
    );
}

#[test]
fn test_absent_account_has_nothing_available() {
    let clock = Clock::new();
    let s = service(two_markets(), risk("absent"), &clock);
    let mut snap = snapshot(1, NOW, NonceWindow::new());
    snap.accounts.clear();
    s.reconcile(&Fake::new(snap)).unwrap();
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskLimit
    );
}

#[test]
fn test_concurrent_requests_sign_exactly_what_fits() {
    let clock = Clock::new();
    let probe = {
        let (s, _fake) = ready("race-price", &clock);
        quoted(s.respond(&request(SERIES_A, TakerSide::Sell, 1)))
            .terms()
            .price
            .get()
            * 100
    };
    let s = Arc::new(service(two_markets(), risk("race"), &clock));
    let mut snap = snapshot(1, NOW, NonceWindow::new());
    snap.accounts[2].1 = TokenAccountState {
        amount: probe * 7,
        delegated_amount: PLENTY,
    };
    s.reconcile(&Fake::new(snap)).unwrap();
    let handles: Vec<_> = (0u8..32)
        .map(|i| {
            let s = Arc::clone(&s);
            std::thread::spawn(move || {
                let series = if i % 2 == 0 { SERIES_A } else { SERIES_B };
                s.respond(&request(series, TakerSide::Sell, 1))
            })
        })
        .collect();
    let answers: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let nonces: HashSet<u64> = answers
        .iter()
        .filter_map(|a| match a {
            ServerMessage::Quote(m) => {
                let q = Quote::decode(&from_hex(&m.quote).unwrap()).unwrap();
                Some(q.terms().nonce.get())
            }
            ServerMessage::Reject(r) => {
                assert_eq!(r.reason, RejectReason::RiskLimit);
                None
            }
            ServerMessage::Status(_) => panic!("status"),
        })
        .collect();
    assert_eq!(nonces.len(), 7);
    assert_eq!(status(&s).open_quotes, 7);
}

// Halt.

#[test]
fn test_halt_file_refuses_new_quotes_and_status_reports_it() {
    let clock = Clock::new();
    let (s, _fake) = ready("halt", &clock);
    let path = no_halt("halt");
    let held = quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    std::fs::write(&path, "").unwrap();
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::Halted
    );
    let st = status(&s);
    assert!(st.halted);
    // A halt does not revoke: the quote already signed is still open.
    assert_eq!(st.open_quotes, 1);
    assert_eq!(st.newest_expires_at, Some(held.terms().expires_at.get()));
    std::fs::remove_file(&path).unwrap();
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    assert!(!status(&s).halted);
}

#[test]
fn test_unreadable_halt_path_fails_closed() {
    let clock = Clock::new();
    let mut cfg = risk("halt-unreadable");
    // A path below a regular file cannot be read as absent: it fails with NotADirectory.
    let file = no_halt("halt-unreadable");
    std::fs::write(&file, "").unwrap();
    cfg.halt_file = file.join("below");
    let s = service(two_markets(), cfg, &clock);
    s.reconcile(&Fake::new(snapshot(1, NOW, NonceWindow::new())))
        .unwrap();
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::Halted
    );
    std::fs::remove_file(file).unwrap();
}

// A quote is not a fill.

#[test]
fn test_expiry_releases_without_counting_a_fill_and_a_fill_counts() {
    let clock = Clock::new();
    let mut cfg = risk("not-fill");
    cfg.max_fills_per_hour = 2;
    let s = service(two_markets(), cfg, &clock);
    let fake = Fake::new(snapshot(1, NOW, NonceWindow::new()));
    s.reconcile(&fake).unwrap();

    // 0 fills + 1 open reaches nothing yet; 2 open would reach the limit.
    let q0 = quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskLimit
    );

    // Nonce 0 filled, nonce 1 expired. One fill in the hour, nothing open.
    let expiry = q0.terms().expires_at.get();
    clock.set(expiry);
    fake.set(snapshot(2, expiry, used(&[0])));
    s.reconcile(&fake).unwrap();
    assert_eq!(status(&s).open_quotes, 0);

    // 1 fill + 0 open allows one more; then 1 + 1 reaches the limit.
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskLimit
    );
}

#[test]
fn test_fill_ages_out_of_the_hour() {
    let clock = Clock::new();
    let mut cfg = risk("hour");
    cfg.max_fills_per_hour = 1;
    let s = service(two_markets(), cfg, &clock);
    let fake = Fake::new(snapshot(1, NOW, NonceWindow::new()));
    s.reconcile(&fake).unwrap();
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    fake.set(snapshot(2, NOW + 1, used(&[0])));
    s.reconcile(&fake).unwrap();
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskLimit
    );
    clock.set(NOW + 1 + 3_600);
    fake.set(snapshot(3, NOW + 1 + 3_600, used(&[0])));
    s.reconcile(&fake).unwrap();
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
}

#[test]
fn test_wall_clock_expiry_releases_nothing() {
    let clock = Clock::new();
    let (s, fake) = ready("wall-clock", &clock);
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    // Wall clock well past the expiry; the finalized Clock is one second short of it.
    clock.set(NOW + TTL + 30);
    fake.set(snapshot(101, NOW + TTL - 1, NonceWindow::new()));
    s.reconcile(&fake).unwrap();
    assert_eq!(status(&s).open_quotes, 1);
    fake.set(snapshot(102, NOW + TTL, NonceWindow::new()));
    s.reconcile(&fake).unwrap();
    assert_eq!(status(&s).open_quotes, 0);
}

#[test]
fn test_cancel_all_releases_every_reservation_below_it() {
    let clock = Clock::new();
    let (s, fake) = ready("cancel", &clock);
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    quoted(s.respond(&request(SERIES_B, TakerSide::Buy, 1)));
    let next = status(&s).next_nonce;
    let mut window = NonceWindow::new();
    window.cancel_all(Nonce::new(next)).unwrap();
    fake.set(snapshot(101, NOW, window));
    s.reconcile(&fake).unwrap();
    assert_eq!(status(&s).open_quotes, 0);
}

// Lag and rollback.

#[test]
fn test_stale_snapshot_refuses_and_a_fresh_one_recovers() {
    let clock = Clock::new();
    let (s, fake) = ready("stale", &clock);
    clock.set(NOW + 61);
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskUnavailable
    );
    fake.set(snapshot(101, NOW + 61, NonceWindow::new()));
    s.reconcile(&fake).unwrap();
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
}

#[test]
fn test_failed_read_refuses_and_releases_nothing() {
    let clock = Clock::new();
    let (s, fake) = ready("rpc-error", &clock);
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    fake.fail();
    assert!(s.reconcile(&fake).is_err());
    assert_eq!(status(&s).open_quotes, 1);
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskUnavailable
    );
    fake.set(snapshot(101, NOW + 1, NonceWindow::new()));
    s.reconcile(&fake).unwrap();
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
}

#[test]
fn test_slot_going_backwards_refuses_and_releases_nothing() {
    let clock = Clock::new();
    let (s, fake) = ready("rollback", &clock);
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    // Slot 100 was the last accepted. This one claims the fill, from an older slot.
    fake.set(snapshot(99, NOW + TTL, used(&[0])));
    s.reconcile(&fake).unwrap();
    assert_eq!(status(&s).open_quotes, 1);
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskUnavailable
    );
    // Back at or above the last slot, the fill never happened: the quote stays held.
    fake.set(snapshot(100, NOW + 1, NonceWindow::new()));
    s.reconcile(&fake).unwrap();
    assert_eq!(status(&s).open_quotes, 1);
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
}

// Restarts.

fn resumed(cfg: RiskConfig, clock: &Clock, entries: Vec<MarketEntry>) -> MakerService {
    MakerService::multi(
        MakerKey::new(Keypair::new()),
        entries,
        Start::Resume {
            window: Box::new(NonceWindow::new()),
            started_at: UnixTimestamp::new(NOW),
        },
        cfg,
        Box::new(clock.clone()),
    )
    .unwrap()
}

#[test]
fn test_restart_waits_for_the_chain_clock_past_the_longest_cold_start() {
    let clock = Clock::new();
    // The longest lifetime across the set is 120 s, so the cold start ends at NOW + 130.
    let s = resumed(
        risk("restart"),
        &clock,
        vec![
            entry(SERIES_A, BASE_A, wide_limits(), 60),
            entry(SERIES_B, BASE_B, wide_limits(), 120),
        ],
    );
    let end = NOW + 130;
    let fake = Fake::new(snapshot(1, NOW, NonceWindow::new()));
    s.reconcile(&fake).unwrap();
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RateLimited
    );
    // The wall clock has passed it; a snapshot's Clock has not.
    clock.set(end + 5);
    fake.set(snapshot(2, end - 1, NonceWindow::new()));
    s.reconcile(&fake).unwrap();
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RateLimited
    );
    // The first free nonce comes from the window read then, above a fill during the wait.
    fake.set(snapshot(3, end, used(&[0, 1, 2])));
    s.reconcile(&fake).unwrap();
    let q = quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    assert_eq!(q.terms().nonce, Nonce::new(3));
}

#[test]
fn test_fills_survive_a_restart() {
    let clock = Clock::new();
    let mut cfg = risk("fill-restart");
    cfg.max_fills_per_hour = 4;
    let s = resumed(
        cfg,
        &clock,
        vec![entry(SERIES_A, BASE_A, wide_limits(), 60)],
    );
    let end = NOW + 70;
    // 4 fills before the restart: three with times, one with no blockTime, inside the hour.
    let fake = Fake::new(snapshot(1, end, NonceWindow::new()));
    *fake.history.lock().unwrap() = Ok(vec![
        Some(UnixTimestamp::new(NOW - 30)),
        Some(UnixTimestamp::new(NOW - 20)),
        Some(UnixTimestamp::new(NOW - 10)),
        None,
    ]);
    clock.set(end);
    s.reconcile(&fake).unwrap();
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskLimit
    );
    // The three dated fills age out; the undated one counts from the snapshot's Clock.
    clock.set(NOW - 10 + 3_600);
    fake.set(snapshot(2, NOW - 10 + 3_600, NonceWindow::new()));
    s.reconcile(&fake).unwrap();
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
}

#[test]
fn test_unreadable_history_refuses_for_an_hour_of_the_chain_clock() {
    let clock = Clock::new();
    let s = resumed(
        risk("no-history"),
        &clock,
        vec![entry(SERIES_A, BASE_A, wide_limits(), 60)],
    );
    let end = NOW + 70;
    let fake = Fake::new(snapshot(1, end, NonceWindow::new()));
    *fake.history.lock().unwrap() = Err(ChainError("page limit".into()));
    clock.set(end);
    s.reconcile(&fake).unwrap();
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskLimit
    );
    // Wall clock past the hour, Clock one second short: still refused.
    clock.set(NOW + 3_600 + 30);
    fake.set(snapshot(2, NOW + 3_599, NonceWindow::new()));
    s.reconcile(&fake).unwrap();
    assert_eq!(
        reason(s.respond(&request(SERIES_A, TakerSide::Buy, 1))),
        RejectReason::RiskLimit
    );
    fake.set(snapshot(3, NOW + 3_600, NonceWindow::new()));
    s.reconcile(&fake).unwrap();
    quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
}

// Counting settles.

#[test]
fn test_settle_count_counts_every_settle_of_the_program() {
    let program = addr(0x51);
    let other = addr(0x52);
    let (settle, cancel) = ([6u8, 1, 2], [4u8, 1]);
    let empty: [u8; 0] = [];
    let ixs: Vec<(&Address, &[u8])> = vec![
        (&program, &settle),
        (&other, &settle),
        (&program, &cancel),
        (&program, &empty),
        (&program, &settle),
    ];
    assert_eq!(settle_count(&program, ixs), 2);
}

// Status.

#[test]
fn test_status_takes_no_nonce_and_holds_nothing() {
    let clock = Clock::new();
    let (s, _fake) = ready("status", &clock);
    let before = status(&s);
    assert_eq!(before.request_id, 5);
    assert_eq!(before.next_nonce, 0);
    assert_eq!(before.open_quotes, 0);
    assert_eq!(before.newest_expires_at, None);
    assert_eq!(before.snapshot_age_secs, Some(0));
    assert_eq!(before.spot_ages.len(), 2);
    assert_eq!(before.spot_ages[0].age_secs, None);
    assert_eq!(status(&s), before);
    let q = quoted(s.respond(&request(SERIES_A, TakerSide::Buy, 1)));
    assert_eq!(q.terms().nonce, Nonce::new(0));
    assert_eq!(q.terms().side, Side::Ask);
    assert_eq!(status(&s).next_nonce, 1);
}

#[tokio::test]
async fn test_status_over_websocket() {
    let clock = Clock::new();
    let (s, _fake) = ready("status-ws", &clock);
    let s = Arc::new(s);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(Arc::clone(&s).serve(listener));
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let ask = ClientMessage::Status(StatusRequest { request_id: 9 });
    ws.send(tokio_tungstenite::tungstenite::Message::text(
        serde_json::to_string(&ask).unwrap(),
    ))
    .await
    .unwrap();
    let frame = ws.next().await.unwrap().unwrap();
    let answer: ServerMessage = serde_json::from_str(frame.to_text().unwrap()).unwrap();
    let ServerMessage::Status(m) = answer else {
        panic!("not a status");
    };
    assert_eq!(m.request_id, 9);
    assert!(!m.halted);
    server.abort();
}
