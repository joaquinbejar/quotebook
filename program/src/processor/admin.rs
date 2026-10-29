//! Admin instructions: `set_config` and `set_series_paused`.
//!
//! `set_config` accounts:
//! 0. `[signer]` admin: `Config.admin`
//! 1. `[writable]` config
//!
//! Args: `new_admin: [u8; 32]`. Only the admin changes; `deployment_id` never does.
//!
//! `set_series_paused` accounts:
//! 0. `[signer]` admin: `Config.admin`
//! 1. `[]` config
//! 2. `[writable]` series
//!
//! Args: `paused: u8`, 0 or 1.

use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};
use quotebook_core::QuotebookError;

use super::signer;
use crate::error::custom;
use crate::pda;
use crate::state::{self, Config, Series, CONFIG_SEED, SERIES_SEED};

/// Checks that `admin` signed and is the admin of the genuine config PDA.
fn check_admin(
    admin: &AccountView,
    config: &Config,
    config_address: &Address,
    program_id: &Address,
) -> ProgramResult {
    signer(admin)?;
    pda::verify(
        &[CONFIG_SEED, &[config.hdr.bump]],
        program_id,
        config_address,
    )?;
    if config.admin != *admin.address() {
        return Err(custom(QuotebookError::NotAuthority));
    }
    Ok(())
}

/// Runs `set_config`.
///
/// # Errors
///
/// `MissingSignature`; the config loader's errors (`Immutable` included), `InvalidPda`;
/// `NotAuthority`; `InvalidInstructionData` for a missing or all-zero new admin.
#[inline(never)]
pub fn process_set_config(
    program_id: &Address,
    accounts: &mut [AccountView],
    args: &[u8],
) -> ProgramResult {
    let [admin, config, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let new_admin: [u8; 32] = args
        .try_into()
        .map_err(|_| custom(QuotebookError::InvalidInstructionData))?;
    if new_admin == [0; 32] {
        return Err(custom(QuotebookError::InvalidInstructionData));
    }
    let config_address = *config.address();
    let mut c = state::load_mut::<Config>(config, program_id)?;
    check_admin(admin, &c, &config_address, program_id)?;
    c.admin = Address::new_from_array(new_admin);
    Ok(())
}

/// Runs `set_series_paused`.
///
/// # Errors
///
/// `MissingSignature`; the config and series loaders' errors, `InvalidPda`; `NotAuthority`;
/// `InvalidInstructionData` unless `paused` is 0 or 1.
#[inline(never)]
pub fn process_set_series_paused(
    program_id: &Address,
    accounts: &mut [AccountView],
    args: &[u8],
) -> ProgramResult {
    let [admin, config, series, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let paused = match args {
        [p @ (0 | 1)] => *p,
        _ => return Err(custom(QuotebookError::InvalidInstructionData)),
    };
    let c = state::load::<Config>(config, program_id)?;
    check_admin(admin, &c, config.address(), program_id)?;
    let series_address = *series.address();
    let mut s = state::load_mut::<Series>(series, program_id)?;
    pda::verify(
        &[
            SERIES_SEED,
            s.base_mint.as_ref(),
            s.quote_mint.as_ref(),
            &s.series_index.to_le_bytes(),
            &[s.hdr.bump],
        ],
        program_id,
        &series_address,
    )?;
    s.paused = paused;
    Ok(())
}
