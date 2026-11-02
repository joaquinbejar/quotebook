//! `withdraw_fees`: move collected series fees out of a Config-owned fee account (ADR 0001,
//! docs/adr/0001-series-fees.md).
//!
//! Accounts:
//! 0. `[signer]` admin: `Config.admin`
//! 1. `[]` config
//! 2. `[writable]` fee account: a token account of `quote_mint` owned by the Config PDA
//! 3. `[writable]` destination: any initialized token account of `quote_mint`
//! 4. `[]` quote mint
//! 5. `[]` the mint's token program
//!
//! Args: `amount: u64` quote atoms, non-zero and at most the fee account's balance.

use pinocchio::account::AccountView;
use pinocchio::cpi::{Seed, Signer};
use pinocchio::error::ProgramError;
use pinocchio::{Address, ProgramResult};
use pinocchio_token::instructions::TransferChecked;
use quotebook_core::instruction::decode_u64;
use quotebook_core::QuotebookError;

use super::signer;
use crate::error::custom;
use crate::mint::{MINT_DECIMALS, TOKEN_2022_PROGRAM, TOKEN_PROGRAM};
use crate::pda;
use crate::state::{self, Config, CONFIG_SEED};
use crate::token::{self, Role};

/// Offsets of a token account's owner and amount.
const OWNER: core::ops::Range<usize> = 32..64;
const AMOUNT: core::ops::Range<usize> = 64..72;

/// Runs `withdraw_fees`.
///
/// # Errors
///
/// In check order: `NotEnoughAccountKeys`; `InvalidInstructionData` for a missing or zero
/// amount; `MissingSignature`; the config loader's errors, `InvalidPda`, `NotAuthority`;
/// `IncorrectProgramId` or `InvalidMint` for the mint and its program; the token-account
/// errors for the fee account (owner the Config PDA) and the destination; `InsufficientFees`;
/// the token program's errors.
#[inline(never)]
pub fn process(program_id: &Address, accounts: &mut [AccountView], args: &[u8]) -> ProgramResult {
    let [admin, config, fee_account, destination, quote_mint, token_program, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let amount = decode_u64(args).map_err(custom)?;
    if amount == 0 {
        return Err(custom(QuotebookError::InvalidInstructionData));
    }
    signer(admin)?;

    let config_address = *config.address();
    let cfg = *state::load::<Config>(config, program_id)?;
    let bump = [cfg.hdr.bump];
    pda::verify(&[CONFIG_SEED, &bump], program_id, &config_address)?;
    if cfg.admin != *admin.address() {
        return Err(custom(QuotebookError::NotAuthority));
    }

    let program = token_program.address();
    if (*program != TOKEN_PROGRAM && *program != TOKEN_2022_PROGRAM)
        || !quote_mint.owned_by(program)
    {
        return Err(ProgramError::IncorrectProgramId);
    }
    let decimals = {
        let data = quote_mint.try_borrow()?;
        data.get(MINT_DECIMALS)
            .copied()
            .ok_or(custom(QuotebookError::InvalidMint))?
    };
    let mint = *quote_mint.address();
    // The source must be the Config PDA's own account of this mint.
    token::check_account(
        fee_account,
        program,
        &mint,
        &config_address,
        Role::PaysAsDelegate,
    )?;
    // The destination may belong to anyone; only its mint, program and state are checked.
    let destination_owner = {
        let data = destination.try_borrow()?;
        let owner: [u8; 32] = data
            .get(OWNER)
            .and_then(|o| o.try_into().ok())
            .ok_or(custom(QuotebookError::InvalidVaultOrTokenAccount))?;
        Address::new_from_array(owner)
    };
    token::check_account(
        destination,
        program,
        &mint,
        &destination_owner,
        Role::Receives,
    )?;
    let balance = {
        let data = fee_account.try_borrow()?;
        let raw: [u8; 8] = data
            .get(AMOUNT)
            .and_then(|a| a.try_into().ok())
            .ok_or(custom(QuotebookError::InvalidVaultOrTokenAccount))?;
        u64::from_le_bytes(raw)
    };
    if amount > balance {
        return Err(custom(QuotebookError::InsufficientFees));
    }

    let seeds = [Seed::from(CONFIG_SEED), Seed::from(&bump)];
    let ix: TransferChecked<'_, '_, &AccountView> = TransferChecked::new(
        fee_account,
        quote_mint,
        destination,
        config,
        amount,
        decimals,
    );
    ix.invoke_signed_with_program(&[Signer::from(&seeds)], program)
}
