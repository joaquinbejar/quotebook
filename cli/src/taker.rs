//! Taker commands: request a quote, check it against the chain, settle it.

use quotebook_core::BaseLots;
use quotebook_maker::client::{self, Answer, Expectation};
use quotebook_maker::market::MarketInfo;
use quotebook_maker::protocol::{QuoteRequest, TakerSide};
use quotebook_sdk::SettleAccounts;
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::Signer;

use crate::market;
use crate::rpc::{clock, existing, send, Rpc};
use crate::CliError;

/// A request for `size` lots on `side` of `series`, bound to `taker`.
#[must_use]
pub fn request(
    id: u64,
    series: &Address,
    taker: &Address,
    side: TakerSide,
    size: BaseLots,
) -> QuoteRequest {
    QuoteRequest {
        request_id: id,
        series: series.to_string(),
        taker: taker.to_string(),
        taker_side: side,
        size_lots: size.get(),
    }
}

/// Asks the maker at `url`. Async: run it on a runtime, then settle outside it (the RPC
/// client is blocking).
///
/// # Errors
///
/// `Quote` if the maker refuses or the answer is malformed.
pub async fn ask(url: &str, request: &QuoteRequest) -> Result<Answer, CliError> {
    Ok(client::request_quote(url, request).await?)
}

/// Checks `answer` against the series read from chain and the cluster clock: the signature
/// and every rule `settle` applies to the quote's terms. Returns the market for [`settle`].
///
/// # Errors
///
/// `Quote` with the program error `settle` would return; the chain reads.
pub fn check(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    series: &Address,
    answer: &Answer,
    taker: &Address,
    side: TakerSide,
    size: BaseLots,
) -> Result<MarketInfo, CliError> {
    let market = market::market(rpc, program_id, series)?;
    let now = clock(rpc)?;
    client::verify(
        &answer.signed,
        &Expectation {
            market,
            taker: *taker,
            taker_side: side,
            size,
            now,
        },
    )?;
    Ok(market)
}

/// Sends `[ed25519 precompile, settle]` for a checked answer.
///
/// # Errors
///
/// The chain reads and the RPC errors, including the program's rejection.
#[allow(clippy::too_many_arguments)]
pub fn settle(
    rpc: &mut dyn Rpc,
    program_id: &Address,
    answer: &Answer,
    taker: &Keypair,
    payer: &Keypair,
    taker_base: &Address,
    taker_quote: &Address,
    size: BaseLots,
) -> Result<Signature, CliError> {
    let series = market::series(
        rpc,
        program_id,
        &answer.signed.quote.terms().series.address(),
    )?;
    let base_token_program = existing(rpc, &series.base_mint)?.owner;
    let quote_token_program = existing(rpc, &series.quote_mint)?.owner;
    let accounts = SettleAccounts {
        taker: taker.pubkey(),
        base_mint: series.base_mint,
        quote_mint: series.quote_mint,
        base_token_program,
        quote_token_program,
        maker_base: answer.maker_base,
        maker_quote: answer.maker_quote,
        taker_base: *taker_base,
        taker_quote: *taker_quote,
    };
    let pair = quotebook_sdk::settle_instructions(
        program_id,
        &answer.signed.quote,
        &answer.signed.signature,
        &accounts,
        size,
        0,
    );
    send(rpc, &pair, payer, &[taker])
}
