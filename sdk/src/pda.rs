//! Program-derived addresses, with their canonical bumps.

use quotebook_core::accounts::{CONFIG_SEED, DELEGATE_SEED, MAKER_SEED, SERIES_SEED};
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

/// The program's `ProgramData` under the upgradeable loader, which `init_config` reads.
#[must_use]
pub fn program_data(program_id: &Address) -> (Address, u8) {
    Address::find_program_address(
        &[program_id.as_ref()],
        &solana_sdk_ids::bpf_loader_upgradeable::ID,
    )
}
