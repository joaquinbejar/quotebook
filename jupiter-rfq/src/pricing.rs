//! Quote amounts from quotebook-maker's pricing (docs/design/jupiter-rfq.md, "What can be
//! reused"): the Avellaneda-Stoikov model with its bps clamps and its deviation guard
//! (`quotebook_maker::pricing`), applied to atoms instead of lots and ticks, since Jupiter quotes
//! in atoms.
//!
//! The maker buys base at the bid and sells it at the ask. Each amount rounds toward the maker:
//! what the maker pays rounds down, what it receives rounds up. The math is exact: the price is
//! first rounded toward the maker to [`PRICE_DP`] decimals, then every amount is an integer
//! quotient in `u128`, with every step checked. A rounded price that strays from the mid by more
//! than the guard, which tiny amounts can cause, is refused.

use quotebook_maker::pricing::{check_bounds, clamped_prices, PricingParams};
use rust_decimal::{Decimal, RoundingStrategy};

use crate::api::QuoteType;

/// Decimal places a price keeps, rounded toward the maker.
pub const PRICE_DP: u32 = 9;
/// The most decimals a mint may have here.
pub const MAX_DECIMALS: u8 = 18;

/// Which way the maker trades base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The taker pays base, the maker pays quote: the maker's bid.
    MakerBuysBase,
    /// The taker pays quote, the maker pays base: the maker's ask.
    MakerSellsBase,
}

/// What a market prices with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricingInputs {
    /// Base mint decimals.
    pub base_decimals: u8,
    /// Quote mint decimals.
    pub quote_decimals: u8,
    /// The maker's pricing settings, checked at start.
    pub params: PricingParams,
}

/// The two amounts of a quote, in atoms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Priced {
    /// What the taker pays.
    pub amount_in: u64,
    /// What the maker pays.
    pub amount_out: u64,
}

/// Why no amounts could be produced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PriceError {
    /// The model or the guard refused.
    #[error("pricing: {0}")]
    Pricing(#[from] quotebook_maker::pricing::PricingError),
    /// An amount rounds to zero.
    #[error("the amount is too small to quote")]
    TooSmall,
    /// An amount does not fit the integer math.
    #[error("the amount is too large to quote")]
    TooLarge,
}

#[derive(Debug, Clone, Copy)]
enum Round {
    Down,
    Up,
}

fn pow10(n: u32) -> Option<u128> {
    10u128.checked_pow(n)
}

fn gcd(a: u128, b: u128) -> u128 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let r = a.checked_rem(b).unwrap_or(0);
        a = b;
        b = r;
    }
    a
}

/// `n × num / den`, rounded as asked; `None` on overflow or a zero denominator.
fn mul_div(n: u128, num: u128, den: u128, round: Round) -> Option<u128> {
    let p = n.checked_mul(num)?;
    let q = p.checked_div(den)?;
    match round {
        Round::Down => Some(q),
        Round::Up => {
            if p.checked_rem(den)? == 0 {
                Some(q)
            } else {
                q.checked_add(1)
            }
        }
    }
}

/// Quote atoms per base atom as a reduced fraction `num / den`.
fn rate(price: Decimal, inputs: &PricingInputs) -> Option<(u128, u128)> {
    let mantissa = u128::try_from(price.mantissa()).ok()?;
    let num = mantissa.checked_mul(pow10(u32::from(inputs.quote_decimals))?)?;
    let den = pow10(price.scale().checked_add(u32::from(inputs.base_decimals))?)?;
    let g = gcd(num, den);
    if g == 0 {
        return None;
    }
    Some((num.checked_div(g)?, den.checked_div(g)?))
}

/// Prices `amount` (the taker's input for exact-in, its output for exact-out) at `mid`, with
/// the maker holding `inventory` base tokens above its target.
///
/// # Errors
///
/// `Pricing` when the model or the guard refuses, `TooSmall` or `TooLarge`.
pub fn price(
    inputs: &PricingInputs,
    mid: Decimal,
    inventory: Decimal,
    side: Side,
    quote_type: QuoteType,
    amount: u64,
) -> Result<Priced, PriceError> {
    let clamped = clamped_prices(mid, inventory, &inputs.params)?;
    let maker_price = match side {
        Side::MakerBuysBase => clamped
            .bid
            .round_dp_with_strategy(PRICE_DP, RoundingStrategy::ToZero),
        Side::MakerSellsBase => clamped
            .ask
            .round_dp_with_strategy(PRICE_DP, RoundingStrategy::AwayFromZero),
    };
    if maker_price <= Decimal::ZERO {
        return Err(PriceError::TooSmall);
    }
    check_bounds(maker_price, mid, inputs.params.max_deviation)?;
    let (num, den) = rate(maker_price, inputs).ok_or(PriceError::TooLarge)?;
    let a = u128::from(amount);
    // base -> quote is × num / den; quote -> base is × den / num.
    let other = match (side, quote_type) {
        // The taker pays `amount` base; the maker pays quote, rounded down.
        (Side::MakerBuysBase, QuoteType::ExactIn) => mul_div(a, num, den, Round::Down),
        // The maker pays `amount` quote; the taker pays base, rounded up.
        (Side::MakerBuysBase, QuoteType::ExactOut) => mul_div(a, den, num, Round::Up),
        // The taker pays `amount` quote; the maker pays base, rounded down.
        (Side::MakerSellsBase, QuoteType::ExactIn) => mul_div(a, den, num, Round::Down),
        // The maker pays `amount` base; the taker pays quote, rounded up.
        (Side::MakerSellsBase, QuoteType::ExactOut) => mul_div(a, num, den, Round::Up),
    }
    .ok_or(PriceError::TooLarge)?;
    let other = u64::try_from(other).map_err(|_| PriceError::TooLarge)?;
    let priced = match quote_type {
        QuoteType::ExactIn => Priced {
            amount_in: amount,
            amount_out: other,
        },
        QuoteType::ExactOut => Priced {
            amount_in: other,
            amount_out: amount,
        },
    };
    if priced.amount_in == 0 || priced.amount_out == 0 {
        return Err(PriceError::TooSmall);
    }
    // The price the atoms actually give, against the same guard.
    let (base, quote) = match side {
        Side::MakerBuysBase => (priced.amount_in, priced.amount_out),
        Side::MakerSellsBase => (priced.amount_out, priced.amount_in),
    };
    let scale = |d: u8| 10u64.checked_pow(u32::from(d)).map(Decimal::from);
    let effective = Decimal::from(quote)
        .checked_mul(scale(inputs.base_decimals).ok_or(PriceError::TooLarge)?)
        .and_then(|q| {
            Decimal::from(base)
                .checked_mul(scale(inputs.quote_decimals)?)
                .and_then(|b| q.checked_div(b))
        })
        .ok_or(PriceError::TooLarge)?;
    check_bounds(effective, mid, inputs.params.max_deviation)?;
    Ok(priced)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_macros)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn inputs() -> PricingInputs {
        // SOL (9 decimals) against USDC (6).
        PricingInputs {
            base_decimals: 9,
            quote_decimals: 6,
            params: PricingParams::relative_defaults(),
        }
    }

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn amounts_round_toward_the_maker() {
        let i = inputs();
        let mid = d("150");
        let bid_in = price(
            &i,
            mid,
            Decimal::ZERO,
            Side::MakerBuysBase,
            QuoteType::ExactIn,
            1_000_000_000,
        )
        .unwrap();
        // 1 SOL in at a bid below 150: less than 150 USDC out.
        assert_eq!(bid_in.amount_in, 1_000_000_000);
        assert!(bid_in.amount_out < 150_000_000 && bid_in.amount_out > 148_000_000);
        // Exact-out of that amount needs at least the same base back.
        let bid_out = price(
            &i,
            mid,
            Decimal::ZERO,
            Side::MakerBuysBase,
            QuoteType::ExactOut,
            bid_in.amount_out,
        )
        .unwrap();
        assert!(bid_out.amount_in <= 1_000_000_000 && bid_out.amount_in > 999_000_000);
        let ask_in = price(
            &i,
            mid,
            Decimal::ZERO,
            Side::MakerSellsBase,
            QuoteType::ExactIn,
            150_000_000,
        )
        .unwrap();
        // 150 USDC in at an ask above 150: less than 1 SOL out.
        assert!(ask_in.amount_out < 1_000_000_000);
        let ask_out = price(
            &i,
            mid,
            Decimal::ZERO,
            Side::MakerSellsBase,
            QuoteType::ExactOut,
            1_000_000_000,
        )
        .unwrap();
        assert!(ask_out.amount_in > 150_000_000);
    }

    #[test]
    fn dust_and_overflow_are_refused() {
        let i = inputs();
        // One SOL atom buys no USDC atom at 150 (0.00015 atoms).
        assert_eq!(
            price(
                &i,
                d("150"),
                Decimal::ZERO,
                Side::MakerBuysBase,
                QuoteType::ExactIn,
                1
            ),
            Err(PriceError::TooSmall)
        );
        // 13 lamports give 1 USDC atom, a price of about 77: far from the mid, so the guard
        // refuses.
        assert!(matches!(
            price(
                &i,
                d("150"),
                Decimal::ZERO,
                Side::MakerBuysBase,
                QuoteType::ExactIn,
                13
            ),
            Err(PriceError::Pricing(_))
        ));
        // u64::MAX USDC atoms out need about 1.2e20 lamports in: above u64.
        assert_eq!(
            price(
                &i,
                d("150"),
                Decimal::ZERO,
                Side::MakerBuysBase,
                QuoteType::ExactOut,
                u64::MAX
            ),
            Err(PriceError::TooLarge)
        );
        // At a price of 1e9 the numerator overflows u128 for the largest amounts too.
        assert_eq!(
            price(
                &i,
                d("1000000000"),
                Decimal::ZERO,
                Side::MakerSellsBase,
                QuoteType::ExactOut,
                u64::MAX
            ),
            Err(PriceError::TooLarge)
        );
    }
}
