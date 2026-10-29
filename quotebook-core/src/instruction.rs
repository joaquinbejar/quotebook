//! Instruction tags and argument encodings (docs/PLAN.md, section 3.2), shared by the program
//! and its clients.
//!
//! Instruction data is a one-byte [`Tag`] followed by the instruction's fixed-size arguments,
//! little-endian. The program's handler modules document each instruction's accounts.

use solana_address::Address;

use crate::{BaseLots, QuotebookError};

/// Instruction tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Tag {
    /// Create `Config`. Args: `deployment_id: [u8; 16]`.
    InitConfig = 0,
    /// Create a `Series`. Args: [`CreateSeriesArgs`].
    CreateSeries = 1,
    /// Pause or resume a series. Args: `paused: u8`.
    SetSeriesPaused = 2,
    /// Create the signer's `MakerState`. No args.
    InitMaker = 3,
    /// Raise the maker's nonce watermark. Args: `new_min: u64`.
    CancelAll = 4,
    /// Mark one nonce used. Args: `nonce: u64`.
    CancelNonce = 5,
    /// Fill a signed quote. Args: [`SettleArgs`].
    Settle = 6,
    /// Hand the admin role to another key. Args: `new_admin: [u8; 32]`.
    SetConfig = 7,
}

impl Tag {
    /// The tag encoded as `value`, or `None` for an unassigned value.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::InitConfig),
            1 => Some(Self::CreateSeries),
            2 => Some(Self::SetSeriesPaused),
            3 => Some(Self::InitMaker),
            4 => Some(Self::CancelAll),
            5 => Some(Self::CancelNonce),
            6 => Some(Self::Settle),
            7 => Some(Self::SetConfig),
            _ => None,
        }
    }

    /// Splits instruction data into its tag and arguments.
    ///
    /// # Errors
    ///
    /// `InvalidInstruction` for empty data or an unassigned tag.
    #[must_use = "the result carries a possible error"]
    pub fn split(data: &[u8]) -> Result<(Self, &[u8]), QuotebookError> {
        let (tag, args) = data
            .split_first()
            .ok_or(QuotebookError::InvalidInstruction)?;
        let tag = Self::from_u8(*tag).ok_or(QuotebookError::InvalidInstruction)?;
        Ok((tag, args))
    }
}

/// `InvalidInstructionData`, the error of every argument decoder.
const BAD: QuotebookError = QuotebookError::InvalidInstructionData;

/// Arguments of `create_series`, 32 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateSeriesArgs {
    /// Base atoms per base lot; non-zero.
    pub base_lot_size: u64,
    /// Quote atoms per quote lot; non-zero.
    pub quote_lot_size: u64,
    /// Longest quote lifetime in seconds; positive.
    pub max_quote_ttl: i64,
    /// Smallest fill in base lots; at least 1.
    pub min_size: u64,
}

impl CreateSeriesArgs {
    /// Encoded length.
    pub const LEN: usize = 32;

    /// The little-endian encoding.
    #[must_use]
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0u8; Self::LEN];
        let (pairs, _) = out.as_chunks_mut::<8>();
        let fields = [
            self.base_lot_size.to_le_bytes(),
            self.quote_lot_size.to_le_bytes(),
            self.max_quote_ttl.to_le_bytes(),
            self.min_size.to_le_bytes(),
        ];
        for (dst, src) in pairs.iter_mut().zip(fields) {
            *dst = src;
        }
        out
    }

    /// Decodes and validates the arguments.
    ///
    /// # Errors
    ///
    /// `InvalidInstructionData` for a wrong length, a zero lot size, a non-positive TTL or a
    /// zero minimum size.
    #[must_use = "the result carries a possible error"]
    pub fn decode(args: &[u8]) -> Result<Self, QuotebookError> {
        let args: &[u8; Self::LEN] = args.try_into().map_err(|_| BAD)?;
        let (words, _) = args.as_chunks::<8>();
        let [a, b, c, d] = words else {
            return Err(BAD);
        };
        let out = Self {
            base_lot_size: u64::from_le_bytes(*a),
            quote_lot_size: u64::from_le_bytes(*b),
            max_quote_ttl: i64::from_le_bytes(*c),
            min_size: u64::from_le_bytes(*d),
        };
        if out.base_lot_size == 0
            || out.quote_lot_size == 0
            || out.max_quote_ttl <= 0
            || out.min_size == 0
        {
            return Err(BAD);
        }
        Ok(out)
    }
}

/// Arguments of `settle`, 9 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleArgs {
    /// Index, in the transaction, of the ed25519 precompile instruction holding the quote; it
    /// must come before `settle`.
    pub sig_ix_index: u8,
    /// Base lots to fill, at most the quote's size.
    pub fill_size: BaseLots,
}

impl SettleArgs {
    /// Encoded length.
    pub const LEN: usize = 9;

    /// The little-endian encoding.
    #[must_use]
    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0u8; Self::LEN];
        if let Some((index, size)) = out.split_first_mut() {
            *index = self.sig_ix_index;
            size.copy_from_slice(&self.fill_size.get().to_le_bytes());
        }
        out
    }

    /// Decodes the arguments.
    ///
    /// # Errors
    ///
    /// `InvalidInstructionData` unless `args` is exactly 9 bytes.
    #[must_use = "the result carries a possible error"]
    pub fn decode(args: &[u8]) -> Result<Self, QuotebookError> {
        let args: &[u8; Self::LEN] = args.try_into().map_err(|_| BAD)?;
        let (index, size) = args.split_first().ok_or(BAD)?;
        let size: [u8; 8] = size.try_into().map_err(|_| BAD)?;
        Ok(Self {
            sig_ix_index: *index,
            fill_size: BaseLots::new(u64::from_le_bytes(size)),
        })
    }
}

/// Decodes a single little-endian `u64` argument (`cancel_all`, `cancel_nonce`).
///
/// # Errors
///
/// `InvalidInstructionData` unless `args` is exactly 8 bytes.
#[must_use = "the result carries a possible error"]
pub fn decode_u64(args: &[u8]) -> Result<u64, QuotebookError> {
    args.try_into().map(u64::from_le_bytes).map_err(|_| BAD)
}

/// Decodes `init_config`'s `deployment_id`.
///
/// # Errors
///
/// `InvalidInstructionData` unless `args` is exactly 16 bytes and not all zero.
#[must_use = "the result carries a possible error"]
pub fn decode_deployment_id(args: &[u8]) -> Result<[u8; 16], QuotebookError> {
    let id: [u8; 16] = args.try_into().map_err(|_| BAD)?;
    if id == [0; 16] {
        return Err(BAD);
    }
    Ok(id)
}

/// Decodes `set_config`'s `new_admin`.
///
/// # Errors
///
/// `InvalidInstructionData` unless `args` is exactly 32 bytes and not all zero.
#[must_use = "the result carries a possible error"]
pub fn decode_new_admin(args: &[u8]) -> Result<Address, QuotebookError> {
    let key: [u8; 32] = args.try_into().map_err(|_| BAD)?;
    if key == [0; 32] {
        return Err(BAD);
    }
    Ok(Address::new_from_array(key))
}

/// Decodes `set_series_paused`'s `paused`.
///
/// # Errors
///
/// `InvalidInstructionData` unless `args` is one byte, 0 or 1.
#[must_use = "the result carries a possible error"]
pub fn decode_paused(args: &[u8]) -> Result<bool, QuotebookError> {
    match args {
        [0] => Ok(false),
        [1] => Ok(true),
        _ => Err(BAD),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_macros)]
mod tests {
    use super::{CreateSeriesArgs, SettleArgs, Tag};
    use crate::{BaseLots, QuotebookError};

    #[test]
    fn test_tag_round_trips_and_rejects_unassigned() {
        for v in 0..=7u8 {
            assert_eq!(Tag::from_u8(v).map(|t| t as u8), Some(v));
        }
        for v in 8..=u8::MAX {
            assert_eq!(Tag::from_u8(v), None);
        }
        assert_eq!(Tag::split(&[]), Err(QuotebookError::InvalidInstruction));
    }

    #[test]
    fn test_args_round_trip() {
        let a = CreateSeriesArgs {
            base_lot_size: 1,
            quote_lot_size: 2,
            max_quote_ttl: 3,
            min_size: 4,
        };
        assert_eq!(CreateSeriesArgs::decode(&a.encode()), Ok(a));
        let s = SettleArgs {
            sig_ix_index: 7,
            fill_size: BaseLots::new(u64::MAX),
        };
        assert_eq!(SettleArgs::decode(&s.encode()), Ok(s));
    }
}
