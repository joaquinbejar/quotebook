//! The nonce-issuance bound of docs/PLAN.md section 4.2: a slide of the replay window never
//! evicts a quote that is still fillable, provided the maker stamps every nonce `M >= N +
//! 16,384` with `issued_at >= min_issued_at_beyond_window(expires_at(N))`.
//!
//! The first test is codex's counterexample to the earlier "16,384 quotes per TTL" bound,
//! which ignored `MAX_CLOCK_SKEW`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    clippy::panic_in_result_fn,
    clippy::disallowed_macros,
    missing_docs
)]

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use quotebook_core::{
    min_issued_at_beyond_window, Address, BaseLots, Nonce, NonceWindow, Quote, QuoteLotsPerBaseLot,
    QuoteTerms, QuotebookError, Seconds, SeriesId, Side, UnixTimestamp, MAX_CLOCK_SKEW,
    WINDOW_BITS,
};

const TTL: Seconds = Seconds::new(60);

fn quote(nonce: u64, issued_at: i64, expires_at: i64) -> Quote {
    Quote::new(QuoteTerms {
        program_id: Address::new_from_array([1; 32]),
        deployment: [2; 16],
        maker: Address::new_from_array([3; 32]),
        series: SeriesId::new(Address::new_from_array([4; 32])),
        taker: Quote::OPEN_TAKER,
        side: Side::Bid,
        price: QuoteLotsPerBaseLot::new(1),
        size: BaseLots::new(1),
        issued_at: UnixTimestamp::new(issued_at),
        expires_at: UnixTimestamp::new(expires_at),
        nonce: Nonce::new(nonce),
    })
    .unwrap()
}

/// Whether `q` passes the time and replay checks of `settle` at on-chain time `now`.
fn fillable(q: &Quote, window: &NonceWindow, now: i64, ttl: Seconds) -> bool {
    q.check_time(UnixTimestamp::new(now), ttl).is_ok() && window.check(q.terms().nonce).is_ok()
}

/// Codex's counterexample: quote 0 is accepted at `now = 0` with `issued_at = 10,
/// expires_at = 70`. A TTL-only horizon allows nonce 16,384 with `issued_at = 71` (61 s
/// later); filling it at `t = 61` evicts quote 0 while quote 0 is still fillable.
#[test]
fn test_eviction_bound_ttl_only_horizon_evicts_a_live_quote() {
    let q0 = quote(0, 10, 70);
    let qm = quote(WINDOW_BITS, 71, 131);
    let mut window = NonceWindow::new();
    assert_eq!(q0.check_time(UnixTimestamp::new(0), TTL), Ok(()));

    let t = 61;
    assert!(fillable(&q0, &window, t, TTL));
    assert!(fillable(&qm, &window, t, TTL));
    window.consume(qm.terms().nonce).unwrap();
    // Quote 0's time window is still open, but its nonce is gone.
    assert_eq!(q0.check_time(UnixTimestamp::new(t), TTL), Ok(()));
    assert_eq!(
        window.check(Nonce::new(0)),
        Err(QuotebookError::NonceOutOfWindow)
    );

    // The bound forbids that stamp: the floor for nonce 16,384 is 70 + 10 = 80.
    let floor = min_issued_at_beyond_window(q0.terms().expires_at).unwrap();
    assert_eq!(floor, UnixTimestamp::new(80));
    assert!(qm.terms().issued_at < floor);
}

/// The same scenario with `issued_at` at the floor: at no on-chain time are both quotes
/// fillable, so the slide that fills nonce 16,384 can only happen after quote 0 expired.
#[test]
fn test_eviction_bound_floor_never_overlaps() {
    let q0 = quote(0, 10, 70);
    let floor = min_issued_at_beyond_window(q0.terms().expires_at)
        .unwrap()
        .get();
    let qm = quote(WINDOW_BITS, floor, floor + 60);
    let window = NonceWindow::new();
    for t in -200..400 {
        assert!(
            !(fillable(&q0, &window, t, TTL) && fillable(&qm, &window, t, TTL)),
            "both fillable at t = {t}"
        );
    }
    // The earliest fill of nonce 16,384 is at 70, exactly when quote 0 expires.
    let first = (-200..400)
        .find(|&t| fillable(&qm, &window, t, TTL))
        .unwrap();
    assert_eq!(first, 70);
    assert_eq!(
        q0.check_time(UnixTimestamp::new(first), TTL),
        Err(QuotebookError::QuoteExpired)
    );
}

#[test]
fn test_eviction_bound_overflow_is_reported() {
    assert_eq!(
        min_issued_at_beyond_window(UnixTimestamp::new(i64::MAX - MAX_CLOCK_SKEW.get() + 1)),
        Err(QuotebookError::Overflow)
    );
}

fn config() -> Config {
    let cases = std::env::var("QUOTEBOOK_PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x0B0C_0003_5EED_0003),
        failure_persistence: None,
        ..Config::default()
    }
}

proptest! {
    #![proptest_config(config())]

    /// For any older quote `N` and any quote `M` stamped at or after the floor, there is no
    /// on-chain time at which both pass `check_time`: every fill of `M`, and so every slide it
    /// causes, comes after `N` stopped being fillable.
    #[test]
    fn test_eviction_bound_holds(
        issued_n in -1_000_000i64..1_000_000,
        life_n in 0i64..=3_600,
        ttl in 0i64..=3_600,
        delay in 0i64..1_000,
        life_m in 0i64..=3_600,
        t in -1_100_000i64..1_200_000,
    ) {
        let ttl = Seconds::new(ttl.max(life_n).max(life_m));
        let qn = quote(0, issued_n, issued_n + life_n);
        let issued_m = min_issued_at_beyond_window(qn.terms().expires_at).unwrap().get() + delay;
        let qm = quote(WINDOW_BITS, issued_m, issued_m + life_m);
        let now = UnixTimestamp::new(t);
        prop_assert!(!(qn.check_time(now, ttl).is_ok() && qm.check_time(now, ttl).is_ok()));
        // Probe the boundary directly too.
        let edge = UnixTimestamp::new(issued_m - MAX_CLOCK_SKEW.get());
        if qm.check_time(edge, ttl).is_ok() {
            prop_assert!(qn.check_time(edge, ttl).is_err());
        }
    }
}
