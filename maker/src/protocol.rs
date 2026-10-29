//! The RFQ wire protocol: JSON text frames over one WebSocket (docs/PLAN.md, section 1: a
//! minimal protocol between one taker and one maker, enough for tests and a demo).
//!
//! The taker sends [`ClientMessage::QuoteRequest`]; the maker answers with
//! [`ServerMessage::Quote`] (the quote's canonical 208 bytes and the maker's signature, both
//! hex) or [`ServerMessage::Reject`]. Addresses are base58.

use serde::{Deserialize, Serialize};

/// What the taker wants to do with the base token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TakerSide {
    /// The taker buys base; the maker quotes an ask.
    Buy,
    /// The taker sells base; the maker quotes a bid.
    Sell,
}

/// A request for a firm quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteRequest {
    /// Echoed in the answer.
    pub request_id: u64,
    /// The series account, base58.
    pub series: String,
    /// The taker's address, base58; the quote is bound to it.
    pub taker: String,
    /// What the taker wants.
    pub taker_side: TakerSide,
    /// Size in base lots.
    pub size_lots: u64,
}

/// A signed, firm quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteMessage {
    /// The request this answers.
    pub request_id: u64,
    /// The quote's canonical 208 bytes, hex.
    pub quote: String,
    /// The maker's ed25519 signature over those bytes, hex.
    pub signature: String,
    /// The maker's base token account, base58: `settle` needs it. Not signed: a wrong one
    /// only makes the taker's transaction fail.
    pub maker_base: String,
    /// The maker's quote token account, base58.
    pub maker_quote: String,
}

/// Why a request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The request is malformed or names another series.
    BadRequest,
    /// The size is outside what the maker quotes.
    Size,
    /// No price passed the model and the bounds.
    Pricing,
    /// The issuance bound, or the cold start after a restart, refuses a nonce right now.
    RateLimited,
    /// Internal failure.
    Internal,
}

/// A refusal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectMessage {
    /// The request this answers, 0 if it could not be parsed.
    pub request_id: u64,
    /// The reason.
    pub reason: RejectReason,
    /// Human-readable detail.
    pub message: String,
}

/// Taker to maker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// Ask for a quote.
    QuoteRequest(QuoteRequest),
}

/// Maker to taker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// A signed quote.
    Quote(QuoteMessage),
    /// A refusal.
    Reject(RejectMessage),
}

/// Lower-case hex of `bytes`.
#[must_use]
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for b in bytes {
        for nibble in [b >> 4, b & 0x0f] {
            out.push(char::from(
                DIGITS.get(usize::from(nibble)).copied().unwrap_or(b'0'),
            ));
        }
    }
    out
}

/// Bytes of a hex string, or `None` if it is not even-length hex.
#[must_use]
pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    let digit = |c: u8| match c {
        b'0'..=b'9' => c.checked_sub(b'0'),
        b'a'..=b'f' => c.checked_sub(b'a').and_then(|v| v.checked_add(10)),
        b'A'..=b'F' => c.checked_sub(b'A').and_then(|v| v.checked_add(10)),
        _ => None,
    };
    let (pairs, rest) = s.as_bytes().as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|[hi, lo]| Some(digit(*hi)?.checked_shl(4)? | digit(*lo)?))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::disallowed_macros)]
mod tests {
    use super::*;

    #[test]
    fn test_hex_round_trips_and_rejects_garbage() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
        assert_eq!(from_hex("abc"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(from_hex("AB").unwrap(), vec![0xab]);
    }

    #[test]
    fn test_messages_use_tagged_json() {
        let m = ClientMessage::QuoteRequest(QuoteRequest {
            request_id: 7,
            series: "S".into(),
            taker: "T".into(),
            taker_side: TakerSide::Buy,
            size_lots: 3,
        });
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains(r#""type":"quote_request""#), "{json}");
        assert!(json.contains(r#""taker_side":"buy""#), "{json}");
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), m);
    }
}
