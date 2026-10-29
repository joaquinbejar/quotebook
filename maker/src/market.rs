//! The series a maker quotes on, and the conversion of a decimal price into on-chain ticks.

use quotebook_core::{BaseLots, QuoteLotsPerBaseLot, Seconds};
use rust_decimal::Decimal;
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
    /// Exact: `price` is `mantissa / 10^scale`, so the tick count is the integer quotient
    /// `mantissa × base_lot_size × 10^quote_decimals / (10^(scale + base_decimals) ×
    /// quote_lot_size)`, computed in 256-bit integers. Dividing step by step (by 10, then by
    /// the quote lot) rounding the same way each time is exact for positive integers,
    /// `floor(floor(n / a) / b) = floor(n / (a × b))` and likewise for the ceiling, so the
    /// result is the true value rounded once, never a rounded quotient rounded again.
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
        let mantissa = u128::try_from(price.mantissa()).map_err(|_| TickError::NotPositive)?;
        let mut n = Wide::from_u128(mantissa)
            .mul_small(self.base_lot_size)
            .ok_or(TickError::Overflow)?;
        for _ in 0..self.quote_decimals {
            n = n.mul_small(10).ok_or(TickError::Overflow)?;
        }
        let tens = price
            .scale()
            .checked_add(u32::from(self.base_decimals))
            .ok_or(TickError::Overflow)?;
        for _ in 0..tens {
            n = n.div_small(10, direction).ok_or(TickError::Overflow)?;
        }
        n = n
            .div_small(self.quote_lot_size, direction)
            .ok_or(TickError::Overflow)?;
        let ticks = n.to_u64().ok_or(TickError::Overflow)?;
        if ticks == 0 {
            return Err(TickError::NotPositive);
        }
        Ok(QuoteLotsPerBaseLot::new(ticks))
    }
}

/// An unsigned 256-bit integer, four little-endian 64-bit limbs; only what
/// [`MarketInfo::price_to_ticks`] needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Wide([u64; 4]);

impl Wide {
    fn from_u128(v: u128) -> Self {
        let [lo, hi] = split(v);
        Self([lo, hi, 0, 0])
    }

    /// `self × m`, or `None` on overflow.
    fn mul_small(self, m: u64) -> Option<Self> {
        let mut out = [0u64; 4];
        let mut carry = 0u128;
        for (dst, &limb) in out.iter_mut().zip(self.0.iter()) {
            let p = u128::from(limb)
                .checked_mul(u128::from(m))?
                .checked_add(carry)?;
            let [lo, hi] = split(p);
            *dst = lo;
            carry = u128::from(hi);
        }
        (carry == 0).then_some(Self(out))
    }

    /// `self / d` rounded in `direction`, or `None` if `d` is zero or the rounding up
    /// overflows.
    fn div_small(self, d: u64, direction: Rounding) -> Option<Self> {
        let d = u128::from(d);
        let mut out = [0u64; 4];
        let mut rem = 0u128;
        for (dst, &limb) in out.iter_mut().zip(self.0.iter()).rev() {
            // `rem < d < 2^64`, so `rem × 2^64 + limb` fits `u128`.
            let cur = rem
                .checked_mul(1u128 << 64)?
                .checked_add(u128::from(limb))?;
            *dst = u64::try_from(cur.checked_div(d)?).ok()?;
            rem = cur.checked_rem(d)?;
        }
        let q = Self(out);
        match direction {
            Rounding::Up if rem != 0 => q.add_one(),
            _ => Some(q),
        }
    }

    fn add_one(self) -> Option<Self> {
        let mut out = self.0;
        for limb in &mut out {
            match limb.checked_add(1) {
                Some(v) => {
                    *limb = v;
                    return Some(Self(out));
                }
                None => *limb = 0,
            }
        }
        None
    }

    fn to_u64(self) -> Option<u64> {
        match self.0 {
            [v, 0, 0, 0] => Some(v),
            _ => None,
        }
    }
}

/// `[low 64 bits, high 64 bits]` of `v`.
fn split(v: u128) -> [u64; 2] {
    let bytes = v.to_le_bytes();
    let (lo, hi) = bytes.split_at(8);
    let word = |b: &[u8]| {
        let mut w = [0u8; 8];
        w.copy_from_slice(b);
        u64::from_le_bytes(w)
    };
    [word(lo), word(hi)]
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

    /// The bug this conversion avoids: a `Decimal` quotient rounds to 28 digits, so
    /// 6.9999999999999999999999999999 / 7 became 1.0000000000000000000000000000 and a bid
    /// floored to 1 tick instead of 0. Here 1 base lot = 1 base token and 1 tick = 7 quote
    /// tokens.
    #[test]
    fn test_price_to_ticks_no_double_rounding() {
        let m = MarketInfo {
            base_decimals: 0,
            quote_decimals: 0,
            base_lot_size: 1,
            quote_lot_size: 7,
            ..market()
        };
        let just_below_7 = d("6.9999999999999999999999999999");
        assert_eq!(
            m.price_to_ticks(just_below_7, Rounding::Down),
            Err(TickError::NotPositive)
        );
        assert_eq!(
            m.price_to_ticks(just_below_7, Rounding::Up).unwrap().get(),
            1
        );
        let just_below_14 = d("13.999999999999999999999999999");
        assert_eq!(
            m.price_to_ticks(just_below_14, Rounding::Down)
                .unwrap()
                .get(),
            1
        );
        assert_eq!(
            m.price_to_ticks(just_below_14, Rounding::Up).unwrap().get(),
            2
        );
        let just_above_7 = d("7.0000000000000000000000000001");
        assert_eq!(
            m.price_to_ticks(just_above_7, Rounding::Down)
                .unwrap()
                .get(),
            1
        );
        assert_eq!(
            m.price_to_ticks(just_above_7, Rounding::Up).unwrap().get(),
            2
        );
        assert_eq!(m.price_to_ticks(d("7"), Rounding::Up).unwrap().get(), 1);
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
