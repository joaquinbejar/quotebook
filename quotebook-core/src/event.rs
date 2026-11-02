//! The fill event `settle` emits with `sol_log_data` after both transfers succeed
//! (docs/PLAN-improvements.md, I7), and its canonical 168-byte encoding.
//!
//! The event is a hint for indexers, not a source of truth: logs can be truncated or dropped,
//! and any program can log the same bytes. `quotebook-sdk::events` only yields events it can
//! attribute to a successful quotebook frame of a successful transaction.

use solana_address::Address;

use crate::{BaseLots, Nonce, QuoteLotsPerBaseLot, Side, UnixTimestamp};

/// The 8-byte tag that starts every fill event.
pub const FILL_EVENT_TAG: [u8; 8] = *b"qb-fill1";

/// A fill, as `settle` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillEvent {
    /// The series filled.
    pub series: Address,
    /// The quote's maker.
    pub maker: Address,
    /// The taker that signed `settle`.
    pub taker: Address,
    /// The quote's nonce, now used.
    pub nonce: Nonce,
    /// The maker's side.
    pub side: Side,
    /// Price in ticks (quote lots per base lot).
    pub price: QuoteLotsPerBaseLot,
    /// Base lots filled.
    pub fill_size: BaseLots,
    /// Base atoms moved: `fill_size × base_lot_size`.
    pub base_atoms: u64,
    /// Quote atoms moved between maker and taker: `price × fill_size × quote_lot_size`.
    pub quote_atoms: u64,
    /// Fee in quote atoms paid by the taker; 0 until series fees exist (I10).
    pub fee: u64,
    /// The on-chain clock at the fill, in Unix seconds.
    pub unix_timestamp: UnixTimestamp,
}

/// Why bytes are not a fill event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventError {
    /// Not exactly [`FillEvent::LEN`] bytes.
    Length,
    /// The first 8 bytes are not [`FILL_EVENT_TAG`].
    Tag,
    /// The side is not 0 or 1, or its padding is not zero.
    Malformed,
}

/// Byte offsets of the layout.
const SERIES: usize = 8;
const MAKER: usize = 40;
const TAKER: usize = 72;
const NONCE: usize = 104;
const SIDE: usize = 112;
const PRICE: usize = 120;
const FILL: usize = 128;
const BASE: usize = 136;
const QUOTE: usize = 144;
const FEE: usize = 152;
const TIME: usize = 160;

impl FillEvent {
    /// Length of the encoding, in bytes: the tag, three addresses, and eight 8-byte words (the
    /// side byte padded to 8).
    pub const LEN: usize = 168;

    /// The canonical encoding, little-endian.
    #[must_use]
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0u8; Self::LEN];
        let parts: [(usize, &[u8]); 12] = [
            (0, &FILL_EVENT_TAG),
            (SERIES, self.series.as_ref()),
            (MAKER, self.maker.as_ref()),
            (TAKER, self.taker.as_ref()),
            (NONCE, &self.nonce.get().to_le_bytes()),
            (SIDE, &[self.side.as_u8()]),
            (PRICE, &self.price.get().to_le_bytes()),
            (FILL, &self.fill_size.get().to_le_bytes()),
            (BASE, &self.base_atoms.to_le_bytes()),
            (QUOTE, &self.quote_atoms.to_le_bytes()),
            (FEE, &self.fee.to_le_bytes()),
            (TIME, &self.unix_timestamp.get().to_le_bytes()),
        ];
        for (at, bytes) in parts {
            if let Some(dst) = out.get_mut(at..at.saturating_add(bytes.len())) {
                dst.copy_from_slice(bytes);
            }
        }
        out
    }

    /// Decodes canonical event bytes: accepts exactly the outputs of [`FillEvent::encode`].
    ///
    /// # Errors
    ///
    /// `Length`, `Tag` or `Malformed`.
    pub fn decode(bytes: &[u8]) -> Result<Self, EventError> {
        let b: &[u8; Self::LEN] = bytes.try_into().map_err(|_| EventError::Length)?;
        if b.get(..8) != Some(&FILL_EVENT_TAG[..]) {
            return Err(EventError::Tag);
        }
        let word = |at: usize| -> Result<[u8; 8], EventError> {
            b.get(at..at.saturating_add(8))
                .and_then(|s| s.try_into().ok())
                .ok_or(EventError::Length)
        };
        let address = |at: usize| -> Result<Address, EventError> {
            let a: [u8; 32] = b
                .get(at..at.saturating_add(32))
                .and_then(|s| s.try_into().ok())
                .ok_or(EventError::Length)?;
            Ok(Address::new_from_array(a))
        };
        let side_word = word(SIDE)?;
        let (side, pad) = side_word.split_first().ok_or(EventError::Malformed)?;
        if pad.iter().any(|p| *p != 0) {
            return Err(EventError::Malformed);
        }
        let side = Side::from_u8(*side).ok_or(EventError::Malformed)?;
        Ok(Self {
            series: address(SERIES)?,
            maker: address(MAKER)?,
            taker: address(TAKER)?,
            nonce: Nonce::new(u64::from_le_bytes(word(NONCE)?)),
            side,
            price: QuoteLotsPerBaseLot::new(u64::from_le_bytes(word(PRICE)?)),
            fill_size: BaseLots::new(u64::from_le_bytes(word(FILL)?)),
            base_atoms: u64::from_le_bytes(word(BASE)?),
            quote_atoms: u64::from_le_bytes(word(QUOTE)?),
            fee: u64::from_le_bytes(word(FEE)?),
            unix_timestamp: UnixTimestamp::new(i64::from_le_bytes(word(TIME)?)),
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_macros
)]
mod tests {
    use super::{EventError, FillEvent, FILL_EVENT_TAG};
    use crate::{BaseLots, Nonce, QuoteLotsPerBaseLot, Side, UnixTimestamp};
    use solana_address::Address;

    fn event() -> FillEvent {
        FillEvent {
            series: Address::new_from_array([1; 32]),
            maker: Address::new_from_array([2; 32]),
            taker: Address::new_from_array([3; 32]),
            nonce: Nonce::new(u64::MAX),
            side: Side::Ask,
            price: QuoteLotsPerBaseLot::new(7),
            fill_size: BaseLots::new(8),
            base_atoms: 9,
            quote_atoms: 10,
            fee: 11,
            unix_timestamp: UnixTimestamp::new(-12),
        }
    }

    #[test]
    fn test_round_trip_and_layout() {
        let e = event();
        let b = e.encode();
        assert_eq!(&b[..8], &FILL_EVENT_TAG);
        assert_eq!(&b[8..40], &[1; 32]);
        assert_eq!(b[112], 1);
        assert_eq!(&b[160..168], &(-12i64).to_le_bytes());
        assert_eq!(FillEvent::decode(&b), Ok(e));
        let bid = FillEvent {
            side: Side::Bid,
            ..e
        };
        assert_eq!(FillEvent::decode(&bid.encode()), Ok(bid));
    }

    #[test]
    fn test_decode_rejects_non_canonical() {
        let b = event().encode();
        assert_eq!(FillEvent::decode(&b[..167]), Err(EventError::Length));
        let mut long = b.to_vec();
        long.push(0);
        assert_eq!(FillEvent::decode(&long), Err(EventError::Length));
        let mut tag = b;
        tag[0] ^= 1;
        assert_eq!(FillEvent::decode(&tag), Err(EventError::Tag));
        let mut side = b;
        side[112] = 2;
        assert_eq!(FillEvent::decode(&side), Err(EventError::Malformed));
        let mut pad = b;
        pad[113] = 1;
        assert_eq!(FillEvent::decode(&pad), Err(EventError::Malformed));
    }
}
