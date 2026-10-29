//! Admin commands: initialize, create series, pause, hand over the admin role.

use quotebook_core::instruction::CreateSeriesArgs;
use quotebook_sdk::{deployment_id_from_genesis_hash, ix, pda};
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::Signer;

use crate::market;
use crate::rpc::{send, Rpc};
use crate::CliError;

/// `init_config` with the deployment id from the cluster's genesis hash. `admin` must be the
/// program's upgrade authority.
///
/// # Errors
///
/// The RPC errors, including the program's rejection.
pub fn init_config(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    admin: &Keypair,
    payer: &Keypair,
) -> Result<([u8; 16], Signature), CliError> {
    let genesis = rpc.genesis_hash()?;
    let deployment = deployment_id_from_genesis_hash(&genesis.to_bytes());
    let ix = ix::init_config(program_id, &admin.pubkey(), &payer.pubkey(), deployment);
    let sig = send(rpc, &[ix], payer, &[admin])?;
    Ok((deployment, sig))
}

/// `create_series` at the config's next index; returns the series address.
///
/// # Errors
///
/// The config read and the RPC errors.
pub fn create_series(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    admin: &Keypair,
    payer: &Keypair,
    base_mint: &Address,
    quote_mint: &Address,
    args: &CreateSeriesArgs,
) -> Result<(Address, Signature), CliError> {
    let index = market::config(rpc, program_id)?.series_count;
    let ix = ix::create_series(
        program_id,
        &admin.pubkey(),
        &payer.pubkey(),
        base_mint,
        quote_mint,
        index,
        args,
    );
    let sig = send(rpc, &[ix], payer, &[admin])?;
    Ok((pda::series(program_id, base_mint, quote_mint, index).0, sig))
}

/// `set_series_paused`.
///
/// # Errors
///
/// The RPC errors.
pub fn set_paused(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    admin: &Keypair,
    payer: &Keypair,
    series: &Address,
    paused: bool,
) -> Result<Signature, CliError> {
    let ix = ix::set_series_paused(program_id, &admin.pubkey(), series, paused);
    send(rpc, &[ix], payer, &[admin])
}

/// `set_config`: hands the admin role to `new_admin`.
///
/// # Errors
///
/// The RPC errors.
pub fn set_admin(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    admin: &Keypair,
    payer: &Keypair,
    new_admin: &Address,
) -> Result<Signature, CliError> {
    let ix = ix::set_config(program_id, &admin.pubkey(), new_admin);
    send(rpc, &[ix], payer, &[admin])
}

/// `init_maker` for `maker`.
///
/// # Errors
///
/// The RPC errors.
pub fn init_maker(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    maker: &Keypair,
    payer: &Keypair,
) -> Result<Signature, CliError> {
    let ix = ix::init_maker(program_id, &maker.pubkey(), &payer.pubkey());
    send(rpc, &[ix], payer, &[maker])
}

/// Approves the maker's delegate PDA for `amount` atoms on `token_account`.
///
/// # Errors
///
/// The RPC errors.
pub fn approve(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    maker: &Keypair,
    payer: &Keypair,
    token_account: &Address,
    amount: u64,
) -> Result<Signature, CliError> {
    let program = crate::rpc::existing(rpc, token_account)?.owner;
    let ix = ix::approve_delegate(program_id, &program, token_account, &maker.pubkey(), amount);
    send(rpc, &[ix], payer, &[maker])
}

/// `cancel_all`: every nonce of `maker` below `new_min` stops being fillable.
///
/// # Errors
///
/// The RPC errors.
pub fn cancel_all(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    maker: &Keypair,
    payer: &Keypair,
    new_min: quotebook_core::Nonce,
) -> Result<Signature, CliError> {
    let ix = ix::cancel_all(program_id, &maker.pubkey(), new_min);
    send(rpc, &[ix], payer, &[maker])
}

/// `cancel_nonce`: one nonce of `maker` stops being fillable.
///
/// # Errors
///
/// The RPC errors.
pub fn cancel_nonce(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    maker: &Keypair,
    payer: &Keypair,
    nonce: quotebook_core::Nonce,
) -> Result<Signature, CliError> {
    let ix = ix::cancel_nonce(program_id, &maker.pubkey(), nonce);
    send(rpc, &[ix], payer, &[maker])
}
