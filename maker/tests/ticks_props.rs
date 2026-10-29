//! `price_to_ticks` against an exact rational reference: for `price = m / 10^s` the tick
//! count is `m × base_lot × 10^qd / (10^(s + bd) × quote_lot)`, here computed directly in
//! `u128` whenever both sides fit, rounded once (floor for bids, ceiling for asks).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    clippy::disallowed_macros,
    clippy::panic_in_result_fn,
    missing_docs
)]

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use quotebook_core::{BaseLots, Seconds};
use quotebook_maker::market::{MarketInfo, Rounding, TickError};
use rust_decimal::Decimal;
use solana_address::Address;

fn config() -> Config {
    let cases = std::env::var("QUOTEBOOK_PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50_000);
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x0B0C_0006_5EED_0006),
        failure_persistence: None,
        ..Config::default()
    }
}

fn reference(
    m: u128,
    s: u32,
    base_lot: u64,
    quote_lot: u64,
    bd: u8,
    qd: u8,
) -> Option<(u128, u128)> {
    let num = m
        .checked_mul(u128::from(base_lot))?
        .checked_mul(10u128.checked_pow(u32::from(qd))?)?;
    let den = 10u128
        .checked_pow(s.checked_add(u32::from(bd))?)?
        .checked_mul(u128::from(quote_lot))?;
    let floor = num / den;
    let ceil = floor + u128::from(num % den != 0);
    Some((floor, ceil))
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn test_price_to_ticks_matches_exact_reference(
        // Up to the full 96-bit Decimal mantissa, at every scale.
        m in prop_oneof![1u128..1_000_000, 1u128..(1u128 << 96)],
        s in 0u32..=28,
        base_lot in prop_oneof![Just(1u64), 1u64..1_000_000_000],
        quote_lot in prop_oneof![Just(1u64), Just(7u64), 1u64..1_000_000],
        bd in 0u8..=12,
        qd in 0u8..=12,
    ) {
        let price = Decimal::from_i128_with_scale(i128::try_from(m).unwrap(), s);
        let market = MarketInfo {
            program_id: Address::new_from_array([1; 32]),
            deployment: [0; 16],
            series: Address::new_from_array([2; 32]),
            base_decimals: bd,
            quote_decimals: qd,
            base_lot_size: base_lot,
            quote_lot_size: quote_lot,
            max_quote_ttl: Seconds::new(60),
            min_size: BaseLots::new(1),
        };
        if let Some((floor, ceil)) = reference(m, s, base_lot, quote_lot, bd, qd) {
            for (direction, want) in [(Rounding::Down, floor), (Rounding::Up, ceil)] {
                let got = market.price_to_ticks(price, direction);
                match u64::try_from(want) {
                    Ok(0) => prop_assert_eq!(got, Err(TickError::NotPositive)),
                    Ok(w) => prop_assert_eq!(got.map(|t| t.get()), Ok(w)),
                    Err(_) => prop_assert_eq!(got, Err(TickError::Overflow)),
                }
            }
        }
    }
}

#[test]
fn test_reference_covers_large_mantissas() {
    // Guard against the property passing vacuously: a 28-digit mantissa with small
    // multipliers must be checked by the reference.
    assert!(reference((1u128 << 96) - 1, 28, 1, 7, 0, 0).is_some());
    assert!(reference((1u128 << 96) - 1, 0, 1_000_000_000, 1, 0, 12).is_none());
}
