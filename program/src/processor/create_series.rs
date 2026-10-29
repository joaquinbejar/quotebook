//! `create_series`: create a tradable pair.
//!
//! Accounts:
//! 0. `[signer]` admin: `Config.admin`
//! 1. `[signer, writable]` payer
//! 2. `[writable]` config
//! 3. `[writable]` series PDA `["series", base_mint, quote_mint, series_count_le]`
//! 4. `[]` base mint
//! 5. `[]` quote mint
//! 6. `[]` system program

use pinocchio::cpi::Seed;
use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};
use quotebook_core::{BaseAtoms, BaseLots, QuoteAtoms, QuotebookError, Seconds};

use super::{signer, system_program, writable};
use crate::error::custom;
use crate::instruction::CreateSeriesArgs;
use crate::state::{self, Config, ProgramAccount, Series, CONFIG_SEED, SERIES_SEED};
use crate::{mint, pda};

/// Runs `create_series`.
///
/// # Errors
///
/// The signer, writable and system checks; `InvalidAccountOwner` / `InvalidAccountData` /
/// `InvalidPda` for a forged config; `NotAuthority` if `admin` is not `Config.admin`;
/// `InvalidMint` for equal mints or a mint that is not an initialized token mint;
/// `MintExtensionUnsupported` (see [`mint::validate`]); `InvalidPda` for a series account at
/// another address; `AccountAlreadyInitialized`; `InvalidInstructionData` for bad arguments;
/// `Overflow` if the series count is exhausted.
#[inline(never)]
pub fn process(program_id: &Address, accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let [admin, payer, config, series, base_mint, quote_mint, system, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let args = CreateSeriesArgs::decode(args).map_err(custom)?;
    signer(admin)?;
    signer(payer)?;
    writable(payer)?;
    writable(series)?;
    system_program(system)?;

    let config_address = *config.address();
    let mut cfg = state::load_mut::<Config>(config, program_id)?;
    pda::verify(&[CONFIG_SEED, &[cfg.hdr.bump]], program_id, &config_address)?;
    if cfg.admin != *admin.address() {
        return Err(custom(QuotebookError::NotAuthority));
    }

    if base_mint.address() == quote_mint.address() {
        return Err(custom(QuotebookError::InvalidMint));
    }
    let base_is_2022 = mint::validate(base_mint)?;
    let quote_is_2022 = mint::validate(quote_mint)?;

    let index = cfg.series_count;
    let index_seed = index.to_le_bytes();
    let (address, bump) = pda::find(
        &[
            SERIES_SEED,
            base_mint.address().as_ref(),
            quote_mint.address().as_ref(),
            &index_seed,
        ],
        program_id,
    )?;
    if *series.address() != address {
        return Err(custom(QuotebookError::InvalidPda));
    }
    let bump_seed = [bump];
    pda::create(
        series,
        Series::LEN,
        program_id,
        payer,
        &[
            Seed::from(SERIES_SEED),
            Seed::from(base_mint.address().as_ref()),
            Seed::from(quote_mint.address().as_ref()),
            Seed::from(&index_seed),
            Seed::from(&bump_seed),
        ],
    )?;
    let deployment_id = cfg.deployment_id;
    state::init(series, |s: &mut Series| {
        s.hdr = state::new_hdr::<Series>(bump);
        s.series_index = index;
        s.deployment_id = deployment_id;
        s.base_mint = *base_mint.address();
        s.quote_mint = *quote_mint.address();
        s.base_lot_size = BaseAtoms::new(args.base_lot_size);
        s.quote_lot_size = QuoteAtoms::new(args.quote_lot_size);
        s.max_quote_ttl = Seconds::new(args.max_quote_ttl);
        s.min_size = BaseLots::new(args.min_size);
        s.paused = 0;
        s.base_is_2022 = u8::from(base_is_2022);
        s.quote_is_2022 = u8::from(quote_is_2022);
    })?;
    cfg.series_count = index
        .checked_add(1)
        .ok_or(custom(QuotebookError::Overflow))?;
    Ok(())
}
