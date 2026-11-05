//! Marks of metalbook series: one `optfix::price_strip` per expiry.
//!
//! Every conversion follows `metalbook-maker`'s `mark` and `OptfixPricer` step by step (inputs
//! in Q64.64 rounded down, each price back to 18 decimals rounded down, `C(K) − C(cap)` or
//! `P(K)` times the multiplier, clamped to `[0, max_payout]`), because a strip's legs are
//! bit-identical to single prices. `tests/marks.rs` holds the result to fixtures from that crate.

use metalbook_core::{OptionKind, SeriesTerms, MILLI_OUNCES_PER_OUNCE};
use optfix::{
    price_strip, Fixed, MarketInputs, Price, PricingError, Rate, Rounding, Spot, Strike, StripLeg,
    Volatility, Years,
};
use rust_decimal::Decimal;

/// Seconds in the 365-day year used for time to expiry, as `metalbook-maker`.
const SECONDS_PER_YEAR: i64 = 31_536_000;
/// USDC atoms per USD.
const ATOMS_PER_USD: i64 = 1_000_000;
/// Decimal places of a price converted back from Q64.64.
const OUT_DECIMALS: u8 = 18;
/// One USDC atom. A value further than this outside the payoff's bounds is a pricer bug, not
/// rounding: optfix's envelope is about `10^-11` USD at metal prices.
const OUT_OF_BOUNDS_TOLERANCE: Decimal = Decimal::from_parts(1, 0, 0, false, 6);

/// Why a strip could not be priced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MarkError {
    /// optfix refused an input or a leg.
    #[error("optfix: {0:?}")]
    Pricer(PricingError),
    /// The series in one strip do not share an expiry.
    #[error("a strip's series must share an expiry")]
    MixedExpiry,
    /// `now` is at or past the expiry.
    #[error("no time left to expiry")]
    Expired,
    /// A value is outside the payoff's bounds by more than rounding explains.
    #[error("a mark is outside the payoff's bounds")]
    OutOfBounds,
    /// A `Decimal` operation overflowed.
    #[error("overflow")]
    Overflow,
}

/// The market inputs of one strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkInputs {
    /// Spot, USD per ounce.
    pub spot: Decimal,
    /// Annualized volatility.
    pub vol: Decimal,
    /// Continuously compounded risk-free rate.
    pub rate: Decimal,
    /// Continuous carry.
    pub carry: Decimal,
    /// Unix seconds.
    pub now: i64,
}

/// The mark of one contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    /// USD per contract, clamped to `[0, max_payout]`.
    pub value: Decimal,
    /// The contractual maximum payout of one contract, USD.
    pub max_payout: Decimal,
}

/// The legs of a series: `Call K` and `Call cap` for a capped call, `Put K` for a put.
fn legs_of(terms: &SeriesTerms) -> Result<Vec<StripLeg>, MarkError> {
    let leg = |kind, atoms| {
        Ok(StripLeg {
            kind,
            strike: Strike::try_new(to_fixed(usd(atoms)?)?).map_err(MarkError::Pricer)?,
        })
    };
    let strike = terms.key().strike().get();
    match terms.key().kind() {
        OptionKind::Put => Ok(vec![leg(optfix::OptionKind::Put, strike)?]),
        OptionKind::Call => Ok(vec![
            leg(optfix::OptionKind::Call, strike)?,
            leg(optfix::OptionKind::Call, terms.cap().get())?,
        ]),
    }
}

/// Marks every series of one underlying and one expiry with a single `price_strip`, in order.
///
/// # Errors
///
/// [`MarkError`]; no marks at all if any leg fails.
pub fn price_expiry(series: &[SeriesTerms], inputs: &MarkInputs) -> Result<Vec<Mark>, MarkError> {
    let Some(expiry) = series.first().map(|t| t.key().expiry().get()) else {
        return Ok(Vec::new());
    };
    if series.iter().any(|t| t.key().expiry().get() != expiry) {
        return Err(MarkError::MixedExpiry);
    }
    let seconds = expiry.checked_sub(inputs.now).ok_or(MarkError::Overflow)?;
    if seconds <= 0 {
        return Err(MarkError::Expired);
    }
    let years = div(Decimal::from(seconds), Decimal::from(SECONDS_PER_YEAR))?;
    let years = Years::try_new(to_fixed(years)?).map_err(MarkError::Pricer)?;
    let market = MarketInputs {
        spot: Spot::try_new(to_fixed(inputs.spot)?).map_err(MarkError::Pricer)?,
        vol: Volatility::try_new(to_fixed(inputs.vol)?).map_err(MarkError::Pricer)?,
        rate: Rate::try_new(to_fixed(inputs.rate)?).map_err(MarkError::Pricer)?,
        carry: Rate::try_new(to_fixed(inputs.carry)?).map_err(MarkError::Pricer)?,
    };
    let mut legs = Vec::new();
    for terms in series {
        legs.extend(legs_of(terms)?);
    }
    let mut prices = vec![Price::MIN; legs.len()];
    price_strip(years, &market, &legs, &mut prices).map_err(MarkError::Pricer)?;

    let mut prices = prices.into_iter();
    let mut next = || -> Result<Decimal, MarkError> {
        to_decimal(prices.next().ok_or(MarkError::Overflow)?.get())
    };
    series
        .iter()
        .map(|terms| {
            let (per_oz, bound_per_oz) = match terms.key().kind() {
                OptionKind::Put => (next()?, usd(terms.key().strike().get())?),
                OptionKind::Call => {
                    let long = next()?;
                    let short = next()?;
                    let bound = sub(usd(terms.cap().get())?, usd(terms.key().strike().get())?)?;
                    (sub(long, short)?, bound)
                }
            };
            let ounces = div(
                Decimal::from(terms.multiplier().get()),
                Decimal::from(MILLI_OUNCES_PER_OUNCE),
            )?;
            let raw = mul(per_oz, ounces)?;
            let max_payout = mul(bound_per_oz, ounces)?;
            let low = sub(Decimal::ZERO, OUT_OF_BOUNDS_TOLERANCE)?;
            let high = max_payout
                .checked_add(OUT_OF_BOUNDS_TOLERANCE)
                .ok_or(MarkError::Overflow)?;
            if raw < low || raw > high {
                return Err(MarkError::OutOfBounds);
            }
            Ok(Mark {
                value: raw.max(Decimal::ZERO).min(max_payout),
                max_payout,
            })
        })
        .collect()
}

/// USDC atoms to USD, exact.
fn usd(atoms: u64) -> Result<Decimal, MarkError> {
    div(Decimal::from(atoms), Decimal::from(ATOMS_PER_USD))
}

/// A decimal as Q64.64, rounded down.
fn to_fixed(d: Decimal) -> Result<Fixed, MarkError> {
    let scale = u8::try_from(d.scale()).map_err(|_| MarkError::Overflow)?;
    Fixed::from_scaled(d.mantissa(), scale, Rounding::Down).map_err(MarkError::Pricer)
}

/// A Q64.64 price as a decimal with 18 places, rounded down.
fn to_decimal(f: Fixed) -> Result<Decimal, MarkError> {
    let scaled = f
        .to_scaled(OUT_DECIMALS, Rounding::Down)
        .map_err(MarkError::Pricer)?;
    Decimal::try_from_i128_with_scale(scaled, u32::from(OUT_DECIMALS))
        .map_err(|_| MarkError::Overflow)
}

fn sub(a: Decimal, b: Decimal) -> Result<Decimal, MarkError> {
    a.checked_sub(b).ok_or(MarkError::Overflow)
}

fn mul(a: Decimal, b: Decimal) -> Result<Decimal, MarkError> {
    a.checked_mul(b).ok_or(MarkError::Overflow)
}

fn div(a: Decimal, b: Decimal) -> Result<Decimal, MarkError> {
    a.checked_div(b).ok_or(MarkError::Overflow)
}
