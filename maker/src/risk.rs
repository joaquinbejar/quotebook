//! The risk book: what every signed quote holds until it can no longer fill
//! (docs/PLAN-phase2.md, D1).
//!
//! A firm quote stays fillable after it is signed, so a limit is only real if checking it,
//! reserving what the quote needs and taking its nonce are one atomic step. [`Book`] holds the
//! [`NonceIssuer`] and the reservations together, behind the service's one lock.
//!
//! # Releases
//!
//! A reservation ends only on a finalized [`ChainSnapshot`], when in that same snapshot either
//! the nonce can no longer fill (used, cancelled or below the window), so the balances already
//! include its fill, or the Clock has reached its expiry, so the chain can never fill it. Nothing
//! is released on wall-clock time or `confirmed` data, so a dropped fork cannot release funds.
//!
//! # Fail closed
//!
//! A failed read, a snapshot's slot going backwards, a snapshot older than `max_snapshot_age` or
//! an unreadable halt file refuses every quote and releases nothing.
//!
//! # Restarts
//!
//! Reservations live in memory, so a resumed service quotes nothing until the cold start has
//! passed **and** a snapshot's Clock is past its end: every older quote is then dead and its fills
//! are in the balances. The hour's fills are rebuilt from the chain at that point
//! ([`ChainView::fill_times_since`]); if that fails, the limit counts as used up until the Clock
//! is [`FILL_WINDOW_SECS`] past the restart.

use std::collections::VecDeque;
use std::path::PathBuf;

use quotebook_core::instruction::Tag;
use quotebook_core::{Nonce, NonceWindow, Seconds, Side, UnixTimestamp};
use solana_address::Address;

use crate::issuer::{IssueError, NonceIssuer};

/// The span `max_fills_per_hour` counts over, in seconds of the chain's Clock.
pub const FILL_WINDOW_SECS: i64 = 3_600;

/// One token account, as a finalized snapshot read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenAccountState {
    /// The balance, in atoms.
    pub amount: u64,
    /// What the maker's delegate PDA may still move, in atoms; 0 if it is not the delegate.
    pub delegated_amount: u64,
}

/// One read of the chain at `finalized`, all from one slot (one `getMultipleAccounts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSnapshot {
    /// The slot the read is from.
    pub slot: u64,
    /// The Clock sysvar at that slot.
    pub clock: UnixTimestamp,
    /// `MakerState.nonces` at that slot.
    pub window: NonceWindow,
    /// Every maker token account at that slot. An absent account has nothing available.
    pub accounts: Vec<(Address, TokenAccountState)>,
}

/// A failed chain read. Its text is logged, so implementations must leave RPC URLs out of it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("chain read failed: {0}")]
pub struct ChainError(pub String);

/// The chain, as the risk book reads it. Both reads are at `finalized`.
pub trait ChainView: Send + Sync {
    /// One consistent read of the Clock, the maker's window and its token accounts.
    ///
    /// # Errors
    ///
    /// Any failed read.
    fn snapshot(&self) -> Result<ChainSnapshot, ChainError>;

    /// The time of every `settle` on this maker since `since`: one entry per instruction, inner
    /// ones included ([`settle_count`]), `None` when its transaction has no `blockTime`.
    ///
    /// # Errors
    ///
    /// When the history cannot be read in full: a failed read, a page limit, or a transaction
    /// that does not decode. Partial results are an error, never a shorter list.
    fn fill_times_since(
        &self,
        since: UnixTimestamp,
    ) -> Result<Vec<Option<UnixTimestamp>>, ChainError>;
}

/// How many of a transaction's instructions are `settle`s of `program_id`. Pass its top-level and
/// inner instructions as `(program, data)`, so a transaction holding several counts for all.
pub fn settle_count<'a>(
    program_id: &Address,
    instructions: impl IntoIterator<Item = (&'a Address, &'a [u8])>,
) -> u64 {
    let settle = Tag::Settle as u8;
    let mut count = 0u64;
    for (program, data) in instructions {
        if program == program_id && data.first() == Some(&settle) {
            count = count.saturating_add(1);
        }
    }
    count
}

/// Service-wide limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskConfig {
    /// The most quotes held open across all markets.
    pub max_open_quotes: u32,
    /// The check is `fills in the last hour + open quotes < limit`: each firm quote counts as a
    /// possible fill.
    pub max_fills_per_hour: u32,
    /// A snapshot whose Clock is older than this refuses everything.
    pub max_snapshot_age: Seconds,
    /// The only takers quotes are signed for. Empty refuses everyone.
    pub allowed_takers: Vec<Address>,
    /// When this path exists, or reading it fails with anything but not found, everything is
    /// refused. It stops new quotes only: signed ones stay fillable until they expire.
    pub halt_file: PathBuf,
}

/// Per-market limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MarketLimits {
    /// The most quote atoms held by the market's open quotes.
    pub max_notional: u64,
    /// The highest position, in base lots, with every open bid assumed filled.
    pub max_long: u64,
    /// The lowest position below zero, in base lots, with every open ask assumed filled.
    pub max_short: u64,
}

/// Why the book refused a quote.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The halt file exists or cannot be read.
    #[error("halted")]
    Halted,
    /// The book cannot tell what is at risk.
    #[error("risk state unavailable: {0}")]
    Unavailable(&'static str),
    /// A limit would be exceeded.
    #[error("risk limit: {0}")]
    Limit(&'static str),
    /// The nonce issuer refused.
    #[error(transparent)]
    Issue(#[from] IssueError),
}

/// What a quote would need from the book.
#[derive(Debug, Clone, Copy)]
pub struct Order {
    /// The series.
    pub series: Address,
    /// The maker's side.
    pub side: Side,
    /// Size in base lots.
    pub lots: u64,
    /// Quote atoms a fill exchanges.
    pub notional: u64,
    /// The account a fill takes from: the base account on an ask, the quote account on a bid.
    pub debit_account: Address,
    /// Atoms a fill takes from `debit_account`.
    pub debit_atoms: u64,
    /// The market's base account, for the position.
    pub base_account: Address,
    /// Base atoms per lot, for the position.
    pub base_lot_size: u64,
    /// The market's limits.
    pub limits: MarketLimits,
    /// When the quote is stamped.
    pub issued_at: UnixTimestamp,
    /// When it expires.
    pub expires_at: UnixTimestamp,
}

#[derive(Debug, Clone, Copy)]
struct Reservation {
    nonce: Nonce,
    series: Address,
    side: Side,
    lots: u64,
    notional: u64,
    debit_account: Address,
    debit_atoms: u64,
    expires_at: UnixTimestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Startup {
    /// A resumed service, waiting for a snapshot whose Clock has reached `until`.
    Waiting {
        until: i64,
        started_at: i64,
    },
    Ready,
}

#[derive(Debug)]
struct RiskState {
    config: RiskConfig,
    startup: Startup,
    reservations: Vec<Reservation>,
    snapshot: Option<ChainSnapshot>,
    /// Why the last read was not accepted, until one is.
    failure: Option<&'static str>,
    /// Clock times of fills, including those rebuilt after a restart.
    fills: VecDeque<i64>,
    /// When the fill history could not be rebuilt: refuse until the Clock reaches this.
    fills_blocked_until: Option<i64>,
}

/// What a status request reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookStatus {
    /// Whether new quotes are refused by the halt file.
    pub halted: bool,
    /// Quotes still holding a reservation.
    pub open_quotes: u32,
    /// The nonce the next quote would take: the `--new-min` of an emergency `cancel-all`.
    pub next_nonce: u64,
    /// The latest expiry of any quote issued since start. A drain waits for a finalized Clock
    /// at or past it.
    pub newest_expires_at: Option<i64>,
    /// Seconds between `now` and the latest snapshot's Clock.
    pub snapshot_age_secs: Option<i64>,
}

/// The nonce issuer and the reservations, behind the service's one lock.
#[derive(Debug)]
pub struct Book {
    issuer: NonceIssuer,
    newest_expires_at: Option<i64>,
    risk: Option<RiskState>,
}

fn halted(config: &RiskConfig) -> bool {
    match std::fs::symlink_metadata(&config.halt_file) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::NotFound,
    }
}

impl RiskState {
    fn account(&self, address: &Address) -> Option<TokenAccountState> {
        let snapshot = self.snapshot.as_ref()?;
        snapshot
            .accounts
            .iter()
            .find(|(a, _)| a == address)
            .map(|(_, state)| *state)
    }

    fn clock(&self) -> Option<i64> {
        self.snapshot.as_ref().map(|s| s.clock.get())
    }

    fn recent_fills(&self) -> Option<usize> {
        let floor = self.clock()?.saturating_sub(FILL_WINDOW_SECS);
        Some(self.fills.iter().filter(|t| **t > floor).count())
    }

    /// Every check, in the order the refusals should read. Changes nothing.
    fn check(&self, order: &Order) -> Result<(), Refusal> {
        if halted(&self.config) {
            return Err(Refusal::Halted);
        }
        if let Startup::Waiting { until, .. } = self.startup {
            return Err(IssueError::ColdStart { until }.into());
        }
        if let Some(why) = self.failure {
            return Err(Refusal::Unavailable(why));
        }
        let clock = self
            .clock()
            .ok_or(Refusal::Unavailable("no snapshot yet"))?;
        if order.issued_at.get().saturating_sub(clock) > self.config.max_snapshot_age.get() {
            return Err(Refusal::Unavailable("snapshot too old"));
        }
        if self.fills_blocked_until.is_some_and(|until| clock < until) {
            return Err(Refusal::Limit("fill history unavailable since the restart"));
        }
        let open = self.reservations.len();
        if open >= usize::try_from(self.config.max_open_quotes).unwrap_or(usize::MAX) {
            return Err(Refusal::Limit("max_open_quotes"));
        }
        let fills = self
            .recent_fills()
            .ok_or(Refusal::Unavailable("no snapshot yet"))?;
        if fills.saturating_add(open)
            >= usize::try_from(self.config.max_fills_per_hour).unwrap_or(usize::MAX)
        {
            return Err(Refusal::Limit("max_fills_per_hour"));
        }
        self.check_market(order)?;
        self.check_funds(order)
    }

    fn check_market(&self, order: &Order) -> Result<(), Refusal> {
        let mut notional = u128::from(order.notional);
        let (mut asks, mut bids) = (0u128, 0u128);
        for r in self
            .reservations
            .iter()
            .filter(|r| r.series == order.series)
        {
            notional = notional.saturating_add(u128::from(r.notional));
            match r.side {
                Side::Ask => asks = asks.saturating_add(u128::from(r.lots)),
                Side::Bid => bids = bids.saturating_add(u128::from(r.lots)),
            }
        }
        if notional > u128::from(order.limits.max_notional) {
            return Err(Refusal::Limit("max_notional"));
        }
        let held = self
            .account(&order.base_account)
            .map_or(0, |a| a.amount)
            .checked_div(order.base_lot_size)
            .ok_or(Refusal::Limit("base lot size is zero"))?;
        let held = u128::from(held);
        let lots = u128::from(order.lots);
        match order.side {
            Side::Bid => {
                // Position with this and every open bid filled, no ask filled.
                let high = held.saturating_add(bids).saturating_add(lots);
                if high > u128::from(order.limits.max_long) {
                    return Err(Refusal::Limit("inventory band: max_long"));
                }
            }
            Side::Ask => {
                // Position with this and every open ask filled, no bid filled, compared as
                // `held + max_short >= asks + lots`, so nothing goes negative.
                let floor = held.saturating_add(u128::from(order.limits.max_short));
                if asks.saturating_add(lots) > floor {
                    return Err(Refusal::Limit("inventory band: max_short"));
                }
            }
        }
        Ok(())
    }

    fn check_funds(&self, order: &Order) -> Result<(), Refusal> {
        let state = self.account(&order.debit_account).unwrap_or_default();
        let usable = u128::from(state.amount.min(state.delegated_amount));
        let reserved = self
            .reservations
            .iter()
            .filter(|r| r.debit_account == order.debit_account)
            .fold(u128::from(order.debit_atoms), |sum, r| {
                sum.saturating_add(u128::from(r.debit_atoms))
            });
        if reserved > usable {
            return Err(Refusal::Limit("funds: balance or allowance"));
        }
        Ok(())
    }
}

impl Book {
    /// A book with no limits: the single-series service without a [`RiskConfig`].
    #[must_use]
    pub fn unchecked(issuer: NonceIssuer) -> Self {
        Self {
            issuer,
            newest_expires_at: None,
            risk: None,
        }
    }

    /// A book enforcing `config`. `started_at` is `Some` for a resumed issuer: it quotes nothing
    /// until a snapshot's Clock passes the issuer's cold start.
    #[must_use]
    pub fn checked(
        issuer: NonceIssuer,
        config: RiskConfig,
        started_at: Option<UnixTimestamp>,
    ) -> Self {
        let startup = match (issuer.cold_start_until(), started_at) {
            (Some(until), Some(started_at)) => Startup::Waiting {
                until,
                started_at: started_at.get(),
            },
            _ => Startup::Ready,
        };
        Self {
            issuer,
            newest_expires_at: None,
            risk: Some(RiskState {
                config,
                startup,
                reservations: Vec::new(),
                snapshot: None,
                failure: None,
                fills: VecDeque::new(),
                fills_blocked_until: None,
            }),
        }
    }

    /// The issuer, for a service with no [`RiskConfig`].
    pub fn issuer_mut(&mut self) -> &mut NonceIssuer {
        &mut self.issuer
    }

    /// Checks every limit, reserves and takes the nonce, as one step.
    ///
    /// # Errors
    ///
    /// The first [`Refusal`] that applies; nothing changes then.
    pub fn admit(&mut self, order: &Order) -> Result<Nonce, Refusal> {
        if let Some(risk) = &self.risk {
            risk.check(order)?;
        }
        let nonce = self.issuer.issue(order.issued_at, order.expires_at)?;
        let expiry = order.expires_at.get();
        self.newest_expires_at = Some(self.newest_expires_at.map_or(expiry, |e| e.max(expiry)));
        if let Some(risk) = &mut self.risk {
            risk.reservations.push(Reservation {
                nonce,
                series: order.series,
                side: order.side,
                lots: order.lots,
                notional: order.notional,
                debit_account: order.debit_account,
                debit_atoms: order.debit_atoms,
                expires_at: order.expires_at,
            });
        }
        Ok(nonce)
    }

    /// Drops the reservation of a quote that was never signed, so it can never fill.
    pub fn forget(&mut self, nonce: Nonce) {
        if let Some(risk) = &mut self.risk {
            risk.reservations.retain(|r| r.nonce != nonce);
        }
    }

    /// Records a failed read: everything is refused until a snapshot is accepted.
    pub fn read_failed(&mut self) {
        if let Some(risk) = &mut self.risk {
            risk.failure = Some("chain read failed");
        }
    }

    /// Whether a snapshot at `clock` ends the cold start, so the fill history must be rebuilt
    /// and passed to [`Book::apply`].
    #[must_use]
    pub fn needs_fill_history(&self, clock: UnixTimestamp) -> bool {
        matches!(
            self.risk.as_ref().map(|r| r.startup),
            Some(Startup::Waiting { until, .. }) if clock.get() >= until
        )
    }

    /// Applies a finalized snapshot: releases what can no longer fill, and ends the cold start
    /// if its Clock has passed it. A slot going backwards is refused and releases nothing.
    pub fn apply(
        &mut self,
        snapshot: ChainSnapshot,
        history: Option<Result<Vec<Option<UnixTimestamp>>, ChainError>>,
    ) {
        let Some(risk) = &mut self.risk else {
            return;
        };
        if risk
            .snapshot
            .as_ref()
            .is_some_and(|last| snapshot.slot < last.slot)
        {
            risk.failure = Some("snapshot slot went backwards");
            return;
        }
        let clock = snapshot.clock.get();
        let window = snapshot.window;
        let mut filled = 0usize;
        risk.reservations.retain(|r| {
            if window.check(r.nonce).is_err() {
                filled = filled.saturating_add(1);
                false
            } else {
                clock < r.expires_at.get()
            }
        });
        risk.fills.extend(std::iter::repeat_n(clock, filled));
        if let Startup::Waiting { until, started_at } = risk.startup {
            if clock >= until {
                if self.issuer.refresh(&window, snapshot.clock).is_err() {
                    risk.failure = Some("issuer refused the window");
                    return;
                }
                match history {
                    Some(Ok(times)) => risk
                        .fills
                        .extend(times.iter().map(|t| t.map_or(clock, UnixTimestamp::get))),
                    Some(Err(_)) | None => {
                        risk.fills_blocked_until =
                            Some(started_at.saturating_add(FILL_WINDOW_SECS));
                    }
                }
                risk.startup = Startup::Ready;
            }
        }
        let floor = clock.saturating_sub(FILL_WINDOW_SECS);
        risk.fills.retain(|t| *t > floor);
        risk.snapshot = Some(snapshot);
        risk.failure = None;
    }

    /// The book's state at `now`.
    #[must_use]
    pub fn status(&self, now: UnixTimestamp) -> BookStatus {
        let (is_halted, open, age) = self.risk.as_ref().map_or((false, 0, None), |risk| {
            (
                halted(&risk.config),
                u32::try_from(risk.reservations.len()).unwrap_or(u32::MAX),
                risk.clock().map(|c| now.get().saturating_sub(c)),
            )
        });
        BookStatus {
            halted: is_halted,
            open_quotes: open,
            next_nonce: self.issuer.peek().get(),
            newest_expires_at: self.newest_expires_at,
            snapshot_age_secs: age,
        }
    }
}
