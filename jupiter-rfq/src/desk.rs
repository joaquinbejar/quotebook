//! The desk: quotes, reservations and limits behind one lock (docs/design/jupiter-rfq.md,
//! "What D1's risk book carries over").
//!
//! What changes from D1, which assumes a nonce and a signed quote:
//!
//! - **Refuse at `/quote`, reserve at `/swap`.** A 404 costs nothing, a rejected swap counts
//!   against the maker, so every limit runs at `/quote` and again at `/swap` as the last
//!   defence. `/quote` only records a bounded entry by `quoteId` with a 55 s life; `/swap`
//!   reserves.
//! - **`quoteId` is single use**: consumed at `/swap`, so a second transaction for one quote is
//!   refused, while a retry of the same message is idempotent.
//! - **Release:** when the transaction is finalized, the reservation leaves and its fee becomes
//!   spent, success or failure; when it is absent and the finalized Clock is strictly past
//!   `expire_at` (`fill` still passes at equality), the reservation is released.
//! - **No on-chain cap:** the hot wallet's balance is all there is, so the inventory limit reads
//!   the balance alone.
//! - **The fee budget** (`sol_floor`, `max_fees_per_hour`) is reserved under the same lock, so
//!   two concurrent swaps cannot both pass against the same lamports.
//! - **The fee is a liability:** each reservation also holds `ceil(out × fee_bps / 10,000)` of
//!   the output mint, and a landed fill accrues it as debt. Debt is held against the balance
//!   like a reservation until it is collected, it lives in a durable [`DebtLedger`] so a restart
//!   keeps it, and only [`Desk::record_collection`] releases it (see [`crate::ledger`]).
//! - **A retry is idempotent only while it is safe:** the same message gets the same answer
//!   while its reservation is held, the desk is ready and `expire_at` still leaves time to land.
//!   Once the reservation is released (finalized, failed or expired), the retry gets that
//!   terminal answer, never a new permission to sign.
//! - **After a restart**, nothing co-signs until a finalized Clock is past the last `expire_at`
//!   the previous run could have co-signed; the hour's fills and fees are rebuilt from the
//!   wallet's finalized history, and if that fails co-signing stays refused for 3,600 s of
//!   chain Clock.
//!
//! The prototype never signs: the HTTP layer turns a [`SwapDecision::WouldSign`] into a
//! rejection outside tests. Reads of the chain happen outside the lock, through
//! [`ChainView`], as in D1.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Mutex;

use rust_decimal::Decimal;
use solana_address::Address;

use crate::api::{QuoteRequest, QuoteResponse, QuoteType, RejectionReason, SwapRequest};
use crate::ledger::{DebtLedger, LedgerState, PendingFill as Reservation};
use crate::pricing::{self, PriceError, PricingInputs, Side};
use crate::tx::Transaction;
use crate::validate::{self, Expected, FeeBudget, Invalid, Validated};

/// Seconds in the rolling windows.
pub const HOUR_SECS: i64 = 3_600;
/// Clock skew allowed between the maker's wall clock and Jupiter's.
pub const MAX_CLOCK_SKEW_SECS: i64 = 10;
/// The longest id the desk stores.
pub const MAX_ID_LEN: usize = 128;

/// The wall clock.
pub trait Clock: Send + Sync {
    /// Unix seconds now.
    fn now(&self) -> i64;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
}

/// A stored mid: no RPC read happens inside a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spot {
    /// Quote tokens per base token.
    pub mid: Decimal,
    /// When it was published, Unix seconds.
    pub published_at: i64,
}

/// Where mids come from.
pub trait SpotSource: Send + Sync {
    /// The latest mid of market `index`, if any.
    fn spot(&self, index: usize) -> Option<Spot>;
}

/// A transaction the desk tracks, by the taker's signature until the maker would sign.
pub type TxId = [u8; 64];

/// A tracked transaction found finalized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Landed {
    /// Which transaction.
    pub id: TxId,
    /// The fee its metadata reports, lamports.
    pub fee_lamports: u64,
    /// Its block time.
    pub at: i64,
    /// Whether it failed: the fee is spent, nothing moved.
    pub failed: bool,
}

/// The maker's wallet at one finalized slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The slot.
    pub slot: u64,
    /// The Clock at that slot.
    pub clock: i64,
    /// The wallet's lamports.
    pub sol_lamports: u64,
    /// The maker's token balance per mint, atoms.
    pub balances: Vec<(Address, u64)>,
    /// The tracked transactions found finalized in this snapshot.
    pub landed: Vec<Landed>,
}

/// One past transaction of the wallet, for the rebuild after a restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spent {
    /// Its block time.
    pub at: i64,
    /// Its fee, lamports.
    pub fee_lamports: u64,
    /// Whether it was a successful Order Engine `fill` with this maker.
    pub fill: bool,
}

/// A failed read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("chain read failed: {0}")]
pub struct ChainError(pub String);

/// Finalized reads of the maker's wallet. Implementations never run inside the desk's lock.
pub trait ChainView {
    /// The wallet now, and which of `tracked` are finalized.
    ///
    /// # Errors
    ///
    /// Any failed or partial read.
    fn snapshot(&self, tracked: &[TxId]) -> Result<Snapshot, ChainError>;

    /// The wallet's finalized transactions since `since`.
    ///
    /// # Errors
    ///
    /// Any failed or partial read.
    fn history_since(&self, since: i64) -> Result<Vec<Spent>, ChainError>;
}

/// One market: a base and a quote mint the maker trades both ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Market {
    /// The base mint.
    pub base_mint: Address,
    /// The quote mint.
    pub quote_mint: Address,
    /// The maker's base token account.
    pub maker_base_account: Address,
    /// The maker's quote token account.
    pub maker_quote_account: Address,
    /// Decimals and pricing settings.
    pub pricing: PricingInputs,
    /// The most base atoms one quote may pay out.
    pub max_base_out: u64,
    /// The most quote atoms one quote may pay out.
    pub max_quote_out: u64,
    /// The base balance the maker aims at; above it, the model skews to sell.
    pub target_base_atoms: u64,
}

/// The desk's limits; all come from config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Live quote entries kept at most.
    pub max_quotes: usize,
    /// A quote's life: 55 s, fixed by Jupiter (Quoting).
    pub quote_life_secs: i64,
    /// `expire_at` must leave at least this much time to land.
    pub min_landing_secs: i64,
    /// The oldest snapshot accepted, by its Clock.
    pub max_snapshot_age_secs: i64,
    /// The oldest spot accepted.
    pub max_spot_age_secs: i64,
    /// Reservations held at most.
    pub max_open_swaps: usize,
    /// Fills in the last hour, landed or reserved, at most.
    pub max_fills_per_hour: usize,
    /// Fees spent in the last hour plus those reserved, lamports, at most.
    pub max_fees_per_hour: u64,
    /// The wallet's lamports never go below this, counting reserved fees.
    pub sol_floor: u64,
    /// Its existence refuses every quote and swap.
    pub halt_file: PathBuf,
}

/// Everything the desk is configured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeskConfig {
    /// The maker's wallet, the fee payer. The desk holds no key for it.
    pub maker: Address,
    /// The markets.
    pub markets: Vec<Market>,
    /// The limits.
    pub limits: Limits,
    /// The per-transaction fee caps.
    pub budget: FeeBudget,
}

/// How a desk starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// No previous run could have co-signed anything.
    Fresh,
    /// A restart at `started_at`: wait out the previous run's transactions and rebuild the
    /// hour from the chain.
    Resume {
        /// Wall clock at start.
        started_at: i64,
    },
}

/// Whether a valid swap is reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Commit {
    /// Check everything, change nothing: what the HTTP layer uses, since it never signs.
    DryRun,
    /// Reserve, as a co-signing maker would. Tests and the desk's own API only.
    Reserve,
}

/// Why `/quote` declines. Everything but `BadRequest` is a 404, which costs nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Decline {
    /// The request is malformed: a 400.
    #[error("bad request: {0}")]
    BadRequest(&'static str),
    /// The pair is not quoted.
    #[error("pair not supported")]
    Unsupported,
    /// A receiver other than the taker, which needs a transfer the prototype refuses.
    #[error("a separate receiver is not supported")]
    Receiver,
    /// The halt file exists.
    #[error("halted")]
    Halted,
    /// No usable snapshot.
    #[error("risk state unavailable: {0}")]
    Unavailable(&'static str),
    /// The restart's cold start, or a failed rebuild of the hour.
    #[error("not quoting until {0}")]
    NotYet(i64),
    /// The spot is missing or stale.
    #[error("no fresh spot")]
    Spot,
    /// The model, the guard or the amount refused.
    #[error("{0}")]
    Price(PriceError),
    /// A limit refuses.
    #[error("limit: {0}")]
    Limit(&'static str),
    /// This `quoteId` was already swapped.
    #[error("quote id already used")]
    QuoteIdUsed,
}

/// Why `/swap` rejects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    /// A reason Jupiter knows, when one applies.
    pub known: Option<RejectionReason>,
    /// The reason in words, for `rejectionReason` and the log.
    pub message: String,
}

impl Rejection {
    fn plain(message: impl Into<String>) -> Self {
        Self {
            known: None,
            message: message.into(),
        }
    }

    fn known(known: RejectionReason, message: impl Into<String>) -> Self {
        Self {
            known: Some(known),
            message: message.into(),
        }
    }
}

/// What `/swap` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapDecision {
    /// Every check passed: this is what a co-signing maker would sign.
    WouldSign {
        /// The message bytes the maker would sign.
        message: Vec<u8>,
        /// The validated fill.
        validated: Validated,
    },
    /// Refused.
    Reject(Rejection),
}

/// What the desk reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeskStatus {
    /// Whether the halt file exists.
    pub halted: bool,
    /// Live quote entries.
    pub quotes: usize,
    /// Reservations held.
    pub open_swaps: usize,
    /// Fees reserved, lamports.
    pub reserved_fees: u64,
    /// Fills in the last hour of chain Clock.
    pub fills_last_hour: usize,
    /// Fees spent in the last hour of chain Clock, lamports.
    pub fees_last_hour: u64,
    /// Jupiter's fee accrued on landed fills and not yet collected, per output mint, atoms.
    pub accrued_fees: Vec<(Address, u64)>,
    /// Why the debt ledger refuses, if it does.
    pub ledger_failure: Option<&'static str>,
}

/// Why a collection is not recorded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CollectionError {
    /// No evidence, or evidence too long.
    #[error("evidence of collection is required (1 to 128 characters)")]
    Evidence,
    /// More than is owed on that mint.
    #[error("collected {collected} atoms but only {owed} are owed")]
    MoreThanOwed {
        /// Atoms recorded as collected.
        collected: u64,
        /// Atoms owed.
        owed: u64,
    },
    /// The ledger was never loaded, so nothing it holds is known.
    #[error("the debt ledger was not loaded")]
    NotLoaded,
    /// The release could not be written; nothing was released.
    #[error("{0}")]
    Write(String),
}

#[derive(Debug, Clone)]
struct StoredQuote {
    market: usize,
    side: Side,
    quote_type: QuoteType,
    taker: Option<Address>,
    amount_in: u64,
    amount_out: u64,
    fee_bps: u16,
    quoted_at: i64,
    consumed: bool,
}

/// A transaction already accepted: its quote, what was validated and, once the reservation is
/// released, the terminal outcome a retry gets.
#[derive(Debug, Clone)]
struct Accepted {
    quote_id: String,
    validated: Validated,
    /// The verified transaction's signatures: a retry must carry the same ones.
    signatures: Vec<[u8; 64]>,
    outcome: Option<&'static str>,
}

#[derive(Debug, Clone, Copy)]
enum Startup {
    Waiting { until: i64, started_at: i64 },
    Ready,
}

#[derive(Debug)]
struct State {
    quotes: HashMap<String, StoredQuote>,
    reservations: Vec<Reservation>,
    /// Message bytes already accepted: a retry is idempotent while it is safe.
    accepted: HashMap<Vec<u8>, Accepted>,
    snapshot: Option<Snapshot>,
    failure: Option<&'static str>,
    startup: Startup,
    blocked_until: Option<i64>,
    fills: VecDeque<i64>,
    fees: VecDeque<(i64, u64)>,
    /// Fee debt per output mint, atoms: held against the balance until collected.
    accrued: HashMap<Address, u64>,
    /// Whether the ledger was loaded; if not, it is never written over.
    ledger_loaded: bool,
    /// An accrual not yet written.
    ledger_dirty: bool,
    ledger_failure: Option<&'static str>,
}

/// The desk.
pub struct Desk {
    config: DeskConfig,
    spot: Box<dyn SpotSource>,
    clock: Box<dyn Clock>,
    ledger: Box<dyn DebtLedger>,
    state: Mutex<State>,
}

impl std::fmt::Debug for Desk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Desk")
            .field("maker", &self.config.maker)
            .field("markets", &self.config.markets.len())
            .finish_non_exhaustive()
    }
}

fn halted(limits: &Limits) -> bool {
    match std::fs::symlink_metadata(&limits.halt_file) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

fn parse_address(s: &str) -> Option<Address> {
    Address::from_str(s).ok()
}

fn parse_amount(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok().filter(|a| *a > 0)
}

/// `ceil(amount × bps / 10,000)`.
fn fee_liability(amount: u64, bps: u16) -> Option<u64> {
    let p = u128::from(amount).checked_mul(u128::from(bps))?;
    let q = p.checked_add(9_999)?.checked_div(10_000)?;
    u64::try_from(q).ok()
}

impl State {
    fn owed(&self, mint: &Address) -> u64 {
        self.accrued.get(mint).copied().unwrap_or(0)
    }

    /// What reservations and uncollected fee debt hold of `mint`.
    fn reserved_out(&self, mint: &Address) -> Option<u64> {
        self.reservations
            .iter()
            .filter(|r| &r.output_mint == mint)
            .try_fold(self.owed(mint), |n, r| {
                n.checked_add(r.amount_out)?.checked_add(r.fee_liability)
            })
    }

    fn ledger_state(&self) -> LedgerState {
        LedgerState {
            debts: self.debts(),
            pending: self.reservations.clone(),
        }
    }

    fn debts(&self) -> Vec<(Address, u64)> {
        let mut out: Vec<(Address, u64)> = self
            .accrued
            .iter()
            .filter(|(_, a)| **a > 0)
            .map(|(m, a)| (*m, *a))
            .collect();
        out.sort_by_key(|(m, _)| m.to_string());
        out
    }

    fn reserved_fees(&self) -> Option<u64> {
        self.reservations
            .iter()
            .try_fold(0u64, |n, r| n.checked_add(r.fee_lamports))
    }

    fn fees_since(&self, floor: i64) -> Option<u64> {
        self.fees
            .iter()
            .filter(|(t, _)| *t > floor)
            .try_fold(0u64, |n, (_, f)| n.checked_add(*f))
    }

    fn fills_since(&self, floor: i64) -> usize {
        self.fills.iter().filter(|t| **t > floor).count()
    }

    fn balance(&self, mint: &Address) -> u64 {
        self.snapshot
            .as_ref()
            .and_then(|s| s.balances.iter().find(|(m, _)| m == mint).map(|(_, b)| *b))
            .unwrap_or(0)
    }

    /// What may still go out of `mint`: the balance less what reservations and debt hold.
    fn available(&self, mint: &Address) -> Option<u64> {
        self.balance(mint).checked_sub(self.reserved_out(mint)?)
    }
}

impl Desk {
    /// A desk over `config`, with the fee debt loaded from `ledger`. A ledger that cannot be
    /// loaded refuses every quote and swap.
    #[must_use]
    pub fn new(
        config: DeskConfig,
        spot: Box<dyn SpotSource>,
        clock: Box<dyn Clock>,
        ledger: Box<dyn DebtLedger>,
        start: Start,
    ) -> Self {
        let (accrued, reservations, ledger_loaded, ledger_failure) = match ledger.load() {
            Ok(l) => (
                l.debts.into_iter().collect::<HashMap<_, _>>(),
                l.pending,
                true,
                None,
            ),
            Err(e) => {
                tracing::warn!(error = %e, "debt ledger not loaded; refusing until restarted with a readable ledger");
                (
                    HashMap::new(),
                    Vec::new(),
                    false,
                    Some("debt ledger unreadable"),
                )
            }
        };
        let startup = match start {
            Start::Fresh => Startup::Ready,
            Start::Resume { started_at } => Startup::Waiting {
                // A quote lives 55 s; the previous run's last co-signed `expire_at` is at most
                // that, plus skew, after its last quote, which is before `started_at`.
                until: started_at
                    .saturating_add(config.limits.quote_life_secs)
                    .saturating_add(MAX_CLOCK_SKEW_SECS),
                started_at,
            },
        };
        Self {
            config,
            spot,
            clock,
            ledger,
            state: Mutex::new(State {
                quotes: HashMap::new(),
                reservations,
                accepted: HashMap::new(),
                snapshot: None,
                failure: None,
                startup,
                blocked_until: None,
                fills: VecDeque::new(),
                fees: VecDeque::new(),
                accrued,
                ledger_loaded,
                ledger_dirty: false,
                ledger_failure,
            }),
        }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &DeskConfig {
        &self.config
    }

    /// Every mint the desk quotes, for `GET /tokens`.
    #[must_use]
    pub fn tokens(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for m in &self.config.markets {
            for mint in [m.base_mint, m.quote_mint] {
                let s = mint.to_string();
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
        out
    }

    /// Reads the chain, outside the lock, and applies it.
    ///
    /// # Errors
    ///
    /// The read's error; the desk then refuses until a read succeeds, and releases nothing.
    pub fn reconcile(&self, chain: &dyn ChainView) -> Result<(), ChainError> {
        let (tracked, waiting) = {
            let state = self.lock();
            (
                state.reservations.iter().map(|r| r.id).collect::<Vec<_>>(),
                matches!(state.startup, Startup::Waiting { .. }),
            )
        };
        let snapshot = match chain.snapshot(&tracked) {
            Ok(s) => s,
            Err(e) => {
                self.lock().failure = Some("snapshot read failed");
                return Err(e);
            }
        };
        let history = if waiting {
            Some(chain.history_since(snapshot.clock.saturating_sub(HOUR_SECS)))
        } else {
            None
        };
        self.apply(snapshot, history);
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A poisoned lock means a panic elsewhere; the state is still the last consistent one,
        // since every mutation below is a single assignment or push.
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn apply(&self, snapshot: Snapshot, history: Option<Result<Vec<Spent>, ChainError>>) {
        let mut state = self.lock();
        if state
            .snapshot
            .as_ref()
            .is_some_and(|last| snapshot.slot < last.slot)
        {
            state.failure = Some("snapshot slot went backwards");
            return;
        }
        let clock = snapshot.clock;
        let mut kept = Vec::with_capacity(state.reservations.len());
        let mut released: Vec<(TxId, &'static str)> = Vec::new();
        let reservations = std::mem::take(&mut state.reservations);
        for r in reservations {
            if let Some(landed) = snapshot.landed.iter().find(|l| l.id == r.id) {
                state.fees.push_back((landed.at, landed.fee_lamports));
                if landed.failed {
                    released.push((r.id, "finalized as failed"));
                } else {
                    state.fills.push_back(landed.at);
                    let owed = state.owed(&r.output_mint);
                    match owed.checked_add(r.fee_liability) {
                        Some(n) => {
                            state.accrued.insert(r.output_mint, n);
                        }
                        None => {
                            // Never wraps: the debt is pinned at the maximum and the desk refuses.
                            state.accrued.insert(r.output_mint, u64::MAX);
                            state.ledger_failure = Some("fee debt overflows");
                        }
                    }
                    state.ledger_dirty = true;
                    released.push((r.id, "finalized"));
                }
            } else if clock <= r.expire_at {
                // `fill` still passes when Clock equals `expire_at`: release only past it.
                kept.push(r);
            } else {
                released.push((r.id, "expired unlanded and released"));
            }
        }
        state.reservations = kept;
        // Every release leaves the ledger, in the same write as any fee it accrued.
        state.ledger_dirty = state.ledger_dirty || !released.is_empty();
        for (id, outcome) in released {
            for a in state.accepted.values_mut() {
                if a.validated.taker_signature == id {
                    a.outcome = Some(outcome);
                }
            }
        }
        // A released transaction's terminal answer is kept an hour past its `expire_at`; after
        // that its quote id is refused anyway.
        state.accepted.retain(|_, a| {
            a.outcome.is_none() || clock <= a.validated.expire_at.saturating_add(HOUR_SECS)
        });
        if state.ledger_dirty && state.ledger_loaded {
            let contents = state.ledger_state();
            match self.ledger.store(&contents) {
                Ok(()) => {
                    state.ledger_dirty = false;
                    if state.ledger_failure == Some("debt ledger write failed") {
                        state.ledger_failure = None;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "debt ledger write failed; refusing until it succeeds");
                    if state.ledger_failure.is_none() {
                        state.ledger_failure = Some("debt ledger write failed");
                    }
                }
            }
        }
        if let Startup::Waiting { until, started_at } = state.startup {
            if clock >= until {
                match history {
                    Some(Ok(spent)) => {
                        for s in spent {
                            state.fees.push_back((s.at, s.fee_lamports));
                            if s.fill {
                                state.fills.push_back(s.at);
                            }
                        }
                    }
                    Some(Err(_)) | None => {
                        state.blocked_until = Some(started_at.saturating_add(HOUR_SECS));
                    }
                }
                state.startup = Startup::Ready;
            }
        }
        let floor = clock.saturating_sub(HOUR_SECS);
        state.fills.retain(|t| *t > floor);
        state.fees.retain(|(t, _)| *t > floor);
        state.snapshot = Some(snapshot);
        state.failure = None;
    }

    /// Why nothing can be quoted or co-signed now, if anything.
    fn readiness(&self, state: &State, now: i64) -> Result<i64, Decline> {
        if halted(&self.config.limits) {
            return Err(Decline::Halted);
        }
        if let Some(why) = state.ledger_failure {
            return Err(Decline::Unavailable(why));
        }
        if let Some(why) = state.failure {
            return Err(Decline::Unavailable(why));
        }
        let snapshot = state
            .snapshot
            .as_ref()
            .ok_or(Decline::Unavailable("no snapshot yet"))?;
        if now.saturating_sub(snapshot.clock) > self.config.limits.max_snapshot_age_secs {
            return Err(Decline::Unavailable("snapshot too old"));
        }
        if let Startup::Waiting { until, .. } = state.startup {
            return Err(Decline::NotYet(until));
        }
        if let Some(until) = state.blocked_until {
            if snapshot.clock < until {
                return Err(Decline::NotYet(until));
            }
        }
        Ok(snapshot.clock)
    }

    /// The limits a swap must pass: the reservation count, fills, inventory and fee budget.
    fn check_limits(
        &self,
        state: &State,
        clock: i64,
        output_mint: &Address,
        out_with_liability: u64,
        fee_lamports: u64,
    ) -> Result<(), Decline> {
        let l = &self.config.limits;
        if state.reservations.len() >= l.max_open_swaps {
            return Err(Decline::Limit("open swaps"));
        }
        let floor = clock.saturating_sub(HOUR_SECS);
        if state
            .fills_since(floor)
            .saturating_add(state.reservations.len())
            >= l.max_fills_per_hour
        {
            return Err(Decline::Limit("fills per hour"));
        }
        let available = state
            .available(output_mint)
            .ok_or(Decline::Limit("inventory"))?;
        if out_with_liability > available {
            return Err(Decline::Limit("inventory"));
        }
        let reserved_fees = state.reserved_fees().ok_or(Decline::Limit("fee budget"))?;
        let sol = state.snapshot.as_ref().map_or(0, |s| s.sol_lamports);
        let left = sol
            .checked_sub(reserved_fees)
            .and_then(|s| s.checked_sub(fee_lamports));
        if left.is_none_or(|left| left < l.sol_floor) {
            return Err(Decline::Limit("sol floor"));
        }
        let hour = state
            .fees_since(floor)
            .and_then(|f| f.checked_add(reserved_fees))
            .and_then(|f| f.checked_add(fee_lamports));
        if hour.is_none_or(|h| h > l.max_fees_per_hour) {
            return Err(Decline::Limit("fees per hour"));
        }
        Ok(())
    }

    /// Answers `/quote`: a price, or a decline.
    ///
    /// # Errors
    ///
    /// See [`Decline`].
    pub fn quote(&self, req: &QuoteRequest) -> Result<QuoteResponse, Decline> {
        if req.request_id.len() > MAX_ID_LEN || req.quote_id.len() > MAX_ID_LEN {
            return Err(Decline::BadRequest("id too long"));
        }
        if req.quote_id.is_empty() {
            return Err(Decline::BadRequest("empty quote id"));
        }
        let amount = parse_amount(&req.amount).ok_or(Decline::BadRequest("amount"))?;
        if req.fee_bps > 10_000 {
            return Err(Decline::BadRequest("fee bps above 10000"));
        }
        let (input, output) = match (parse_address(&req.token_in), parse_address(&req.token_out)) {
            (Some(i), Some(o)) => (i, o),
            _ => return Err(Decline::Unsupported),
        };
        let (index, market, side) = self
            .config
            .markets
            .iter()
            .enumerate()
            .find_map(|(i, m)| {
                if m.base_mint == input && m.quote_mint == output {
                    Some((i, m, Side::MakerBuysBase))
                } else if m.quote_mint == input && m.base_mint == output {
                    Some((i, m, Side::MakerSellsBase))
                } else {
                    None
                }
            })
            .ok_or(Decline::Unsupported)?;
        let taker = match &req.taker {
            Some(t) => Some(parse_address(t).ok_or(Decline::BadRequest("taker"))?),
            None => None,
        };
        if let Some(receiver) = &req.receiver {
            let r = parse_address(receiver).ok_or(Decline::BadRequest("receiver"))?;
            if Some(r) != taker {
                return Err(Decline::Receiver);
            }
        }
        let now = self.clock.now();
        let spot = self.spot.spot(index).ok_or(Decline::Spot)?;
        let spot_age = now.saturating_sub(spot.published_at);
        if spot_age > self.config.limits.max_spot_age_secs || spot_age < -MAX_CLOCK_SKEW_SECS {
            return Err(Decline::Spot);
        }

        let mut state = self.lock();
        let clock = self.readiness(&state, now)?;
        if state.quotes.get(&req.quote_id).is_some_and(|q| q.consumed) {
            return Err(Decline::QuoteIdUsed);
        }
        let base_held = i128::from(state.balance(&market.base_mint))
            .saturating_sub(i128::from(
                state.reserved_out(&market.base_mint).unwrap_or(u64::MAX),
            ))
            .saturating_sub(i128::from(market.target_base_atoms));
        let inventory =
            Decimal::try_from_i128_with_scale(base_held, u32::from(market.pricing.base_decimals))
                .map_err(|_| Decline::Limit("inventory"))?;
        let priced = pricing::price(
            &market.pricing,
            spot.mid,
            inventory,
            side,
            req.quote_type,
            amount,
        )
        .map_err(Decline::Price)?;
        let (output_mint, cap) = match side {
            Side::MakerBuysBase => (market.quote_mint, market.max_quote_out),
            Side::MakerSellsBase => (market.base_mint, market.max_base_out),
        };
        if priced.amount_out > cap {
            return Err(Decline::Limit("size per quote"));
        }
        let liability =
            fee_liability(priced.amount_out, req.fee_bps).ok_or(Decline::Limit("inventory"))?;
        let out = priced
            .amount_out
            .checked_add(liability)
            .ok_or(Decline::Limit("inventory"))?;
        // At `/quote` the fee is not known yet: the per-transaction cap stands in for it.
        self.check_limits(
            &state,
            clock,
            &output_mint,
            out,
            self.config.budget.max_fee_lamports,
        )?;

        let life = self
            .config
            .limits
            .quote_life_secs
            .saturating_add(MAX_CLOCK_SKEW_SECS);
        // A live entry lasts the quote's life; a consumed one an hour, so its id stays refused.
        state.quotes.retain(|_, q| {
            let age = now.saturating_sub(q.quoted_at);
            if q.consumed {
                age <= HOUR_SECS
            } else {
                age <= life
            }
        });
        if !state.quotes.contains_key(&req.quote_id)
            && state.quotes.len() >= self.config.limits.max_quotes
        {
            return Err(Decline::Limit("quote store full"));
        }
        state.quotes.insert(
            req.quote_id.clone(),
            StoredQuote {
                market: index,
                side,
                quote_type: req.quote_type,
                taker,
                amount_in: priced.amount_in,
                amount_out: priced.amount_out,
                fee_bps: req.fee_bps,
                quoted_at: now,
                consumed: false,
            },
        );
        drop(state);
        Ok(QuoteResponse {
            request_id: req.request_id.clone(),
            quote_id: req.quote_id.clone(),
            token_in: req.token_in.clone(),
            amount_in: priced.amount_in.to_string(),
            token_out: req.token_out.clone(),
            quote_type: req.quote_type,
            protocol: req.protocol,
            amount_out: priced.amount_out.to_string(),
            maker: self.config.maker.to_string(),
            prioritization_fee_to_use: req
                .suggested_prioritization_fees
                .map(|p| p.min(self.config.budget.max_cu_price)),
            taker: req.taker.clone(),
            receiver: req.receiver.clone(),
            source: req.source.clone(),
        })
    }

    /// Answers `/swap`: what a co-signing maker would sign, or a rejection.
    #[must_use]
    pub fn swap(&self, req: &SwapRequest, commit: Commit) -> SwapDecision {
        match self.swap_inner(req, commit) {
            Ok(d) => d,
            Err(r) => SwapDecision::Reject(r),
        }
    }

    fn swap_inner(&self, req: &SwapRequest, commit: Commit) -> Result<SwapDecision, Rejection> {
        if req.request_id.len() > MAX_ID_LEN || req.quote_id.len() > MAX_ID_LEN {
            return Err(Rejection::plain("id too long"));
        }
        if halted(&self.config.limits) {
            return Err(Rejection::plain("halted"));
        }
        let tx = Transaction::from_base64(&req.transaction)
            .map_err(|e| Rejection::plain(format!("malformed transaction: {e}")))?;
        let now = self.clock.now();
        let mut state = self.lock();
        if let Some(a) = state.accepted.get(&tx.message_bytes).cloned() {
            if a.quote_id != req.quote_id {
                return Err(Rejection::plain(
                    "transaction already accepted for another quote",
                ));
            }
            if let Some(outcome) = a.outcome {
                return Err(Rejection::plain(format!(
                    "transaction already {outcome}; not co-signed again"
                )));
            }
            if !state
                .reservations
                .iter()
                .any(|r| r.id == a.validated.taker_signature)
            {
                return Err(Rejection::plain(
                    "transaction no longer reserved; not co-signed again",
                ));
            }
            if tx.signatures != a.signatures {
                return Err(Rejection::known(
                    RejectionReason::SignatureVerificationFailed,
                    "the signatures differ from the accepted transaction's",
                ));
            }
            let clock = self
                .readiness(&state, now)
                .map_err(|d| Rejection::plain(d.to_string()))?;
            let landing = now.saturating_add(self.config.limits.min_landing_secs);
            if clock > a.validated.expire_at || landing > a.validated.expire_at {
                return Err(Rejection::plain("too close to expire_at to co-sign again"));
            }
            return Ok(SwapDecision::WouldSign {
                message: tx.message_bytes,
                validated: a.validated,
            });
        }
        let quote = state
            .quotes
            .get(&req.quote_id)
            .cloned()
            .ok_or_else(|| Rejection::plain("unknown quote"))?;
        if quote.consumed {
            return Err(Rejection::plain("quote already used"));
        }
        if now.saturating_sub(quote.quoted_at) > self.config.limits.quote_life_secs {
            return Err(Rejection::plain("quote expired"));
        }
        let taker = quote
            .taker
            .ok_or_else(|| Rejection::plain("the quote named no taker"))?;
        let clock = self
            .readiness(&state, now)
            .map_err(|d| Rejection::plain(d.to_string()))?;
        let market = self
            .config
            .markets
            .get(quote.market)
            .ok_or_else(|| Rejection::plain("unknown market"))?;
        let (input_mint, output_mint, maker_in, maker_out) = match quote.side {
            Side::MakerBuysBase => (
                market.base_mint,
                market.quote_mint,
                market.maker_base_account,
                market.maker_quote_account,
            ),
            Side::MakerSellsBase => (
                market.quote_mint,
                market.base_mint,
                market.maker_quote_account,
                market.maker_base_account,
            ),
        };
        let expected = Expected {
            maker: self.config.maker,
            taker,
            input_mint,
            output_mint,
            input_amount: quote.amount_in,
            output_amount: quote.amount_out,
            maker_input_account: maker_in,
            maker_output_account: maker_out,
            fee_bps: quote.fee_bps,
            quote_type: quote.quote_type,
            expire_at_min: quote.quoted_at,
            expire_at_max: quote
                .quoted_at
                .saturating_add(self.config.limits.quote_life_secs)
                .saturating_add(MAX_CLOCK_SKEW_SECS),
            now,
            min_landing_secs: self.config.limits.min_landing_secs,
        };
        let validated =
            validate::validate(&tx, &expected, &self.config.budget).map_err(|e| match e {
                Invalid::TakerSignature => {
                    Rejection::known(RejectionReason::SignatureVerificationFailed, e.to_string())
                }
                other => Rejection::plain(other.to_string()),
            })?;
        let liability = fee_liability(quote.amount_out, quote.fee_bps)
            .ok_or_else(|| Rejection::plain("fee liability overflows"))?;
        let out = quote
            .amount_out
            .checked_add(liability)
            .ok_or_else(|| Rejection::plain("amount overflows"))?;
        self.check_limits(&state, clock, &output_mint, out, validated.fee_lamports)
            .map_err(|d| match d {
                Decline::Limit("inventory") => {
                    Rejection::known(RejectionReason::InsufficientBalance, d.to_string())
                }
                other => Rejection::plain(other.to_string()),
            })?;
        if commit == Commit::Reserve {
            state.reservations.push(Reservation {
                id: validated.taker_signature,
                output_mint,
                amount_out: quote.amount_out,
                fee_liability: liability,
                fee_lamports: validated.fee_lamports,
                expire_at: validated.expire_at,
            });
            // Durable before any permission to sign, so a restart cannot lose this fill.
            let contents = state.ledger_state();
            if let Err(e) = self.ledger.store(&contents) {
                state.reservations.pop();
                tracing::warn!(error = %e, "fill not written; refused");
                return Err(Rejection::plain("debt ledger write failed"));
            }
            if let Some(q) = state.quotes.get_mut(&req.quote_id) {
                q.consumed = true;
            }
            state.accepted.insert(
                tx.message_bytes.clone(),
                Accepted {
                    quote_id: req.quote_id.clone(),
                    validated,
                    signatures: tx.signatures.clone(),
                    outcome: None,
                },
            );
        }
        Ok(SwapDecision::WouldSign {
            message: tx.message_bytes,
            validated,
        })
    }

    /// The desk's state.
    #[must_use]
    pub fn status(&self) -> DeskStatus {
        let state = self.lock();
        let clock = state.snapshot.as_ref().map_or(0, |s| s.clock);
        let floor = clock.saturating_sub(HOUR_SECS);
        let accrued = state.debts();
        DeskStatus {
            halted: halted(&self.config.limits),
            quotes: state.quotes.len(),
            open_swaps: state.reservations.len(),
            reserved_fees: state.reserved_fees().unwrap_or(u64::MAX),
            fills_last_hour: state.fills_since(floor),
            fees_last_hour: state.fees_since(floor).unwrap_or(u64::MAX),
            accrued_fees: accrued,
            ledger_failure: state.ledger_failure,
        }
    }

    /// Records that `atoms` of the fee debt on `mint` were collected, with the operator's
    /// `evidence` (a transaction signature or an invoice reference), and returns what is still
    /// owed. The release is written to the ledger first; if that fails nothing is released.
    ///
    /// # Errors
    ///
    /// See [`CollectionError`].
    pub fn record_collection(
        &self,
        mint: Address,
        atoms: u64,
        evidence: &str,
    ) -> Result<u64, CollectionError> {
        let evidence = evidence.trim();
        if evidence.is_empty() || evidence.chars().count() > MAX_ID_LEN {
            return Err(CollectionError::Evidence);
        }
        let mut state = self.lock();
        if !state.ledger_loaded {
            return Err(CollectionError::NotLoaded);
        }
        let owed = state.owed(&mint);
        let left = owed
            .checked_sub(atoms)
            .ok_or(CollectionError::MoreThanOwed {
                collected: atoms,
                owed,
            })?;
        let mut debts: Vec<(Address, u64)> = state
            .debts()
            .into_iter()
            .filter(|(m, _)| *m != mint)
            .collect();
        if left > 0 {
            debts.push((mint, left));
        }
        self.ledger
            .store(&LedgerState {
                debts,
                pending: state.reservations.clone(),
            })
            .map_err(|e| CollectionError::Write(e.to_string()))?;
        if left > 0 {
            state.accrued.insert(mint, left);
        } else {
            state.accrued.remove(&mint);
        }
        state.ledger_dirty = false;
        if state.ledger_failure == Some("debt ledger write failed") {
            state.ledger_failure = None;
        }
        drop(state);
        tracing::info!(%mint, atoms, left, evidence, "fee collection recorded");
        Ok(left)
    }
}
