//! The `init_config` gate: only the program's upgrade authority may initialize it.
//!
//! Assumes the program is deployed with the upgradeable BPF loader (`BPFLoaderUpgradeab1e`),
//! whose `ProgramData` account at PDA `[program_id]` records the upgrade authority. A move to
//! loader-v4 changes where the authority lives and needs a new gate (docs/PLAN.md, 3.3).

use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};
use quotebook_core::QuotebookError;

use crate::error::custom;
use crate::pda;

/// The upgradeable BPF loader.
pub const BPF_LOADER_UPGRADEABLE: Address =
    solana_address::address!("BPFLoaderUpgradeab1e11111111111111111111111");

/// `UpgradeableLoaderState::ProgramData` discriminant (bincode `u32`).
const PROGRAM_DATA_TAG: u32 = 3;

/// `tag: u32`, `slot: u64`, `Option<Address>` (`u8` + 32 bytes).
const METADATA_LEN: usize = 45;

/// Checks that `program_data` is the program's `ProgramData` account and that `authority`
/// is its current upgrade authority.
///
/// # Errors
///
/// - `InvalidAccountOwner` if the loader does not own `program_data`.
/// - `InvalidPda` if it is not the program's `ProgramData` address.
/// - `NotAuthority` if the program has no upgrade authority or another one.
/// - `InvalidAccountData` if the metadata is malformed.
#[must_use = "the result carries a possible error"]
pub fn check_upgrade_authority(
    program_id: &Address,
    program_data: &AccountView,
    authority: &Address,
) -> ProgramResult {
    if !program_data.owned_by(&BPF_LOADER_UPGRADEABLE) {
        return Err(custom(QuotebookError::InvalidAccountOwner));
    }
    let (expected, _) = pda::find(&[program_id.as_ref()], &BPF_LOADER_UPGRADEABLE)?;
    if *program_data.address() != expected {
        return Err(custom(QuotebookError::InvalidPda));
    }
    let data = program_data.try_borrow()?;
    let (meta, _) = data
        .split_first_chunk::<METADATA_LEN>()
        .ok_or(ProgramError::InvalidAccountData)?;
    let (tag, rest) = meta
        .split_first_chunk::<4>()
        .ok_or(ProgramError::InvalidAccountData)?;
    if u32::from_le_bytes(*tag) != PROGRAM_DATA_TAG {
        return Err(ProgramError::InvalidAccountData);
    }
    // Skip the deployment slot.
    let (_, option) = rest
        .split_first_chunk::<8>()
        .ok_or(ProgramError::InvalidAccountData)?;
    match option.split_first() {
        Some((&1, key)) if key == authority.as_ref() => Ok(()),
        Some((&(0 | 1), _)) => Err(custom(QuotebookError::NotAuthority)),
        _ => Err(ProgramError::InvalidAccountData),
    }
}
