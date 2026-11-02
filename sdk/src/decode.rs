//! Program accounts from RPC account data.

use quotebook_core::accounts::{self, Config, MakerProfile, MakerState, ProgramAccount, Series};
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

/// Decodes a `MakerProfile` account (ADR 0002) and checks that it is canonical, and that it
/// sits at the maker's own PDA. Its endpoint and name are maker-provided text: treat them as
/// untrusted, and bind any quote obtained through the endpoint to `profile.maker`.
///
/// # Errors
///
/// `WrongOwner` or `InvalidData`, also for a non-canonical profile or a profile whose address
/// is not `["profile", maker]`.
pub fn maker_profile(
    program_id: &Address,
    address: &Address,
    owner: &Address,
    data: &[u8],
) -> Result<MakerProfile, DecodeError> {
    let p: MakerProfile = decode(program_id, owner, data, "MakerProfile")?;
    quotebook_core::profile::check_canonical(&p)
        .map_err(|_| DecodeError::InvalidData("MakerProfile"))?;
    if crate::pda::profile(program_id, &p.maker).0 != *address {
        return Err(DecodeError::InvalidData("MakerProfile"));
    }
    Ok(p)
}

/// The context-free fill-event decoder, also at [`crate::events::fill_event_payload`]: it says
/// nothing about who logged the bytes.
pub use crate::events::fill_event_payload;
