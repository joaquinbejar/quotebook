//! The maker service: prices a request, signs a firm quote, answers over WebSocket.
//!
//! Logs carry request ids, sides, sizes, nonces and prices; never keys or signatures.

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use quotebook_core::{
    BaseLots, Nonce, NonceWindow, Quote, QuoteTerms, Seconds, SeriesId, Side, UnixTimestamp,
};
use rust_decimal::Decimal;
use solana_address::Address;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

use crate::issuer::{IssueError, NonceIssuer};
use crate::keys::MakerKey;
use crate::market::MarketInfo;
use crate::pricing::{self, PricingConfigError, PricingParams};
use crate::protocol::{
    to_hex, ClientMessage, QuoteMessage, QuoteRequest, RejectMessage, RejectReason, ServerMessage,
    SpotAge, StatusMessage, TakerSide,
};
use crate::risk::{
    Book, ChainError, ChainView, MarketLimits, Order, Refusal, RiskConfig, FILL_WINDOW_SECS,
};

/// The reference price, in quote tokens per base token.
pub trait PriceFeed: Send + Sync {
    /// The current mid, or `None` if the feed is stale or down.
    fn mid(&self) -> Option<Decimal>;

    /// Seconds since the reference price was published, for the status answer; `None` if the
    /// feed does not report it.
    fn age_secs(&self) -> Option<i64> {
        None
    }
}

/// The maker's position, in base tokens (positive when long).
pub trait Inventory: Send + Sync {
    /// The current inventory.
    fn inventory(&self) -> Decimal;
}

/// The clock quotes are stamped with. It should track the cluster's clock: quotes are checked
/// against the Clock sysvar, with `MAX_CLOCK_SKEW` of tolerance.
pub trait TimeSource: Send + Sync {
    /// Now, in Unix seconds.
    fn now(&self) -> UnixTimestamp;
}

/// A fixed mid (configured feed, tests, demo).
#[derive(Debug, Clone, Copy)]
pub struct FixedMid(pub Decimal);

impl PriceFeed for FixedMid {
    fn mid(&self) -> Option<Decimal> {
        Some(self.0)
    }
}

/// A fixed inventory.
#[derive(Debug, Clone, Copy)]
pub struct FixedInventory(pub Decimal);

impl Inventory for FixedInventory {
    fn inventory(&self) -> Decimal {
        self.0
    }
}

/// A fixed clock.
#[derive(Debug, Clone, Copy)]
pub struct FixedTime(pub UnixTimestamp);

impl TimeSource for FixedTime {
    fn now(&self) -> UnixTimestamp {
        self.0
    }
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemTime;

impl TimeSource for SystemTime {
    fn now(&self) -> UnixTimestamp {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        UnixTimestamp::new(i64::try_from(secs).unwrap_or(i64::MAX))
    }
}

/// Service settings.
#[derive(Debug, Clone, Copy)]
pub struct ServiceConfig {
    /// The series quoted.
    pub market: MarketInfo,
    /// Model and bounds.
    pub pricing: PricingParams,
    /// Lifetime of each quote; at most the series' `max_quote_ttl`.
    pub ttl: Seconds,
    /// Largest size quoted, in base lots.
    pub max_size: BaseLots,
    /// The maker's base token account, sent with every quote.
    pub maker_base: Address,
    /// The maker's quote token account, sent with every quote.
    pub maker_quote: Address,
}

/// Why a service could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// The TTL is not positive or exceeds the series' maximum.
    #[error("quote ttl must be in 1..=max_quote_ttl")]
    Ttl,
    /// The maximum size is below the series' minimum.
    #[error("max size below the series minimum")]
    Size,
    /// The pricing settings fail a startup check.
    #[error("pricing: {0}")]
    Pricing(PricingConfigError),
    /// A multi-series service needs at least one market.
    #[error("no markets")]
    NoMarkets,
    /// Two markets name the same series.
    #[error("duplicate series")]
    DuplicateSeries,
    /// All markets must be on one program and deployment.
    #[error("markets on different programs or deployments")]
    MixedDeployments,
    /// The issuer could not be built.
    #[error("issuer: {0}")]
    Issuer(IssueError),
}

/// One market of a multi-series service.
pub struct MarketEntry {
    /// The market's settings.
    pub config: ServiceConfig,
    /// The market's limits.
    pub limits: MarketLimits,
    /// The market's reference price.
    pub feed: Box<dyn PriceFeed>,
    /// The maker's position in this market, for the pricing model.
    pub inventory: Box<dyn Inventory>,
}

impl std::fmt::Debug for MarketEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MarketEntry")
            .field("config", &self.config)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// How a multi-series service starts its nonces.
#[derive(Debug, Clone)]
pub enum Start {
    /// A maker key that has never signed a quote, from this nonce.
    Fresh(Nonce),
    /// Any restart: the maker's on-chain window, read at start, and when the service started.
    /// The cold start uses the longest `max_quote_ttl` across the markets.
    Resume {
        /// `MakerState.nonces`.
        window: Box<NonceWindow>,
        /// Now.
        started_at: UnixTimestamp,
    },
}

/// The maker service.
pub struct MakerService {
    key: MakerKey,
    /// The first market's settings.
    first: ServiceConfig,
    markets: Vec<MarketEntry>,
    /// Limits, nonces and reservations move together, under this one lock.
    book: Mutex<Book>,
    /// `None` when the service has no risk book.
    allowed_takers: Option<Vec<Address>>,
    /// What a request for a series outside the set gets.
    outside_set: RejectReason,
    clock: Box<dyn TimeSource>,
}

impl std::fmt::Debug for MakerService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MakerService")
            .field("key", &self.key)
            .field("markets", &self.markets)
            .finish_non_exhaustive()
    }
}

fn validate(config: &ServiceConfig) -> Result<(), ConfigError> {
    if config.ttl.get() <= 0 || config.ttl > config.market.max_quote_ttl {
        return Err(ConfigError::Ttl);
    }
    if config.max_size < config.market.min_size {
        return Err(ConfigError::Size);
    }
    config.pricing.validate().map_err(ConfigError::Pricing)
}

fn reject(request_id: u64, reason: RejectReason, message: impl Into<String>) -> ServerMessage {
    ServerMessage::Reject(RejectMessage {
        request_id,
        reason,
        message: message.into(),
    })
}

impl MakerService {
    /// Builds the service. `issuer` comes from [`NonceIssuer::resume`] on every restart, or
    /// [`NonceIssuer::fresh`] for a key that has never signed.
    ///
    /// # Errors
    ///
    /// `Ttl` or `Size` for settings the series would reject; `Pricing` for pricing settings
    /// that fail [`PricingParams::validate`].
    pub fn new(
        key: MakerKey,
        config: ServiceConfig,
        issuer: NonceIssuer,
        feed: Box<dyn PriceFeed>,
        inventory: Box<dyn Inventory>,
        clock: Box<dyn TimeSource>,
    ) -> Result<Self, ConfigError> {
        validate(&config)?;
        Ok(Self {
            key,
            first: config,
            markets: vec![MarketEntry {
                config,
                limits: MarketLimits::default(),
                feed,
                inventory,
            }],
            book: Mutex::new(Book::unchecked(issuer)),
            allowed_takers: None,
            outside_set: RejectReason::BadRequest,
            clock,
        })
    }

    /// Builds a service for several series under one key, sharing one nonce issuer and one risk
    /// book. It quotes nothing until [`MakerService::reconcile`] has applied a snapshot.
    ///
    /// # Errors
    ///
    /// `NoMarkets`, `DuplicateSeries`, `MixedDeployments`; a market's `Ttl`, `Size` or
    /// `Pricing`; `Issuer` if the cold start overflows.
    pub fn multi(
        key: MakerKey,
        markets: Vec<MarketEntry>,
        start: Start,
        risk: RiskConfig,
        clock: Box<dyn TimeSource>,
    ) -> Result<Self, ConfigError> {
        let first_config = markets.first().ok_or(ConfigError::NoMarkets)?.config;
        let first = first_config.market;
        let mut longest_ttl = first.max_quote_ttl;
        for (i, entry) in markets.iter().enumerate() {
            validate(&entry.config)?;
            let market = &entry.config.market;
            if market.program_id != first.program_id || market.deployment != first.deployment {
                return Err(ConfigError::MixedDeployments);
            }
            if markets
                .iter()
                .skip(i.saturating_add(1))
                .any(|other| other.config.market.series == market.series)
            {
                return Err(ConfigError::DuplicateSeries);
            }
            longest_ttl = longest_ttl.max(market.max_quote_ttl);
        }
        let allowed_takers = risk.allowed_takers.clone();
        let book = match start {
            Start::Fresh(first_nonce) => Book::checked(NonceIssuer::fresh(first_nonce), risk, None),
            Start::Resume { window, started_at } => {
                let issuer = NonceIssuer::resume(&window, started_at, longest_ttl)
                    .map_err(ConfigError::Issuer)?;
                Book::checked(issuer, risk, Some(started_at))
            }
        };
        Ok(Self {
            key,
            first: first_config,
            markets,
            book: Mutex::new(book),
            allowed_takers: Some(allowed_takers),
            outside_set: RejectReason::UnknownSeries,
            clock,
        })
    }

    /// Reads the chain and applies it to the risk book. The reads run outside the lock. Call it
    /// on a timer; it blocks for as long as `view` does.
    ///
    /// # Errors
    ///
    /// A failed snapshot, after marking the book unavailable, or a poisoned lock. A failed
    /// history read is not an error here: the book refuses until the hour has passed.
    pub fn reconcile(&self, view: &dyn ChainView) -> Result<(), ChainError> {
        let poisoned = || ChainError("book lock poisoned".into());
        let snapshot = match view.snapshot() {
            Ok(snapshot) => snapshot,
            Err(e) => {
                self.book.lock().map_err(|_| poisoned())?.read_failed();
                return Err(e);
            }
        };
        let needs_history = self
            .book
            .lock()
            .map_err(|_| poisoned())?
            .needs_fill_history(snapshot.clock);
        let history = if needs_history {
            let since = snapshot.clock.get().saturating_sub(FILL_WINDOW_SECS);
            let history = view.fill_times_since(UnixTimestamp::new(since));
            if let Err(e) = &history {
                tracing::warn!(error = %e, "fill history unreadable: refusing for the hour");
            }
            Some(history)
        } else {
            None
        };
        self.book
            .lock()
            .map_err(|_| poisoned())?
            .apply(snapshot, history);
        Ok(())
    }

    /// The service's state; takes no nonce and holds nothing.
    #[must_use]
    pub fn status(&self, request_id: u64) -> ServerMessage {
        let Ok(book) = self.book.lock() else {
            return reject(request_id, RejectReason::Internal, "book lock poisoned");
        };
        let status = book.status(self.clock.now());
        drop(book);
        ServerMessage::Status(StatusMessage {
            request_id,
            halted: status.halted,
            open_quotes: status.open_quotes,
            next_nonce: status.next_nonce,
            newest_expires_at: status.newest_expires_at,
            snapshot_age_secs: status.snapshot_age_secs,
            spot_ages: self
                .markets
                .iter()
                .map(|m| SpotAge {
                    series: m.config.market.series.to_string(),
                    age_secs: m.feed.age_secs(),
                })
                .collect(),
        })
    }

    /// Applies the maker's on-chain window, read at `read_at`, before the service's first
    /// quote (see [`NonceIssuer::refresh`]). A resumed service quotes only after a refresh
    /// with a window read at or after the end of its cold start.
    ///
    /// # Errors
    ///
    /// The issuer's refresh error, or `Invalid` if its lock is poisoned.
    pub fn refresh_window(
        &self,
        window: &quotebook_core::NonceWindow,
        read_at: UnixTimestamp,
    ) -> Result<(), IssueError> {
        self.book
            .lock()
            .map_err(|_| IssueError::Invalid)?
            .issuer_mut()
            .refresh(window, read_at)
    }

    /// The first market's settings: the only market of a single-series service.
    #[must_use]
    pub fn config(&self) -> &ServiceConfig {
        &self.first
    }

    /// Every market's settings.
    pub fn configs(&self) -> impl Iterator<Item = &ServiceConfig> {
        self.markets.iter().map(|m| &m.config)
    }

    /// The maker's address.
    #[must_use]
    pub fn maker(&self) -> Address {
        self.key.address()
    }

    /// Answers one request: a signed quote or a refusal.
    #[must_use]
    pub fn respond(&self, request: &QuoteRequest) -> ServerMessage {
        let id = request.request_id;
        let Ok(series) = request.series.parse::<Address>() else {
            return reject(id, RejectReason::BadRequest, "series is not an address");
        };
        let Some(entry) = self
            .markets
            .iter()
            .find(|m| m.config.market.series == series)
        else {
            return reject(id, self.outside_set, "series not quoted here");
        };
        let config = &entry.config;
        let market = &config.market;
        let Ok(taker) = request.taker.parse::<Address>() else {
            return reject(id, RejectReason::BadRequest, "taker is not an address");
        };
        if let Some(allowed) = &self.allowed_takers {
            if !allowed.contains(&taker) {
                return reject(id, RejectReason::TakerNotAllowed, "taker not allowed");
            }
        }
        let size = BaseLots::new(request.size_lots);
        if size < market.min_size || size > config.max_size {
            return reject(id, RejectReason::Size, "size outside the quoted range");
        }
        let Some(mid) = entry.feed.mid() else {
            return reject(id, RejectReason::Pricing, "no reference price");
        };
        // Model, clamps in bps of the mid, rounding toward the maker (bid down, ask up), then
        // the guard on the rounded prices (crate::pricing). Both sides are judged, so a crossed
        // or too-coarse pair is refused whichever side is asked for.
        let quoted = match pricing::quote(mid, entry.inventory.inventory(), &config.pricing, market)
        {
            Ok(q) => q,
            Err(e) => return reject(id, RejectReason::Pricing, e.to_string()),
        };
        // The taker buys from the maker's ask; it sells into the maker's bid.
        let (side, ticks) = match request.taker_side {
            TakerSide::Buy => (Side::Ask, quoted.ask_ticks),
            TakerSide::Sell => (Side::Bid, quoted.bid_ticks),
        };
        let issued_at = self.clock.now();
        let Some(expires_at) = issued_at.get().checked_add(config.ttl.get()) else {
            return reject(id, RejectReason::Internal, "clock overflow");
        };
        let expires_at = UnixTimestamp::new(expires_at);
        // A fill exchanges `size × base_lot_size` for `price × size × quote_lot_size`, exact.
        let lots = size.get();
        let base_atoms = u128::from(lots).checked_mul(u128::from(market.base_lot_size));
        let notional = u128::from(ticks.get())
            .checked_mul(u128::from(lots))
            .and_then(|v| v.checked_mul(u128::from(market.quote_lot_size)));
        let (Some(Ok(base_atoms)), Some(Ok(notional))) =
            (base_atoms.map(u64::try_from), notional.map(u64::try_from))
        else {
            return reject(id, RejectReason::Size, "fill does not fit an amount");
        };
        let (debit_account, debit_atoms) = match side {
            Side::Ask => (config.maker_base, base_atoms),
            Side::Bid => (config.maker_quote, notional),
        };
        let order = Order {
            series,
            side,
            lots,
            notional,
            debit_account,
            debit_atoms,
            base_account: config.maker_base,
            base_lot_size: market.base_lot_size,
            limits: entry.limits,
            issued_at,
            expires_at,
        };
        // Halt, limits, reservation and nonce are one critical section. Signing is outside it.
        let nonce = match self.book.lock() {
            Ok(mut book) => match book.admit(&order) {
                Ok(n) => n,
                Err(refusal) => {
                    let message = refusal.to_string();
                    let reason = match refusal {
                        Refusal::Halted => RejectReason::Halted,
                        Refusal::Unavailable(_) => RejectReason::RiskUnavailable,
                        Refusal::Limit(_) => RejectReason::RiskLimit,
                        Refusal::Issue(
                            IssueError::TooSoon { .. } | IssueError::ColdStart { .. },
                        ) => RejectReason::RateLimited,
                        Refusal::Issue(_) => RejectReason::Internal,
                    };
                    return reject(id, reason, message);
                }
            },
            Err(_) => return reject(id, RejectReason::Internal, "book lock poisoned"),
        };
        let quote = match Quote::new(QuoteTerms {
            program_id: market.program_id,
            deployment: market.deployment,
            maker: self.key.address(),
            series: SeriesId::new(market.series),
            taker,
            side,
            price: ticks,
            size,
            issued_at,
            expires_at,
            nonce,
        }) {
            Ok(q) => q,
            Err(e) => {
                // Nothing signed left the service, so this quote can never fill.
                if let Ok(mut book) = self.book.lock() {
                    book.forget(nonce);
                }
                return reject(id, RejectReason::Internal, e.to_string());
            }
        };
        let signature = self.key.sign(&quote);
        tracing::info!(
            request_id = id,
            side = ?side,
            size = size.get(),
            price_ticks = ticks.get(),
            nonce = nonce.get(),
            expires_at = expires_at.get(),
            "quoted"
        );
        ServerMessage::Quote(QuoteMessage {
            request_id: id,
            quote: to_hex(&quote.encode()),
            signature: to_hex(&signature),
            maker_base: config.maker_base.to_string(),
            maker_quote: config.maker_quote.to_string(),
        })
    }

    /// Accepts WebSocket connections on `listener` until it fails.
    ///
    /// # Errors
    ///
    /// The listener's accept error.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> std::io::Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            let service = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(e) = service.handle(stream).await {
                    tracing::warn!(%peer, error = %e, "connection closed with an error");
                }
            });
        }
    }

    async fn handle(&self, stream: TcpStream) -> Result<(), tokio_tungstenite::tungstenite::Error> {
        let mut ws = tokio_tungstenite::accept_async(stream).await?;
        while let Some(frame) = ws.next().await {
            let text = match frame? {
                Message::Text(text) => text,
                Message::Close(_) => break,
                _ => continue,
            };
            let answer = match serde_json::from_str::<ClientMessage>(&text) {
                Ok(ClientMessage::QuoteRequest(request)) => self.respond(&request),
                Ok(ClientMessage::Status(request)) => self.status(request.request_id),
                Err(e) => reject(0, RejectReason::BadRequest, e.to_string()),
            };
            let json = serde_json::to_string(&answer).unwrap_or_default();
            ws.send(Message::text(json)).await?;
        }
        Ok(())
    }
}
