//! The writer cycle (`docs/PLAN-phase2.md`, D5): the demo maker writes metalbook XAG capped calls
//! and sells their long tokens through quotebook. Devnet only.
//!
//! One run reads the chain at `finalized` ([`read_view`]), plans what is missing ([`plan`]) and
//! does only that ([`run_cycle`]):
//!
//! 1. **Pairs:** every tradable expiry of the 1 oz XAG capped calls gets one pair, the strike
//!    nearest a fresh spot. A pair already configured for the expiry is kept, and without one a
//!    series the maker has an open position on is taken, so the strike is chosen once even
//!    after a run that minted and then failed.
//! 2. **Series:** a pair without a quotebook series gets one (`create_series`, signed by the
//!    quotebook admin), with series 1's terms: `base_lot` 1, `quote_lot` 10,000, `fee_bps` 0.
//! 3. **Mint:** up to the inventory band, the unlocked USDC and the writer cap, over every
//!    tradable pair in expiry order.
//! 4. **Approve:** the delegate on each pair's long-token account and on the USDC account.
//! 5. **After settlement:** `claim_writer` on every settled or voided position, `redeem` of the
//!    maker's own long tokens of a settled series, and a token `Revoke` on its long-token
//!    account, for every series paired or written. A pair with nothing left to do leaves the configuration.
//!
//! Steps 3 to 5 spend USDC that bid quotes reserve or replace an allowance, so they run only
//! **between a D1 halt and drain and a finalized confirmation**: the halt file stops new quotes,
//! the drain waits until a finalized Clock passes every quote the service signed, the plan is
//! made again from a finalized read after the drain, and the run ends only when a finalized read
//! shows every step's effect. A run that fails leaves the halt file in place: the maker stays
//! halted, which the watcher reports, until someone looks. A halt file that was already there
//! (an incident, or a revoke whose approvals must not come back on their own) stops the run
//! before anything in steps 3 to 5 is sent.
//!
//! **The cap** ([`crate::writer_cap`]) is read from the chain alone and released only by a
//! finalized `close` or `claim_writer`: a claim sent in this run frees nothing until the next.
//! Every read fails closed: a failed or partial read plans nothing.
//!
//! **The admin key** signs only `create_series`. In a scheduled job this is devnet only; on
//! mainnet a scheduled job never holds an admin key.

use std::cmp::Ordering;

use metalbook_core::{OptionKind, SeriesState, Underlying};
use metalbook_sdk::{decode as metal_decode, pda as metal_pda};
use quotebook_cli::rpc::Rpc;
use quotebook_cli::CliError;
use quotebook_core::accounts::{AccountTag, ProgramAccount, Series as BookSeriesAccount};
use quotebook_maker::protocol::StatusMessage;
use quotebook_maker::risk::TokenAccountState;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::Serialize;
use solana_address::Address;

use crate::chain::{parse_clock, parse_token_account, WatchedAccount, CLOCK_SYSVAR, TOKEN_PROGRAM};
use crate::config::{DemoConfig, SeriesPair};
use crate::host::associated_token_account;
use crate::ops::{self, Drain};
use crate::spot::SpotCell;
use crate::watch::allowance_low;
use crate::writer_cap::{exposure_of, read_positions, Exposure, Position};

/// The allowance approved on each pair's long-token account: the hard cap on chain (D5).
pub const LONG_APPROVAL: u64 = 2;
/// The rent metalbook charges the writer for a new `Writer` position, lamports (measured on
/// devnet: the maker key pays it, not the fee payer).
pub const POSITION_RENT: u64 = 1_056_640;
/// A system account left with lamports must hold at least this (rent exemption of 0 bytes); a
/// mint leaving the maker key between 1 and this minus 1 fails.
pub const MIN_ACCOUNT_LAMPORTS: u64 = 650_240;
/// The allowance approved on the USDC account, atoms (4 USDC; D5).
pub const USDC_APPROVAL: u64 = 4_000_000;
/// One troy ounce in milli-ounces: the demo trades 1 oz contracts only.
pub const ONE_OUNCE: u32 = 1_000;
/// The quotebook series' base lot: one long token.
pub const SERIES_BASE_LOT: u64 = 1;
/// The quotebook series' quote lot: 0.01 USDC per tick.
pub const SERIES_QUOTE_LOT: u64 = 10_000;
/// The quotebook series' longest quote lifetime, seconds.
pub const SERIES_MAX_QUOTE_TTL: i64 = 60;
/// The quotebook series' smallest fill, lots.
pub const SERIES_MIN_SIZE: u64 = 1;
/// USDC atoms per USD.
const ATOMS_PER_USD: u64 = 1_000_000;
/// Accounts per `getMultipleAccounts`.
const ACCOUNTS_PER_READ: usize = 100;

fn rpc_error(what: impl Into<String>) -> CliError {
    CliError::Rpc(what.into())
}

/// A metalbook series as the cycle judges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetalSeries {
    /// The series account.
    pub address: Address,
    /// Its long mint.
    pub long_mint: Address,
    /// Its state.
    pub state: SeriesState,
    /// The metal.
    pub underlying: Underlying,
    /// Call or put.
    pub kind: OptionKind,
    /// Strike, USDC atoms per ounce.
    pub strike: u64,
    /// Cap, USDC atoms per ounce; 0 for an uncapped series.
    pub cap: u64,
    /// Contract size, milli-ounces.
    pub multiplier: u32,
    /// Expiry, Unix seconds.
    pub expiry: i64,
    /// The settlement window before expiry, seconds.
    pub window_s: u32,
    /// The collateral one contract locks, USDC atoms.
    pub max_payout: u64,
    /// The collateral mint.
    pub usdc_mint: Address,
    /// The Pyth feed id it settles on.
    pub feed_id: [u8; 32],
}

impl MetalSeries {
    /// Whether it can still be written and traded at `now`: active, before its settlement window.
    #[must_use]
    pub fn tradable(&self, now: i64) -> bool {
        self.state == SeriesState::Active
            && self
                .expiry
                .checked_sub(i64::from(self.window_s))
                .is_some_and(|start| now < start)
    }

    /// Whether settlement is over: longs redeem and writers claim.
    #[must_use]
    pub fn finished(&self) -> bool {
        matches!(self.state, SeriesState::Settled | SeriesState::Voided)
    }

    /// Whether the demo trades it: a tradable 1 oz XAG capped call on this demo's USDC and feed.
    #[must_use]
    pub fn eligible(&self, demo: &DemoConfig, now: i64) -> bool {
        let feed = demo.underlying(Underlying::Xag).map(|u| u.feed_id);
        self.tradable(now)
            && self.underlying == Underlying::Xag
            && self.kind == OptionKind::Call
            && self.cap > 0
            && self.multiplier == ONE_OUNCE
            && self.usdc_mint == demo.usdc_mint
            && feed == Some(self.feed_id)
            && self.max_payout > 0
    }
}

/// A quotebook series as the cycle judges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookSeries {
    /// The series account.
    pub address: Address,
    /// Its index in the config.
    pub index: u64,
    /// Base mint.
    pub base_mint: Address,
    /// Quote mint.
    pub quote_mint: Address,
    /// Base lot, atoms.
    pub base_lot: u64,
    /// Quote lot, atoms.
    pub quote_lot: u64,
    /// Fee, basis points.
    pub fee_bps: u16,
}

impl BookSeries {
    /// Whether it trades `long_mint` against `usdc_mint` with the demo's terms.
    #[must_use]
    pub fn trades(&self, long_mint: &Address, usdc_mint: &Address) -> bool {
        self.base_mint == *long_mint
            && self.quote_mint == *usdc_mint
            && self.base_lot == SERIES_BASE_LOT
            && self.quote_lot == SERIES_QUOTE_LOT
            && self.fee_bps == 0
    }
}

/// A token account of the maker: `None` when it does not exist.
pub type Token = Option<TokenAccountState>;

/// Everything one cycle reads, at one `finalized` view of the chain (each part is read at
/// `finalized`; the slot is the first read's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// The maker.
    pub maker: Address,
    /// The USDC mint.
    pub usdc_mint: Address,
    /// The maker's USDC account: its associated token account.
    pub usdc_account: Address,
    /// The slot of the first read.
    pub slot: u64,
    /// The finalized Clock.
    pub now: i64,
    /// The XAG spot in USDC atoms per ounce, when the feed is fresh.
    pub spot: Option<u64>,
    /// Every metalbook series that decodes.
    pub metal: Vec<MetalSeries>,
    /// Every quotebook series that decodes.
    pub book: Vec<BookSeries>,
    /// The configured pairs.
    pub pairs: Vec<SeriesPair>,
    /// Every `Writer` position of the maker.
    pub positions: Vec<Position>,
    /// The maker key's lamports: it pays the rent of a new position.
    pub maker_lamports: u64,
    /// The maker's USDC account.
    pub usdc: Token,
    /// The maker's long-token account of each long mint read.
    pub longs: Vec<(Address, Token)>,
}

impl View {
    /// The metalbook series at `address`.
    #[must_use]
    pub fn metal(&self, address: &Address) -> Option<&MetalSeries> {
        self.metal.iter().find(|m| m.address == *address)
    }

    /// The maker's position on `series`.
    #[must_use]
    pub fn position(&self, series: &Address) -> Option<&Position> {
        self.positions.iter().find(|p| p.series == *series)
    }

    /// The maker's long-token account of `long_mint`; `None` when it does not exist or was not
    /// read.
    #[must_use]
    pub fn long(&self, long_mint: &Address) -> Token {
        self.longs
            .iter()
            .find(|(m, _)| m == long_mint)
            .and_then(|(_, t)| *t)
    }

    /// The exposure of the positions.
    ///
    /// # Errors
    ///
    /// `Rpc` when an open position's series was not read: the cap cannot be judged.
    pub fn exposure(&self) -> Result<Exposure, CliError> {
        let payouts: Vec<(Address, u64)> = self
            .metal
            .iter()
            .map(|m| (m.address, m.max_payout))
            .collect();
        exposure_of(&self.positions, &payouts)
    }
}

/// One thing the cycle sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    /// `create_series` on quotebook for `long_mint` against USDC, signed by the admin.
    CreateSeries {
        /// The metalbook series.
        #[serde(serialize_with = "address")]
        metal: Address,
        /// Its long mint.
        #[serde(serialize_with = "address")]
        long_mint: Address,
    },
    /// `claim_writer` on the maker's position.
    Claim {
        /// The metalbook series.
        #[serde(serialize_with = "address")]
        metal: Address,
    },
    /// `redeem` of the maker's own long tokens.
    Redeem {
        /// The metalbook series.
        #[serde(serialize_with = "address")]
        metal: Address,
        /// Long tokens burnt.
        contracts: u64,
    },
    /// Token `Revoke` on a long-token account.
    Revoke {
        /// The account.
        #[serde(serialize_with = "address")]
        account: Address,
    },
    /// The maker's long-token account (created if missing), then `mint`.
    Mint {
        /// The metalbook series.
        #[serde(serialize_with = "address")]
        metal: Address,
        /// Contracts written.
        contracts: u64,
    },
    /// Approves the maker's delegate PDA on a token account.
    Approve {
        /// The account.
        #[serde(serialize_with = "address")]
        account: Address,
        /// The allowance, atoms.
        amount: u64,
    },
}

// The `Serialize` derive hands a reference to the field.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn address<S: serde::Serializer>(a: &Address, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(a)
}

impl Step {
    /// Whether it may run only while the maker is halted and drained (steps 3 to 5).
    #[must_use]
    pub const fn needs_halt(&self) -> bool {
        !matches!(self, Self::CreateSeries { .. })
    }

    /// Whether `after`, a finalized view read after it was sent, shows its effect; `before` is
    /// the view it was planned from.
    #[must_use]
    pub fn confirmed(&self, before: &View, after: &View) -> bool {
        match self {
            Self::CreateSeries { long_mint, .. } => after
                .book
                .iter()
                .any(|b| b.trades(long_mint, &before.usdc_mint)),
            // `close_position` may already have removed a claimed position.
            Self::Claim { metal } => after.position(metal).is_none_or(|p| p.claimed),
            Self::Redeem { metal, contracts } => {
                let held = |v: &View| {
                    v.metal(metal)
                        .and_then(|m| v.long(&m.long_mint))
                        .map_or(0, |t| t.amount)
                };
                held(after) <= held(before).saturating_sub(*contracts)
            }
            Self::Revoke { account } => {
                token_of(after, account).is_none_or(|t| t.delegated_amount == 0)
            }
            Self::Mint { metal, contracts } => {
                let written = |v: &View| v.position(metal).map_or(0, |p| p.written);
                written(after) >= written(before).saturating_add(*contracts)
            }
            Self::Approve { account, amount } => {
                token_of(after, account).is_some_and(|t| t.delegated_amount == *amount)
            }
        }
    }
}

/// The token account `account` in `view`, by address: the USDC account or a long-token account.
fn token_of(view: &View, account: &Address) -> Token {
    if *account == view.usdc_account {
        return view.usdc;
    }
    view.longs
        .iter()
        .find(|(mint, _)| associated_token_account(&view.maker, mint, &TOKEN_PROGRAM) == *account)
        .and_then(|(_, t)| *t)
}

/// The maker's long-token account of `long_mint`.
#[must_use]
pub fn long_account(maker: &Address, long_mint: &Address) -> Address {
    associated_token_account(maker, long_mint, &TOKEN_PROGRAM)
}

/// What one cycle will do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CyclePlan {
    /// The steps, in the order they run: `create_series`, then claims, redeems, revokes, mints
    /// and approvals.
    pub steps: Vec<Step>,
    /// The configuration's pairs after the cycle.
    pub pairs: Vec<SeriesPair>,
    /// Why something was not done.
    pub notes: Vec<String>,
}

impl CyclePlan {
    /// The steps that run without a halt (`create_series`).
    #[must_use]
    pub fn creates(&self) -> Vec<Step> {
        self.steps
            .iter()
            .filter(|s| !s.needs_halt())
            .copied()
            .collect()
    }

    /// The steps that run only while the maker is halted and drained.
    #[must_use]
    pub fn halted(&self) -> Vec<Step> {
        self.steps
            .iter()
            .filter(|s| s.needs_halt())
            .copied()
            .collect()
    }

    /// As JSON, for the dry run and the logs.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "steps": self.steps,
            "pairs": pairs_json(&self.pairs),
            "notes": self.notes,
        })
    }
}

/// `pairs` as the configuration's JSON.
#[must_use]
pub fn pairs_json(pairs: &[SeriesPair]) -> serde_json::Value {
    pairs
        .iter()
        .map(|p| {
            serde_json::json!({
                "metalbook_series": p.metalbook_series.to_string(),
                "quotebook_series": p.quotebook_series.to_string(),
            })
        })
        .collect()
}

fn overflow() -> CliError {
    rpc_error("collateral overflows")
}

/// Plans one cycle from `view`, with `max_long` the inventory band's top (the settings'
/// `market_limits.max_long`). See the module documentation for the steps and their order.
///
/// # Errors
///
/// `Rpc` when the cap cannot be judged (an open position whose series was not read) or on an
/// overflow: nothing is planned.
pub fn plan(view: &View, demo: &DemoConfig, max_long: u64) -> Result<CyclePlan, CliError> {
    let exposure = view.exposure()?;
    let now = view.now;
    let mut out = CyclePlan {
        pairs: view.pairs.clone(),
        ..CyclePlan::default()
    };

    // 1 and 2: one pair per tradable expiry.
    let mut eligible: Vec<&MetalSeries> = view
        .metal
        .iter()
        .filter(|m| m.eligible(demo, now))
        .collect();
    eligible.sort_by(|a, b| a.expiry.cmp(&b.expiry).then(a.strike.cmp(&b.strike)));
    let mut expiries: Vec<i64> = eligible.iter().map(|m| m.expiry).collect();
    expiries.dedup();
    let mut targets: Vec<MetalSeries> = Vec::new();
    for expiry in expiries {
        let configured = out
            .pairs
            .iter()
            .filter_map(|p| view.metal(&p.metalbook_series))
            .find(|m| m.expiry == expiry && m.eligible(demo, now));
        if let Some(m) = configured {
            targets.push(*m);
            continue;
        }
        // A contract already written fixes the strike: a run that minted and then failed never
        // recorded its pair, and the next one must not write a second strike for the expiry.
        let written = eligible
            .iter()
            .find(|m| m.expiry == expiry && view.position(&m.address).is_some_and(|p| p.open()));
        let chosen = match (written, view.spot) {
            (Some(m), _) => Some(m),
            (None, Some(spot)) => eligible
                .iter()
                .filter(|m| m.expiry == expiry)
                .min_by(|a, b| nearest(a.strike, b.strike, spot)),
            (None, None) => {
                out.notes.push(format!(
                    "expiry {expiry} not paired: no fresh XAG spot to choose its strike"
                ));
                continue;
            }
        };
        let Some(m) = chosen.copied() else {
            continue;
        };
        let book = view
            .book
            .iter()
            .filter(|b| b.trades(&m.long_mint, &demo.usdc_mint))
            .min_by_key(|b| b.index);
        match book {
            Some(b) => {
                out.pairs.push(SeriesPair {
                    metalbook_series: m.address,
                    quotebook_series: b.address,
                });
                targets.push(*m);
            }
            None => out.steps.push(Step::CreateSeries {
                metal: m.address,
                long_mint: m.long_mint,
            }),
        }
    }

    // 5: after settlement.
    let mut claims = Vec::new();
    for p in view.positions.iter().filter(|p| p.open()) {
        if view.metal(&p.series).is_some_and(MetalSeries::finished) {
            claims.push(Step::Claim { metal: p.series });
        }
    }
    let mut redeems = Vec::new();
    let mut revokes = Vec::new();
    let mut dropped: Vec<Address> = Vec::new();
    // Every series the maker wrote, paired or not: a position whose pair was never recorded
    // still has long tokens to redeem and an allowance to revoke.
    let mut settling: Vec<Address> = view.pairs.iter().map(|p| p.metalbook_series).collect();
    for p in &view.positions {
        if !settling.contains(&p.series) {
            settling.push(p.series);
        }
    }
    for series in settling {
        let Some(m) = view.metal(&series) else {
            out.notes.push(format!(
                "pair {series} dropped: its metalbook series is gone"
            ));
            dropped.push(series);
            continue;
        };
        if !m.finished() {
            continue;
        }
        let mut pending = claims
            .iter()
            .any(|c| matches!(c, Step::Claim { metal } if *metal == m.address));
        if let Some(t) = view.long(&m.long_mint) {
            if m.state == SeriesState::Settled && t.amount > 0 {
                redeems.push(Step::Redeem {
                    metal: m.address,
                    contracts: t.amount,
                });
                pending = true;
            }
            if t.delegated_amount > 0 {
                revokes.push(Step::Revoke {
                    account: long_account(&view.maker, &m.long_mint),
                });
                pending = true;
            }
        }
        if !pending {
            dropped.push(m.address);
        }
    }
    out.pairs.retain(|p| !dropped.contains(&p.metalbook_series));

    // 3: mint within the band, the funding and the cap.
    let mut free = view.usdc.map_or(0, |t| t.amount);
    if view.usdc.is_none() && !targets.is_empty() {
        out.notes.push("no USDC account: nothing minted".into());
    }
    let mut cap = exposure;
    let mut mints = Vec::new();
    let mut lamports = view.maker_lamports;
    for m in &targets {
        if view.position(&m.address).is_none() {
            let left = lamports.checked_sub(POSITION_RENT);
            let Some(left) = left.filter(|l| *l == 0 || *l >= MIN_ACCOUNT_LAMPORTS) else {
                out.notes.push(format!(
                    "{}: nothing minted, the maker key holds {lamports} lamports: the \
                     {POSITION_RENT} rent of a new position must leave it 0 or at least \
                     {MIN_ACCOUNT_LAMPORTS}",
                    m.address
                ));
                continue;
            };
            lamports = left;
        }
        let held = view.long(&m.long_mint).map_or(0, |t| t.amount);
        let band = max_long.saturating_sub(held);
        let funding = free.checked_div(m.max_payout).unwrap_or(0);
        let room = cap.room(m.max_payout);
        let n = band.min(funding).min(room);
        if n == 0 {
            let why = if room == 0 {
                "the writer cap is full"
            } else if band == 0 {
                "the inventory band is full"
            } else {
                "not enough unlocked USDC"
            };
            out.notes
                .push(format!("{}: nothing minted, {why}", m.address));
            continue;
        }
        let locked = n.checked_mul(m.max_payout).ok_or_else(overflow)?;
        free = free.checked_sub(locked).ok_or_else(overflow)?;
        cap.contracts = cap.contracts.checked_add(n).ok_or_else(overflow)?;
        cap.committed = cap.committed.checked_add(locked).ok_or_else(overflow)?;
        mints.push(Step::Mint {
            metal: m.address,
            contracts: n,
        });
    }

    // 4: approvals, on accounts that hold or will hold something.
    let work = !(claims.is_empty() && redeems.is_empty() && revokes.is_empty() && mints.is_empty());
    let mut approvals = Vec::new();
    for m in &targets {
        let token = view.long(&m.long_mint);
        let minted = mints
            .iter()
            .any(|s| matches!(s, Step::Mint { metal, .. } if *metal == m.address));
        let held = token.map_or(0, |t| t.amount);
        if !minted && held == 0 {
            continue;
        }
        let delegated = token.map_or(0, |t| t.delegated_amount);
        if delegated != LONG_APPROVAL && (work || allowance_low(delegated, LONG_APPROVAL)) {
            approvals.push(Step::Approve {
                account: long_account(&view.maker, &m.long_mint),
                amount: LONG_APPROVAL,
            });
        }
    }
    let work = work || !approvals.is_empty();
    if let (false, Some(t)) = (targets.is_empty(), view.usdc) {
        let held = t.amount > 0;
        if held
            && t.delegated_amount != USDC_APPROVAL
            && (work || allowance_low(t.delegated_amount, USDC_APPROVAL))
        {
            approvals.push(Step::Approve {
                account: view.usdc_account,
                amount: USDC_APPROVAL,
            });
        }
    }

    out.steps.extend(claims);
    out.steps.extend(redeems);
    out.steps.extend(revokes);
    out.steps.extend(mints);
    out.steps.extend(approvals);
    Ok(out)
}

/// Orders strikes by distance to `spot`, the lower strike first on a tie (it locks less).
fn nearest(a: u64, b: u64, spot: u64) -> Ordering {
    a.abs_diff(spot).cmp(&b.abs_diff(spot)).then(a.cmp(&b))
}

/// A spot in USD per ounce as USDC atoms per ounce, rounded down.
fn atoms(price: Decimal) -> Option<u64> {
    price
        .checked_mul(Decimal::from(ATOMS_PER_USD))
        .and_then(|a| a.floor().to_u64())
}

fn token(
    account: Option<&solana_account::Account>,
    address: Address,
    mint: Address,
    maker: &Address,
    delegate: &Address,
) -> Result<Token, CliError> {
    account
        .map(|a| {
            parse_token_account(Some(a), &WatchedAccount { address, mint }, maker, delegate)
                .map_err(|e| rpc_error(e.0))
        })
        .transpose()
}

fn metal_series(
    demo: &DemoConfig,
    address: &Address,
    account: &solana_account::Account,
) -> Result<MetalSeries, String> {
    let s = metal_decode::series(&demo.metalbook_program, &account.owner, &account.data)
        .map_err(|e| format!("{e:?}"))?;
    let state = SeriesState::try_from(s.state).map_err(|e| format!("{e:?}"))?;
    let terms = s.terms().map_err(|e| format!("{e:?}"))?;
    let max_payout = terms
        .max_payout_per_contract()
        .map_err(|e| format!("{e:?}"))?
        .get();
    let long_mint = Address::new_from_array(s.long_mint);
    if long_mint != metal_pda::long_mint(&demo.metalbook_program, address).0 {
        return Err("long mint is not the series' PDA".into());
    }
    Ok(MetalSeries {
        address: *address,
        long_mint,
        state,
        underlying: terms.key().underlying(),
        kind: terms.key().kind(),
        strike: terms.key().strike().get(),
        cap: terms.cap().get(),
        multiplier: terms.multiplier().get(),
        expiry: terms.key().expiry().get(),
        window_s: s.window_s,
        max_payout,
        usdc_mint: Address::new_from_array(s.usdc_mint),
        feed_id: s.feed_id,
    })
}

/// Reads one cycle's [`View`] at `finalized`: the Clock, the maker's USDC account and the XAG
/// feed in one call, then every metalbook and quotebook series, the maker's positions, and its
/// long-token account of every series it has a pair, a position or a candidate on.
///
/// # Errors
///
/// `Rpc` on any failed or partial read, a token account that fails its checks, or a position
/// that does not decode; `Args` when the configuration has no XAG. Nothing is planned from a
/// failed read.
pub fn read_view(rpc: &mut dyn Rpc, demo: &DemoConfig, maker: &Address) -> Result<View, CliError> {
    let xag = demo
        .underlying(Underlying::Xag)
        .ok_or_else(|| CliError::Args("the configuration has no XAG".into()))?;
    let usdc_account = associated_token_account(maker, &demo.usdc_mint, &TOKEN_PROGRAM);
    let delegate = quotebook_sdk::pda::delegate(&demo.quotebook_program, maker).0;
    let (slot, first) =
        rpc.finalized_accounts(&[CLOCK_SYSVAR, usdc_account, xag.feed_account, *maker])?;
    let [clock, usdc, feed, maker_account]: [Option<solana_account::Account>; 4] = first
        .try_into()
        .map_err(|_| rpc_error("getMultipleAccounts returned the wrong number of accounts"))?;
    let now = parse_clock(clock.as_ref(), slot)
        .map_err(|e| rpc_error(e.0))?
        .get();
    let usdc = token(
        usdc.as_ref(),
        usdc_account,
        demo.usdc_mint,
        maker,
        &delegate,
    )?;
    let spot = feed.and_then(|a| {
        let mut cell = SpotCell::new(xag.feed_id);
        cell.ingest(&a.owner.to_bytes(), &a.data, now).ok()?;
        cell.current(now).and_then(|s| atoms(s.price))
    });

    let metal_len = u64::try_from(std::mem::size_of::<metalbook_core::accounts::Series>())
        .map_err(|_| rpc_error("series size"))?;
    let mut metal = Vec::new();
    for (address, account) in rpc.finalized_program_accounts(
        &demo.metalbook_program,
        metal_len,
        &[(0, vec![metalbook_core::accounts::AccountTag::Series as u8])],
    )? {
        match metal_series(demo, &address, &account) {
            Ok(m) => metal.push(m),
            Err(e) => tracing::debug!(series = %address, error = %e, "metalbook series skipped"),
        }
    }

    let book_len = u64::try_from(BookSeriesAccount::LEN).map_err(|_| rpc_error("series size"))?;
    let mut book = Vec::new();
    for (address, account) in rpc.finalized_program_accounts(
        &demo.quotebook_program,
        book_len,
        &[(0, vec![AccountTag::Series as u8])],
    )? {
        match quotebook_sdk::decode::series(&demo.quotebook_program, &account.owner, &account.data)
        {
            Ok(s) => book.push(BookSeries {
                address,
                index: s.series_index,
                base_mint: s.base_mint,
                quote_mint: s.quote_mint,
                base_lot: s.base_lot_size.get(),
                quote_lot: s.quote_lot_size.get(),
                fee_bps: s.fee_bps(),
            }),
            Err(e) => {
                tracing::debug!(series = %address, error = ?e, "quotebook series skipped");
            }
        }
    }

    let positions = read_positions(rpc, &demo.metalbook_program, maker)?;

    let mut mints: Vec<Address> = Vec::new();
    let mut want = |series: &Address| {
        if let Some(m) = metal.iter().find(|m| m.address == *series) {
            if !mints.contains(&m.long_mint) {
                mints.push(m.long_mint);
            }
        }
    };
    for p in &demo.pairs {
        want(&p.metalbook_series);
    }
    for p in &positions {
        want(&p.series);
    }
    for m in metal.iter().filter(|m| m.eligible(demo, now)) {
        want(&m.address);
    }
    let mut longs = Vec::with_capacity(mints.len());
    for chunk in mints.chunks(ACCOUNTS_PER_READ) {
        let addresses: Vec<Address> = chunk.iter().map(|m| long_account(maker, m)).collect();
        let (_, accounts) = rpc.finalized_accounts(&addresses)?;
        if accounts.len() != chunk.len() {
            return Err(rpc_error(
                "getMultipleAccounts returned the wrong number of accounts",
            ));
        }
        for ((mint, address), account) in chunk.iter().zip(addresses).zip(accounts) {
            longs.push((
                *mint,
                token(account.as_ref(), address, *mint, maker, &delegate)?,
            ));
        }
    }

    Ok(View {
        maker: *maker,
        usdc_mint: demo.usdc_mint,
        usdc_account,
        slot,
        now,
        spot,
        metal,
        book,
        pairs: demo.pairs.clone(),
        positions,
        maker_lamports: maker_account.map_or(0, |a| a.lamports),
        usdc,
        longs,
    })
}

/// What [`run_cycle`] needs from the world: reads, the maker's status, the halt file and sends.
pub trait CycleIo {
    /// A [`View`] at `finalized`.
    ///
    /// # Errors
    ///
    /// As [`read_view`].
    fn read(&mut self) -> Result<View, CliError>;
    /// One `Status` request to the maker.
    ///
    /// # Errors
    ///
    /// No answer, as text.
    fn status(&mut self) -> Result<StatusMessage, String>;
    /// The finalized Clock.
    ///
    /// # Errors
    ///
    /// The RPC error.
    fn finalized_clock(&mut self) -> Result<i64, CliError>;
    /// Creates the halt file; `true` when this call created it.
    ///
    /// # Errors
    ///
    /// The I/O error.
    fn halt(&mut self) -> Result<bool, CliError>;
    /// Sends `step`, planned from `view`, and returns its signature once confirmed.
    ///
    /// # Errors
    ///
    /// The RPC error, including the program's rejection.
    fn send(&mut self, step: &Step, view: &View) -> Result<String, CliError>;
    /// Waits before the next poll.
    fn pause(&mut self);
}

/// How long [`run_cycle`] waits, in polls of [`CycleIo::pause`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Patience {
    /// For the drain.
    pub drain_polls: u32,
    /// For each finalized confirmation.
    pub confirm_polls: u32,
}

/// One step sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sent {
    /// The step.
    #[serde(flatten)]
    pub step: Step,
    /// Its transaction's signature.
    pub signature: String,
}

/// What a cycle did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// What it sent, in order.
    pub sent: Vec<Sent>,
    /// The configuration's pairs after the cycle.
    pub pairs: Vec<SeriesPair>,
    /// Whether they differ from the configured ones: the maker and the watcher need a restart.
    pub config_changed: bool,
    /// Whether this run created the halt file, so it may remove it.
    pub halt_created: bool,
    /// Why something was not done.
    pub notes: Vec<String>,
}

/// Why a cycle stopped, with what it had sent. The halt file, if any, stays.
#[derive(Debug)]
pub struct Failure {
    /// The error.
    pub error: CliError,
    /// What it sent before stopping.
    pub sent: Vec<Sent>,
    /// Whether this run created the halt file.
    pub halt_created: bool,
}

struct Run<'a> {
    io: &'a mut dyn CycleIo,
    sent: Vec<Sent>,
    halt_created: bool,
}

impl Run<'_> {
    fn fail(self, error: CliError) -> Failure {
        Failure {
            error,
            sent: self.sent,
            halt_created: self.halt_created,
        }
    }

    fn send_all(&mut self, steps: &[Step], view: &View) -> Result<(), CliError> {
        for step in steps {
            let signature = self.io.send(step, view)?;
            tracing::info!(step = ?step, %signature, "sent");
            self.sent.push(Sent {
                step: *step,
                signature,
            });
        }
        Ok(())
    }

    /// Reads until a finalized view shows every step's effect. A failed read is retried within
    /// the same patience.
    fn confirm(&mut self, steps: &[Step], before: &View, polls: u32) -> Result<View, CliError> {
        let mut last = None;
        for _ in 0..=polls {
            match self.io.read() {
                Ok(after) if steps.iter().all(|s| s.confirmed(before, &after)) => return Ok(after),
                Ok(_) => {}
                Err(e) => last = Some(e),
            }
            self.io.pause();
        }
        Err(CliError::Maker(match last {
            Some(e) => format!("not confirmed at finalized before the timeout; last read: {e}"),
            None => "not confirmed at finalized before the timeout".into(),
        }))
    }

    /// Waits until the maker reports the halt and a finalized Clock passes every quote it signed.
    fn drain(&mut self, polls: u32) -> Result<(), CliError> {
        for _ in 0..=polls {
            let status = self.io.status().map_err(|e| {
                CliError::Maker(format!(
                    "no status from the maker, so the drain cannot be judged: {e}"
                ))
            })?;
            if status.halted {
                let clock = self.io.finalized_clock()?;
                if ops::drain(&status, quotebook_core::UnixTimestamp::new(clock))? == Drain::Done {
                    tracing::info!(
                        open_quotes = status.open_quotes,
                        next_nonce = status.next_nonce,
                        newest_expires_at = ?status.newest_expires_at,
                        finalized_clock = clock,
                        "drained"
                    );
                    return Ok(());
                }
            }
            self.io.pause();
        }
        Err(CliError::Maker(
            "not drained before the timeout: nothing minted or approved".into(),
        ))
    }
}

/// Runs one cycle: plans from a finalized read, sends any `create_series` and confirms it, and
/// if anything else is due, halts the maker, waits for the drain, plans again from a finalized
/// read, sends steps 3 to 5 and confirms each at `finalized`.
///
/// # Errors
///
/// [`Failure`], with what was sent. A failure after the halt leaves the halt file.
pub fn run_cycle(
    io: &mut dyn CycleIo,
    demo: &DemoConfig,
    max_long: u64,
    patience: Patience,
) -> Result<Outcome, Failure> {
    let mut run = Run {
        io,
        sent: Vec::new(),
        halt_created: false,
    };
    let result = (|| {
        let first = run.io.read()?;
        let configured = first.pairs.clone();
        let mut view = first;
        let mut planned = plan(&view, demo, max_long)?;
        let creates = planned.creates();
        if !creates.is_empty() {
            run.send_all(&creates, &view)?;
            view = run.confirm(&creates, &view, patience.confirm_polls)?;
            planned = plan(&view, demo, max_long)?;
        }
        if !planned.halted().is_empty() {
            run.halt_created = run.io.halt()?;
            if !run.halt_created {
                // Someone else halted it: an incident, or a revoke whose approvals must not come
                // back on their own. The cycle leaves it alone.
                return Err(CliError::Maker(
                    "the maker was already halted (halt file present): nothing minted, approved or \
                     claimed; remove the halt file once the halt is over"
                        .into(),
                ));
            }
            run.drain(patience.drain_polls)?;
            // Fills during the drain change balances: plan again from what is finalized now.
            view = run.io.read()?;
            planned = plan(&view, demo, max_long)?;
            let halted = planned.halted();
            run.send_all(&halted, &view)?;
            let after = run.confirm(&halted, &view, patience.confirm_polls)?;
            if let Ok(later) = plan(&after, demo, max_long) {
                planned.pairs = later.pairs;
            }
        }
        Ok::<_, CliError>(Outcome {
            sent: Vec::new(),
            config_changed: planned.pairs != configured,
            pairs: planned.pairs,
            halt_created: false,
            notes: planned.notes,
        })
    })();
    match result {
        Ok(outcome) => Ok(Outcome {
            sent: run.sent,
            halt_created: run.halt_created,
            ..outcome
        }),
        Err(e) => Err(run.fail(e)),
    }
}

/// The devnet genesis hash: the roll and the taker refuse any other cluster.
pub const DEVNET_GENESIS: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";

/// Refuses any cluster but devnet, before a key is loaded or anything is sent.
///
/// # Errors
///
/// `Args` on another cluster; the RPC error.
pub fn require_devnet(rpc: &mut dyn Rpc) -> Result<(), CliError> {
    let genesis = rpc.genesis_hash()?.to_string();
    if genesis == DEVNET_GENESIS {
        Ok(())
    } else {
        Err(CliError::Args(format!(
            "not devnet (genesis {genesis}): the demo cycle runs on devnet only"
        )))
    }
}

/// The configuration `json` with its pairs replaced by `pairs`, checked as a [`DemoConfig`].
///
/// # Errors
///
/// `Args` when `json` is not an object or the result does not validate.
pub fn with_pairs(json: &str, pairs: &[SeriesPair]) -> Result<String, CliError> {
    let mut value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| CliError::Args(format!("configuration: {e}")))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| CliError::Args("configuration: not an object".into()))?;
    object.insert("pairs".into(), pairs_json(pairs));
    let mut text = serde_json::to_string_pretty(&value)
        .map_err(|e| CliError::Args(format!("configuration: {e}")))?;
    text.push('\n');
    let checked = DemoConfig::from_json(&text)
        .map_err(|e| CliError::Args(format!("new configuration: {e}")))?;
    if checked.pairs != pairs {
        return Err(CliError::Args(
            "new configuration: the pairs did not round-trip".into(),
        ));
    }
    Ok(text)
}
