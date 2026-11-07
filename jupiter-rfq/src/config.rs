//! The prototype's JSON configuration. It holds no key and no secret: the maker is a public
//! address, and the wallet and mids are static, since the prototype reads no chain and never
//! fills (docs/design/jupiter-rfq.md: "The prototype can use D2's marks because it never
//! fills"; a real integration needs a fresh price source and finalized RPC reads).
//!
//! Every limit and fee cap is required: none has a built-in default.

use std::path::PathBuf;
use std::str::FromStr;

use quotebook_maker::pricing::PricingParams;
use rust_decimal::Decimal;
use serde::Deserialize;
use solana_address::Address;

use crate::desk::{
    ChainError, ChainView, Clock, DeskConfig, Limits, Market, Snapshot, Spent, Spot, SpotSource,
    TxId,
};
use crate::pricing::{PricingInputs, MAX_DECIMALS};
use crate::validate::FeeBudget;

/// A configuration that cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// Not the expected JSON.
    #[error("config: {0}")]
    Json(String),
    /// A field is invalid.
    #[error("config: {0}")]
    Invalid(String),
}

/// The file's shape.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// The maker's wallet, base58. Only its address: the prototype holds no key.
    pub maker: String,
    /// The `/quote` deadline, milliseconds; Jupiter's `x-request-timeout` lowers it.
    pub quote_deadline_ms: u64,
    /// The limits.
    pub limits: LimitsFile,
    /// The fee caps.
    pub fee_budget: FeeBudgetFile,
    /// The markets.
    pub markets: Vec<MarketFile>,
    /// The static wallet the binary reports as its snapshot.
    pub wallet: WalletFile,
}

/// [`Limits`] in the file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs)]
pub struct LimitsFile {
    pub max_quotes: usize,
    pub quote_life_secs: i64,
    pub min_landing_secs: i64,
    pub max_snapshot_age_secs: i64,
    pub max_spot_age_secs: i64,
    pub max_open_swaps: usize,
    pub max_fills_per_hour: usize,
    pub halt_file: PathBuf,
}

/// The fee caps in the file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs)]
pub struct FeeBudgetFile {
    pub max_cu_limit: u32,
    pub max_cu_price: u64,
    pub max_fee_lamports: u64,
    pub max_fees_per_hour: u64,
    pub sol_floor: u64,
    pub lamports_per_signature: u64,
}

/// One market in the file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(missing_docs)]
pub struct MarketFile {
    pub base_mint: String,
    pub quote_mint: String,
    pub base_decimals: u8,
    pub quote_decimals: u8,
    pub maker_base_account: String,
    pub maker_quote_account: String,
    /// The static mid, quote tokens per base token, as a decimal string.
    pub mid: String,
    pub min_half_spread_bps: u32,
    pub max_half_spread_bps: u32,
    pub max_skew_bps: u32,
    /// The deviation guard, a fraction as a decimal string (`"0.05"` is 5%).
    pub max_deviation: String,
    pub max_base_out: u64,
    pub max_quote_out: u64,
    pub target_base_atoms: u64,
}

/// The static wallet.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WalletFile {
    /// Lamports.
    pub sol_lamports: u64,
    /// `[mint, atoms]` pairs.
    pub balances: Vec<(String, u64)>,
}

/// A loaded configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The desk's configuration.
    pub desk: DeskConfig,
    /// The `/quote` deadline.
    pub quote_deadline: std::time::Duration,
    /// The static mids, one per market.
    pub mids: Vec<Decimal>,
    /// The static wallet.
    pub wallet: (u64, Vec<(Address, u64)>),
}

fn address(field: &str, s: &str) -> Result<Address, ConfigError> {
    Address::from_str(s).map_err(|_| ConfigError::Invalid(format!("{field}: not an address")))
}

fn decimal(field: &str, s: &str) -> Result<Decimal, ConfigError> {
    Decimal::from_str(s).map_err(|_| ConfigError::Invalid(format!("{field}: not a decimal")))
}

impl ConfigFile {
    /// Parses JSON.
    ///
    /// # Errors
    ///
    /// `Json` with serde's message.
    pub fn from_json(text: &str) -> Result<Self, ConfigError> {
        serde_json::from_str(text).map_err(|e| ConfigError::Json(e.to_string()))
    }

    /// Checks every field and builds the desk's configuration.
    ///
    /// # Errors
    ///
    /// `Invalid` with the first field that fails.
    pub fn load(&self) -> Result<Loaded, ConfigError> {
        let maker = address("maker", &self.maker)?;
        let l = &self.limits;
        if l.quote_life_secs <= 0
            || l.min_landing_secs < 0
            || l.max_snapshot_age_secs <= 0
            || l.max_spot_age_secs <= 0
        {
            return Err(ConfigError::Invalid(
                "limits: times must be positive".into(),
            ));
        }
        if l.max_quotes == 0 || l.max_open_swaps == 0 || l.max_fills_per_hour == 0 {
            return Err(ConfigError::Invalid(
                "limits: counts must be positive".into(),
            ));
        }
        if self.quote_deadline_ms == 0 {
            return Err(ConfigError::Invalid(
                "quote_deadline_ms must be positive".into(),
            ));
        }
        let f = &self.fee_budget;
        let mut markets = Vec::with_capacity(self.markets.len());
        let mut mids = Vec::with_capacity(self.markets.len());
        for (i, m) in self.markets.iter().enumerate() {
            let name = |f: &str| format!("markets[{i}].{f}");
            if m.base_decimals > MAX_DECIMALS || m.quote_decimals > MAX_DECIMALS {
                return Err(ConfigError::Invalid(name("decimals: at most 18")));
            }
            let mut params = PricingParams::relative_defaults();
            params.min_half_spread_bps = m.min_half_spread_bps;
            params.max_half_spread_bps = m.max_half_spread_bps;
            params.max_skew_bps = m.max_skew_bps;
            params.max_deviation = decimal(&name("max_deviation"), &m.max_deviation)?;
            params
                .validate()
                .map_err(|e| ConfigError::Invalid(format!("{}: {e}", name("pricing"))))?;
            let mid = decimal(&name("mid"), &m.mid)?;
            if mid <= Decimal::ZERO {
                return Err(ConfigError::Invalid(name("mid: must be positive")));
            }
            let market = Market {
                base_mint: address(&name("base_mint"), &m.base_mint)?,
                quote_mint: address(&name("quote_mint"), &m.quote_mint)?,
                maker_base_account: address(&name("maker_base_account"), &m.maker_base_account)?,
                maker_quote_account: address(&name("maker_quote_account"), &m.maker_quote_account)?,
                pricing: PricingInputs {
                    base_decimals: m.base_decimals,
                    quote_decimals: m.quote_decimals,
                    params,
                },
                max_base_out: m.max_base_out,
                max_quote_out: m.max_quote_out,
                target_base_atoms: m.target_base_atoms,
            };
            if market.base_mint == market.quote_mint {
                return Err(ConfigError::Invalid(name("mints: base equals quote")));
            }
            markets.push(market);
            mids.push(mid);
        }
        if markets.is_empty() {
            return Err(ConfigError::Invalid("markets: none".into()));
        }
        let balances = self
            .wallet
            .balances
            .iter()
            .map(|(m, a)| address("wallet.balances", m).map(|m| (m, *a)))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Loaded {
            desk: DeskConfig {
                maker,
                markets,
                limits: Limits {
                    max_quotes: l.max_quotes,
                    quote_life_secs: l.quote_life_secs,
                    min_landing_secs: l.min_landing_secs,
                    max_snapshot_age_secs: l.max_snapshot_age_secs,
                    max_spot_age_secs: l.max_spot_age_secs,
                    max_open_swaps: l.max_open_swaps,
                    max_fills_per_hour: l.max_fills_per_hour,
                    max_fees_per_hour: f.max_fees_per_hour,
                    sol_floor: f.sol_floor,
                    halt_file: l.halt_file.clone(),
                },
                budget: FeeBudget {
                    max_cu_limit: f.max_cu_limit,
                    max_cu_price: f.max_cu_price,
                    max_fee_lamports: f.max_fee_lamports,
                    lamports_per_signature: f.lamports_per_signature,
                },
            },
            quote_deadline: std::time::Duration::from_millis(self.quote_deadline_ms),
            mids,
            wallet: (self.wallet.sol_lamports, balances),
        })
    }
}

/// Static mids, published "now": the binary's spot source.
pub struct StaticSpot {
    mids: Vec<Decimal>,
    clock: Box<dyn Clock>,
}

impl StaticSpot {
    /// Mids per market.
    #[must_use]
    pub fn new(mids: Vec<Decimal>, clock: Box<dyn Clock>) -> Self {
        Self { mids, clock }
    }
}

impl SpotSource for StaticSpot {
    fn spot(&self, index: usize) -> Option<Spot> {
        self.mids.get(index).map(|mid| Spot {
            mid: *mid,
            published_at: self.clock.now(),
        })
    }
}

/// A static wallet at the wall clock: the binary's chain. It never reports a landed
/// transaction, since the prototype never sends one.
pub struct StaticChain {
    sol_lamports: u64,
    balances: Vec<(Address, u64)>,
    clock: Box<dyn Clock>,
    slot: std::sync::atomic::AtomicU64,
}

impl StaticChain {
    /// A wallet of `sol_lamports` and `balances`.
    #[must_use]
    pub fn new(sol_lamports: u64, balances: Vec<(Address, u64)>, clock: Box<dyn Clock>) -> Self {
        Self {
            sol_lamports,
            balances,
            clock,
            slot: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl ChainView for StaticChain {
    fn snapshot(&self, _tracked: &[TxId]) -> Result<Snapshot, ChainError> {
        let slot = self.slot.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Snapshot {
            slot,
            clock: self.clock.now(),
            sol_lamports: self.sol_lamports,
            balances: self.balances.clone(),
            landed: Vec::new(),
        })
    }

    fn history_since(&self, _since: i64) -> Result<Vec<Spent>, ChainError> {
        Ok(Vec::new())
    }
}
