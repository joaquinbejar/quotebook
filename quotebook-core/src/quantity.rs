//! Quantity newtypes and the maker's side.
//!
//! Each newtype wraps one integer and has no conversion into another, so the compiler rejects a
//! price passed as a size. They are `#[repr(transparent)]` and `Pod`, so program accounts embed
//! them directly. Zero is representable in every type; where zero is invalid for a quote
//! (`price`, `size`), [`Quote::new`](crate::Quote::new) and
//! [`Quote::decode`](crate::Quote::decode) reject it.

use bytemuck::{Pod, Zeroable};
use solana_address::Address;

/// Defines a `#[repr(transparent)]`, `Pod` newtype over an integer.
macro_rules! quantity {
    (
        $(#[$doc:meta])*
        $name:ident($inner:ty) $(, $derive:ident)*
    ) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash $(, $derive)*)]
        #[derive(Pod, Zeroable)]
        #[repr(transparent)]
        pub struct $name($inner);

        impl $name {
            /// Wraps `value`.
            #[must_use]
            #[inline]
            pub const fn new(value: $inner) -> Self {
                Self(value)
            }

            /// The wrapped value.
            #[must_use]
            #[inline]
            pub const fn get(self) -> $inner {
                self.0
            }
        }
    };
}

quantity!(
    /// A size in base lots. A quote's size is non-zero.
    BaseLots(u64)
);

quantity!(
    /// A price in ticks: quote lots per base lot. A quote's price is non-zero.
    QuoteLotsPerBaseLot(u64)
);

quantity!(
    /// A token amount in the base mint's atoms.
    BaseAtoms(u64),
    Default
);

quantity!(
    /// A token amount in the quote mint's atoms.
    QuoteAtoms(u64),
    Default
);

quantity!(
    /// A Unix time in seconds, as the Clock sysvar reports it.
    UnixTimestamp(i64),
    Default
);

quantity!(
    /// A maker's quote nonce. Makers allocate nonces from a counter; the program accepts each
    /// at most once (see `NonceWindow`).
    Nonce(u64),
    Default
);

/// A series, named by its account address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Pod, Zeroable)]
#[repr(transparent)]
pub struct SeriesId(Address);

impl SeriesId {
    /// Wraps the series account's address.
    #[must_use]
    #[inline]
    pub const fn new(address: Address) -> Self {
        Self(address)
    }

    /// The series account's address.
    #[must_use]
    #[inline]
    pub const fn address(self) -> Address {
        self.0
    }
}

/// The maker's side of a quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Side {
    /// The maker buys base: it pays quote atoms and receives base atoms.
    Bid = 0,
    /// The maker sells base: it pays base atoms and receives quote atoms.
    Ask = 1,
}

impl Side {
    /// The side encoded as `value`, or `None` for anything but 0 or 1.
    #[must_use]
    #[inline]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Bid),
            1 => Some(Self::Ask),
            _ => None,
        }
    }

    /// The stable one-byte encoding.
    #[must_use]
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_macros)]
mod tests {
    use super::{BaseLots, Nonce, Side, UnixTimestamp};

    #[test]
    fn test_side_from_u8_accepts_only_zero_and_one() {
        assert_eq!(Side::from_u8(0), Some(Side::Bid));
        assert_eq!(Side::from_u8(1), Some(Side::Ask));
        for v in 2..=u8::MAX {
            assert_eq!(Side::from_u8(v), None);
        }
    }

    #[test]
    fn test_side_as_u8_round_trips() {
        for s in [Side::Bid, Side::Ask] {
            assert_eq!(Side::from_u8(s.as_u8()), Some(s));
        }
    }

    #[test]
    fn test_quantity_new_get_round_trips() {
        assert_eq!(BaseLots::new(7).get(), 7);
        assert_eq!(Nonce::new(u64::MAX).get(), u64::MAX);
        assert_eq!(UnixTimestamp::new(-1).get(), -1);
    }

    #[test]
    fn test_quantity_is_transparent() {
        assert_eq!(core::mem::size_of::<BaseLots>(), 8);
        assert_eq!(
            core::mem::align_of::<BaseLots>(),
            core::mem::align_of::<u64>()
        );
    }
}
