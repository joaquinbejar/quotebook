//! Reading quotebook state from chain.

use quotebook_core::accounts::{Config, MakerState, Series};
use quotebook_maker::market::MarketInfo;
use quotebook_sdk::{decode, pda};
use solana_account::Account;
use solana_address::Address;

use crate::rpc::{existing, Rpc};
use crate::CliError;

/// Offset of `Mint::decimals` in SPL Token and Token-2022 mints.
const MINT_DECIMALS: usize = 44;

fn bad(what: &str, e: impl std::fmt::Display) -> CliError {
    CliError::Account(format!("{what}: {e}"))
}

/// The program's `Config`.
///
/// # Errors
///
/// `Account` if it is missing or not a `Config`; the RPC error.
pub fn config(rpc: &mut dyn Rpc, program_id: &Address) -> Result<Config, CliError> {
    let a = existing(rpc, &pda::config(program_id).0)?;
    decode::config(program_id, &a.owner, &a.data).map_err(|e| bad("config", e))
}

/// A `Series`.
///
/// # Errors
///
/// `Account` if it is missing or not a `Series`; the RPC error.
pub fn series(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    address: &Address,
) -> Result<Series, CliError> {
    let a = existing(rpc, address)?;
    decode::series(program_id, &a.owner, &a.data).map_err(|e| bad("series", e))
}

/// A maker's `MakerState`.
///
/// # Errors
///
/// `Account` if it is missing or not a `MakerState`; the RPC error.
pub fn maker_state(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    maker: &Address,
) -> Result<MakerState, CliError> {
    let a = existing(rpc, &pda::maker_state(program_id, maker).0)?;
    decode::maker_state(program_id, &a.owner, &a.data).map_err(|e| bad("maker state", e))
}

/// A mint account and its decimals.
///
/// # Errors
///
/// `Account` if it is missing or too short; the RPC error.
pub fn mint(rpc: &mut dyn Rpc, address: &Address) -> Result<(Account, u8), CliError> {
    let a = existing(rpc, address)?;
    let decimals = a
        .data
        .get(MINT_DECIMALS)
        .copied()
        .ok_or_else(|| CliError::Account(format!("{address} is not a mint")))?;
    Ok((a, decimals))
}

/// Everything the maker service and the taker's checks need about a series.
///
/// # Errors
///
/// The errors of [`series`] and [`mint`].
pub fn market(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    series_address: &Address,
) -> Result<MarketInfo, CliError> {
    let s = series(rpc, program_id, series_address)?;
    let (_, base_decimals) = mint(rpc, &s.base_mint)?;
    let (_, quote_decimals) = mint(rpc, &s.quote_mint)?;
    Ok(MarketInfo {
        program_id: *program_id,
        deployment: s.deployment_id,
        series: *series_address,
        base_decimals,
        quote_decimals,
        base_lot_size: s.base_lot_size.get(),
        quote_lot_size: s.quote_lot_size.get(),
        max_quote_ttl: s.max_quote_ttl,
        min_size: s.min_size,
    })
}
