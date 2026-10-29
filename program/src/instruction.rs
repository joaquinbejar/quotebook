//! Instruction tags and argument decoding (docs/PLAN.md, section 3.2).
//!
//! Instruction data is a one-byte [`Tag`] followed by the instruction's fixed-size arguments,
//! little-endian. Each handler documents its accounts.

use pinocchio::error::ProgramError;
use quotebook_core::QuotebookError;

use crate::error::custom;

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
    /// Fill a signed quote. Args: `sig_ix_index: u8`, `fill_size: u64`.
    Settle = 6,
    /// Hand the admin role to another key. Args: `new_admin: [u8; 32]`.
    SetConfig = 7,
}

impl Tag {
    /// Splits instruction data into its tag and arguments.
    ///
    /// # Errors
    ///
    /// `InvalidInstruction` for empty data or an unassigned tag.
    #[must_use = "the result carries a possible error"]
    pub fn split(data: &[u8]) -> Result<(Self, &[u8]), ProgramError> {
        let (tag, args) = data
            .split_first()
            .ok_or(custom(QuotebookError::InvalidInstruction))?;
        let tag = match tag {
            0 => Self::InitConfig,
            1 => Self::CreateSeries,
            2 => Self::SetSeriesPaused,
            3 => Self::InitMaker,
            4 => Self::CancelAll,
            5 => Self::CancelNonce,
            6 => Self::Settle,
            7 => Self::SetConfig,
            _ => return Err(custom(QuotebookError::InvalidInstruction)),
        };
        Ok((tag, args))
    }
}

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
    /// Decodes and validates the arguments.
    ///
    /// # Errors
    ///
    /// `InvalidInstructionData` for a wrong length, a zero lot size, a non-positive TTL or a
    /// zero minimum size.
    #[must_use = "the result carries a possible error"]
    pub fn decode(args: &[u8]) -> Result<Self, ProgramError> {
        let bad = || custom(QuotebookError::InvalidInstructionData);
        let args: &[u8; 32] = args.try_into().map_err(|_| bad())?;
        let (a, rest) = args.split_first_chunk::<8>().ok_or_else(bad)?;
        let (b, rest) = rest.split_first_chunk::<8>().ok_or_else(bad)?;
        let (c, d) = rest.split_first_chunk::<8>().ok_or_else(bad)?;
        let d: &[u8; 8] = d.try_into().map_err(|_| bad())?;
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
            return Err(bad());
        }
        Ok(out)
    }
}

/// Decodes a single little-endian `u64` argument.
///
/// # Errors
///
/// `InvalidInstructionData` unless `args` is exactly 8 bytes.
#[must_use = "the result carries a possible error"]
pub fn decode_u64(args: &[u8]) -> Result<u64, ProgramError> {
    args.try_into()
        .map(u64::from_le_bytes)
        .map_err(|_| custom(QuotebookError::InvalidInstructionData))
}

/// Decodes `init_config`'s `deployment_id`.
///
/// # Errors
///
/// `InvalidInstructionData` unless `args` is exactly 16 bytes and not all zero.
#[must_use = "the result carries a possible error"]
pub fn decode_deployment_id(args: &[u8]) -> Result<[u8; 16], ProgramError> {
    let id: [u8; 16] = args
        .try_into()
        .map_err(|_| custom(QuotebookError::InvalidInstructionData))?;
    if id == [0; 16] {
        return Err(custom(QuotebookError::InvalidInstructionData));
    }
    Ok(id)
}
