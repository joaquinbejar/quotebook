//! `init_maker`: create the signer's permanent `MakerState`.
//!
//! Accounts:
//! 0. `[signer]` maker
//! 1. `[signer, writable]` payer
//! 2. `[writable]` maker state PDA `["maker", maker]`
//! 3. `[]` system program

use pinocchio::cpi::Seed;
use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};
use quotebook_core::QuotebookError;

use super::{signer, system_program, writable};
use crate::error::custom;
use crate::pda;
use crate::state::{self, MakerState, ProgramAccount, DELEGATE_SEED, MAKER_SEED};

/// Runs `init_maker`. The nonce window starts empty (the zeroed account is
/// `NonceWindow::new()`), and the canonical bump of the maker's delegate PDA is stored for
/// `settle`.
///
/// # Errors
///
/// The signer, writable and system checks; `InvalidInstructionData` if any argument is given;
/// `InvalidPda` for a maker state at another address; `AccountAlreadyInitialized` if it
/// exists.
#[inline(never)]
pub fn process(program_id: &Address, accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let [maker, payer, maker_state, system, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if !args.is_empty() {
        return Err(custom(QuotebookError::InvalidInstructionData));
    }
    signer(maker)?;
    signer(payer)?;
    writable(payer)?;
    writable(maker_state)?;
    system_program(system)?;

    let (address, bump) = pda::find(&[MAKER_SEED, maker.address().as_ref()], program_id)?;
    if *maker_state.address() != address {
        return Err(custom(QuotebookError::InvalidPda));
    }
    let (_, delegate_bump) = pda::find(&[DELEGATE_SEED, maker.address().as_ref()], program_id)?;
    let bump_seed = [bump];
    pda::create(
        maker_state,
        MakerState::LEN,
        program_id,
        payer,
        &[
            Seed::from(MAKER_SEED),
            Seed::from(maker.address().as_ref()),
            Seed::from(&bump_seed),
        ],
    )?;
    state::init(maker_state, |m: &mut MakerState| {
        m.hdr = state::new_hdr::<MakerState>(bump);
        m.maker = *maker.address();
        m.delegate_bump = delegate_bump;
    })
}
