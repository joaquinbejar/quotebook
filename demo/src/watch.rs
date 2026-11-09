//! The checks behind `quotebook-demo-watch` (`docs/PLAN-phase2.md`, D4): one [`pass`] reads the
//! chain, the maker's `Status` and two local files, and returns what it saw and what is wrong. It
//! holds no key, signs nothing and sends no transaction. An [`AlertBook`] decides when an alert is
//! delivered again.
//!
//! # Reads
//!
//! Every chain read is at `finalized`, and the pass's clock is the finalized Clock:
//! 1. one `getMultipleAccounts`: the Clock, the payer, metalbook's config and the XAG calendar,
//!    every configured feed account and every configured metalbook series;
//! 2. one `getMultipleAccounts`: the maker's USDC account and its long-token account of each
//!    series, and the taker's USDC account;
//! 3. the writer's exposure ([`crate::writer_cap::read_exposure`]).
//!
//! The reads are from different slots, which is enough for alerts. A read that fails is not an
//! observation: it reports an `rpc` alert and resolves nothing it could not see.
//!
//! # Alert kinds
//!
//! `balance`, `inventory`, `collateral`, `allowance`, `maker-down`, `stale-spot`,
//! `risk-unavailable`, `halted`, `writer-cap`, `no-series` and `fill-error`, as the plan lists
//! them, and `rpc` for a read that failed. Each alert's `MESSAGE` is built here from numbers and
//! addresses; the one free text, the taker's failure reason, is cut to [`MAX_REASON_CHARS`]
//! printable characters.
//!
//! # `stale-spot` and the market's closures
//!
//! A spot older than [`MAX_SPOT_AGE_SECS`] alerts, except while it went stale inside a listed
//! closure ([`crate::schedule`]) and the clock is still inside that closure or within
//! [`MAX_SPOT_AGE_SECS`] of its scheduled reopening. Then the pass writes one `expected` line per
//! feed and closure, and no alert. An alert already firing when a closure starts (the spot went
//! stale before it) keeps firing. Only the two listed windows count, so a holiday alerts.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use metalbook_core::{OptionKind, Underlying};
use metalbook_sdk::{decode, pda};
use pyth_lite::{FeedId, PriceRequirements, PriceUpdate, RECEIVER_PROGRAM_ID};
use quotebook_cli::rpc::Rpc;
use quotebook_maker::protocol::StatusMessage;
use quotebook_maker::risk::TokenAccountState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use solana_account::Account;
use solana_address::Address;

use crate::chain::{parse_clock, parse_token_account, WatchedAccount, CLOCK_SYSVAR, TOKEN_PROGRAM};
use crate::config::{DemoConfig, SeriesPair};
use crate::host::associated_token_account;
use crate::logging::RotatingFile;
use crate::schedule::{closure_at, last_closure_before, market_open, Closure};
use crate::series::{classify, ListedSeries};
use crate::spot::{MAX_CONF_BPS, MAX_FUTURE_SECS, MAX_SPOT_AGE_SECS};
use crate::writer_cap::{read_exposure, Exposure, MAX_COMMITTED_COLLATERAL, MAX_WRITTEN_CONTRACTS};

/// An alert that stays active is delivered again after this long, seconds.
pub const REALERT_SECS: i64 = 3_600;
/// `maker-down` when `Status` gets no answer within this.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(2);
/// The roller lists each expiry at least this long before it (D5), seconds.
pub const LISTING_LEAD_SECS: i64 = 3 * 86_400;
/// The metal the demo trades (D5: XAG capped calls only); `no-series` reads its calendar.
pub const TRADED: Underlying = Underlying::Xag;
/// The taker's failure reason in an alert is cut to this many characters.
pub const MAX_REASON_CHARS: usize = 200;
/// The tail of the taker's log read for its last line, bytes.
pub const TAKER_LOG_TAIL: u64 = 64 * 1024;
/// Closures remembered for their `expected` line, seconds.
const EXPECTED_MEMORY_SECS: i64 = 8 * 86_400;

/// What a check found wrong.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Alert {
    /// One of the kinds in the module documentation.
    pub kind: &'static str,
    /// The observation that produced it: an alert resolves only when its scope was observed again
    /// and the alert was not reported.
    pub scope: String,
    /// The account, feed or component it is about.
    pub subject: String,
    /// One line for a human.
    pub message: String,
}

fn alert(kind: &'static str, scope: &str, subject: impl Into<String>, message: String) -> Alert {
    Alert {
        kind,
        scope: scope.to_owned(),
        subject: subject.into(),
        message,
    }
}

/// When an alert is delivered: as soon as it is seen, then again every `realert_s` while it stays
/// active; a delivery that fails is retried at the next pass. An alert resolves only when its
/// scope was observed again and it was not reported, so a failed read never resolves what it
/// could not see. An active alert whose scope was not observed is still due, marked last known.
/// (varswap's `varswap-watch` book.)
#[derive(Debug, Default)]
pub struct AlertBook {
    active: HashMap<(&'static str, String), (Alert, Option<i64>)>,
}

impl AlertBook {
    /// Records the alerts reported and the scopes observed at `now`. Returns the alerts due for
    /// delivery and the `(kind, subject)` of those resolved.
    pub fn update(
        &mut self,
        reported: &[Alert],
        observed: &[String],
        now: i64,
        realert_s: i64,
    ) -> (Vec<Alert>, Vec<(&'static str, String)>) {
        let keys: Vec<(&'static str, String)> = reported
            .iter()
            .map(|a| (a.kind, a.subject.clone()))
            .collect();
        let resolved: Vec<_> = self
            .active
            .iter()
            .filter(|(k, (a, _))| !keys.contains(k) && observed.contains(&a.scope))
            .map(|(k, _)| k.clone())
            .collect();
        for k in &resolved {
            self.active.remove(k);
        }
        for a in reported {
            self.active
                .entry((a.kind, a.subject.clone()))
                .and_modify(|e| e.0 = a.clone())
                .or_insert_with(|| (a.clone(), None));
        }
        let mut due = Vec::new();
        for (k, (a, last)) in &self.active {
            if !last.is_none_or(|l| now.saturating_sub(l) >= realert_s) {
                continue;
            }
            if keys.contains(k) {
                due.push(a.clone());
            } else {
                let mut stale = a.clone();
                stale.message = format!("{} (last known: not observed this pass)", a.message);
                due.push(stale);
            }
        }
        due.sort_by(|x, y| (x.kind, &x.subject).cmp(&(y.kind, &y.subject)));
        let mut resolved = resolved;
        resolved.sort();
        (due, resolved)
    }

    /// Records a successful delivery of `a` at `now`; a failed one is not recorded.
    pub fn delivered(&mut self, a: &Alert, now: i64) {
        if let Some(e) = self.active.get_mut(&(a.kind, a.subject.clone())) {
            e.1 = Some(now);
        }
    }

    /// Alerts active now.
    #[must_use]
    pub fn active(&self) -> usize {
        self.active.len()
    }
}

/// Floors and approvals the checks compare against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `balance` under this many lamports on the payer.
    pub min_balance: u64,
    /// `inventory` under this many USDC atoms on the maker.
    pub min_usdc: u64,
    /// `inventory` under this many long tokens on a maker account of a writable series.
    pub min_long: u64,
    /// `inventory` under this many USDC atoms on the taker.
    pub min_taker_usdc: u64,
    /// The USDC approval to the delegate, atoms: `allowance` under 20% of it.
    pub usdc_approval: u64,
    /// The long-token approval, tokens: `allowance` under 20% of it.
    pub long_approval: u64,
    /// `risk-unavailable` when the maker's snapshot is older than this, seconds.
    pub max_snapshot_age: i64,
}

impl Default for Limits {
    /// The plan's devnet values: 0.3 SOL, 1 USDC on either side, no long floor (the band starts
    /// at 0), approvals of 4 USDC and 2 long tokens (D5), and the maker's 60 s snapshot age.
    fn default() -> Self {
        Self {
            min_balance: 300_000_000,
            min_usdc: 1_000_000,
            min_long: 0,
            min_taker_usdc: 1_000_000,
            usdc_approval: 4_000_000,
            long_approval: 2,
            max_snapshot_age: 60,
        }
    }
}

/// What the watcher watches.
#[derive(Debug, Clone)]
pub struct Watched {
    /// The demo's configuration: programs, USDC, feeds and pairs.
    pub demo: DemoConfig,
    /// The maker, which is also the writer.
    pub maker: Address,
    /// The payer of the maker's admin transactions.
    pub payer: Address,
    /// The taker, whose USDC is checked when given.
    pub taker: Option<Address>,
    /// The maker's halt file.
    pub halt_file: PathBuf,
    /// The taker's `trades.jsonl` ([`TakerRun`]), when there is one.
    pub taker_log: Option<PathBuf>,
    /// The floors.
    pub limits: Limits,
}

/// The maker's `Status`.
pub trait StatusSource {
    /// One `Status` request.
    ///
    /// # Errors
    ///
    /// Any failure or no answer within [`STATUS_TIMEOUT`], as text.
    fn status(&mut self) -> Result<StatusMessage, String>;
}

/// `Status` over the maker's WebSocket, with [`STATUS_TIMEOUT`]. It takes no nonce and no
/// reservation.
#[derive(Debug)]
pub struct WsStatus {
    endpoint: String,
    runtime: tokio::runtime::Runtime,
    next_id: u64,
}

impl WsStatus {
    /// A client of `endpoint`.
    ///
    /// # Errors
    ///
    /// The runtime's I/O error.
    pub fn new(endpoint: &str) -> std::io::Result<Self> {
        Ok(Self {
            endpoint: endpoint.to_owned(),
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
            next_id: 0,
        })
    }
}

impl StatusSource for WsStatus {
    fn status(&mut self) -> Result<StatusMessage, String> {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let endpoint = self.endpoint.clone();
        self.runtime.block_on(async move {
            match tokio::time::timeout(
                STATUS_TIMEOUT,
                quotebook_maker::client::request_status(&endpoint, id),
            )
            .await
            {
                Ok(Ok(s)) => Ok(s),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err(format!("no answer within {} s", STATUS_TIMEOUT.as_secs())),
            }
        })
    }
}

/// How one run of the D5 taker ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TakerOutcome {
    /// It settled a fill.
    Filled,
    /// It had nothing to do (market closed, settlement window, `no inventory`): not a failure.
    Skipped,
    /// It failed.
    Failed,
}

/// The fields the watcher reads from one line of the taker's `trades.jsonl`. The D5 taker writes
/// one JSON object per run, ending in a newline, with at least these keys; the line may carry
/// more (series, side, price, size, fee, signature, slot, CU, balances), which the watcher
/// ignores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakerRun {
    /// When the run ended, Unix seconds.
    pub time: i64,
    /// How it ended.
    pub outcome: TakerOutcome,
    /// Why it failed or was skipped.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The last complete line of the taker's log, or `None` when the file does not exist or has no
/// complete line yet.
///
/// # Errors
///
/// An I/O error other than a missing file, or a last line that is not a [`TakerRun`].
pub fn last_taker_run(path: &Path) -> Result<Option<TakerRun>, String> {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot open the taker log: {e}")),
    };
    let len = file
        .metadata()
        .map_err(|e| format!("cannot read the taker log: {e}"))?
        .len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAKER_LOG_TAIL)))
        .map_err(|e| format!("cannot read the taker log: {e}"))?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail)
        .map_err(|e| format!("cannot read the taker log: {e}"))?;
    // Only lines that end in a newline are complete: a run may be writing the next one.
    let Some(end) = tail.iter().rposition(|b| *b == b'\n') else {
        return Ok(None);
    };
    let complete = tail.get(..end).unwrap_or_default();
    let line = complete
        .split(|b| *b == b'\n')
        .rev()
        .find(|l| !l.iter().all(u8::is_ascii_whitespace));
    let Some(line) = line else {
        return Ok(None);
    };
    serde_json::from_slice(line)
        .map(Some)
        .map_err(|e| format!("the taker log's last line is not a run: {e}"))
}

/// `text` with control characters dropped, cut to [`MAX_REASON_CHARS`].
#[must_use]
pub fn printable(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_REASON_CHARS)
        .collect()
}

/// The `publish_time` of a feed account's update, after the owner, discriminator, `Full`
/// verification, feed id, positive price and confidence checks (not its age).
///
/// # Errors
///
/// The check that failed, as text.
pub fn spot_publish_time(owner: &Address, data: &[u8], feed_id: [u8; 32]) -> Result<i64, String> {
    let update = PriceUpdate::load(&owner.to_bytes(), data, &RECEIVER_PROGRAM_ID)
        .map_err(|e| format!("{e:?}"))?;
    let requirements = PriceRequirements::new(FeedId::new(feed_id), None, MAX_CONF_BPS);
    update
        .verify(&requirements, pyth_lite::UnixTimestamp::new(0))
        .map(|v| v.publish_time().get())
        .map_err(|e| format!("{e:?}"))
}

/// The listed closure a spot that went stale may stay quiet in at `now`: the one containing
/// `now`, or the last one if `now` is within [`MAX_SPOT_AGE_SECS`] of its reopening.
#[must_use]
pub fn quiet_closure(now: i64) -> Option<Closure> {
    closure_at(now).or_else(|| {
        last_closure_before(now).filter(|c| now < c.end.saturating_add(MAX_SPOT_AGE_SECS))
    })
}

/// What a feed's age means at `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpotVerdict {
    /// Fresh enough.
    Fresh,
    /// Stale inside a listed closure (or its reopening grace): one `expected` line.
    Expected(Closure),
    /// Stale: an alert.
    Stale,
    /// Ahead of the clock by more than [`MAX_FUTURE_SECS`]: an alert.
    Ahead,
}

/// Judges a spot published at `publish_time`, at `now`.
#[must_use]
pub fn judge_spot(publish_time: i64, now: i64) -> SpotVerdict {
    let Some(age) = now.checked_sub(publish_time) else {
        return SpotVerdict::Stale;
    };
    if age < 0 {
        return if age.checked_neg().is_none_or(|lead| lead > MAX_FUTURE_SECS) {
            SpotVerdict::Ahead
        } else {
            SpotVerdict::Fresh
        };
    }
    if age <= MAX_SPOT_AGE_SECS {
        return SpotVerdict::Fresh;
    }
    let stale_since = publish_time.saturating_add(MAX_SPOT_AGE_SECS);
    match quiet_closure(now) {
        Some(c) if stale_since >= c.start => SpotVerdict::Expected(c),
        _ => SpotVerdict::Stale,
    }
}

/// What the watcher remembers between passes: the closures already reported `expected` for each
/// feed.
#[derive(Debug, Default)]
pub struct WatchState {
    expected: HashSet<(Address, i64)>,
}

/// One pass: the clock it judged on, the JSON lines it wrote, the alerts it reported and the
/// scopes it observed.
#[derive(Debug, Default)]
pub struct Pass {
    /// The finalized Clock, or the host's clock when the chain could not be read.
    pub now: i64,
    /// JSON lines for stdout.
    pub lines: Vec<Value>,
    /// What is wrong now.
    pub alerts: Vec<Alert>,
    /// The scopes actually observed.
    pub observed: Vec<String>,
}

impl Pass {
    fn observe(&mut self, scope: &str) {
        self.observed.push(scope.to_owned());
    }
}

/// One configured metalbook series as read.
#[derive(Debug, Clone)]
struct SeriesRead {
    pair: SeriesPair,
    /// The long mint and its token program, from the raw account (any state).
    long: Option<(Address, Address)>,
    listed: Result<ListedSeries, String>,
}

impl SeriesRead {
    /// Quotable and writable at `now`: listed (active, valid, this demo's), before its settlement
    /// window.
    fn before_window(&self, now: i64) -> Option<&ListedSeries> {
        self.listed.as_ref().ok().filter(|s| {
            s.expiry()
                .checked_sub(i64::from(s.window_s))
                .is_some_and(|start| now < start)
        })
    }
}

fn token(
    account: Option<&Account>,
    address: Address,
    mint: Address,
    owner: &Address,
    delegate: &Address,
) -> Result<TokenAccountState, String> {
    parse_token_account(account, &WatchedAccount { address, mint }, owner, delegate)
        .map_err(|e| e.0)
}

/// `true` when `allowance` is under 20% of `approval`.
#[must_use]
pub fn allowance_low(allowance: u64, approval: u64) -> bool {
    u128::from(allowance).saturating_mul(5) < u128::from(approval)
}

/// Whether the calendar says a series should be listed at `now`: an expiry `e` with
/// `e − LISTING_LEAD_SECS <= now < e − window_s`. Outside every such span (after an expiry, until
/// the roller's next listing; a stretch with no expiry) nothing is expected.
#[must_use]
pub fn listing_expected(expiries: &[i64], window_s: u32, now: i64) -> bool {
    expiries.iter().any(|e| {
        let from = e.saturating_sub(LISTING_LEAD_SECS);
        let until = e.saturating_sub(i64::from(window_s));
        from <= now && now < until
    })
}

/// The checks that need no chain: `Status` and the halt file. They run even when the cluster
/// cannot be read.
fn maker_checks(
    status: &mut dyn StatusSource,
    w: &Watched,
    p: &mut Pass,
    line: &mut Map<String, Value>,
) {
    // maker-down, risk-unavailable
    p.observe("maker");
    let answer = status.status();
    match &answer {
        Ok(s) => {
            p.observe("status");
            line.insert(
                "status".into(),
                json!({
                    "halted": s.halted,
                    "open_quotes": s.open_quotes,
                    "next_nonce": s.next_nonce,
                    "newest_expires_at": s.newest_expires_at,
                    "snapshot_age_secs": s.snapshot_age_secs,
                }),
            );
            let fresh = s
                .snapshot_age_secs
                .is_some_and(|a| a <= w.limits.max_snapshot_age);
            if !fresh {
                p.alerts.push(alert(
                    "risk-unavailable",
                    "status",
                    w.maker.to_string(),
                    match s.snapshot_age_secs {
                        Some(a) => format!(
                            "the maker's snapshot is {a} s old, above {}: it refuses every quote",
                            w.limits.max_snapshot_age
                        ),
                        None => "the maker has no snapshot: it refuses every quote".to_string(),
                    },
                ));
            }
        }
        Err(e) => {
            line.insert("status".into(), json!({"error": e}));
            p.alerts.push(alert(
                "maker-down",
                "maker",
                w.maker.to_string(),
                format!("the maker does not answer Status: {e}"),
            ));
        }
    }

    // halted
    let halt = match std::fs::metadata(&w.halt_file) {
        Ok(_) => Some(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    };
    // Decided only from what was seen: the file says halted by itself, but its absence says
    // nothing about the service unless `Status` answered.
    let verdict = match (halt, &answer) {
        (Some(true), _) => Some((true, true)),
        (Some(false), Ok(s)) => Some((false, s.halted)),
        (Some(false), Err(_)) | (None, _) => None,
    };
    if let Some((file, service)) = verdict {
        p.observe("halt");
        if file || service {
            p.alerts.push(alert(
                "halted",
                "halt",
                w.maker.to_string(),
                format!(
                    "the maker is halted{}: no new quote until {} is removed",
                    if file { " by its halt file" } else { "" },
                    w.halt_file.display()
                ),
            ));
        }
    }
}

/// One pass at the watcher's current settings. `host_now` is used only when the chain cannot be
/// read.
#[allow(clippy::too_many_lines)]
pub fn pass(
    rpc: &mut dyn Rpc,
    status: &mut dyn StatusSource,
    w: &Watched,
    state: &mut WatchState,
    host_now: i64,
) -> Pass {
    let mut p = Pass {
        now: host_now,
        ..Pass::default()
    };
    let demo = &w.demo;
    let mut line = Map::new();

    // 1. Clock, payer, metalbook config and calendar, feeds, series.
    let config_pda = pda::config(&demo.metalbook_program).0;
    let calendar_pda = pda::calendar(&demo.metalbook_program, TRADED as u8).0;
    let mut first = vec![CLOCK_SYSVAR, w.payer, config_pda, calendar_pda];
    first.extend(demo.underlyings.iter().map(|u| u.feed_account));
    first.extend(demo.pairs.iter().map(|p| p.metalbook_series));
    let read = rpc
        .finalized_accounts(&first)
        .map_err(|e| e.to_string())
        .and_then(|(slot, got)| {
            if got.len() == first.len() {
                Ok((slot, got))
            } else {
                Err("getMultipleAccounts returned the wrong number of accounts".into())
            }
        })
        .and_then(|(slot, got)| {
            let clock =
                parse_clock(got.first().cloned().flatten().as_ref(), slot).map_err(|e| e.0)?;
            Ok((slot, clock.get(), got))
        });
    let (slot, now, got) = match read {
        Ok(r) => r,
        Err(e) => {
            p.alerts.push(alert(
                "rpc",
                "rpc",
                "cluster",
                format!("the watcher cannot read the cluster at finalized: {e}"),
            ));
            maker_checks(status, w, &mut p, &mut line);
            line.insert("now".into(), json!(host_now));
            line.insert("error".into(), json!(e));
            p.lines.insert(0, json!({ "pass": Value::Object(line) }));
            return p;
        }
    };
    p.now = now;
    p.observe("rpc");
    line.insert("now".into(), json!(now));
    line.insert("slot".into(), json!(slot));
    let mut got = got.into_iter().skip(1);
    let payer = got.next().flatten();
    let config_account = got.next().flatten();
    let calendar_account = got.next().flatten();
    let feeds: Vec<Option<Account>> = got.by_ref().take(demo.underlyings.len()).collect();
    let series_accounts: Vec<Option<Account>> = got.collect();

    // balance
    let lamports = payer.as_ref().map_or(0, |a| a.lamports);
    line.insert("payer_lamports".into(), json!(lamports));
    p.observe("balance");
    if lamports < w.limits.min_balance {
        p.alerts.push(alert(
            "balance",
            "balance",
            w.payer.to_string(),
            format!(
                "payer {} has {lamports} lamports, under {}: top it up from 77ie…CPPV with `solana transfer --url devnet`",
                w.payer, w.limits.min_balance
            ),
        ));
    }

    // Series.
    let series: Vec<SeriesRead> = demo
        .pairs
        .iter()
        .zip(series_accounts)
        .map(|(pair, account)| {
            let Some(account) = account else {
                return SeriesRead {
                    pair: *pair,
                    long: None,
                    listed: Err("not found".into()),
                };
            };
            let long = decode::series(&demo.metalbook_program, &account.owner, &account.data)
                .ok()
                .map(|s| {
                    (
                        Address::new_from_array(s.long_mint),
                        Address::new_from_array(s.token_program),
                    )
                });
            SeriesRead {
                pair: *pair,
                long,
                listed: classify(demo, pair, &account.owner, &account.data)
                    .map_err(|e| e.to_string()),
            }
        })
        .collect();
    let writable: Vec<&ListedSeries> = series
        .iter()
        .filter_map(|s| s.before_window(now))
        .filter(|s| s.underlying() == TRADED)
        .collect();
    line.insert(
        "series".into(),
        Value::Array(
            series
                .iter()
                .map(|s| {
                    json!({
                        "series": s.pair.metalbook_series.to_string(),
                        "listed": s.listed.is_ok(),
                        "before_window": s.before_window(now).is_some(),
                        "skip": s.listed.as_ref().err(),
                    })
                })
                .collect(),
        ),
    );

    // 2. Token accounts.
    let delegate = quotebook_sdk::pda::delegate(&demo.quotebook_program, &w.maker).0;
    let maker_usdc = associated_token_account(&w.maker, &demo.usdc_mint, &TOKEN_PROGRAM);
    let longs: Vec<(Address, Address, Address)> = series
        .iter()
        .filter_map(|s| {
            s.long.map(|(mint, program)| {
                (
                    s.pair.metalbook_series,
                    associated_token_account(&w.maker, &mint, &program),
                    mint,
                )
            })
        })
        .collect();
    let taker_usdc = w
        .taker
        .map(|t| associated_token_account(&t, &demo.usdc_mint, &TOKEN_PROGRAM));
    let mut second = vec![maker_usdc];
    second.extend(longs.iter().map(|l| l.1));
    second.extend(taker_usdc);
    let tokens = rpc
        .finalized_accounts(&second)
        .map_err(|e| e.to_string())
        .and_then(|(_, got)| {
            if got.len() == second.len() {
                Ok(got)
            } else {
                Err("getMultipleAccounts returned the wrong number of accounts".to_string())
            }
        });
    p.observe("rpc:tokens");
    let mut maker_usdc_amount: Option<u64> = None;
    // Low allowances on empty long-token accounts, kept until the writer cap has been read.
    let mut empty_low_allowance = Vec::new();
    match tokens {
        Err(e) => p.alerts.push(alert(
            "rpc",
            "rpc:tokens",
            "token accounts",
            format!("the token accounts cannot be read at finalized: {e}"),
        )),
        Ok(got) => {
            let mut got = got.into_iter();
            let mut accounts = Vec::new();
            let usdc = token(
                got.next().flatten().as_ref(),
                maker_usdc,
                demo.usdc_mint,
                &w.maker,
                &delegate,
            );
            p.observe("inventory");
            p.observe("allowance");
            match usdc {
                Ok(s) => {
                    maker_usdc_amount = Some(s.amount);
                    accounts.push(json!({"account": maker_usdc.to_string(), "role": "maker usdc", "amount": s.amount, "allowance": s.delegated_amount}));
                    if s.amount < w.limits.min_usdc {
                        p.alerts.push(alert(
                            "inventory",
                            "inventory",
                            maker_usdc.to_string(),
                            format!(
                                "the maker's USDC account {maker_usdc} holds {} atoms, under {}",
                                s.amount, w.limits.min_usdc
                            ),
                        ));
                    }
                    if allowance_low(s.delegated_amount, w.limits.usdc_approval) {
                        p.alerts.push(alert(
                            "allowance",
                            "allowance",
                            maker_usdc.to_string(),
                            format!(
                                "the delegate may move {} USDC atoms from {maker_usdc}, under 20% of the {} approved: approve again",
                                s.delegated_amount, w.limits.usdc_approval
                            ),
                        ));
                    }
                }
                Err(e) => p.alerts.push(alert(
                    "inventory",
                    "inventory",
                    maker_usdc.to_string(),
                    format!("the maker's USDC account is not usable: {e}"),
                )),
            }
            for (series_address, address, mint) in &longs {
                let account = got.next().flatten();
                let live = writable
                    .iter()
                    .any(|s| s.metalbook_series == *series_address);
                match token(account.as_ref(), *address, *mint, &w.maker, &delegate) {
                    Ok(s) => {
                        accounts.push(json!({"account": address.to_string(), "role": "maker long", "series": series_address.to_string(), "amount": s.amount, "allowance": s.delegated_amount}));
                        // Only a series still before its window is quoted and approved; an old
                        // one's approval is revoked after settlement (D5).
                        if !live {
                            continue;
                        }
                        if s.amount < w.limits.min_long {
                            p.alerts.push(alert(
                                "inventory",
                                "inventory",
                                address.to_string(),
                                format!(
                                    "the maker's long-token account {address} holds {}, under {}",
                                    s.amount, w.limits.min_long
                                ),
                            ));
                        }
                        if allowance_low(s.delegated_amount, w.limits.long_approval) {
                            let low = alert(
                                "allowance",
                                "allowance",
                                address.to_string(),
                                format!(
                                    "the delegate may move {} long tokens from {address}, under 20% of the {} approved: approve again",
                                    s.delegated_amount, w.limits.long_approval
                                ),
                            );
                            if s.amount == 0 {
                                empty_low_allowance.push(low);
                            } else {
                                p.alerts.push(low);
                            }
                        }
                    }
                    Err(e) if live => p.alerts.push(alert(
                        "inventory",
                        "inventory",
                        address.to_string(),
                        format!("the maker's long-token account is not usable: {e}"),
                    )),
                    Err(_) => {}
                }
            }
            if let (Some(taker), Some(address)) = (w.taker, taker_usdc) {
                match token(
                    got.next().flatten().as_ref(),
                    address,
                    demo.usdc_mint,
                    &taker,
                    &Address::default(),
                ) {
                    Ok(s) => {
                        accounts.push(json!({"account": address.to_string(), "role": "taker usdc", "amount": s.amount}));
                        if s.amount < w.limits.min_taker_usdc {
                            p.alerts.push(alert(
                                "inventory",
                                "inventory",
                                address.to_string(),
                                format!(
                                    "the taker's USDC account {address} holds {} atoms, under {}: its round trips are running out",
                                    s.amount, w.limits.min_taker_usdc
                                ),
                            ));
                        }
                    }
                    Err(e) => p.alerts.push(alert(
                        "inventory",
                        "inventory",
                        address.to_string(),
                        format!("the taker's USDC account is not usable: {e}"),
                    )),
                }
            }
            line.insert("accounts".into(), Value::Array(accounts));
        }
    }

    // 3. Writer cap.
    p.observe("writer");
    let exposure: Option<Exposure> = match read_exposure(rpc, &demo.metalbook_program, &w.maker) {
        Ok(e) => {
            line.insert(
                "exposure".into(),
                json!({"contracts": e.contracts, "committed": e.committed, "positions": e.positions}),
            );
            if e.over_cap() {
                p.alerts.push(alert(
                    "writer-cap",
                    "writer",
                    w.maker.to_string(),
                    format!(
                        "the writer has {} contracts and {} USDC atoms committed, above the cap of {MAX_WRITTEN_CONTRACTS} and {MAX_COMMITTED_COLLATERAL}: a bug, or the key used elsewhere",
                        e.contracts, e.committed
                    ),
                ));
            }
            Some(e)
        }
        Err(e) => {
            p.alerts.push(alert(
                "writer-cap",
                "writer",
                w.maker.to_string(),
                format!("the writer's positions cannot be read at finalized, so the cap fails closed and nothing is minted: {e}"),
            ));
            None
        }
    };

    // An empty account needs no allowance only when nothing can be minted into it: the cap was
    // read and is full. With room, or with the cap unread, it may be about to receive tokens.
    if !exposure.is_some_and(|e| !e.has_room()) {
        p.alerts.append(&mut empty_low_allowance);
    }

    // collateral: a cycle could write one more contract, but the USDC cannot cover it.
    // Observed only when both inputs were read, so a failed read resolves nothing.
    if exposure.is_some() && maker_usdc_amount.is_some() {
        p.observe("collateral");
    }
    let cheapest = writable
        .iter()
        .filter(|s| s.terms.key().kind() == OptionKind::Call)
        .filter_map(|s| s.terms.max_payout_per_contract().ok())
        .map(metalbook_core::UsdcAtoms::get)
        .min();
    if let (Some(e), Some(m), Some(usdc)) = (exposure, cheapest, maker_usdc_amount) {
        if e.room_for(m) && usdc < m {
            p.alerts.push(alert(
                "collateral",
                "collateral",
                w.maker.to_string(),
                format!(
                    "the writer holds {usdc} USDC atoms, under the {m} one contract locks, while the cap has room for it: the next cycle cannot write"
                ),
            ));
        }
    }

    // stale-spot
    let mut spots = Vec::new();
    p.observe("spot");
    state
        .expected
        .retain(|(_, start)| now.saturating_sub(*start) < EXPECTED_MEMORY_SECS);
    for (u, account) in demo.underlyings.iter().zip(feeds) {
        let feed = u.feed_account;
        let judged = account
            .ok_or_else(|| "feed account not found".to_string())
            .and_then(|a| spot_publish_time(&a.owner, &a.data, u.feed_id))
            .map(|t| (t, judge_spot(t, now)));
        let name = format!("{:?}", u.underlying);
        match judged {
            Ok((t, verdict)) => {
                spots.push(
                    json!({"underlying": name, "publish_time": t, "age": now.saturating_sub(t)}),
                );
                match verdict {
                    SpotVerdict::Fresh => {}
                    SpotVerdict::Expected(c) => {
                        if state.expected.insert((feed, c.start)) {
                            p.lines.push(json!({
                                "expected": "stale-spot",
                                "subject": feed.to_string(),
                                "underlying": name,
                                "closure_start": c.start,
                                "closure_end": c.end,
                                "message": format!("{name} spot stale during a listed closure; an alert from {} s after the reopening", MAX_SPOT_AGE_SECS),
                            }));
                        }
                    }
                    SpotVerdict::Stale => p.alerts.push(alert(
                        "stale-spot",
                        "spot",
                        feed.to_string(),
                        format!(
                            "the {name} spot is {} s old, above {MAX_SPOT_AGE_SECS}, outside a listed closure: the maker quotes nothing on it",
                            now.saturating_sub(t)
                        ),
                    )),
                    SpotVerdict::Ahead => p.alerts.push(alert(
                        "stale-spot",
                        "spot",
                        feed.to_string(),
                        format!("the {name} spot's publish_time {t} is ahead of the clock {now}"),
                    )),
                }
            }
            Err(e) => {
                spots.push(json!({"underlying": name, "error": e}));
                p.alerts.push(alert(
                    "stale-spot",
                    "spot",
                    feed.to_string(),
                    format!("the {name} feed account gives no usable spot: {e}"),
                ));
            }
        }
    }
    line.insert("spots".into(), Value::Array(spots));

    maker_checks(status, w, &mut p, &mut line);

    // no-series
    p.observe("series");
    let quotable = market_open(now) && !writable.is_empty();
    let calendar = calendar_account
        .as_ref()
        .ok_or_else(|| "calendar not found".to_string())
        .and_then(|a| {
            decode::calendar(&demo.metalbook_program, &a.owner, &a.data)
                .map_err(|e| format!("calendar: {e:?}"))
        })
        .map(|c| {
            c.expiries
                .iter()
                .take(usize::from(c.count))
                .copied()
                .collect::<Vec<i64>>()
        });
    let window_s = config_account
        .as_ref()
        .ok_or_else(|| "metalbook config not found".to_string())
        .and_then(|a| {
            decode::config(&demo.metalbook_program, &a.owner, &a.data)
                .map_err(|e| format!("metalbook config: {e:?}"))
        })
        .map(|c| c.window_s);
    line.insert("quotable".into(), json!(quotable));
    if market_open(now) && !quotable {
        let expected = match (&calendar, &window_s) {
            (Ok(e), Ok(win)) => Ok(listing_expected(e, *win, now)),
            (Err(e), _) | (_, Err(e)) => Err(e.clone()),
        };
        // A full cap blocks the next series on purpose (D5): silent.
        let blocked = exposure.is_some_and(|e| !e.has_room());
        match expected {
            Ok(true) if !blocked => p.alerts.push(alert(
                "no-series",
                "series",
                "XAG",
                "the market is open and the calendar lists an expiry due, but no configured XAG series is quotable: roll it (D5) and restart the maker with the new pair".to_string(),
            )),
            Err(e) if !blocked => p.alerts.push(alert(
                "no-series",
                "series",
                "XAG",
                format!("no configured XAG series is quotable and the calendar cannot be read: {e}"),
            )),
            _ => {}
        }
    }

    // fill-error
    if let Some(path) = &w.taker_log {
        p.observe("taker");
        match last_taker_run(path) {
            Ok(Some(run)) => {
                line.insert(
                    "taker".into(),
                    json!({"time": run.time, "outcome": run.outcome}),
                );
                if run.outcome == TakerOutcome::Failed {
                    p.alerts.push(alert(
                        "fill-error",
                        "taker",
                        "taker",
                        format!(
                            "the taker's run at {} failed: {}",
                            run.time,
                            printable(run.reason.as_deref().unwrap_or("no reason given"))
                        ),
                    ));
                }
            }
            Ok(None) => {}
            Err(e) => p
                .alerts
                .push(alert("fill-error", "taker", "taker", printable(&e))),
        }
    }

    p.lines.insert(0, json!({ "pass": Value::Object(line) }));
    p
}

/// Where the JSON lines go: stdout, or a file rotated by size (10 MB, 5 kept, in the binary), so
/// the watcher's output stays bounded on disk.
#[derive(Debug)]
pub enum LineOut {
    /// Standard output.
    Stdout,
    /// A rotated file.
    File(RotatingFile),
}

impl LineOut {
    /// Writes one line in a single write, so a rotation never splits it.
    ///
    /// # Errors
    ///
    /// The I/O error.
    pub fn line(&mut self, value: &Value) -> std::io::Result<()> {
        let mut text = value.to_string();
        text.push('\n');
        match self {
            Self::Stdout => {
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(text.as_bytes())?;
                stdout.flush()
            }
            Self::File(f) => {
                f.write_all(text.as_bytes())?;
                f.flush()
            }
        }
    }
}

/// One line of the alert log for `a` at `now`: UTC time, kind, subject and message, separated by
/// spaces, with any line break in the subject or message replaced by a space so one alert is one
/// line.
#[must_use]
pub fn alert_log_line(a: &Alert, now: i64) -> String {
    let flat = |s: &str| s.replace(['\n', '\r'], " ");
    let time = crate::schedule::utc_iso(now).unwrap_or_else(|| now.to_string());
    format!(
        "{time} {} {} {}\n",
        a.kind,
        flat(&a.subject),
        flat(&a.message)
    )
}

/// Appends `a` to the alert log `w` in one write. `true` if it was written.
pub fn log_alert(w: &mut impl std::io::Write, a: &Alert, now: i64) -> bool {
    match w
        .write_all(alert_log_line(a, now).as_bytes())
        .and_then(|()| w.flush())
    {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(kind = a.kind, error = %e, "alert log not written; retried at the next pass");
            false
        }
    }
}

/// The longest an alert command may run before it is killed and retried.
pub const ALERT_CMD_TIMEOUT: Duration = Duration::from_secs(10);

/// [`deliver_within`] with [`ALERT_CMD_TIMEOUT`].
#[must_use]
pub fn deliver(cmd: Option<&str>, a: &Alert) -> bool {
    deliver_within(cmd, a, ALERT_CMD_TIMEOUT)
}

/// Runs `cmd` with `sh -c` for one alert, with the alert in `QUOTEBOOK_ALERT_KIND`,
/// `QUOTEBOOK_ALERT_SUBJECT` and `QUOTEBOOK_ALERT_MESSAGE` (never on the command line). `true` if
/// it exited 0 within `timeout`, or there is no command. A command still running at `timeout` is
/// killed, so it cannot stop the watcher's loop.
#[must_use]
pub fn deliver_within(cmd: Option<&str>, a: &Alert, timeout: Duration) -> bool {
    let Some(cmd) = cmd.filter(|c| !c.trim().is_empty()) else {
        return true;
    };
    let mut child = match std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("QUOTEBOOK_ALERT_KIND", a.kind)
        .env("QUOTEBOOK_ALERT_SUBJECT", &a.subject)
        .env("QUOTEBOOK_ALERT_MESSAGE", &a.message)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(kind = a.kind, error = %e, "alert command did not start; retried at the next pass");
            return false;
        }
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => return true,
            Ok(Some(s)) => {
                tracing::warn!(kind = a.kind, status = %s, "alert command failed; retried at the next pass");
                return false;
            }
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(
                    kind = a.kind,
                    "alert command timed out and was killed; retried at the next pass"
                );
                return false;
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(kind = a.kind, error = %e, "alert command not awaited; retried at the next pass");
                return false;
            }
        }
    }
}
