//! Reads the configured metalbook series from chain and keeps what the demo can quote.
//!
//! A series is quoted only when it is `Active` with valid terms (a capped call or a put), carries
//! this demo's USDC and feed, has the long mint its PDA says, and its quotebook pair trades that
//! long mint against USDC. Anything else is skipped with a warning, never quoted. Time is not
//! judged here: the market hours and the settlement window are checked on every mid
//! ([`crate::feed`]).

use metalbook_core::{SeriesState, SeriesTerms, Underlying};
use metalbook_sdk::{decode, pda};
use quotebook_cli::rpc::Rpc;
use quotebook_cli::{market, CliError};
use solana_address::Address;

use crate::config::{DemoConfig, SeriesPair};

/// A series the demo can quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListedSeries {
    /// The metalbook series account.
    pub metalbook_series: Address,
    /// The quotebook series that trades its long token.
    pub quotebook_series: Address,
    /// The long mint: the quotebook series' base.
    pub long_mint: Address,
    /// The validated terms.
    pub terms: SeriesTerms,
    /// The settlement window before expiry, seconds: no quote from `expiry − window_s` on.
    pub window_s: u32,
}

impl ListedSeries {
    /// The metal.
    #[must_use]
    pub const fn underlying(&self) -> Underlying {
        self.terms.key().underlying()
    }

    /// The expiry, Unix seconds.
    #[must_use]
    pub const fn expiry(&self) -> i64 {
        self.terms.key().expiry().get()
    }
}

/// Why a series is not quoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SeriesSkip {
    /// The account does not decode as a metalbook series.
    #[error("not a metalbook series: {0:?}")]
    Decode(decode::DecodeError),
    /// Its state or terms do not validate.
    #[error("invalid state or terms: {0:?}")]
    Invalid(metalbook_core::CoreError),
    /// It is not `Active`.
    #[error("not active")]
    NotActive,
    /// Its collateral is not this demo's USDC.
    #[error("another collateral mint")]
    WrongCollateral,
    /// Its long mint is not the PDA of the series.
    #[error("long mint is not the series' PDA")]
    WrongLongMint,
    /// This demo has no configuration for the metal.
    #[error("underlying not configured")]
    NoUnderlying,
    /// Its feed is not the one this demo reads for the metal.
    #[error("another feed id")]
    WrongFeed,
    /// The quotebook series does not trade the long mint against USDC.
    #[error("the quotebook series is not long mint against USDC")]
    WrongPair,
}

/// Judges one metalbook series account.
///
/// # Errors
///
/// [`SeriesSkip`].
pub fn classify(
    config: &DemoConfig,
    pair: &SeriesPair,
    owner: &Address,
    data: &[u8],
) -> Result<ListedSeries, SeriesSkip> {
    let series =
        decode::series(&config.metalbook_program, owner, data).map_err(SeriesSkip::Decode)?;
    let state = SeriesState::try_from(series.state).map_err(SeriesSkip::Invalid)?;
    if state != SeriesState::Active {
        return Err(SeriesSkip::NotActive);
    }
    let terms = series.terms().map_err(SeriesSkip::Invalid)?;
    if Address::new_from_array(series.usdc_mint) != config.usdc_mint {
        return Err(SeriesSkip::WrongCollateral);
    }
    let long_mint = Address::new_from_array(series.long_mint);
    let (expected, _) = pda::long_mint(&config.metalbook_program, &pair.metalbook_series);
    if long_mint != expected {
        return Err(SeriesSkip::WrongLongMint);
    }
    let underlying = config
        .underlying(terms.key().underlying())
        .ok_or(SeriesSkip::NoUnderlying)?;
    if series.feed_id != underlying.feed_id {
        return Err(SeriesSkip::WrongFeed);
    }
    Ok(ListedSeries {
        metalbook_series: pair.metalbook_series,
        quotebook_series: pair.quotebook_series,
        long_mint,
        terms,
        window_s: series.window_s,
    })
}

/// Checks that the quotebook series trades the long mint against USDC.
///
/// # Errors
///
/// [`SeriesSkip::WrongPair`].
pub fn check_pair(
    config: &DemoConfig,
    long_mint: &Address,
    base_mint: &Address,
    quote_mint: &Address,
) -> Result<(), SeriesSkip> {
    if base_mint == long_mint && *quote_mint == config.usdc_mint {
        Ok(())
    } else {
        Err(SeriesSkip::WrongPair)
    }
}

/// Reads every configured series and returns those that pass.
///
/// # Errors
///
/// `CliError::Rpc` if the RPC fails, so the caller can keep what it has rather than lose every
/// series to one failed call. A series that is missing or fails a check is skipped with a warning.
pub fn read_series(rpc: &mut dyn Rpc, config: &DemoConfig) -> Result<Vec<ListedSeries>, CliError> {
    let mut listed = Vec::new();
    for pair in &config.pairs {
        let address = pair.metalbook_series;
        let Some(account) = rpc.account(&address)? else {
            tracing::warn!(series = %address, "metalbook series not found");
            continue;
        };
        let series = match classify(config, pair, &account.owner, &account.data) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(series = %address, error = %e, "series not quoted");
                continue;
            }
        };
        let quotebook = match market::series(rpc, &config.quotebook_program, &pair.quotebook_series)
        {
            Ok(q) => q,
            Err(e @ CliError::Rpc(_)) => return Err(e),
            Err(e) => {
                tracing::warn!(series = %address, error = %e, "quotebook pair not readable");
                continue;
            }
        };
        if let Err(e) = check_pair(
            config,
            &series.long_mint,
            &quotebook.base_mint,
            &quotebook.quote_mint,
        ) {
            tracing::warn!(series = %address, error = %e, "series not quoted");
            continue;
        }
        listed.push(series);
    }
    Ok(listed)
}
