//! I12 (docs/PLAN-improvements.md): risk parameters relative to the mid. The pipeline is the
//! model, then clamps in bps of the mid, then rounding toward the maker, then the guard on the
//! rounded prices; refusals are typed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    missing_docs
)]

use std::str::FromStr;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use quotebook_core::{BaseLots, Seconds};
use quotebook_maker::market::{MarketInfo, Rounding, TickError};
use quotebook_maker::pricing::{self, clamped_prices, PricingError, PricingParams, Quoted};
use rust_decimal::Decimal;
use solana_address::Address;

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// A market whose tick is `quote_lot / 10^quote_decimals` quote tokens per base token:
/// `base_lot_size` = one whole base token.
fn market(base_decimals: u8, quote_decimals: u8, quote_lot_size: u64) -> MarketInfo {
    MarketInfo {
        program_id: Address::new_from_array([1; 32]),
        deployment: [2; 16],
        series: Address::new_from_array([3; 32]),
        base_decimals,
        quote_decimals,
        base_lot_size: 10u64.pow(u32::from(base_decimals)),
        quote_lot_size,
        max_quote_ttl: Seconds::new(60),
        min_size: BaseLots::new(1),
    }
}

fn tick(m: &MarketInfo) -> Decimal {
    m.ticks_to_price(quotebook_core::QuoteLotsPerBaseLot::new(1))
        .unwrap()
}

/// The ordering rules for a successful quote (I12, "Ordering").
fn assert_ordering(q: &Quoted, mid: Decimal, inventory: Decimal, p: &PricingParams) {
    assert!(q.bid_ticks < q.ask_ticks, "bid < ask");
    assert!(q.bid_price < q.clamped.reservation && q.clamped.reservation < q.ask_price);
    let skew_cap = mid * Decimal::from(p.max_skew_bps) / BPS;
    assert!((q.clamped.reservation - mid).abs() <= skew_cap, "skew cap");
    for price in [q.bid_price, q.ask_price] {
        assert!((price - mid).abs() / mid <= p.max_deviation, "guard");
    }
    if inventory.is_zero() {
        assert!(
            q.bid_price < mid && mid < q.ask_price,
            "bid < mid < ask at zero inventory"
        );
    }
}

#[test]
fn test_default_quote_at_mid_1_20_and_3000() {
    let p = PricingParams::relative_defaults();
    // tick 0.01 USDC: quote decimals 6, quote lot 10,000.
    for (mid, m) in [
        (d("1.20"), market(0, 6, 10_000)),
        (d("3000"), market(6, 6, 10_000)),
    ] {
        assert_eq!(tick(&m), d("0.01"));
        let q = pricing::quote(mid, Decimal::ZERO, &p, &m).unwrap();
        // Before rounding: the half-spread lies within the bps bounds.
        let lo = mid * Decimal::from(p.min_half_spread_bps) / BPS;
        let hi = mid * Decimal::from(p.max_half_spread_bps) / BPS;
        assert!(lo <= q.clamped.half_spread && q.clamped.half_spread <= hi);
        // After rounding: each side is at most cap + one tick away from the mid.
        let allowance = hi + tick(&m);
        assert!((q.bid_price - mid).abs() <= allowance);
        assert!((q.ask_price - mid).abs() <= allowance);
        assert_ordering(&q, mid, Decimal::ZERO, &p);
    }
}

#[test]
fn test_i9_regression_absolute_defaults_at_mid_1_20_now_quote() {
    // The I9 demo: a metalbook long token (0 decimals) against USDC on a 0.01 tick, with the
    // CLI's absolute defaults, which gave a 54% deviation before I12.
    let p = PricingParams::relative_defaults();
    let m = market(0, 6, 10_000);
    let q = pricing::quote(d("1.20"), Decimal::ZERO, &p, &m).unwrap();
    // About 0.645% each side: bid 1.19226 rounds down to 1.19, ask 1.20774 up to 1.21.
    assert_eq!((q.bid_ticks.get(), q.ask_ticks.get()), (119, 121));
}

#[test]
fn test_overrides_are_clamped_at_both_caps() {
    let m = market(6, 6, 1);
    let mid = d("100");
    // A wide model spread (intensity 0.1: about 6.9%) is cut to the cap.
    let wide = PricingParams {
        order_intensity: d("0.1"),
        ..PricingParams::relative_defaults()
    };
    let c = clamped_prices(mid, Decimal::ZERO, &wide).unwrap();
    assert_eq!(c.half_spread, d("1")); // 100 bps of 100
                                       // A tiny model spread (huge intensity) is raised to the floor.
    let tight = PricingParams {
        order_intensity: d("1000000000"),
        ..PricingParams::relative_defaults()
    };
    let c = clamped_prices(mid, Decimal::ZERO, &tight).unwrap();
    assert_eq!(c.half_spread, d("0.05")); // 5 bps of 100
                                          // A large inventory skews the reservation, but no further than the skew cap.
    let skewed = PricingParams {
        risk_aversion: d("10"),
        ..PricingParams::relative_defaults()
    };
    let c = clamped_prices(mid, d("10000"), &skewed).unwrap();
    assert_eq!(c.reservation, d("99")); // 100 bps below the mid
    for p in [wide, tight, skewed] {
        assert!(pricing::quote(mid, Decimal::ZERO, &p, &m).is_ok());
    }
}

#[test]
fn test_typed_refusals() {
    let p = PricingParams::relative_defaults();
    // Mid 0.01 on a 0.01 tick: the bid (about 0.00994) rounds down to zero.
    assert_eq!(
        pricing::quote(d("0.01"), Decimal::ZERO, &p, &market(6, 6, 10_000)),
        Err(PricingError::NonPositiveBid)
    );
    // Mid 1.00 on a 0.10 tick: bid 0.99 rounds to 0.90, ask 1.01 to 1.10, 10% away.
    assert!(matches!(
        pricing::quote(d("1.00"), Decimal::ZERO, &p, &market(6, 6, 100_000)),
        Err(PricingError::TickTooCoarse { .. })
    ));
}

/// The first rule a rounded pair fails, recomputed independently of `pricing::quote` from the
/// clamped prices and the (separately property-tested) tick conversion.
fn expected(
    mid: Decimal,
    inventory: Decimal,
    p: &PricingParams,
    m: &MarketInfo,
) -> Result<(), &'static str> {
    let c = clamped_prices(mid, inventory, p).unwrap();
    if c.bid <= Decimal::ZERO {
        return Err("NonPositiveBid");
    }
    let bid = match m.price_to_ticks(c.bid, Rounding::Down) {
        Ok(t) => t,
        Err(TickError::NotPositive) => return Err("NonPositiveBid"),
        Err(TickError::Overflow) => return Err("Tick"),
    };
    let Ok(ask) = m.price_to_ticks(c.ask, Rounding::Up) else {
        return Err("Tick");
    };
    if bid >= ask {
        return Err("Crossed");
    }
    for t in [bid, ask] {
        let price = m.ticks_to_price(t).unwrap();
        if (price - mid).abs() / mid > p.max_deviation {
            return Err("TickTooCoarse");
        }
    }
    Ok(())
}

fn name(e: &PricingError) -> &'static str {
    match e {
        PricingError::NonPositiveBid => "NonPositiveBid",
        PricingError::Crossed { .. } => "Crossed",
        PricingError::TickTooCoarse { .. } => "TickTooCoarse",
        PricingError::Tick(_) => "Tick",
        other => panic!("unexpected refusal {other:?}"),
    }
}

fn config() -> Config {
    let cases = std::env::var("QUOTEBOOK_PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_000);
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x0B0C_0012_0B05_0001),
        failure_persistence: None,
        ..Config::default()
    }
}

proptest! {
    #![proptest_config(config())]

    /// Mids from 0.01 to 1,000,000 (in cents), ticks from 0.000001 to 1, inventory from -100
    /// to 100 base tokens.
    #[test]
    fn test_quote_over_mids_ticks_and_inventory(
        cents in 1u64..=100_000_000,
        quote_lot in 1u64..=1_000_000,
        inventory in -100i64..=100,
    ) {
        let mid = Decimal::new(i64::try_from(cents).unwrap(), 2);
        let inventory = Decimal::from(inventory);
        let p = PricingParams::relative_defaults();
        let m = market(6, 6, quote_lot);
        let inside = tick(&m) <= mid * p.max_deviation / Decimal::TWO;
        let got = pricing::quote(mid, inventory, &p, &m);
        match &got {
            Ok(q) => assert_ordering(q, mid, inventory, &p),
            Err(e) => {
                prop_assert!(!inside, "refused inside the domain: {e:?} (mid {mid}, tick {})", tick(&m));
            }
        }
        // Outside the domain, a refusal must be exactly the first rule that fails, and a pair
        // that would pass is never refused.
        let want = expected(mid, inventory, &p, &m);
        prop_assert_eq!(got.as_ref().map(|_| ()).map_err(name), want);
    }
}

/// The two model unit modes, unclamped (caps opened wide) at a mid other than 100, against
/// direct calls of the model: `Absolute` is exactly the pre-I12 call on the actual mid,
/// `Relative` is the call at 100 scaled by `mid / 100`.
#[test]
fn test_model_units_against_the_direct_model_call() {
    use market_maker_rs::strategy::avellaneda_stoikov::calculate_optimal_quotes;
    use quotebook_maker::pricing::{ModelUnits, MODEL_MID};
    let (mid, inventory) = (d("25"), d("3"));
    let wide = |model_units| PricingParams {
        min_half_spread_bps: 1,
        max_half_spread_bps: 10_000,
        max_skew_bps: 10_000,
        model_units,
        ..PricingParams::relative_defaults()
    };
    let direct = |at: Decimal, p: &PricingParams| {
        let (b, a) = calculate_optimal_quotes(
            at,
            inventory,
            p.risk_aversion,
            p.volatility,
            p.time_to_terminal_ms,
            p.order_intensity,
        )
        .unwrap();
        ((b + a) / Decimal::TWO, (a - b) / Decimal::TWO)
    };

    let abs = wide(ModelUnits::Absolute);
    let c = clamped_prices(mid, inventory, &abs).unwrap();
    let (r, h) = direct(mid, &abs);
    assert_eq!(
        (c.reservation, c.half_spread),
        (r, h),
        "absolute = the pre-I12 call"
    );

    let rel = wide(ModelUnits::Relative);
    let c = clamped_prices(mid, inventory, &rel).unwrap();
    let (r, h) = direct(MODEL_MID, &rel);
    assert_eq!(
        (c.reservation, c.half_spread),
        (r * mid / MODEL_MID, h * mid / MODEL_MID),
        "relative = the call at 100, scaled"
    );
    // The two differ at a mid other than 100: absolute's spread term is in price units.
    assert_ne!(
        clamped_prices(mid, inventory, &abs).unwrap().half_spread,
        clamped_prices(mid, inventory, &rel).unwrap().half_spread
    );
}
