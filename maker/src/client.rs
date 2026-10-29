//! The taker side: request quotes, check them, rank them.
//!
//! A taker should never submit a quote it has not checked: a bad one only fails on chain,
//! after the fee is paid. [`verify`] checks the maker's signature and every rule `settle`
//! applies to the quote's own terms, in the program's order, and reports the program's own
//! error ([`ClientError::WouldFail`]); `maker/tests/agreement.rs` runs each rule through both
//! and requires the same answer. Rules about on-chain state (a used or cancelled nonce, a
//! paused series, balances, allowances) need the chain and are not checked here. [`rank`]
//! orders quotes from several makers by price (the `BestPriceStrategy` pattern of otc-rfq's
//! `ranking_strategy.rs`, on integer ticks).

use futures_util::{SinkExt, StreamExt};
use quotebook_core::{BaseLots, Quote, QuotebookError, Side, UnixTimestamp};
use solana_address::Address;
use solana_signature::Signature;
use tokio_tungstenite::tungstenite::Message;

use crate::market::MarketInfo;
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
    /// `settle` would reject the quote with this error.
    #[error("settle would fail: {0}")]
    WouldFail(QuotebookError),
    /// The quote is valid but not what the taker asked for (the other side).
    #[error("quote does not match the request: {0}")]
    Mismatch(&'static str),
    /// The signature does not verify for the quote's maker; the ed25519 precompile would fail
    /// the transaction.
    #[error("signature does not verify")]
    BadSignature,
}

/// A maker's answer: the signed quote and the token accounts `settle` needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    /// The signed quote.
    pub signed: SignedQuote,
    /// The maker's base token account.
    pub maker_base: Address,
    /// The maker's quote token account.
    pub maker_quote: Address,
}

/// Sends `request` to the maker at `url` (`ws://host:port`) and returns its answer.
///
/// # Errors
///
/// `Transport`, `Protocol` or `Rejected`.
pub async fn request_quote(url: &str, request: &QuoteRequest) -> Result<Answer, ClientError> {
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
            let account = |s: &str| {
                s.parse::<Address>()
                    .map_err(|_| ClientError::Protocol("maker account".into()))
            };
            Ok(Answer {
                signed: SignedQuote { quote, signature },
                maker_base: account(&m.maker_base)?,
                maker_quote: account(&m.maker_quote)?,
            })
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
    /// The series, read from chain: program, deployment, lot sizes, TTL, minimum size.
    pub market: MarketInfo,
    /// The taker, who signs `settle`.
    pub taker: Address,
    /// What the taker wants.
    pub taker_side: TakerSide,
    /// The size the taker will fill, in base lots.
    pub size: BaseLots,
    /// The cluster clock now.
    pub now: UnixTimestamp,
}

/// Checks the signature, then every rule `settle` applies to the quote's terms in the
/// program's order (docs/PLAN.md, section 2), then that the quote is on the side asked for.
///
/// # Errors
///
/// `BadSignature`; `WouldFail` with `WrongProgram`, `WrongSeries` (series or deployment),
/// `SelfTrade`, `WrongTaker`, `QuoteExpired`, `QuoteTtlTooLong`, `FillSizeInvalid` or
/// `Overflow`; `Mismatch` for the other side.
pub fn verify(signed: &SignedQuote, expected: &Expectation) -> Result<(), ClientError> {
    let t = signed.quote.terms();
    let m = &expected.market;
    let fail = |e| Err(ClientError::WouldFail(e));
    if !Signature::from(signed.signature).verify(t.maker.as_ref(), &signed.quote.encode()) {
        return Err(ClientError::BadSignature);
    }
    if t.program_id != m.program_id {
        return fail(QuotebookError::WrongProgram);
    }
    if t.series.address() != m.series || t.deployment != m.deployment {
        return fail(QuotebookError::WrongSeries);
    }
    if expected.taker == t.maker {
        return fail(QuotebookError::SelfTrade);
    }
    if !signed.quote.allows_taker(&expected.taker) {
        return fail(QuotebookError::WrongTaker);
    }
    signed
        .quote
        .check_time(expected.now, m.max_quote_ttl)
        .map_err(ClientError::WouldFail)?;
    if expected.size < m.min_size || expected.size > t.size {
        return fail(QuotebookError::FillSizeInvalid);
    }
    let fill = expected.size.get();
    let base = fill.checked_mul(m.base_lot_size);
    let quote = u128::from(t.price.get())
        .checked_mul(u128::from(fill))
        .and_then(|v| v.checked_mul(u128::from(m.quote_lot_size)))
        .and_then(|v| u64::try_from(v).ok());
    if base.is_none() || quote.is_none() {
        return fail(QuotebookError::Overflow);
    }
    let wanted = match expected.taker_side {
        TakerSide::Buy => Side::Ask,
        TakerSide::Sell => Side::Bid,
    };
    if t.side != wanted {
        return Err(ClientError::Mismatch("the quote is on the other side"));
    }
    Ok(())
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
