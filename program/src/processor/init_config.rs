//! `init_config`: create the program's `Config`.
//!
//! Accounts:
//! 0. `[signer]` admin: must be the program's upgrade authority
//! 1. `[signer, writable]` payer
//! 2. `[writable]` config PDA `["config"]`
//! 3. `[]` the program's `ProgramData` (upgradeable loader)
//! 4. `[]` system program

use pinocchio::cpi::Seed;
use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};
use quotebook_core::QuotebookError;

use super::{signer, system_program, writable};
use crate::error::custom;
use crate::instruction::decode_deployment_id;
use crate::state::{self, Config, ProgramAccount, CONFIG_SEED};
use crate::{loader, pda};

/// Runs `init_config`.
///
/// # Errors
///
/// `MissingSignature`, `Immutable`, `IncorrectProgramId` for the signer, writable and system
/// checks; the errors of [`loader::check_upgrade_authority`]; `InvalidPda` for a config
/// account at another address; `AccountAlreadyInitialized` if it exists;
/// `InvalidInstructionData` for a missing or all-zero `deployment_id`.
#[inline(never)]
pub fn process(program_id: &Address, accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let [admin, payer, config, program_data, system, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let deployment_id = decode_deployment_id(args).map_err(custom)?;
    signer(admin)?;
    signer(payer)?;
    writable(payer)?;
    writable(config)?;
    system_program(system)?;
    loader::check_upgrade_authority(program_id, program_data, admin.address())?;

    let (address, bump) = pda::find(&[CONFIG_SEED], program_id)?;
    if *config.address() != address {
        return Err(custom(QuotebookError::InvalidPda));
    }
    let bump_seed = [bump];
    pda::create(
        config,
        Config::LEN,
        program_id,
        payer,
        &[Seed::from(CONFIG_SEED), Seed::from(&bump_seed)],
    )?;
    state::init(config, |c: &mut Config| {
        c.hdr = state::new_hdr::<Config>(bump);
        c.admin = *admin.address();
        c.series_count = 0;
        c.deployment_id = deployment_id;
    })
}
