//! Prices from market-maker-rs's Avellaneda-Stoikov model, clamped in basis points of the
//! mid, rounded to ticks toward the maker, and checked against the mid last
//! (docs/PLAN-improvements.md, I12).
//!
//! The model's parameters are absolute: its spread term `(2/γ) ln(1 + γ/k)` is in price
//! units, so one set of defaults cannot suit every price level (at a 1.20 mid the old defaults
//! put the quote 54% away; at 3,000 they would quote about 0.04% wide). Two things fix that:
//! - by default ([`ModelUnits::Relative`]) the model runs on the mid normalized to
//!   [`MODEL_MID`], and its prices are scaled back. Explicit absolute overrides
//!   ([`ModelUnits::Absolute`]) run it on the actual mid, as before;
//! - whatever the model returns, the half-spread is clamped to
//!   `[min_half_spread_bps, max_half_spread_bps]` of the mid, and the reservation price to
//!   within `max_skew_bps` of it.
//!
//! Precedence, in order:
//!
//! 1. the model (defaults or the absolute overrides) gives a reservation price and a
//!    half-spread;
//! 2. the clamps apply, always, overrides included;
//! 3. both prices round to ticks toward the maker (bid down, ask up), with exact integer math;
//! 4. the deviation guard checks the **rounded** prices. Failing it is a refusal, never a
//!    silent re-price.
//!
//! The deviation check follows otc-rfq's `application/services/price_bounds.rs` (MIT,
//! joaquinbejar/otc-rfq 0.1.1): reject a price whose `|price - reference| / reference` exceeds
//! a tolerance, before anything is signed.

use market_maker_rs::strategy::avellaneda_stoikov::calculate_optimal_quotes;
use quotebook_core::QuoteLotsPerBaseLot;
use rust_decimal::Decimal;

use crate::market::{MarketInfo, Rounding, TickError};

/// Basis points per unit.
const BPS: u32 = 10_000;

/// The price level the model runs at: the mid is normalized to it and the model's prices are
/// scaled back. The model's parameters (gamma, sigma, k) are therefore those of an asset
/// priced 100. With the defaults this gives a half-spread of about 0.65% at any mid.
pub const MODEL_MID: Decimal = Decimal::from_parts(100, 0, 0, false, 0);

/// Pricing settings. `guard`, `max_deviation`, is a fraction (`0.05` is 5%); the `*_bps`
/// fields are basis points of the mid (1 bp = 0.0001).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricingParams {
    /// Risk aversion, gamma, in [`ModelUnits`].
    pub risk_aversion: Decimal,
    /// Annualized volatility, in [`ModelUnits`].
    pub volatility: Decimal,
    /// Time to the strategy's horizon, in milliseconds.
    pub time_to_terminal_ms: u64,
    /// Order intensity, k, in [`ModelUnits`].
    pub order_intensity: Decimal,
    /// Largest accepted `|price - mid| / mid` of a rounded price, as a fraction.
    pub max_deviation: Decimal,
    /// Floor of the half-spread, in bps of the mid; at least 1.
    pub min_half_spread_bps: u32,
    /// Cap of the half-spread, in bps of the mid.
    pub max_half_spread_bps: u32,
    /// Cap of `|reservation - mid|`, in bps of the mid.
    pub max_skew_bps: u32,
    /// The units of `risk_aversion`, `volatility` and `order_intensity`.
    pub model_units: ModelUnits,
}

/// How the model's parameters are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelUnits {
    /// The model runs on the mid normalized to [`MODEL_MID`] and its prices scale back by
    /// `mid / MODEL_MID`, so the parameters are those of an asset priced 100 and mean the same
    /// at every price level. The default.
    Relative,
    /// The model runs on the actual mid, as before I12: the parameters are in the asset's own
    /// price units (the absolute advanced overrides). The bps clamps still apply.
    Absolute,
}

impl PricingParams {
    /// Defaults that fit any price level: half-spread 5 to 100 bps, skew up to 100 bps, guard
    /// 5%. The model parameters are the CLI's historical defaults.
    #[must_use]
    pub fn relative_defaults() -> Self {
        Self {
            risk_aversion: Decimal::new(1, 1),
            volatility: Decimal::new(6, 1),
            time_to_terminal_ms: 3_600_000,
            order_intensity: Decimal::new(15, 1),
            max_deviation: Decimal::new(5, 2),
            min_half_spread_bps: 5,
            max_half_spread_bps: 100,
            max_skew_bps: 100,
            model_units: ModelUnits::Relative,
        }
    }

    /// The startup checks: each failure is its own error, never a silent fallback.
    ///
    /// # Errors
    ///
    /// - `HalfSpreadBounds` unless `0 < min_half_spread_bps <= max_half_spread_bps`.
    /// - `Guard` unless `0 < max_deviation < 2/3`.
    /// - `CapsExceedGuard` unless `(max_half_spread_bps + max_skew_bps) / 10,000 <=
    ///   max_deviation / 2`: then a rounded side deviates by at most `guard / 2 + tick / mid`,
    ///   which is within the guard whenever `tick <= mid × guard / 2`.
    pub fn validate(&self) -> Result<(), PricingConfigError> {
        if self.min_half_spread_bps == 0 || self.min_half_spread_bps > self.max_half_spread_bps {
            return Err(PricingConfigError::HalfSpreadBounds);
        }
        let two_thirds = Decimal::new(2, 0)
            .checked_div(Decimal::new(3, 0))
            .ok_or(PricingConfigError::Guard)?;
        if self.max_deviation <= Decimal::ZERO || self.max_deviation >= two_thirds {
            return Err(PricingConfigError::Guard);
        }
        let caps = self
            .max_half_spread_bps
            .checked_add(self.max_skew_bps)
            .ok_or(PricingConfigError::CapsExceedGuard)?;
        let caps = Decimal::from(caps)
            .checked_div(Decimal::from(BPS))
            .ok_or(PricingConfigError::CapsExceedGuard)?;
        let half_guard = self
            .max_deviation
            .checked_div(Decimal::new(2, 0))
            .ok_or(PricingConfigError::CapsExceedGuard)?;
        if caps > half_guard {
            return Err(PricingConfigError::CapsExceedGuard);
        }
        Ok(())
    }
}

/// A pricing configuration the startup checks reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PricingConfigError {
    /// The half-spread bounds are not `0 < min <= max`.
    #[error("half-spread bounds must satisfy 0 < min_half_spread_bps <= max_half_spread_bps")]
    HalfSpreadBounds,
    /// The guard is not in `(0, 2/3)`.
    #[error("max_deviation must be in (0, 2/3)")]
    Guard,
    /// The caps can push a price beyond half the guard.
    #[error("(max_half_spread_bps + max_skew_bps) / 10000 must be at most max_deviation / 2")]
    CapsExceedGuard,
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
    /// The rounded bid is zero or below.
    #[error("the rounded bid is not positive")]
    NonPositiveBid,
    /// Rounding left the bid at or above the ask.
    #[error("rounding crossed the quotes: bid {bid} ticks, ask {ask} ticks")]
    Crossed {
        /// Bid ticks.
        bid: u64,
        /// Ask ticks.
        ask: u64,
    },
    /// A rounded side deviates from the mid by more than the guard: the tick is too coarse
    /// for this price.
    #[error("price deviates {deviation} from the reference, above the {max} tolerance (tick too coarse)")]
    TickTooCoarse {
        /// `|rounded price - mid| / mid`.
        deviation: Decimal,
        /// The guard.
        max: Decimal,
    },
    /// A tick conversion overflowed.
    #[error("tick conversion: {0}")]
    Tick(TickError),
    /// A decimal operation overflowed.
    #[error("decimal overflow")]
    Overflow,
}

/// The clamped, unrounded prices, before ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Clamped {
    /// Reservation price after the skew clamp.
    pub reservation: Decimal,
    /// Half-spread after the bps clamp.
    pub half_spread: Decimal,
    /// `reservation - half_spread`.
    pub bid: Decimal,
    /// `reservation + half_spread`.
    pub ask: Decimal,
}

/// Rounded quotes, in ticks, with the prices they stand for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quoted {
    /// The clamped prices the ticks were rounded from.
    pub clamped: Clamped,
    /// The bid, rounded down.
    pub bid_ticks: QuoteLotsPerBaseLot,
    /// The ask, rounded up.
    pub ask_ticks: QuoteLotsPerBaseLot,
    /// The bid's price in quote tokens per base token.
    pub bid_price: Decimal,
    /// The ask's price in quote tokens per base token.
    pub ask_price: Decimal,
}

fn bps_of(mid: Decimal, bps: u32) -> Result<Decimal, PricingError> {
    mid.checked_mul(Decimal::from(bps))
        .and_then(|v| v.checked_div(Decimal::from(BPS)))
        .ok_or(PricingError::Overflow)
}

/// Steps 1 and 2: the model's reservation and half-spread for `inventory` (base tokens,
/// positive when long), clamped in bps of `mid`.
///
/// # Errors
///
/// `BadReference`, `Model` or `Overflow`.
pub fn clamped_prices(
    mid: Decimal,
    inventory: Decimal,
    params: &PricingParams,
) -> Result<Clamped, PricingError> {
    if mid <= Decimal::ZERO {
        return Err(PricingError::BadReference);
    }
    // Relative: the model runs on the mid normalized to MODEL_MID and its prices scale back
    // by `mid / MODEL_MID`. Absolute: it runs on the mid itself (scale 1).
    let model_mid = match params.model_units {
        ModelUnits::Relative => MODEL_MID,
        ModelUnits::Absolute => mid,
    };
    let (bid, ask) = calculate_optimal_quotes(
        model_mid,
        inventory,
        params.risk_aversion,
        params.volatility,
        params.time_to_terminal_ms,
        params.order_intensity,
    )
    .map_err(|e| PricingError::Model(e.to_string()))?;
    let two = Decimal::new(2, 0);
    let overflow = || PricingError::Overflow;
    let scale = |v: Decimal| match params.model_units {
        ModelUnits::Relative => v
            .checked_mul(mid)
            .and_then(|x| x.checked_div(MODEL_MID))
            .ok_or_else(overflow),
        ModelUnits::Absolute => Ok(v),
    };
    let reservation = scale(
        bid.checked_add(ask)
            .and_then(|s| s.checked_div(two))
            .ok_or_else(overflow)?,
    )?;
    let half = scale(
        ask.checked_sub(bid)
            .and_then(|s| s.checked_div(two))
            .ok_or_else(overflow)?,
    )?;
    let skew = bps_of(mid, params.max_skew_bps)?;
    let lo = mid.checked_sub(skew).ok_or_else(overflow)?;
    let hi = mid.checked_add(skew).ok_or_else(overflow)?;
    // `max` then `min`, not `clamp`, which panics when the bounds are inverted (an
    // unvalidated configuration): the cap wins then.
    let reservation = reservation.max(lo).min(hi);
    let half_spread = half
        .max(bps_of(mid, params.min_half_spread_bps)?)
        .min(bps_of(mid, params.max_half_spread_bps)?);
    Ok(Clamped {
        reservation,
        half_spread,
        bid: reservation.checked_sub(half_spread).ok_or_else(overflow)?,
        ask: reservation.checked_add(half_spread).ok_or_else(overflow)?,
    })
}

/// The whole pipeline: clamped prices, rounded toward the maker, then judged. Refuses with
/// the first rule that fails: `NonPositiveBid`, `Crossed`, then `TickTooCoarse`.
///
/// # Errors
///
/// The errors of [`clamped_prices`], the three refusals, or `Tick` for a conversion overflow.
pub fn quote(
    mid: Decimal,
    inventory: Decimal,
    params: &PricingParams,
    market: &MarketInfo,
) -> Result<Quoted, PricingError> {
    let clamped = clamped_prices(mid, inventory, params)?;
    if clamped.bid <= Decimal::ZERO {
        return Err(PricingError::NonPositiveBid);
    }
    let bid_ticks = match market.price_to_ticks(clamped.bid, Rounding::Down) {
        Ok(t) => t,
        Err(TickError::NotPositive) => return Err(PricingError::NonPositiveBid),
        Err(e) => return Err(PricingError::Tick(e)),
    };
    let ask_ticks = market
        .price_to_ticks(clamped.ask, Rounding::Up)
        .map_err(PricingError::Tick)?;
    if bid_ticks >= ask_ticks {
        return Err(PricingError::Crossed {
            bid: bid_ticks.get(),
            ask: ask_ticks.get(),
        });
    }
    let bid_price = market
        .ticks_to_price(bid_ticks)
        .ok_or(PricingError::Overflow)?;
    let ask_price = market
        .ticks_to_price(ask_ticks)
        .ok_or(PricingError::Overflow)?;
    for price in [bid_price, ask_price] {
        check_bounds(price, mid, params.max_deviation)?;
    }
    Ok(Quoted {
        clamped,
        bid_ticks,
        ask_ticks,
        bid_price,
        ask_price,
    })
}

/// Checks `|price - reference| / reference <= max`.
///
/// # Errors
///
/// `BadReference`, `TickTooCoarse` or `Overflow`.
pub fn check_bounds(price: Decimal, reference: Decimal, max: Decimal) -> Result<(), PricingError> {
    if reference <= Decimal::ZERO {
        return Err(PricingError::BadReference);
    }
    let deviation = price
        .checked_sub(reference)
        .and_then(|d| d.abs().checked_div(reference))
        .ok_or(PricingError::Overflow)?;
    if deviation > max {
        return Err(PricingError::TickTooCoarse { deviation, max });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_macros)]
mod tests {
    use super::{check_bounds, PricingConfigError, PricingError, PricingParams};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn test_validate_each_startup_check() {
        assert_eq!(PricingParams::relative_defaults().validate(), Ok(()));
        let p = |f: fn(&mut PricingParams)| {
            let mut p = PricingParams::relative_defaults();
            f(&mut p);
            p.validate()
        };
        let bounds = Err(PricingConfigError::HalfSpreadBounds);
        assert_eq!(p(|p| p.min_half_spread_bps = 0), bounds);
        assert_eq!(p(|p| p.min_half_spread_bps = 101), bounds);
        assert_eq!(
            p(|p| p.max_deviation = d("0")),
            Err(PricingConfigError::Guard)
        );
        assert_eq!(
            p(|p| p.max_deviation = d("0.7")),
            Err(PricingConfigError::Guard)
        );
        // 100 + 100 bps = 2% > 0.03 / 2.
        assert_eq!(
            p(|p| p.max_deviation = d("0.03")),
            Err(PricingConfigError::CapsExceedGuard)
        );
        // Exactly at the bound passes: 2% = 0.04 / 2.
        assert_eq!(p(|p| p.max_deviation = d("0.04")), Ok(()));
    }

    #[test]
    fn test_check_bounds() {
        assert_eq!(check_bounds(d("104"), d("100"), d("0.05")), Ok(()));
        assert_eq!(check_bounds(d("95"), d("100"), d("0.05")), Ok(()));
        assert!(matches!(
            check_bounds(d("106"), d("100"), d("0.05")),
            Err(PricingError::TickTooCoarse { .. })
        ));
        assert_eq!(
            check_bounds(d("1"), d("0"), d("0.05")),
            Err(PricingError::BadReference)
        );
    }
}
