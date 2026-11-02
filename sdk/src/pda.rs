//! Program-derived addresses, with their canonical bumps.

use quotebook_core::accounts::{CONFIG_SEED, DELEGATE_SEED, MAKER_SEED, PROFILE_SEED, SERIES_SEED};
use solana_address::Address;

/// `Config`: `["config"]`.
#[must_use]
pub fn config(program_id: &Address) -> (Address, u8) {
    Address::find_program_address(&[CONFIG_SEED], program_id)
}

/// A `Series`: `["series", base_mint, quote_mint, series_index_le]`.
#[must_use]
pub fn series(
    program_id: &Address,
    base_mint: &Address,
    quote_mint: &Address,
    series_index: u64,
) -> (Address, u8) {
    Address::find_program_address(
        &[
            SERIES_SEED,
            base_mint.as_ref(),
            quote_mint.as_ref(),
            &series_index.to_le_bytes(),
        ],
        program_id,
    )
}

/// A maker's `MakerState`: `["maker", maker]`.
#[must_use]
pub fn maker_state(program_id: &Address, maker: &Address) -> (Address, u8) {
    Address::find_program_address(&[MAKER_SEED, maker.as_ref()], program_id)
}

/// A maker's delegate authority: `["delegate", maker]`. The maker approves it on the token
/// account it pays from.
#[must_use]
pub fn delegate(program_id: &Address, maker: &Address) -> (Address, u8) {
    Address::find_program_address(&[DELEGATE_SEED, maker.as_ref()], program_id)
}

/// A maker's profile: `["profile", maker]` (ADR 0002).
#[must_use]
pub fn profile(program_id: &Address, maker: &Address) -> (Address, u8) {
    Address::find_program_address(&[PROFILE_SEED, maker.as_ref()], program_id)
}

/// The program's `ProgramData` under the upgradeable loader, which `init_config` reads.
#[must_use]
pub fn program_data(program_id: &Address) -> (Address, u8) {
    Address::find_program_address(
        &[program_id.as_ref()],
        &solana_sdk_ids::bpf_loader_upgradeable::ID,
    )
}

/// The SPL Associated Token Account program.
pub const ASSOCIATED_TOKEN_PROGRAM: Address =
    solana_address::address!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// The canonical fee account of a quote mint (ADR 0001): the Config PDA's associated token
/// account for `quote_mint` under `token_program`. Any token account of the quote mint owned by
/// the Config PDA is accepted by `settle`; clients use this one by convention.
#[must_use]
pub fn fee_account(program_id: &Address, quote_mint: &Address, token_program: &Address) -> Address {
    let owner = config(program_id).0;
    Address::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), quote_mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM,
    )
    .0
}
