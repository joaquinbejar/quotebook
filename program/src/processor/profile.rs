//! The maker registry (ADR 0002, docs/adr/0002-maker-registry.md): `set_profile` and
//! `close_profile`. `settle` never reads a profile.
//!
//! `set_profile` accounts:
//! 0. `[signer]` maker
//! 1. `[signer, writable]` payer
//! 2. `[writable]` profile: PDA `["profile", maker]`
//! 3. `[]` system program
//!
//! Args: [`ProfileArgs`]. Creates the profile if it does not exist, otherwise overwrites it.
//!
//! `close_profile` accounts:
//! 0. `[signer]` maker
//! 1. `[writable]` profile
//! 2. `[writable]` lamport destination
//!
//! No args. Safe to close: the profile holds no replay state.

use pinocchio::cpi::Seed;
use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};
use quotebook_core::profile::{check_canonical, ProfileArgs};
use quotebook_core::QuotebookError;

use super::{signer, system_program, writable};
use crate::error::custom;
use crate::pda;
use crate::state::{self, MakerProfile, ProgramAccount, PROFILE_SEED};

/// For an existing profile: owner, length, tag and version (the loader), the PDA with the
/// stored bump, and the stored maker equal to `maker`.
fn check_existing(
    profile: &mut AccountView,
    maker: &Address,
    program_id: &Address,
) -> ProgramResult {
    let address = *profile.address();
    let p = *state::load::<MakerProfile>(profile, program_id)?;
    pda::verify(
        &[PROFILE_SEED, maker.as_ref(), &[p.hdr.bump]],
        program_id,
        &address,
    )?;
    if p.maker != *maker {
        return Err(custom(QuotebookError::NotAuthority));
    }
    Ok(())
}

/// Runs `set_profile`.
///
/// # Errors
///
/// `NotEnoughAccountKeys`; the argument errors (`InvalidInstructionData`, `InvalidProfile`);
/// `MissingSignature`, `Immutable`; for a new profile, `InvalidPda` and the creation errors;
/// for an existing one, the loader errors, `InvalidPda` and `NotAuthority`.
#[inline(never)]
pub fn process_set_profile(
    program_id: &Address,
    accounts: &mut [AccountView],
    args: &[u8],
) -> ProgramResult {
    let [maker, payer, profile, system, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let args = ProfileArgs::decode(args).map_err(custom)?;
    signer(maker)?;
    signer(payer)?;
    writable(payer)?;
    writable(profile)?;
    let maker_key = *maker.address();
    if profile.owned_by(program_id) {
        check_existing(profile, &maker_key, program_id)?;
    } else {
        system_program(system)?;
        let (address, bump) = pda::find(&[PROFILE_SEED, maker_key.as_ref()], program_id)?;
        if *profile.address() != address {
            return Err(custom(QuotebookError::InvalidPda));
        }
        let bump_seed = [bump];
        pda::create(
            profile,
            MakerProfile::LEN,
            program_id,
            payer,
            &[
                Seed::from(PROFILE_SEED),
                Seed::from(maker_key.as_ref()),
                Seed::from(&bump_seed),
            ],
        )?;
        state::init(profile, |p: &mut MakerProfile| {
            p.hdr = state::new_hdr::<MakerProfile>(bump);
            p.maker = maker_key;
        })?;
    }
    let mut p = state::load_mut::<MakerProfile>(profile, program_id)?;
    args.write_to(&mut p);
    // The program never stores a non-canonical profile.
    check_canonical(&p).map_err(custom)
}

/// Runs `close_profile`.
///
/// # Errors
///
/// `NotEnoughAccountKeys`; `MissingSignature`, `Immutable`; the loader errors, `InvalidPda` and `NotAuthority`;
/// `Overflow` for the destination's lamports.
#[inline(never)]
pub fn process_close_profile(
    program_id: &Address,
    accounts: &mut [AccountView],
    _args: &[u8],
) -> ProgramResult {
    let [maker, profile, destination, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    signer(maker)?;
    writable(profile)?;
    writable(destination)?;
    if profile.address() == destination.address() {
        return Err(ProgramError::InvalidArgument);
    }
    let maker_key = *maker.address();
    check_existing(profile, &maker_key, program_id)?;
    let lamports = profile.lamports();
    let total = destination
        .lamports()
        .checked_add(lamports)
        .ok_or(custom(QuotebookError::Overflow))?;
    destination.set_lamports(total);
    profile.set_lamports(0);
    profile.close()
}
