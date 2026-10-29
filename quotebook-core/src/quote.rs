//! The signed quote and its canonical 208-byte encoding.
//!
//! The encoding is defined once, here, by [`QuoteWire`]: a `#[repr(C)]` struct of byte arrays
//! only, so it has alignment 1, no padding and the same layout on host and SBF. Integers are
//! little-endian. The program never re-serializes a quote: it decodes the exact bytes the
//! ed25519 precompile verified, which may sit at any offset of the precompile data.

use solana_address::Address;

use crate::{BaseLots, Nonce, QuoteLotsPerBaseLot, QuotebookError, SeriesId, Side, UnixTimestamp};

/// Domain-separation tag, the first 16 bytes of every signed quote.
pub const QUOTE_DOMAIN: [u8; 16] = *b"quotebook-v1\0\0\0\0";

/// How far, in seconds, a quote's `issued_at` may lie ahead of the on-chain clock. The Clock
/// sysvar's `unix_timestamp` is a stake-weighted estimate that drifts from wall time.
pub const MAX_CLOCK_SKEW: i64 = 10;

/// Pod layouts. `derive(Pod)` on a multi-field struct checks for padding with a
/// `const _: () = assert!(..)` item: a compile-time check that cannot panic at run time, but
/// the generated item sits in this module, so the macro ban is lifted here and only here. Keep
/// this module free of logic.
#[allow(clippy::disallowed_macros)]
mod layout {
    use bytemuck::{Pod, Zeroable};

    /// The byte layout of a signed quote. Byte arrays only: alignment 1, no implicit padding.
    #[derive(Clone, Copy, Pod, Zeroable)]
    #[repr(C)]
    pub(super) struct QuoteWire {
        pub(super) domain: [u8; 16],
        pub(super) program_id: [u8; 32],
        pub(super) deployment: [u8; 16],
        pub(super) maker: [u8; 32],
        pub(super) series: [u8; 32],
        pub(super) taker: [u8; 32],
        pub(super) side: u8,
        pub(super) pad: [u8; 7],
        pub(super) price: [u8; 8],
        pub(super) size: [u8; 8],
        pub(super) issued_at: [u8; 8],
        pub(super) expires_at: [u8; 8],
        pub(super) nonce: [u8; 8],
    }
}

use layout::QuoteWire;

// Compile-time check that the layout is exactly `Quote::LEN` bytes.
const _: [(); Quote::LEN] = [(); core::mem::size_of::<QuoteWire>()];

/// Everything a maker commits to in a quote, before validation.
///
/// Build one, then pass it to [`Quote::new`]. The domain tag is not a field: it is always
/// [`QUOTE_DOMAIN`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuoteTerms {
    /// The quotebook program the signature is valid for.
    pub program_id: Address,
    /// The deployment the signature is valid for: the first 16 bytes of the cluster's genesis
    /// hash, as stored in `Config` and every `Series`.
    pub deployment: [u8; 16],
    /// The maker's wallet; its ed25519 key signs the quote.
    pub maker: Address,
    /// The series the quote trades.
    pub series: SeriesId,
    /// The only taker allowed to fill, or all zeros for an open quote.
    pub taker: Address,
    /// The maker's side.
    pub side: Side,
    /// Price in ticks; non-zero.
    pub price: QuoteLotsPerBaseLot,
    /// Maximum fill size in base lots; non-zero.
    pub size: BaseLots,
    /// When the maker issued the quote, in Unix seconds.
    pub issued_at: UnixTimestamp,
    /// The quote is fillable while the on-chain clock is below this, in Unix seconds.
    pub expires_at: UnixTimestamp,
    /// The maker's replay nonce.
    pub nonce: Nonce,
}

/// A structurally valid quote: non-zero price and size, encodable to canonical bytes.
///
/// Time, nonce and account checks belong to `settle`, not to this type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quote(QuoteTerms);

impl Quote {
    /// Length of the canonical encoding, in bytes.
    pub const LEN: usize = 208;

    /// The all-zero taker that marks an open quote.
    pub const OPEN_TAKER: Address = Address::new_from_array([0; 32]);

    /// Validates `terms`.
    ///
    /// # Errors
    ///
    /// `MalformedQuote` if the price or the size is zero.
    #[must_use = "the result carries a possible error"]
    pub fn new(terms: QuoteTerms) -> Result<Self, QuotebookError> {
        if terms.price.get() == 0 || terms.size.get() == 0 {
            return Err(QuotebookError::MalformedQuote);
        }
        Ok(Self(terms))
    }

    /// The validated terms.
    #[must_use]
    #[inline]
    pub const fn terms(&self) -> &QuoteTerms {
        &self.0
    }

    /// Whether any taker may fill the quote.
    #[must_use]
    #[inline]
    pub fn is_open(&self) -> bool {
        self.0.taker == Self::OPEN_TAKER
    }

    /// Whether `taker` may fill the quote: it is open or bound to `taker`.
    #[must_use]
    #[inline]
    pub fn allows_taker(&self, taker: &Address) -> bool {
        self.is_open() || self.0.taker == *taker
    }

    /// The canonical bytes a maker signs.
    #[must_use]
    pub fn encode(&self) -> [u8; Self::LEN] {
        let t = &self.0;
        let wire = QuoteWire {
            domain: QUOTE_DOMAIN,
            program_id: t.program_id.to_bytes(),
            deployment: t.deployment,
            maker: t.maker.to_bytes(),
            series: t.series.address().to_bytes(),
            taker: t.taker.to_bytes(),
            side: t.side.as_u8(),
            pad: [0; 7],
            price: t.price.get().to_le_bytes(),
            size: t.size.get().to_le_bytes(),
            issued_at: t.issued_at.get().to_le_bytes(),
            expires_at: t.expires_at.get().to_le_bytes(),
            nonce: t.nonce.get().to_le_bytes(),
        };
        bytemuck::must_cast(wire)
    }

    /// Decodes canonical quote bytes from a slice at any alignment.
    ///
    /// Accepts exactly the outputs of [`Quote::encode`]: `decode(&q.encode()) == Ok(q)` for
    /// every quote, and any other input is rejected.
    ///
    /// # Errors
    ///
    /// - `QuoteLengthMismatch` if `bytes` is not [`Quote::LEN`] long.
    /// - `WrongDomain` if the first 16 bytes are not [`QUOTE_DOMAIN`].
    /// - `MalformedQuote` if the side is not 0 or 1, the padding is not zero, or the price or
    ///   the size is zero.
    #[must_use = "the result carries a possible error"]
    pub fn decode(bytes: &[u8]) -> Result<Self, QuotebookError> {
        let bytes: &[u8; Self::LEN] = bytes
            .try_into()
            .map_err(|_| QuotebookError::QuoteLengthMismatch)?;
        let wire: &QuoteWire = bytemuck::must_cast_ref(bytes);
        if wire.domain != QUOTE_DOMAIN {
            return Err(QuotebookError::WrongDomain);
        }
        if wire.pad != [0; 7] {
            return Err(QuotebookError::MalformedQuote);
        }
        let side = Side::from_u8(wire.side).ok_or(QuotebookError::MalformedQuote)?;
        Self::new(QuoteTerms {
            program_id: Address::new_from_array(wire.program_id),
            deployment: wire.deployment,
            maker: Address::new_from_array(wire.maker),
            series: SeriesId::new(Address::new_from_array(wire.series)),
            taker: Address::new_from_array(wire.taker),
            side,
            price: QuoteLotsPerBaseLot::new(u64::from_le_bytes(wire.price)),
            size: BaseLots::new(u64::from_le_bytes(wire.size)),
            issued_at: UnixTimestamp::new(i64::from_le_bytes(wire.issued_at)),
            expires_at: UnixTimestamp::new(i64::from_le_bytes(wire.expires_at)),
            nonce: Nonce::new(u64::from_le_bytes(wire.nonce)),
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros
)]
mod tests {
    use super::{Quote, QuoteTerms, QuoteWire, QUOTE_DOMAIN};
    use crate::{
        BaseLots, Nonce, QuoteLotsPerBaseLot, QuotebookError, SeriesId, Side, UnixTimestamp,
    };
    use core::mem::offset_of;
    use solana_address::Address;

    fn sample() -> Quote {
        Quote::new(QuoteTerms {
            program_id: Address::new_from_array([1; 32]),
            deployment: [2; 16],
            maker: Address::new_from_array([3; 32]),
            series: SeriesId::new(Address::new_from_array([4; 32])),
            taker: Address::new_from_array([5; 32]),
            side: Side::Ask,
            price: QuoteLotsPerBaseLot::new(0x0102_0304_0506_0708),
            size: BaseLots::new(9),
            issued_at: UnixTimestamp::new(1_700_000_000),
            expires_at: UnixTimestamp::new(1_700_000_060),
            nonce: Nonce::new(42),
        })
        .unwrap()
    }

    #[test]
    fn test_quote_wire_layout_matches_plan() {
        assert_eq!(core::mem::size_of::<QuoteWire>(), Quote::LEN);
        assert_eq!(core::mem::align_of::<QuoteWire>(), 1);
        assert_eq!(offset_of!(QuoteWire, domain), 0);
        assert_eq!(offset_of!(QuoteWire, program_id), 16);
        assert_eq!(offset_of!(QuoteWire, deployment), 48);
        assert_eq!(offset_of!(QuoteWire, maker), 64);
        assert_eq!(offset_of!(QuoteWire, series), 96);
        assert_eq!(offset_of!(QuoteWire, taker), 128);
        assert_eq!(offset_of!(QuoteWire, side), 160);
        assert_eq!(offset_of!(QuoteWire, pad), 161);
        assert_eq!(offset_of!(QuoteWire, price), 168);
        assert_eq!(offset_of!(QuoteWire, size), 176);
        assert_eq!(offset_of!(QuoteWire, issued_at), 184);
        assert_eq!(offset_of!(QuoteWire, expires_at), 192);
        assert_eq!(offset_of!(QuoteWire, nonce), 200);
    }

    #[test]
    fn test_quote_encode_is_little_endian_at_plan_offsets() {
        let b = sample().encode();
        assert_eq!(&b[0..16], &QUOTE_DOMAIN);
        assert_eq!(&b[16..48], &[1; 32]);
        assert_eq!(&b[48..64], &[2; 16]);
        assert_eq!(b[160], 1);
        assert_eq!(&b[161..168], &[0; 7]);
        assert_eq!(&b[168..176], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&b[200..208], &[42, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn test_quote_decode_wrong_length_is_rejected() {
        let b = sample().encode();
        assert_eq!(
            Quote::decode(&b[..Quote::LEN - 1]),
            Err(QuotebookError::QuoteLengthMismatch)
        );
        let mut long = [0u8; Quote::LEN + 1];
        long[..Quote::LEN].copy_from_slice(&b);
        assert_eq!(
            Quote::decode(&long),
            Err(QuotebookError::QuoteLengthMismatch)
        );
        assert_eq!(Quote::decode(&[]), Err(QuotebookError::QuoteLengthMismatch));
    }

    #[test]
    fn test_quote_decode_wrong_domain_is_rejected() {
        let mut b = sample().encode();
        b[11] = b'2';
        assert_eq!(Quote::decode(&b), Err(QuotebookError::WrongDomain));
    }

    #[test]
    fn test_quote_decode_nonzero_padding_is_rejected() {
        for i in 161..168 {
            let mut b = sample().encode();
            b[i] = 1;
            assert_eq!(Quote::decode(&b), Err(QuotebookError::MalformedQuote));
        }
    }

    #[test]
    fn test_quote_zero_price_or_size_is_rejected() {
        let mut t = *sample().terms();
        t.price = QuoteLotsPerBaseLot::new(0);
        assert_eq!(Quote::new(t), Err(QuotebookError::MalformedQuote));
        let mut t = *sample().terms();
        t.size = BaseLots::new(0);
        assert_eq!(Quote::new(t), Err(QuotebookError::MalformedQuote));

        let mut b = sample().encode();
        b[168..176].fill(0);
        assert_eq!(Quote::decode(&b), Err(QuotebookError::MalformedQuote));
        let mut b = sample().encode();
        b[176..184].fill(0);
        assert_eq!(Quote::decode(&b), Err(QuotebookError::MalformedQuote));
    }

    #[test]
    fn test_quote_taker_binding() {
        let q = sample();
        assert!(!q.is_open());
        assert!(q.allows_taker(&Address::new_from_array([5; 32])));
        assert!(!q.allows_taker(&Address::new_from_array([6; 32])));

        let mut t = *q.terms();
        t.taker = Quote::OPEN_TAKER;
        let open = Quote::new(t).unwrap();
        assert!(open.is_open());
        assert!(open.allows_taker(&Address::new_from_array([6; 32])));
    }
}
