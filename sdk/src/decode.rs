//! Program accounts from RPC account data.

use quotebook_core::accounts::{self, Config, MakerState, ProgramAccount, Series};
use solana_address::Address;

/// Why an account could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The account is not owned by the quotebook program.
    #[error("account is not owned by the quotebook program")]
    WrongOwner,
    /// The data has the wrong length, tag or version for the expected account type.
    #[error("account data is not a {0}")]
    InvalidData(&'static str),
}

fn decode<T: ProgramAccount>(
    program_id: &Address,
    owner: &Address,
    data: &[u8],
    name: &'static str,
) -> Result<T, DecodeError> {
    if owner != program_id {
        return Err(DecodeError::WrongOwner);
    }
    accounts::decode::<T>(data).ok_or(DecodeError::InvalidData(name))
}

/// Decodes a `Config` account.
///
/// # Errors
///
/// `WrongOwner` or `InvalidData`.
pub fn config(program_id: &Address, owner: &Address, data: &[u8]) -> Result<Config, DecodeError> {
    decode(program_id, owner, data, "Config")
}

/// Decodes a `Series` account.
///
/// # Errors
///
/// `WrongOwner` or `InvalidData`.
pub fn series(program_id: &Address, owner: &Address, data: &[u8]) -> Result<Series, DecodeError> {
    decode(program_id, owner, data, "Series")
}

/// Decodes a `MakerState` account.
///
/// # Errors
///
/// `WrongOwner` or `InvalidData`.
pub fn maker_state(
    program_id: &Address,
    owner: &Address,
    data: &[u8],
) -> Result<MakerState, DecodeError> {
    decode(program_id, owner, data, "MakerState")
}
