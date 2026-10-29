//! The taker side: request quotes, check them, rank them.
//!
//! A taker should never submit a quote it has not checked: a bad one only fails on chain,
//! after the fee is paid. [`verify`] checks the maker's signature and every term the
//! program will check, and [`rank`] orders quotes from several makers by price (the
//! `BestPriceStrategy` pattern of otc-rfq's `ranking_strategy.rs`, on integer ticks).

use futures_util::{SinkExt, StreamExt};
use quotebook_core::{BaseLots, Quote, Seconds, Side, UnixTimestamp};
use solana_address::Address;
use solana_signature::Signature;
use tokio_tungstenite::tungstenite::Message;

use crate::protocol::{
    from_hex, ClientMessage, QuoteRequest, RejectReason, ServerMessage, TakerSide,
};

/// A quote with the maker's signature over its canonical bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedQuote {
    /// The quote.
    pub quote: Quote,
    /// The maker's ed25519 signature.
    pub signature: [u8; 64],
}

/// Why a request or a quote failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The connection or the WebSocket failed.
    #[error("transport: {0}")]
    Transport(String),
    /// The maker sent something that is not a valid answer.
    #[error("protocol: {0}")]
    Protocol(String),
    /// The maker refused.
    #[error("rejected ({reason:?}): {message}")]
    Rejected {
        /// The maker's reason.
        reason: RejectReason,
        /// Its detail.
        message: String,
    },
    /// The quote fails a check the program would make, or does not match the request.
    #[error("quote fails check: {0}")]
    BadQuote(&'static str),
    /// The signature does not verify for the quote's maker.
    #[error("signature does not verify")]
    BadSignature,
}

/// Sends `request` to the maker at `url` (`ws://host:port`) and returns its quote.
///
/// # Errors
///
/// `Transport`, `Protocol` or `Rejected`.
pub async fn request_quote(url: &str, request: &QuoteRequest) -> Result<SignedQuote, ClientError> {
    let transport =
        |e: tokio_tungstenite::tungstenite::Error| ClientError::Transport(e.to_string());
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(transport)?;
    let json = serde_json::to_string(&ClientMessage::QuoteRequest(request.clone()))
        .map_err(|e| ClientError::Protocol(e.to_string()))?;
    ws.send(Message::text(json)).await.map_err(transport)?;
    let answer = loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => break text,
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(transport(e)),
            None => return Err(ClientError::Protocol("closed before answering".into())),
        }
    };
    let _ = ws.close(None).await;
    match serde_json::from_str::<ServerMessage>(&answer)
        .map_err(|e| ClientError::Protocol(e.to_string()))?
    {
        ServerMessage::Quote(m) if m.request_id == request.request_id => {
            let bytes = from_hex(&m.quote).ok_or(ClientError::Protocol("quote hex".into()))?;
            let quote = Quote::decode(&bytes).map_err(|e| ClientError::Protocol(e.to_string()))?;
            let sig =
                from_hex(&m.signature).ok_or(ClientError::Protocol("signature hex".into()))?;
            let signature: [u8; 64] = sig
                .try_into()
                .map_err(|_| ClientError::Protocol("signature length".into()))?;
            Ok(SignedQuote { quote, signature })
        }
        ServerMessage::Quote(_) => Err(ClientError::Protocol("answer to another request".into())),
        ServerMessage::Reject(r) => Err(ClientError::Rejected {
            reason: r.reason,
            message: r.message,
        }),
    }
}

/// What the taker asked for and the market it trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expectation {
    /// The quotebook program.
    pub program_id: Address,
    /// The cluster's deployment id.
    pub deployment: [u8; 16],
    /// The series.
    pub series: Address,
    /// The taker; the quote must be open or bound to it.
    pub taker: Address,
    /// What the taker wants.
    pub taker_side: TakerSide,
    /// The size asked for, in base lots; the quote must cover it.
    pub size: BaseLots,
    /// The cluster clock now.
    pub now: UnixTimestamp,
    /// The series' longest quote lifetime.
    pub max_quote_ttl: Seconds,
}

/// Checks the signature and every term the program checks, against `expected`.
///
/// # Errors
///
/// `BadSignature` or `BadQuote`.
pub fn verify(signed: &SignedQuote, expected: &Expectation) -> Result<(), ClientError> {
    let t = signed.quote.terms();
    if !Signature::from(signed.signature).verify(t.maker.as_ref(), &signed.quote.encode()) {
        return Err(ClientError::BadSignature);
    }
    if t.program_id != expected.program_id {
        return Err(ClientError::BadQuote("another program"));
    }
    if t.deployment != expected.deployment {
        return Err(ClientError::BadQuote("another deployment"));
    }
    if t.series.address() != expected.series {
        return Err(ClientError::BadQuote("another series"));
    }
    if !signed.quote.allows_taker(&expected.taker) {
        return Err(ClientError::BadQuote("bound to another taker"));
    }
    let wanted = match expected.taker_side {
        TakerSide::Buy => Side::Ask,
        TakerSide::Sell => Side::Bid,
    };
    if t.side != wanted {
        return Err(ClientError::BadQuote("wrong side"));
    }
    if t.size < expected.size {
        return Err(ClientError::BadQuote("smaller than requested"));
    }
    signed
        .quote
        .check_time(expected.now, expected.max_quote_ttl)
        .map_err(|_| ClientError::BadQuote("outside its time window"))
}

/// Orders quotes best first for a taker on `side`: lowest price when buying, highest when
/// selling. Ties keep their order.
#[must_use]
pub fn rank(mut quotes: Vec<SignedQuote>, side: TakerSide) -> Vec<SignedQuote> {
    quotes.sort_by(|a, b| {
        let (pa, pb) = (a.quote.terms().price, b.quote.terms().price);
        match side {
            TakerSide::Buy => pa.cmp(&pb),
            TakerSide::Sell => pb.cmp(&pa),
        }
    });
    quotes
}
