//! PDA derivation and creation.
//!
//! Creation always derives the canonical bump with `find_program_address`, so a client cannot
//! create a second account for the same seeds with another bump; later instructions re-derive
//! the address from the stored bump with `create_program_address`.

use pinocchio::cpi::{Seed, Signer};
use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};
use quotebook_core::QuotebookError;

use crate::error::custom;

/// The canonical PDA and bump for `seeds`.
///
/// # Errors
///
/// `InvalidSeeds` if no bump yields an off-curve address (statistically impossible).
#[must_use = "the result carries a possible error"]
#[inline]
pub fn find(seeds: &[&[u8]], program_id: &Address) -> Result<(Address, u8), ProgramError> {
    Address::try_find_program_address(seeds, program_id).ok_or(ProgramError::InvalidSeeds)
}

/// Checks that `seeds` (bump included) derive exactly `expected`.
///
/// # Errors
///
/// `InvalidPda` if the derivation fails or gives another address.
#[must_use = "the result carries a possible error"]
#[inline]
pub fn verify(seeds: &[&[u8]], program_id: &Address, expected: &Address) -> ProgramResult {
    match Address::create_program_address(seeds, program_id) {
        Ok(address) if address == *expected => Ok(()),
        _ => Err(custom(QuotebookError::InvalidPda)),
    }
}

/// Creates the program-owned PDA `account` with `space` zeroed bytes, rent exempt, funded by
/// `payer`. `seeds` include the bump.
///
/// Anyone can send lamports to a PDA address before it exists, which makes a plain
/// `CreateAccount` fail. `create_account_with_minimum_balance_signed` handles that case by
/// topping the balance up to rent exemption, then `Allocate` + `Assign`.
///
/// # Errors
///
/// `AccountAlreadyInitialized` if the account is not an empty system-owned account; any
/// error of the system program CPIs.
#[must_use = "the result carries a possible error"]
pub fn create(
    account: &mut AccountView,
    space: usize,
    program_id: &Address,
    payer: &AccountView,
    seeds: &[Seed],
) -> ProgramResult {
    if !account.owned_by(&pinocchio_system::ID) || !account.is_data_empty() {
        return Err(ProgramError::AccountAlreadyInitialized);
    }
    let signer = Signer::from(seeds);
    pinocchio_system::create_account_with_minimum_balance_signed(
        account,
        space,
        program_id,
        payer,
        None,
        &[signer],
    )
}
