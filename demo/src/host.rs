//! The hosted demo maker (`docs/PLAN-phase2.md`, D3): its settings, the markets it quotes and
//! the service that ties D1's risk book to D2's marks.
//!
//! The settings file holds public values only: limits, pricing bounds and the allowed taker. Key
//! paths and the RPC URL are command-line arguments, never in a file of this repository.
//!
//! The market set is fixed at start: every configured pair whose quotebook series reads, quoting
//! USDC. A metalbook series that stops being quotable (settled, paused, past its settlement
//! window) stays in the set and gets no mid ([`crate::feed`]), so the service keeps answering
//! `Status` and refuses quotes with `Pricing`. A new pair needs a restart, which pays the cold
//! start.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use quotebook_cli::rpc::Rpc;
use quotebook_cli::{market, CliError};
use quotebook_core::{BaseLots, Seconds};
use quotebook_maker::keys::MakerKey;
use quotebook_maker::market::MarketInfo;
use quotebook_maker::pricing::{ModelUnits, PricingParams};
use quotebook_maker::risk::{MarketLimits, RiskConfig};
use quotebook_maker::server::{MakerService, MarketEntry, ServiceConfig, Start, TimeSource};
use rust_decimal::Decimal;
use serde::Deserialize;
use solana_address::Address;
use solana_keypair::Keypair;

use crate::chain::{BalanceInventory, Balances, WatchedAccount, TOKEN_2022_PROGRAM, TOKEN_PROGRAM};
use crate::config::{ConfigError, DemoConfig, SeriesPair};
use crate::feed::Board;

/// The SPL Associated Token Account program.
pub const ASSOCIATED_TOKEN_PROGRAM: Address =
    solana_address::address!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// The demo maker's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostSettings {
    /// Lifetime of each quote.
    pub ttl: Seconds,
    /// Largest size quoted, base lots.
    pub max_size: BaseLots,
    /// Seconds between finalized snapshots.
    pub reconcile_secs: u64,
    /// Model and bounds.
    pub pricing: PricingParams,
    /// Service-wide limits, without the halt file (a command-line path).
    pub max_open_quotes: u32,
    /// See [`RiskConfig::max_fills_per_hour`].
    pub max_fills_per_hour: u32,
    /// See [`RiskConfig::max_snapshot_age`].
    pub max_snapshot_age: Seconds,
    /// The only takers quotes are signed for.
    pub allowed_takers: Vec<Address>,
    /// The limits of every market.
    pub limits: MarketLimits,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSettings {
    ttl_secs: i64,
    max_size_lots: u64,
    reconcile_secs: u64,
    pricing: RawPricing,
    risk: RawRisk,
    market_limits: RawLimits,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPricing {
    risk_aversion: String,
    volatility: String,
    time_to_terminal_ms: u64,
    order_intensity: String,
    max_deviation: String,
    min_half_spread_bps: u32,
    max_half_spread_bps: u32,
    max_skew_bps: u32,
    model_units: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRisk {
    max_open_quotes: u32,
    max_fills_per_hour: u32,
    max_snapshot_age_secs: i64,
    allowed_takers: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLimits {
    max_notional: u64,
    max_long: u64,
    max_short: u64,
}

fn invalid(field: &'static str, problem: impl Into<String>) -> ConfigError {
    ConfigError::Field {
        field,
        problem: problem.into(),
    }
}

fn decimal(field: &'static str, s: &str) -> Result<Decimal, ConfigError> {
    Decimal::from_str(s).map_err(|_| invalid(field, "not a decimal"))
}

impl HostSettings {
    /// Parses and checks the settings.
    ///
    /// # Errors
    ///
    /// [`ConfigError`]: bad JSON, a non-positive TTL, size, interval or snapshot age, no allowed
    /// taker, an invalid address or decimal, or pricing that fails its startup checks.
    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let raw: RawSettings = serde_json::from_str(json)?;
        if raw.ttl_secs <= 0 {
            return Err(invalid("ttl_secs", "must be positive"));
        }
        if raw.max_size_lots == 0 {
            return Err(invalid("max_size_lots", "must be positive"));
        }
        if raw.reconcile_secs == 0 {
            return Err(invalid("reconcile_secs", "must be positive"));
        }
        if raw.risk.max_snapshot_age_secs <= 0 {
            return Err(invalid("max_snapshot_age_secs", "must be positive"));
        }
        if raw.risk.allowed_takers.is_empty() {
            return Err(invalid(
                "allowed_takers",
                "the demo signs for its own taker only",
            ));
        }
        let allowed_takers = raw
            .risk
            .allowed_takers
            .iter()
            .map(|t| {
                t.parse()
                    .map_err(|_| invalid("allowed_takers", "not an address"))
            })
            .collect::<Result<Vec<Address>, _>>()?;
        let p = &raw.pricing;
        let pricing = PricingParams {
            risk_aversion: decimal("risk_aversion", &p.risk_aversion)?,
            volatility: decimal("volatility", &p.volatility)?,
            time_to_terminal_ms: p.time_to_terminal_ms,
            order_intensity: decimal("order_intensity", &p.order_intensity)?,
            max_deviation: decimal("max_deviation", &p.max_deviation)?,
            min_half_spread_bps: p.min_half_spread_bps,
            max_half_spread_bps: p.max_half_spread_bps,
            max_skew_bps: p.max_skew_bps,
            model_units: match p.model_units.as_str() {
                "relative" => ModelUnits::Relative,
                "absolute" => ModelUnits::Absolute,
                _ => return Err(invalid("model_units", "relative or absolute")),
            },
        };
        pricing
            .validate()
            .map_err(|e| invalid("pricing", e.to_string()))?;
        Ok(Self {
            ttl: Seconds::new(raw.ttl_secs),
            max_size: BaseLots::new(raw.max_size_lots),
            reconcile_secs: raw.reconcile_secs,
            pricing,
            max_open_quotes: raw.risk.max_open_quotes,
            max_fills_per_hour: raw.risk.max_fills_per_hour,
            max_snapshot_age: Seconds::new(raw.risk.max_snapshot_age_secs),
            allowed_takers,
            limits: MarketLimits {
                max_notional: raw.market_limits.max_notional,
                max_long: raw.market_limits.max_long,
                max_short: raw.market_limits.max_short,
            },
        })
    }

    /// The service-wide limits with `halt_file`.
    #[must_use]
    pub fn risk(&self, halt_file: PathBuf) -> RiskConfig {
        RiskConfig {
            max_open_quotes: self.max_open_quotes,
            max_fills_per_hour: self.max_fills_per_hour,
            max_snapshot_age: self.max_snapshot_age,
            allowed_takers: self.allowed_takers.clone(),
            halt_file,
        }
    }
}

/// The associated token account of `owner` for `mint` under `token_program`.
#[must_use]
pub fn associated_token_account(
    owner: &Address,
    mint: &Address,
    token_program: &Address,
) -> Address {
    Address::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ASSOCIATED_TOKEN_PROGRAM,
    )
    .0
}

const fn token_program(is_2022: u8) -> Address {
    if is_2022 == 0 {
        TOKEN_PROGRAM
    } else {
        TOKEN_2022_PROGRAM
    }
}

/// One market the service quotes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedMarket {
    /// The pairing.
    pub pair: SeriesPair,
    /// The quotebook series.
    pub market: MarketInfo,
    /// Its base mint: the metalbook long mint.
    pub base_mint: Address,
    /// The maker's base account: its associated token account for the long mint.
    pub maker_base: Address,
}

/// The markets and the one USDC account they share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The maker.
    pub maker: Address,
    /// The markets.
    pub markets: Vec<PlannedMarket>,
    /// The maker's USDC account, shared by every market.
    pub maker_quote: Address,
}

impl Plan {
    /// Every account the snapshot reads: the USDC account, then each market's base account.
    #[must_use]
    pub fn watched(&self, usdc_mint: Address) -> Vec<WatchedAccount> {
        let mut accounts = vec![WatchedAccount {
            address: self.maker_quote,
            mint: usdc_mint,
        }];
        for m in &self.markets {
            if accounts.iter().all(|a| a.address != m.maker_base) {
                accounts.push(WatchedAccount {
                    address: m.maker_base,
                    mint: m.base_mint,
                });
            }
        }
        accounts
    }

    /// The longest `max_quote_ttl` of the set: what the cold start waits out.
    #[must_use]
    pub fn longest_ttl(&self) -> Option<Seconds> {
        self.markets.iter().map(|m| m.market.max_quote_ttl).max()
    }
}

/// Reads every configured quotebook series and plans the markets.
///
/// # Errors
///
/// The RPC errors; `Maker` when no series can be quoted, or when two series would pay from
/// different USDC accounts.
pub fn plan(rpc: &mut dyn Rpc, config: &DemoConfig, maker: Address) -> Result<Plan, CliError> {
    let mut markets = Vec::new();
    let mut maker_quote: Option<Address> = None;
    for pair in &config.pairs {
        let series = match market::series(rpc, &config.quotebook_program, &pair.quotebook_series) {
            Ok(s) => s,
            Err(e @ CliError::Rpc(_)) => return Err(e),
            Err(e) => {
                tracing::warn!(series = %pair.quotebook_series, error = %e, "quotebook series skipped");
                continue;
            }
        };
        if series.quote_mint != config.usdc_mint {
            tracing::warn!(series = %pair.quotebook_series, "quotebook series skipped: not quoted in USDC");
            continue;
        }
        let quote_account = associated_token_account(
            &maker,
            &config.usdc_mint,
            &token_program(series.quote_is_2022),
        );
        if maker_quote.is_some_and(|q| q != quote_account) {
            return Err(CliError::Maker(
                "series pay from different USDC accounts".into(),
            ));
        }
        maker_quote = Some(quote_account);
        let info = market::market(rpc, &config.quotebook_program, &pair.quotebook_series)?;
        markets.push(PlannedMarket {
            pair: *pair,
            market: info,
            base_mint: series.base_mint,
            maker_base: associated_token_account(
                &maker,
                &series.base_mint,
                &token_program(series.base_is_2022),
            ),
        });
    }
    let maker_quote =
        maker_quote.ok_or_else(|| CliError::Maker("no quotable quotebook series".into()))?;
    Ok(Plan {
        maker,
        markets,
        maker_quote,
    })
}

/// Builds the multi-series service over `plan`, resumed from `start`.
///
/// # Errors
///
/// `Maker` with the service's configuration error.
#[allow(clippy::too_many_arguments)]
pub fn service(
    key: Keypair,
    plan: &Plan,
    settings: &HostSettings,
    board: &Arc<Board>,
    balances: &Arc<Balances>,
    start: Start,
    halt_file: PathBuf,
    clock: Box<dyn TimeSource>,
) -> Result<MakerService, CliError> {
    let markets = plan
        .markets
        .iter()
        .map(|m| MarketEntry {
            config: ServiceConfig {
                market: m.market,
                pricing: settings.pricing,
                ttl: settings.ttl,
                max_size: settings.max_size,
                maker_base: m.maker_base,
                maker_quote: plan.maker_quote,
            },
            limits: settings.limits,
            feed: Box::new(board.feed(m.pair.metalbook_series)),
            inventory: Box::new(BalanceInventory {
                balances: Arc::clone(balances),
                account: m.maker_base,
                decimals: m.market.base_decimals,
            }),
        })
        .collect();
    MakerService::multi(
        MakerKey::new(key),
        markets,
        start,
        settings.risk(halt_file),
        clock,
    )
    .map_err(|e| CliError::Maker(e.to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::disallowed_macros,
        clippy::panic_in_result_fn,
        clippy::unwrap_used
    )]

    use super::*;

    const DEVNET: &str = include_str!("../config/devnet-maker.json");

    #[test]
    fn the_devnet_settings_parse_with_the_plan_limits() {
        let s = HostSettings::from_json(DEVNET).unwrap();
        // D5's limits, sized to the funding.
        assert_eq!(s.max_size, BaseLots::new(1));
        assert_eq!(s.max_fills_per_hour, 4);
        assert_eq!(s.limits.max_long, 2);
        assert_eq!(s.limits.max_short, 0);
        assert_eq!(s.max_snapshot_age, Seconds::new(60));
        assert_eq!(
            s.allowed_takers,
            vec!["3ZBj1qBpdoH5NrMTwAHq2fY6Hbo5M1GURxaP9y8bsqLs"
                .parse::<Address>()
                .unwrap()]
        );
    }

    fn with(from: &str, to: &str) -> Result<HostSettings, ConfigError> {
        assert!(DEVNET.contains(from), "{from}");
        HostSettings::from_json(&DEVNET.replace(from, to))
    }

    #[test]
    fn invalid_settings_are_refused() {
        assert!(with("\"ttl_secs\": 30", "\"ttl_secs\": 0").is_err());
        assert!(with("\"max_size_lots\": 1", "\"max_size_lots\": 0").is_err());
        assert!(with("\"reconcile_secs\": 10", "\"reconcile_secs\": 0").is_err());
        assert!(with(
            "\"max_snapshot_age_secs\": 60",
            "\"max_snapshot_age_secs\": 0"
        )
        .is_err());
        assert!(
            with("[\"3ZBj1qBpdoH5NrMTwAHq2fY6Hbo5M1GURxaP9y8bsqLs\"]", "[]").is_err(),
            "no allowed taker"
        );
        assert!(with("3ZBj1qBpdoH5", "0OIl").is_err());
        assert!(with("\"relative\"", "\"other\"").is_err());
        assert!(
            with(
                "\"max_half_spread_bps\": 300",
                "\"max_half_spread_bps\": 900"
            )
            .is_err(),
            "caps above half the guard"
        );
        assert!(
            with("\"market_limits\"", "\"limits\"").is_err(),
            "unknown field"
        );
    }

    #[test]
    fn the_ata_is_the_canonical_one() {
        // I9's maker USDC account is the ATA of 7tVP… for Circle devnet USDC under SPL Token.
        let owner: Address = "7tVPWX3zmxYCZ5ioTSX4yzdNq8o3Xr1hMSv2kz12cACQ"
            .parse()
            .unwrap();
        let usdc: Address = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU"
            .parse()
            .unwrap();
        assert_eq!(
            associated_token_account(&owner, &usdc, &TOKEN_PROGRAM).to_string(),
            "CTMwGLgvpCBY3oZWaWUrWCjMfkn5VgeMGc5LbhjFL95Q"
        );
    }
}
