//! The series a maker quotes on, and the conversion of a decimal price into on-chain ticks.

use quotebook_core::{BaseLots, QuoteLotsPerBaseLot, Seconds};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use solana_address::Address;

/// Everything about a series the service needs, read from chain once at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketInfo {
    /// The quotebook program.
    pub program_id: Address,
    /// `Config.deployment_id`.
    pub deployment: [u8; 16],
    /// The series account.
    pub series: Address,
    /// Base mint decimals.
    pub base_decimals: u8,
    /// Quote mint decimals.
    pub quote_decimals: u8,
    /// Base atoms per base lot.
    pub base_lot_size: u64,
    /// Quote atoms per quote lot.
    pub quote_lot_size: u64,
    /// The series' longest quote lifetime.
    pub max_quote_ttl: Seconds,
    /// The series' smallest fill.
    pub min_size: BaseLots,
}

/// Which way a price is rounded to a whole tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    /// Toward zero. A maker's bid rounds down: it pays no more than it priced.
    Down,
    /// Away from zero. A maker's ask rounds up: it receives no less than it priced.
    Up,
}

/// Why a price could not become a tick count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TickError {
    /// The price is zero or negative, or rounds to zero ticks.
    #[error("price does not give a positive tick count")]
    NotPositive,
    /// An intermediate value does not fit `Decimal` or the tick count does not fit `u64`.
    #[error("price overflows the tick conversion")]
    Overflow,
}

impl MarketInfo {
    /// Converts `price`, in quote tokens per base token, into ticks (quote lots per base lot):
    /// `price × base_lot_size × 10^quote_decimals / (10^base_decimals × quote_lot_size)`,
    /// rounded in `direction` (toward the maker: bids down, asks up).
    ///
    /// # Errors
    ///
    /// `NotPositive` or `Overflow`.
    pub fn price_to_ticks(
        &self,
        price: Decimal,
        direction: Rounding,
    ) -> Result<QuoteLotsPerBaseLot, TickError> {
        if price <= Decimal::ZERO {
            return Err(TickError::NotPositive);
        }
        let pow10 = |d: u8| (0..d).try_fold(Decimal::ONE, |acc, _| acc.checked_mul(Decimal::TEN));
        let numerator = price
            .checked_mul(Decimal::from(self.base_lot_size))
            .and_then(|v| v.checked_mul(pow10(self.quote_decimals)?))
            .ok_or(TickError::Overflow)?;
        let denominator = pow10(self.base_decimals)
            .and_then(|v| v.checked_mul(Decimal::from(self.quote_lot_size)))
            .ok_or(TickError::Overflow)?;
        let exact = numerator
            .checked_div(denominator)
            .ok_or(TickError::Overflow)?;
        let strategy = match direction {
            Rounding::Down => RoundingStrategy::ToZero,
            Rounding::Up => RoundingStrategy::AwayFromZero,
        };
        let ticks = exact
            .round_dp_with_strategy(0, strategy)
            .to_u64()
            .ok_or(TickError::Overflow)?;
        if ticks == 0 {
            return Err(TickError::NotPositive);
        }
        Ok(QuoteLotsPerBaseLot::new(ticks))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_macros)]
mod tests {
    use super::{MarketInfo, Rounding, TickError};
    use quotebook_core::{BaseLots, Seconds};
    use rust_decimal::Decimal;
    use solana_address::Address;
    use std::str::FromStr;

    /// SOL/USDC-like: 9 and 6 decimals, base lot 0.001 SOL, quote lot 0.000001 USDC.
    fn market() -> MarketInfo {
        MarketInfo {
            program_id: Address::new_from_array([1; 32]),
            deployment: [2; 16],
            series: Address::new_from_array([3; 32]),
            base_decimals: 9,
            quote_decimals: 6,
            base_lot_size: 1_000_000,
            quote_lot_size: 1,
            max_quote_ttl: Seconds::new(60),
            min_size: BaseLots::new(1),
        }
    }

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn test_price_to_ticks_exact_and_directed() {
        let m = market();
        // 150 USDC per SOL = 0.15 USDC per 0.001 SOL = 150,000 quote lots per base lot.
        assert_eq!(
            m.price_to_ticks(d("150"), Rounding::Down).unwrap().get(),
            150_000
        );
        assert_eq!(
            m.price_to_ticks(d("150"), Rounding::Up).unwrap().get(),
            150_000
        );
        // 150.0000005 is half a tick above 150,000.
        assert_eq!(
            m.price_to_ticks(d("150.0000005"), Rounding::Down)
                .unwrap()
                .get(),
            150_000
        );
        assert_eq!(
            m.price_to_ticks(d("150.0000005"), Rounding::Up)
                .unwrap()
                .get(),
            150_001
        );
    }

    #[test]
    fn test_price_to_ticks_rejects_non_positive_and_overflow() {
        let m = market();
        assert_eq!(
            m.price_to_ticks(d("0"), Rounding::Up),
            Err(TickError::NotPositive)
        );
        assert_eq!(
            m.price_to_ticks(d("-1"), Rounding::Up),
            Err(TickError::NotPositive)
        );
        assert_eq!(
            m.price_to_ticks(d("0.0000001"), Rounding::Down),
            Err(TickError::NotPositive)
        );
        assert_eq!(
            m.price_to_ticks(Decimal::MAX, Rounding::Up),
            Err(TickError::Overflow)
        );
    }
}
