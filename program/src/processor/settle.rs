//! `settle`: fill a maker-signed quote (docs/PLAN.md, sections 2, 3.3 and 4).
//!
//! Accounts:
//! 0. `[signer]` taker
//! 1. `[]` series
//! 2. `[writable]` the maker's state (`quote.maker`)
//! 3. `[]` the maker's delegate PDA `["delegate", quote.maker]`
//! 4. `[writable]` maker's base token account
//! 5. `[writable]` maker's quote token account
//! 6. `[writable]` taker's base token account
//! 7. `[writable]` taker's quote token account
//! 8. `[]` base mint
//! 9. `[]` quote mint
//! 10. `[]` base mint's token program
//! 11. `[]` quote mint's token program
//! 12. `[]` instructions sysvar
//!
//! Args: `sig_ix_index: u8`, `fill_size: u64` (base lots). The quote is not in the arguments:
//! it is read from the ed25519 precompile instruction at `sig_ix_index`, which must come
//! before this one. The plan's `maker` account is not needed: everything is checked against
//! `quote.maker`.

use pinocchio::account::AccountView;
use pinocchio::cpi::{Seed, Signer};
use pinocchio::error::ProgramError;
use pinocchio::sysvars::clock::Clock;
use pinocchio::sysvars::instructions::{Instructions, INSTRUCTIONS_ID};
use pinocchio::sysvars::Sysvar;
use pinocchio::{Address, ProgramResult};
use pinocchio_token::instructions::TransferChecked;
use quotebook_core::instruction::SettleArgs;
use quotebook_core::{sigverify, BaseLots, Quote, QuotebookError, Side, UnixTimestamp};

use super::signer;
use crate::error::custom;
use crate::mint::{MINT_DECIMALS, TOKEN_2022_PROGRAM, TOKEN_PROGRAM};
use crate::pda;
use crate::state::{self, MakerState, Series, DELEGATE_SEED, MAKER_SEED, SERIES_SEED};
use crate::token::{self, Role};

/// The ed25519 signature-verification precompile.
pub const ED25519_PROGRAM: Address =
    solana_address::address!("Ed25519SigVerify111111111111111111111111111");

/// Reads the quote the precompile verified at `index`, which must precede this instruction.
fn verified_quote(sysvar: &AccountView, index: u8) -> Result<Quote, ProgramError> {
    if *sysvar.address() != INSTRUCTIONS_ID {
        return Err(custom(QuotebookError::InvalidSysvar));
    }
    let instructions = Instructions::try_from(sysvar)?;
    if u16::from(index) >= instructions.load_current_index() {
        return Err(custom(QuotebookError::MissingSigVerify));
    }
    let ix = instructions
        .load_instruction_at(usize::from(index))
        .map_err(|_| custom(QuotebookError::MissingSigVerify))?;
    if *ix.get_program_id() != ED25519_PROGRAM {
        return Err(custom(QuotebookError::MissingSigVerify));
    }
    sigverify::verified_quote(ix.get_instruction_data()).map_err(custom)
}

/// `mint` is the expected one and `program` is its owner and a token program; returns the
/// mint's decimals.
fn check_mint(
    mint: &AccountView,
    expected: &Address,
    program: &AccountView,
) -> Result<u8, ProgramError> {
    if mint.address() != expected {
        return Err(custom(QuotebookError::InvalidMint));
    }
    let program = program.address();
    if (*program != TOKEN_PROGRAM && *program != TOKEN_2022_PROGRAM) || !mint.owned_by(program) {
        return Err(ProgramError::IncorrectProgramId);
    }
    let data = mint.try_borrow()?;
    data.get(MINT_DECIMALS)
        .copied()
        .ok_or(custom(QuotebookError::InvalidMint))
}

/// `fill_size × base_lot_size` and `price × fill_size × quote_lot_size`, exact. No rounding:
/// both are integer products, computed in `u128` and checked into `u64`.
fn amounts(quote: &Quote, fill: BaseLots, series: &Series) -> Result<(u64, u64), ProgramError> {
    let overflow = || custom(QuotebookError::Overflow);
    let base = fill
        .get()
        .checked_mul(series.base_lot_size.get())
        .ok_or_else(overflow)?;
    let quote_atoms = u128::from(quote.terms().price.get())
        .checked_mul(u128::from(fill.get()))
        .and_then(|v| v.checked_mul(u128::from(series.quote_lot_size.get())))
        .and_then(|v| u64::try_from(v).ok())
        .ok_or_else(overflow)?;
    Ok((base, quote_atoms))
}

/// One `transfer_checked` CPI, signed by `seeds` when given.
#[allow(clippy::too_many_arguments)]
fn transfer(
    program: &AccountView,
    from: &AccountView,
    mint: &AccountView,
    to: &AccountView,
    authority: &AccountView,
    amount: u64,
    decimals: u8,
    seeds: Option<&[Seed]>,
) -> ProgramResult {
    let ix: TransferChecked<'_, '_, &AccountView> =
        TransferChecked::new(from, mint, to, authority, amount, decimals);
    match seeds {
        Some(seeds) => ix.invoke_signed_with_program(&[Signer::from(seeds)], program.address()),
        None => ix.invoke_with_program(program.address()),
    }
}

/// Runs `settle`.
///
/// # Errors
///
/// In check order: `NotEnoughAccountKeys`; `InvalidInstructionData`; `MissingSignature`;
/// `InvalidSysvar`, `MissingSigVerify` and the errors of `sigverify::verified_quote`;
/// `WrongProgram`; the series loader's errors, `InvalidPda`, `WrongSeries` (other series or
/// deployment), `SeriesPaused`; `SelfTrade`, `WrongTaker`; `Quote::check_time`'s errors;
/// `FillSizeInvalid`; `Overflow`; the maker-state loader's errors, `NotAuthority`,
/// `InvalidPda`, the nonce errors; `InvalidPda` for the delegate; `InvalidMint`,
/// `IncorrectProgramId`; the token-account errors; the token program's errors.
#[inline(never)]
pub fn process(program_id: &Address, accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let [taker, series, maker_state, delegate, maker_base, maker_quote, taker_base, taker_quote, base_mint, quote_mint, base_program, quote_program, sysvar, ..] =
        accounts
    else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let args = SettleArgs::decode(args).map_err(custom)?;
    signer(taker)?;

    // 1. The quote the precompile verified, signed by `quote.maker`.
    let quote = verified_quote(sysvar, args.sig_ix_index)?;
    let terms = *quote.terms();
    // 2. Bound to this program, series and deployment.
    if terms.program_id != *program_id {
        return Err(custom(QuotebookError::WrongProgram));
    }
    let series_address = *series.address();
    let s = *state::load::<Series>(series, program_id)?;
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
    if terms.series.address() != series_address || terms.deployment != s.deployment_id {
        return Err(custom(QuotebookError::WrongSeries));
    }
    if s.paused != 0 {
        return Err(custom(QuotebookError::SeriesPaused));
    }
    // 3. The taker.
    if *taker.address() == terms.maker {
        return Err(custom(QuotebookError::SelfTrade));
    }
    if !quote.allows_taker(taker.address()) {
        return Err(custom(QuotebookError::WrongTaker));
    }
    // 4. Time window.
    let now = UnixTimestamp::new(Clock::get()?.unix_timestamp);
    quote.check_time(now, s.max_quote_ttl).map_err(custom)?;
    // 6. Fill size, 7. amounts.
    if args.fill_size < s.min_size || args.fill_size > terms.size {
        return Err(custom(QuotebookError::FillSizeInvalid));
    }
    let (base_atoms, quote_atoms) = amounts(&quote, args.fill_size, &s)?;

    // 5. Replay: consume the nonce before any CPI.
    let state_address = *maker_state.address();
    let delegate_bump = {
        let mut m = state::load_mut::<MakerState>(maker_state, program_id)?;
        if m.maker != terms.maker {
            return Err(custom(QuotebookError::NotAuthority));
        }
        pda::verify(
            &[MAKER_SEED, terms.maker.as_ref(), &[m.hdr.bump]],
            program_id,
            &state_address,
        )?;
        m.nonces.consume(terms.nonce).map_err(custom)?;
        m.fills = m
            .fills
            .checked_add(1)
            .ok_or(custom(QuotebookError::Overflow))?;
        m.delegate_bump
    };
    let delegate_bump_seed = [delegate_bump];
    pda::verify(
        &[DELEGATE_SEED, terms.maker.as_ref(), &delegate_bump_seed],
        program_id,
        delegate.address(),
    )?;

    // Mints, token programs, token accounts.
    let base_decimals = check_mint(base_mint, &s.base_mint, base_program)?;
    let quote_decimals = check_mint(quote_mint, &s.quote_mint, quote_program)?;
    let (maker_base_role, maker_quote_role, taker_base_role, taker_quote_role) = match terms.side {
        Side::Bid => (
            Role::Receives,
            Role::PaysAsDelegate,
            Role::PaysAsOwner,
            Role::Receives,
        ),
        Side::Ask => (
            Role::PaysAsDelegate,
            Role::Receives,
            Role::Receives,
            Role::PaysAsOwner,
        ),
    };
    let base_program_id = base_program.address();
    let quote_program_id = quote_program.address();
    token::check_account(
        maker_base,
        base_program_id,
        &s.base_mint,
        &terms.maker,
        maker_base_role,
    )?;
    token::check_account(
        maker_quote,
        quote_program_id,
        &s.quote_mint,
        &terms.maker,
        maker_quote_role,
    )?;
    token::check_account(
        taker_base,
        base_program_id,
        &s.base_mint,
        taker.address(),
        taker_base_role,
    )?;
    token::check_account(
        taker_quote,
        quote_program_id,
        &s.quote_mint,
        taker.address(),
        taker_quote_role,
    )?;

    // The two legs. The maker's leg is signed by its delegate PDA.
    let seeds = [
        Seed::from(DELEGATE_SEED),
        Seed::from(terms.maker.as_ref()),
        Seed::from(&delegate_bump_seed),
    ];
    match terms.side {
        Side::Bid => {
            transfer(
                base_program,
                taker_base,
                base_mint,
                maker_base,
                taker,
                base_atoms,
                base_decimals,
                None,
            )?;
            transfer(
                quote_program,
                maker_quote,
                quote_mint,
                taker_quote,
                delegate,
                quote_atoms,
                quote_decimals,
                Some(&seeds),
            )
        }
        Side::Ask => {
            transfer(
                base_program,
                maker_base,
                base_mint,
                taker_base,
                delegate,
                base_atoms,
                base_decimals,
                Some(&seeds),
            )?;
            transfer(
                quote_program,
                taker_quote,
                quote_mint,
                maker_quote,
                taker,
                quote_atoms,
                quote_decimals,
                None,
            )
        }
    }
}
