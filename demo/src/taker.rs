//! The demo taker (`docs/PLAN-phase2.md`, D5): one run buys or sells 1 lot from the demo maker
//! and appends one line to `trades.jsonl`. Devnet only, and only its own maker.
//!
//! One run:
//!
//! 1. reads the quotable series ([`crate::series::read_series`]) and keeps the nearest expiry
//!    still before its settlement window;
//! 2. reads both sides' balances at `finalized` and picks the side ([`decide`]): it sells back
//!    what it holds and buys otherwise, so its inventory stays near flat inside the maker's band;
//! 3. requests a quote for 1 lot, checks it as `quotebook-taker` does (signature, every rule
//!    `settle` applies, and the maker's token accounts) with the demo maker as the only maker
//!    allowed, and settles it;
//! 4. reads the transaction and both sides' balances at `finalized`;
//! 5. appends one JSON line ([`TradeLine`]) to `trades.jsonl`, the contract D4's watcher reads:
//!    `time`, `outcome` and, for a skip or a failure, `reason`, plus the fill's details.
//!
//! It skips, as `skipped` and never as a failure, outside market hours, while the maker is halted,
//! with no quotable series or inside the settlement window, and when neither side has anything to
//! sell (`no inventory`).

use std::path::Path;
use std::time::{Duration, SystemTime};

use quotebook_cli::error::CliError;
use quotebook_maker::client::ClientError;
use quotebook_maker::protocol::{RejectReason, TakerSide};
use serde::Serialize;
use solana_address::Address;

use crate::series::ListedSeries;

/// Lots per trade.
pub const SIZE_LOTS: u64 = 1;

/// Token balances around one trade, atoms (long tokens have 0 decimals); a missing account is 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Balances {
    /// The taker's USDC.
    pub taker_usdc: u64,
    /// The taker's long tokens of the series.
    pub taker_long: u64,
    /// The maker's USDC.
    pub maker_usdc: u64,
    /// The maker's long tokens of the series.
    pub maker_long: u64,
}

/// What one run knows before it trades.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Situation {
    /// The finalized Clock.
    pub now: i64,
    /// Whether the metals market is open.
    pub market_open: bool,
    /// Whether the maker's halt file exists.
    pub halted: bool,
    /// The quotable series, as read.
    pub series: Vec<ListedSeries>,
    /// The maker's band top (`market_limits.max_long`): it buys back only below it.
    pub max_maker_long: u64,
}

/// What a run does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing, for this reason.
    Skip(&'static str),
    /// Trades 1 lot of `series` on `side`.
    Trade {
        /// The series.
        series: ListedSeries,
        /// The taker's side.
        side: TakerSide,
    },
}

/// The series to trade: the nearest expiry still before its settlement window.
///
/// # Errors
///
/// The reason to skip.
pub fn choose_series(s: &Situation) -> Result<ListedSeries, &'static str> {
    if !s.market_open {
        return Err("market closed");
    }
    if s.halted {
        return Err("maker halted");
    }
    if s.series.is_empty() {
        return Err("no quotable series");
    }
    s.series
        .iter()
        .filter(|l| {
            l.expiry()
                .checked_sub(i64::from(l.window_s))
                .is_some_and(|start| s.now < start)
        })
        .min_by_key(|l| l.expiry())
        .copied()
        .ok_or("settlement window")
}

/// The side for `series` given the balances: sell back what the taker holds, else buy; the
/// other side when that one cannot fill; `no inventory` when neither can.
#[must_use]
pub fn decide(s: &Situation, series: ListedSeries, b: &Balances) -> Decision {
    let size = SIZE_LOTS;
    let can_buy = b.maker_long >= size;
    let can_sell = b.taker_long >= size && b.maker_long.saturating_add(size) <= s.max_maker_long;
    let side = if can_sell {
        TakerSide::Sell
    } else if can_buy {
        TakerSide::Buy
    } else {
        return Decision::Skip("no inventory");
    };
    Decision::Trade { series, side }
}

/// The reason to record when the maker's risk limits refused the quote: nothing was signed or
/// sent, so the run is a skip and not a failure. A low allowance or balance ends here.
#[must_use]
pub fn refused_by_limits(error: &CliError) -> Option<String> {
    match error {
        CliError::Quote(ClientError::Rejected {
            reason: RejectReason::RiskLimit,
            message,
        }) => Some(format!("maker refused: {message}")),
        _ => None,
    }
}

/// How long a `roll.lock` counts as a live cycle. A cycle drains, sends and restarts in minutes;
/// a lock left by a killed cycle must not stop the taker for good.
pub const ROLL_LOCK_MAX_AGE: Duration = Duration::from_secs(900);

/// Whether a roll holds `lock` (a directory) at `now`: the lock exists and is younger than
/// [`ROLL_LOCK_MAX_AGE`]. A lock whose age cannot be read counts as held.
#[must_use]
pub fn roll_in_progress(lock: &Path, now: SystemTime) -> bool {
    let Ok(meta) = std::fs::metadata(lock) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return true;
    };
    now.duration_since(modified)
        .map_or(true, |age| age < ROLL_LOCK_MAX_AGE)
}

/// How a run ended, as D4's watcher reads it.
pub use crate::watch::TakerOutcome as Outcome;

/// One line of `trades.jsonl`. `time`, `outcome` and `reason` are D4's contract; the rest is
/// present for a trade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TradeLine {
    /// When the run ended, Unix seconds (local clock).
    pub time: i64,
    /// How it ended.
    pub outcome: Outcome,
    /// Why it was skipped or failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The quotebook series.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "opt_address"
    )]
    pub series: Option<Address>,
    /// The metalbook series.
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "opt_address"
    )]
    pub metalbook_series: Option<Address>,
    /// The taker's side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<TakerSide>,
    /// The quote's price, ticks of the series' quote lot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_ticks: Option<u64>,
    /// Lots.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_lots: Option<u64>,
    /// USDC paid or received, atoms, before the fee.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_atoms: Option<u64>,
    /// The series' taker fee, USDC atoms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_atoms: Option<u64>,
    /// The `settle` transaction, once sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Its slot, read at `finalized`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u64>,
    /// Its compute units.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cu: Option<u64>,
    /// Its network fee, lamports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_fee_lamports: Option<u64>,
    /// Balances before, at `finalized`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balances_before: Option<Balances>,
    /// Balances after, at `finalized`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balances_after: Option<Balances>,
}

// The `Serialize` derive hands a reference to the field.
#[allow(clippy::ref_option)]
fn opt_address<S: serde::Serializer>(a: &Option<Address>, s: S) -> Result<S::Ok, S::Error> {
    match a {
        Some(a) => s.collect_str(a),
        None => s.serialize_none(),
    }
}

impl TradeLine {
    /// A line with only `time`, `outcome` and `reason`.
    #[must_use]
    pub fn bare(time: i64, outcome: Outcome, reason: Option<String>) -> Self {
        Self {
            time,
            outcome,
            reason,
            series: None,
            metalbook_series: None,
            side: None,
            price_ticks: None,
            size_lots: None,
            quote_atoms: None,
            fee_atoms: None,
            signature: None,
            slot: None,
            cu: None,
            tx_fee_lamports: None,
            balances_before: None,
            balances_after: None,
        }
    }
}

/// The slot, compute units and fee of a `getTransaction` answer, and whether it failed.
#[must_use]
pub fn transaction_facts(tx: &serde_json::Value) -> (Option<u64>, Option<u64>, Option<u64>, bool) {
    let meta = &tx["meta"];
    (
        tx["slot"].as_u64(),
        meta["computeUnitsConsumed"].as_u64(),
        meta["fee"].as_u64(),
        !meta["err"].is_null(),
    )
}

/// Appends `line` to the log at `path`, as one JSON object and a newline in one write.
///
/// # Errors
///
/// The I/O or encoding error.
pub fn append(path: &std::path::Path, line: &TradeLine) -> std::io::Result<()> {
    use std::io::Write;
    let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
    text.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::disallowed_macros,
        clippy::indexing_slicing
    )]

    use super::*;
    use crate::test_support::capped_call;

    const EXPIRY: i64 = 1_794_603_600;

    fn situation(now: i64) -> Situation {
        Situation {
            now,
            market_open: true,
            halted: false,
            series: vec![capped_call(1, 52, EXPIRY)],
            max_maker_long: 2,
        }
    }

    fn b(taker_long: u64, maker_long: u64) -> Balances {
        Balances {
            taker_usdc: 8_770_000,
            taker_long,
            maker_usdc: 4_000_000,
            maker_long,
        }
    }

    #[test]
    fn it_skips_closed_halted_empty_and_the_settlement_window() {
        let open = situation(EXPIRY - 86_400);
        assert!(choose_series(&open).is_ok());
        let closed = Situation {
            market_open: false,
            ..open.clone()
        };
        assert_eq!(choose_series(&closed), Err("market closed"));
        let halted = Situation {
            halted: true,
            ..open.clone()
        };
        assert_eq!(choose_series(&halted), Err("maker halted"));
        let empty = Situation {
            series: Vec::new(),
            ..open.clone()
        };
        assert_eq!(choose_series(&empty), Err("no quotable series"));
        let window = open.series[0].window_s;
        let late = situation(EXPIRY - i64::from(window));
        assert_eq!(choose_series(&late), Err("settlement window"));
    }

    #[test]
    fn it_takes_the_nearest_expiry_still_tradable() {
        let mut s = situation(EXPIRY - 86_400);
        s.series.push(capped_call(2, 52, EXPIRY + 7 * 86_400));
        assert_eq!(choose_series(&s).unwrap().expiry(), EXPIRY);
        s.now = EXPIRY - 60;
        assert_eq!(choose_series(&s).unwrap().expiry(), EXPIRY + 7 * 86_400);
    }

    #[test]
    fn sides_alternate_and_stay_inside_the_band() {
        let s = situation(EXPIRY - 86_400);
        let series = s.series[0];
        let side = |taker, maker| match decide(&s, series, &b(taker, maker)) {
            Decision::Trade { side, .. } => Some(side),
            Decision::Skip(_) => None,
        };
        assert_eq!(side(0, 2), Some(TakerSide::Buy), "buy first");
        assert_eq!(side(1, 1), Some(TakerSide::Sell), "then sell back");
        assert_eq!(
            side(1, 2),
            Some(TakerSide::Buy),
            "the maker's band is full: buy"
        );
        assert_eq!(side(1, 0), Some(TakerSide::Sell));
        assert_eq!(side(0, 0), None, "no inventory on either side");
        assert_eq!(decide(&s, series, &b(0, 0)), Decision::Skip("no inventory"));
    }

    #[test]
    fn a_roll_lock_holds_the_taker_only_while_fresh() {
        let lock = std::env::temp_dir().join(format!("roll-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir(&lock);
        let now = SystemTime::now();
        assert!(!roll_in_progress(&lock, now), "no lock");
        std::fs::create_dir(&lock).unwrap();
        assert!(roll_in_progress(&lock, SystemTime::now()), "a fresh lock");
        assert!(
            !roll_in_progress(&lock, now + ROLL_LOCK_MAX_AGE + Duration::from_secs(60)),
            "a stale lock does not block for ever"
        );
        std::fs::remove_dir(&lock).unwrap();
    }

    #[test]
    fn only_a_risk_limit_refusal_is_a_skip() {
        let rejected = |reason| {
            CliError::Quote(ClientError::Rejected {
                reason,
                message: "funds: balance or allowance".into(),
            })
        };
        assert_eq!(
            refused_by_limits(&rejected(RejectReason::RiskLimit)).as_deref(),
            Some("maker refused: funds: balance or allowance")
        );
        assert_eq!(refused_by_limits(&rejected(RejectReason::Internal)), None);
        assert_eq!(
            refused_by_limits(&CliError::Rpc("down".into())),
            None,
            "anything else stays a failure"
        );
    }

    #[test]
    fn a_line_carries_the_watchers_contract() {
        let line = TradeLine::bare(10, Outcome::Skipped, Some("no inventory".into()));
        let v: serde_json::Value = serde_json::to_value(&line).unwrap();
        assert_eq!(v["time"], 10);
        assert_eq!(v["outcome"], "skipped");
        assert_eq!(v["reason"], "no inventory");
        assert!(v.get("signature").is_none());
        let dir = std::env::temp_dir().join(format!("quotebook-taker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trades.jsonl");
        append(&path, &line).unwrap();
        let filled = TradeLine {
            side: Some(TakerSide::Buy),
            signature: Some("sig".into()),
            ..TradeLine::bare(11, Outcome::Filled, None)
        };
        append(&path, &filled).unwrap();
        let run = crate::watch::last_taker_run(&path).unwrap().unwrap();
        assert_eq!(run.time, 11);
        assert_eq!(run.outcome, Outcome::Filled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transaction_facts_read_the_meta() {
        let tx = serde_json::json!({
            "slot": 7,
            "meta": {"computeUnitsConsumed": 31_000, "fee": 5_000, "err": null}
        });
        assert_eq!(
            transaction_facts(&tx),
            (Some(7), Some(31_000), Some(5_000), false)
        );
        let failed =
            serde_json::json!({"slot": 7, "meta": {"err": {"InstructionError": [1, "x"]}}});
        assert!(transaction_facts(&failed).3);
    }
}
