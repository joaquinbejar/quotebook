//! Loading program accounts (docs/PLAN.md, section 3.1).
//!
//! The layouts live in [`quotebook_core::accounts`] and are re-exported here. The only way
//! from account data to a typed account is [`load`] / [`load_mut`], which check the owner, the
//! exact length, the tag and the version before the cast.

use pinocchio::account::{Ref, RefMut};
use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};
use quotebook_core::QuotebookError;

use crate::error::custom;

pub use quotebook_core::accounts::{
    new_hdr, AccountTag, Config, Hdr, MakerState, ProgramAccount, Series, CONFIG_SEED,
    DELEGATE_SEED, MAKER_SEED, SERIES_SEED, VERSION,
};

/// Checks owner, length, tag and version of an existing account of type `T`.
fn check<T: ProgramAccount>(
    account: &AccountView,
    program_id: &Address,
) -> Result<(), ProgramError> {
    if !account.owned_by(program_id) {
        return Err(custom(QuotebookError::InvalidAccountOwner));
    }
    if account.data_len() != T::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

fn cast<T: ProgramAccount>(data: &[u8]) -> Result<&T, ProgramError> {
    let value: &T = bytemuck::try_from_bytes(data).map_err(|_| ProgramError::InvalidAccountData)?;
    let hdr = value.hdr();
    if hdr.tag != T::TAG as u8 || hdr.version != VERSION {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(value)
}

/// Borrows an existing program account as `T`.
///
/// # Errors
///
/// `InvalidAccountOwner` if quotebook does not own it; `InvalidAccountData` for a wrong length,
/// tag, version or alignment; a borrow error if it is already borrowed mutably.
#[must_use = "the result carries a possible error"]
pub fn load<'a, T: ProgramAccount>(
    account: &'a AccountView,
    program_id: &Address,
) -> Result<Ref<'a, T>, ProgramError> {
    check::<T>(account, program_id)?;
    let data = account.try_borrow()?;
    Ref::try_map(data, |d| cast::<T>(d)).map_err(|(_, e)| e)
}

/// Borrows an existing program account as `T`, mutably.
///
/// # Errors
///
/// As [`load`], plus `Immutable` if the account is not writable.
#[must_use = "the result carries a possible error"]
pub fn load_mut<'a, T: ProgramAccount>(
    account: &'a mut AccountView,
    program_id: &Address,
) -> Result<RefMut<'a, T>, ProgramError> {
    if !account.is_writable() {
        return Err(ProgramError::Immutable);
    }
    check::<T>(account, program_id)?;
    let data = account.try_borrow_mut()?;
    RefMut::try_map(data, |d| {
        cast::<T>(d)?;
        bytemuck::try_from_bytes_mut(d).map_err(|_| ProgramError::InvalidAccountData)
    })
    .map_err(|(_, e)| e)
}

/// Fills a freshly created, zeroed account of `T::LEN` bytes in place with `f` (in place, so a
/// 2 KiB `MakerState` never sits on the 4 KiB SBF stack).
///
/// # Errors
///
/// `InvalidAccountData` if the length or alignment is wrong; a borrow error.
#[must_use = "the result carries a possible error"]
pub fn init<T: ProgramAccount>(account: &mut AccountView, f: impl FnOnce(&mut T)) -> ProgramResult {
    let mut data = account.try_borrow_mut()?;
    let value: &mut T =
        bytemuck::try_from_bytes_mut(&mut data).map_err(|_| ProgramError::InvalidAccountData)?;
    f(value);
    Ok(())
}
