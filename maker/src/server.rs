//! The maker service: prices a request, signs a firm quote, answers over WebSocket.
//!
//! Logs carry request ids, sides, sizes, nonces and prices; never keys or signatures.

use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use quotebook_core::{BaseLots, Quote, QuoteTerms, Seconds, SeriesId, Side, UnixTimestamp};
use rust_decimal::Decimal;
use solana_address::Address;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

use crate::issuer::{IssueError, NonceIssuer};
use crate::keys::MakerKey;
use crate::market::{MarketInfo, Rounding};
use crate::pricing::{check_bounds, quote_prices, PricingParams};
use crate::protocol::{
    to_hex, ClientMessage, QuoteMessage, QuoteRequest, RejectMessage, RejectReason, ServerMessage,
    TakerSide,
};

/// The reference price, in quote tokens per base token.
pub trait PriceFeed: Send + Sync {
    /// The current mid, or `None` if the feed is stale or down.
    fn mid(&self) -> Option<Decimal>;
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
}

/// The maker service.
pub struct MakerService {
    key: MakerKey,
    config: ServiceConfig,
    issuer: Mutex<NonceIssuer>,
    feed: Box<dyn PriceFeed>,
    inventory: Box<dyn Inventory>,
    clock: Box<dyn TimeSource>,
}

impl std::fmt::Debug for MakerService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MakerService")
            .field("key", &self.key)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
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
    /// `Ttl` or `Size` for settings the series would reject.
    pub fn new(
        key: MakerKey,
        config: ServiceConfig,
        issuer: NonceIssuer,
        feed: Box<dyn PriceFeed>,
        inventory: Box<dyn Inventory>,
        clock: Box<dyn TimeSource>,
    ) -> Result<Self, ConfigError> {
        if config.ttl.get() <= 0 || config.ttl > config.market.max_quote_ttl {
            return Err(ConfigError::Ttl);
        }
        if config.max_size < config.market.min_size {
            return Err(ConfigError::Size);
        }
        Ok(Self {
            key,
            config,
            issuer: Mutex::new(issuer),
            feed,
            inventory,
            clock,
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
        self.issuer
            .lock()
            .map_err(|_| IssueError::Invalid)?
            .refresh(window, read_at)
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
        let market = &self.config.market;
        let Ok(series) = request.series.parse::<Address>() else {
            return reject(id, RejectReason::BadRequest, "series is not an address");
        };
        if series != market.series {
            return reject(id, RejectReason::BadRequest, "series not quoted here");
        }
        let Ok(taker) = request.taker.parse::<Address>() else {
            return reject(id, RejectReason::BadRequest, "taker is not an address");
        };
        let size = BaseLots::new(request.size_lots);
        if size < market.min_size || size > self.config.max_size {
            return reject(id, RejectReason::Size, "size outside the quoted range");
        }
        let Some(mid) = self.feed.mid() else {
            return reject(id, RejectReason::Pricing, "no reference price");
        };
        let (bid, ask) = match quote_prices(mid, self.inventory.inventory(), &self.config.pricing) {
            Ok(prices) => prices,
            Err(e) => return reject(id, RejectReason::Pricing, e.to_string()),
        };
        // The taker buys from the maker's ask; it sells into the maker's bid. Rounding goes
        // toward the maker.
        let (side, price, rounding) = match request.taker_side {
            TakerSide::Buy => (Side::Ask, ask, Rounding::Up),
            TakerSide::Sell => (Side::Bid, bid, Rounding::Down),
        };
        if let Err(e) = check_bounds(price, mid, self.config.pricing.max_deviation) {
            return reject(id, RejectReason::Pricing, e.to_string());
        }
        let ticks = match market.price_to_ticks(price, rounding) {
            Ok(t) => t,
            Err(e) => return reject(id, RejectReason::Pricing, e.to_string()),
        };
        let issued_at = self.clock.now();
        let Some(expires_at) = issued_at.get().checked_add(self.config.ttl.get()) else {
            return reject(id, RejectReason::Internal, "clock overflow");
        };
        let expires_at = UnixTimestamp::new(expires_at);
        let nonce = match self.issuer.lock() {
            Ok(mut issuer) => match issuer.issue(issued_at, expires_at) {
                Ok(n) => n,
                Err(e @ (IssueError::TooSoon { .. } | IssueError::ColdStart { .. })) => {
                    return reject(id, RejectReason::RateLimited, e.to_string())
                }
                Err(e) => return reject(id, RejectReason::Internal, e.to_string()),
            },
            Err(_) => return reject(id, RejectReason::Internal, "issuer lock poisoned"),
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
            Err(e) => return reject(id, RejectReason::Internal, e.to_string()),
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
                Err(e) => reject(0, RejectReason::BadRequest, e.to_string()),
            };
            let json = serde_json::to_string(&answer).unwrap_or_default();
            ws.send(Message::text(json)).await?;
        }
        Ok(())
    }
}
