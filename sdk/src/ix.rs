//! Instruction builders, one per program instruction, in the account order of the program's
//! handler modules. Plus [`approve_delegate`], the token-program instruction a maker sends
//! once so `settle` can pay from its account.

use quotebook_core::instruction::{CreateSeriesArgs, SettleArgs, Tag};
use quotebook_core::{BaseLots, Nonce, Quote};
use solana_address::Address;
use solana_instruction::{AccountMeta, Instruction};
use solana_sdk_ids::{system_program, sysvar};

use crate::pda;

fn data(tag: Tag, args: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(args.len().saturating_add(1));
    v.push(tag as u8);
    v.extend_from_slice(args);
    v
}

/// `init_config`. `admin` must be the program's upgrade authority; `deployment_id` is
/// [`deployment_id_from_genesis_hash`](crate::deployment_id_from_genesis_hash) of the cluster.
#[must_use]
pub fn init_config(
    program_id: &Address,
    admin: &Address,
    payer: &Address,
    deployment_id: [u8; 16],
) -> Instruction {
    Instruction::new_with_bytes(
        *program_id,
        &data(Tag::InitConfig, &deployment_id),
        vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new(pda::config(program_id).0, false),
            AccountMeta::new_readonly(pda::program_data(program_id).0, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
    )
}

/// `set_config`: hands the admin role to `new_admin`.
#[must_use]
pub fn set_config(program_id: &Address, admin: &Address, new_admin: &Address) -> Instruction {
    Instruction::new_with_bytes(
        *program_id,
        &data(Tag::SetConfig, new_admin.as_ref()),
        vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(pda::config(program_id).0, false),
        ],
    )
}

/// `create_series` at `series_index`, which must be the config's current `series_count`.
#[must_use]
pub fn create_series(
    program_id: &Address,
    admin: &Address,
    payer: &Address,
    base_mint: &Address,
    quote_mint: &Address,
    series_index: u64,
    args: &CreateSeriesArgs,
) -> Instruction {
    let series = pda::series(program_id, base_mint, quote_mint, series_index).0;
    Instruction::new_with_bytes(
        *program_id,
        &data(Tag::CreateSeries, &args.encode()),
        vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new(pda::config(program_id).0, false),
            AccountMeta::new(series, false),
            AccountMeta::new_readonly(*base_mint, false),
            AccountMeta::new_readonly(*quote_mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
    )
}

/// `set_series_paused`.
#[must_use]
pub fn set_series_paused(
    program_id: &Address,
    admin: &Address,
    series: &Address,
    paused: bool,
) -> Instruction {
    Instruction::new_with_bytes(
        *program_id,
        &data(Tag::SetSeriesPaused, &[u8::from(paused)]),
        vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new_readonly(pda::config(program_id).0, false),
            AccountMeta::new(*series, false),
        ],
    )
}

/// `init_maker` for `maker`.
#[must_use]
pub fn init_maker(program_id: &Address, maker: &Address, payer: &Address) -> Instruction {
    Instruction::new_with_bytes(
        *program_id,
        &data(Tag::InitMaker, &[]),
        vec![
            AccountMeta::new_readonly(*maker, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new(pda::maker_state(program_id, maker).0, false),
            AccountMeta::new_readonly(system_program::ID, false),
        ],
    )
}

fn cancel(program_id: &Address, tag: Tag, maker: &Address, value: Nonce) -> Instruction {
    Instruction::new_with_bytes(
        *program_id,
        &data(tag, &value.get().to_le_bytes()),
        vec![
            AccountMeta::new_readonly(*maker, true),
            AccountMeta::new(pda::maker_state(program_id, maker).0, false),
        ],
    )
}

/// `cancel_all`: every nonce below `new_min` stops being fillable.
#[must_use]
pub fn cancel_all(program_id: &Address, maker: &Address, new_min: Nonce) -> Instruction {
    cancel(program_id, Tag::CancelAll, maker, new_min)
}

/// `cancel_nonce`: `nonce` stops being fillable.
#[must_use]
pub fn cancel_nonce(program_id: &Address, maker: &Address, nonce: Nonce) -> Instruction {
    cancel(program_id, Tag::CancelNonce, maker, nonce)
}

/// The accounts of a `settle` that the quote does not determine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleAccounts {
    /// The taker, who signs.
    pub taker: Address,
    /// The series' base mint.
    pub base_mint: Address,
    /// The series' quote mint.
    pub quote_mint: Address,
    /// The base mint's token program (SPL Token or Token-2022).
    pub base_token_program: Address,
    /// The quote mint's token program.
    pub quote_token_program: Address,
    /// The maker's base token account.
    pub maker_base: Address,
    /// The maker's quote token account.
    pub maker_quote: Address,
    /// The taker's base token account.
    pub taker_base: Address,
    /// The taker's quote token account.
    pub taker_quote: Address,
}

/// `settle` for `quote`, reading the precompile instruction at `sig_ix_index`. The series,
/// maker state and delegate come from the quote.
#[must_use]
pub fn settle(
    program_id: &Address,
    quote: &Quote,
    accounts: &SettleAccounts,
    sig_ix_index: u8,
    fill_size: BaseLots,
) -> Instruction {
    let terms = quote.terms();
    let args = SettleArgs {
        sig_ix_index,
        fill_size,
    };
    let a = accounts;
    Instruction::new_with_bytes(
        *program_id,
        &data(Tag::Settle, &args.encode()),
        vec![
            AccountMeta::new_readonly(a.taker, true),
            AccountMeta::new_readonly(terms.series.address(), false),
            AccountMeta::new(pda::maker_state(program_id, &terms.maker).0, false),
            AccountMeta::new_readonly(pda::delegate(program_id, &terms.maker).0, false),
            AccountMeta::new(a.maker_base, false),
            AccountMeta::new(a.maker_quote, false),
            AccountMeta::new(a.taker_base, false),
            AccountMeta::new(a.taker_quote, false),
            AccountMeta::new_readonly(a.base_mint, false),
            AccountMeta::new_readonly(a.quote_mint, false),
            AccountMeta::new_readonly(a.base_token_program, false),
            AccountMeta::new_readonly(a.quote_token_program, false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
        ],
    )
}

/// SPL Token / Token-2022 `Approve` (tag 4): lets the maker's delegate PDA move up to
/// `amount` atoms out of `source`. A maker sends it for the account it pays from; settles
/// draw the allowance down.
#[must_use]
pub fn approve_delegate(
    program_id: &Address,
    token_program: &Address,
    source: &Address,
    maker: &Address,
    amount: u64,
) -> Instruction {
    let mut d = vec![4u8];
    d.extend_from_slice(&amount.to_le_bytes());
    Instruction::new_with_bytes(
        *token_program,
        &d,
        vec![
            AccountMeta::new(*source, false),
            AccountMeta::new_readonly(pda::delegate(program_id, maker).0, false),
            AccountMeta::new_readonly(*maker, true),
        ],
    )
}
