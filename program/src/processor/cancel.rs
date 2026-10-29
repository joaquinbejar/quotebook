//! `cancel_all` and `cancel_nonce`: the maker withdraws outstanding quotes.
//!
//! Accounts (both):
//! 0. `[signer]` maker
//! 1. `[writable]` the maker's state

use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};
use quotebook_core::{Nonce, NonceWindow, QuotebookError};

use super::signer;
use crate::error::custom;
use crate::instruction::decode_u64;
use crate::pda;
use crate::state::{self, MakerState, MAKER_SEED};

/// Loads the signer's `MakerState` and applies `f` to its nonce window.
fn with_window(
    program_id: &Address,
    accounts: &mut [AccountView],
    f: impl FnOnce(&mut NonceWindow) -> Result<(), QuotebookError>,
) -> ProgramResult {
    let [maker, maker_state, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    signer(maker)?;
    let state_address = *maker_state.address();
    let mut state = state::load_mut::<MakerState>(maker_state, program_id)?;
    if state.maker != *maker.address() {
        return Err(custom(QuotebookError::NotAuthority));
    }
    pda::verify(
        &[MAKER_SEED, maker.address().as_ref(), &[state.hdr.bump]],
        program_id,
        &state_address,
    )?;
    f(&mut state.nonces).map_err(custom)
}

/// Runs `cancel_all`: every nonce below `new_min` stops being fillable.
///
/// # Errors
///
/// `MissingSignature`; `Immutable`, `InvalidAccountOwner`, `InvalidAccountData`, `InvalidPda`
/// for a forged state; `NotAuthority` for another maker's state; `InvalidInstructionData`;
/// `WatermarkNotIncreasing` unless `new_min` is above the current watermark.
#[inline(never)]
pub fn process_cancel_all(
    program_id: &Address,
    accounts: &mut [AccountView],
    args: &[u8],
) -> ProgramResult {
    let new_min = Nonce::new(decode_u64(args)?);
    with_window(program_id, accounts, |w| w.cancel_all(new_min))
}

/// Runs `cancel_nonce`: marks one nonce used so its quote can no longer be filled.
///
/// # Errors
///
/// As [`process_cancel_all`], with the nonce errors of `NonceWindow::consume` in place of
/// `WatermarkNotIncreasing`.
#[inline(never)]
pub fn process_cancel_nonce(
    program_id: &Address,
    accounts: &mut [AccountView],
    args: &[u8],
) -> ProgramResult {
    let nonce = Nonce::new(decode_u64(args)?);
    with_window(program_id, accounts, |w| w.consume(nonce))
}
