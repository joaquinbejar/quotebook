//! Series taker fees (ADR 0001, docs/adr/0001-series-fees.md).

/// The largest series fee, in basis points: 1%.
pub const MAX_FEE_BPS: u16 = 100;

/// Basis points per unit.
const BPS: u128 = 10_000;

/// The fee on `quote_atoms` at `fee_bps`: `floor(quote_atoms × fee_bps / 10,000)`, rounded down
/// in the taker's favour. It is 0 whenever `quote_atoms × fee_bps < 10,000` (accepted dust,
/// ADR 0001). Never above `quote_atoms` when `fee_bps <= 10,000`.
///
/// Returns `None` only if the result does not fit `u64`, which cannot happen for
/// `fee_bps <= 10,000`.
#[must_use]
#[inline]
pub fn fee(quote_atoms: u64, fee_bps: u16) -> Option<u64> {
    let product = u128::from(quote_atoms).checked_mul(u128::from(fee_bps))?;
    u64::try_from(product.checked_div(BPS)?).ok()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_macros,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::{fee, MAX_FEE_BPS};

    #[test]
    fn test_rounding_boundary() {
        // At 100 bps: 99 pays 0, 100 pays 1, 199 pays 1, 200 pays 2.
        let at = |q| fee(q, 100).unwrap();
        assert_eq!([at(99), at(100), at(199), at(200)], [0, 1, 1, 2]);
        // At 1 bp around 10,000.
        let one = |q| fee(q, 1).unwrap();
        assert_eq!(
            [one(9_999), one(10_000), one(19_999), one(20_000)],
            [0, 1, 1, 2]
        );
        assert_eq!(fee(12_345, 0), Some(0));
    }

    #[test]
    fn test_extremes_and_proceeds() {
        assert_eq!(fee(u64::MAX, MAX_FEE_BPS), Some(184_467_440_737_095_516)); // floor(u64::MAX / 100)
        assert_eq!(fee(u64::MAX, 10_000), Some(u64::MAX));
        // Bid proceeds `q - fee` stay at least 1 for every q >= 1 at the maximum fee.
        for q in [1u64, 2, 99, 100, 101, 10_000, u64::MAX] {
            assert!(q - fee(q, MAX_FEE_BPS).unwrap() >= 1, "{q}");
        }
    }
}
