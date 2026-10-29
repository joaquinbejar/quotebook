//! Prices from market-maker-rs's Avellaneda-Stoikov model, checked against the mid.
//!
//! The deviation check follows otc-rfq's `application/services/price_bounds.rs` (MIT,
//! joaquinbejar/otc-rfq 0.1.1): reject a price whose `|price - reference| / reference` exceeds
//! a tolerance, before anything is signed.

use market_maker_rs::strategy::avellaneda_stoikov::calculate_optimal_quotes;
use rust_decimal::Decimal;

/// Avellaneda-Stoikov parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricingParams {
    /// Risk aversion, gamma.
    pub risk_aversion: Decimal,
    /// Annualized volatility.
    pub volatility: Decimal,
    /// Time to the strategy's horizon, in milliseconds.
    pub time_to_terminal_ms: u64,
    /// Order intensity, k.
    pub order_intensity: Decimal,
    /// Largest accepted `|price - mid| / mid`, as a fraction (`0.02` is 2%).
    pub max_deviation: Decimal,
}

/// Why no price could be produced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PricingError {
    /// market-maker-rs rejected the inputs.
    #[error("pricing model: {0}")]
    Model(String),
    /// The reference price is zero or negative.
    #[error("reference price is not positive")]
    BadReference,
    /// The price deviates from the reference by more than the tolerance.
    #[error("price deviates {deviation} from the reference, above the {max} tolerance")]
    OutOfBounds {
        /// `|price - reference| / reference`.
        deviation: Decimal,
        /// The tolerance.
        max: Decimal,
    },
    /// A decimal operation overflowed.
    #[error("decimal overflow")]
    Overflow,
}

/// `(bid, ask)` around `mid` for `inventory` (in base tokens, positive when long).
///
/// # Errors
///
/// `Model` if market-maker-rs rejects the inputs.
pub fn quote_prices(
    mid: Decimal,
    inventory: Decimal,
    params: &PricingParams,
) -> Result<(Decimal, Decimal), PricingError> {
    calculate_optimal_quotes(
        mid,
        inventory,
        params.risk_aversion,
        params.volatility,
        params.time_to_terminal_ms,
        params.order_intensity,
    )
    .map_err(|e| PricingError::Model(e.to_string()))
}

/// Checks `|price - reference| / reference <= max`.
///
/// # Errors
///
/// `BadReference`, `OutOfBounds` or `Overflow`.
pub fn check_bounds(price: Decimal, reference: Decimal, max: Decimal) -> Result<(), PricingError> {
    if reference <= Decimal::ZERO {
        return Err(PricingError::BadReference);
    }
    let deviation = price
        .checked_sub(reference)
        .and_then(|d| d.abs().checked_div(reference))
        .ok_or(PricingError::Overflow)?;
    if deviation > max {
        return Err(PricingError::OutOfBounds { deviation, max });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_macros)]
mod tests {
    use super::{check_bounds, quote_prices, PricingError, PricingParams};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn params() -> PricingParams {
        PricingParams {
            risk_aversion: d("0.1"),
            volatility: d("0.2"),
            time_to_terminal_ms: 3_600_000,
            order_intensity: d("1.5"),
            max_deviation: d("0.05"),
        }
    }

    #[test]
    fn test_quote_prices_straddle_the_mid_and_skew_with_inventory() {
        let (bid, ask) = quote_prices(d("100"), d("0"), &params()).unwrap();
        assert!(bid < d("100") && d("100") < ask);
        // Long inventory lowers both prices: the maker leans toward selling.
        let (long_bid, long_ask) = quote_prices(d("100"), d("50"), &params()).unwrap();
        assert!(long_bid < bid && long_ask < ask);
    }

    #[test]
    fn test_check_bounds() {
        assert_eq!(check_bounds(d("104"), d("100"), d("0.05")), Ok(()));
        assert_eq!(check_bounds(d("95"), d("100"), d("0.05")), Ok(()));
        assert!(matches!(
            check_bounds(d("106"), d("100"), d("0.05")),
            Err(PricingError::OutOfBounds { .. })
        ));
        assert_eq!(
            check_bounds(d("1"), d("0"), d("0.05")),
            Err(PricingError::BadReference)
        );
    }
}
