//! The demo's static configuration: which quotebook series pairs each metalbook series, and the
//! vol, rate and feed of each underlying. It holds public addresses only, no keys and no RPC URL.

use std::str::FromStr;

use metalbook_core::Underlying;
use quotebook_maker::protocol::from_hex;
use rust_decimal::Decimal;
use serde::Deserialize;
use solana_address::Address;

/// Why a configuration was refused.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// It is not the expected JSON.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// A field is not valid.
    #[error("{field}: {problem}")]
    Field {
        /// The field.
        field: &'static str,
        /// What is wrong with it.
        problem: String,
    },
}

/// One metalbook series and the quotebook series that trades its long token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeriesPair {
    /// The metalbook series account.
    pub metalbook_series: Address,
    /// The quotebook series account: the long mint as base, USDC as quote.
    pub quotebook_series: Address,
}

/// One underlying's pricing inputs and spot source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnderlyingConfig {
    /// The metal.
    pub underlying: Underlying,
    /// The Pyth push-feed account.
    pub feed_account: Address,
    /// The feed id, which the account and every series of this underlying must carry.
    pub feed_id: [u8; 32],
    /// Annualized volatility: one number, from configuration.
    pub vol: Decimal,
    /// Continuously compounded risk-free rate.
    pub rate: Decimal,
    /// Continuous carry.
    pub carry: Decimal,
}

/// The validated configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoConfig {
    /// The metalbook program.
    pub metalbook_program: Address,
    /// The quotebook program.
    pub quotebook_program: Address,
    /// The USDC mint, the collateral of every series and the quote of every pair.
    pub usdc_mint: Address,
    /// The underlyings; at most one entry for each.
    pub underlyings: Vec<UnderlyingConfig>,
    /// The series the demo may quote.
    pub pairs: Vec<SeriesPair>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    metalbook_program: String,
    quotebook_program: String,
    usdc_mint: String,
    underlyings: Vec<RawUnderlying>,
    pairs: Vec<RawPair>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUnderlying {
    underlying: String,
    feed_account: String,
    feed_id: String,
    vol: String,
    rate: String,
    carry: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPair {
    metalbook_series: String,
    quotebook_series: String,
}

fn invalid(field: &'static str, problem: impl Into<String>) -> ConfigError {
    ConfigError::Field {
        field,
        problem: problem.into(),
    }
}

fn address(field: &'static str, s: &str) -> Result<Address, ConfigError> {
    s.parse().map_err(|_| invalid(field, "not an address"))
}

fn decimal(field: &'static str, s: &str) -> Result<Decimal, ConfigError> {
    Decimal::from_str(s).map_err(|_| invalid(field, "not a decimal"))
}

impl DemoConfig {
    /// Parses and checks a configuration.
    ///
    /// # Errors
    ///
    /// [`ConfigError`]: bad JSON, an invalid field, a non-positive vol, or a duplicate underlying
    /// or metalbook series.
    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let raw: RawConfig = serde_json::from_str(json)?;
        let mut underlyings: Vec<UnderlyingConfig> = Vec::new();
        for u in &raw.underlyings {
            let underlying = match u.underlying.as_str() {
                "XAU" => Underlying::Xau,
                "XAG" => Underlying::Xag,
                _ => return Err(invalid("underlying", "must be XAU or XAG")),
            };
            if underlyings.iter().any(|o| o.underlying == underlying) {
                return Err(invalid("underlying", "listed twice"));
            }
            let feed_id = from_hex(&u.feed_id)
                .and_then(|b| <[u8; 32]>::try_from(b).ok())
                .ok_or_else(|| invalid("feed_id", "not 32 bytes of hex"))?;
            let vol = decimal("vol", &u.vol)?;
            if vol <= Decimal::ZERO {
                return Err(invalid("vol", "must be positive"));
            }
            underlyings.push(UnderlyingConfig {
                underlying,
                feed_account: address("feed_account", &u.feed_account)?,
                feed_id,
                vol,
                rate: decimal("rate", &u.rate)?,
                carry: decimal("carry", &u.carry)?,
            });
        }
        let mut pairs: Vec<SeriesPair> = Vec::new();
        for p in &raw.pairs {
            let pair = SeriesPair {
                metalbook_series: address("metalbook_series", &p.metalbook_series)?,
                quotebook_series: address("quotebook_series", &p.quotebook_series)?,
            };
            if pairs
                .iter()
                .any(|o| o.metalbook_series == pair.metalbook_series)
            {
                return Err(invalid("metalbook_series", "listed twice"));
            }
            pairs.push(pair);
        }
        Ok(Self {
            metalbook_program: address("metalbook_program", &raw.metalbook_program)?,
            quotebook_program: address("quotebook_program", &raw.quotebook_program)?,
            usdc_mint: address("usdc_mint", &raw.usdc_mint)?,
            underlyings,
            pairs,
        })
    }

    /// The entry of `underlying`.
    #[must_use]
    pub fn underlying(&self, underlying: Underlying) -> Option<&UnderlyingConfig> {
        self.underlyings.iter().find(|u| u.underlying == underlying)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_macros, clippy::unwrap_used)]

    use super::*;

    const DEVNET: &str = include_str!("../config/devnet.json");

    #[test]
    fn the_devnet_config_parses() {
        let c = DemoConfig::from_json(DEVNET).unwrap();
        assert_eq!(c.underlyings.len(), 2);
        let xag = c.underlying(Underlying::Xag).unwrap();
        assert_eq!(xag.vol, Decimal::new(30, 2));
        assert_eq!(xag.rate, Decimal::new(4, 2));
        assert_eq!(xag.carry, Decimal::ZERO);
        assert_eq!(
            c.pairs.first().unwrap().metalbook_series.to_string(),
            "B66MftMja6L6g2XFNLWHUWE2Rjq1yTHtToWQTvxsZx21"
        );
    }

    fn with(from: &str, to: &str) -> Result<DemoConfig, ConfigError> {
        DemoConfig::from_json(&DEVNET.replace(from, to))
    }

    #[test]
    fn the_replacements_below_apply() {
        for from in [
            "\"XAG\"",
            "\"0.30\"",
            "\"0.04\"",
            "f2fb02c3",
            "FPPuou5k",
            "\"pairs\"",
        ] {
            assert!(DEVNET.contains(from), "{from}");
        }
    }

    #[test]
    fn invalid_values_are_refused() {
        assert!(with("\"XAG\"", "\"XPT\"").is_err());
        assert!(
            with("\"XAG\"", "\"XAU\"").is_err(),
            "a duplicate underlying"
        );
        assert!(with("\"0.30\"", "\"0\"").is_err(), "a zero vol");
        assert!(with("\"0.30\"", "\"-0.3\"").is_err());
        assert!(with("\"0.04\"", "\"x\"").is_err());
        assert!(with("f2fb02c3", "f2fb02").is_err(), "a short feed id");
        assert!(with("FPPuou5k", "0OIl").is_err(), "not an address");
        assert!(with("\"pairs\"", "\"pears\"").is_err(), "an unknown field");
    }

    #[test]
    fn a_duplicate_series_is_refused() {
        let start = DEVNET.find("\"pairs\": [").unwrap();
        let entry = "{\"metalbook_series\": \"B66MftMja6L6g2XFNLWHUWE2Rjq1yTHtToWQTvxsZx21\", \"quotebook_series\": \"AbfhiX44ebCSpLEB8ba1gw5vL8qJr7yb7RxYk3CwSCAf\"}";
        let mut json = DEVNET.to_owned();
        json.insert_str(start + "\"pairs\": [".len(), &format!("{entry},"));
        assert!(DemoConfig::from_json(&json).is_err());
    }
}
