//! The webhook's wire types, written from Jupiter's published OpenAPI schema
//! (`openapi/openapi.json`) and its enums (`webhook-api/src/enums.rs`), toolkit commit
//! `3aa4fce515d4b5827ee2d48f08041ea706d911cd`, MIT. The schema alone is not enough: the
//! acceptance suite expects `{"rejectedWithReason": "insufficientBalance"}`, which only
//! `enums.rs` defines (docs/design/jupiter-rfq.md, "Where the docs disagree").
//!
//! Unknown fields are accepted, so a field Jupiter adds later does not break a request.

use serde::{Deserialize, Serialize};

/// Whether `amount` is the taker's input or the taker's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QuoteType {
    /// `amount` is what the taker pays in.
    #[serde(alias = "exact_in", alias = "ExactIn")]
    ExactIn,
    /// `amount` is what the taker receives.
    #[serde(alias = "exact_out", alias = "ExactOut")]
    ExactOut,
}

/// The only protocol V1 webhooks serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Protocol {
    /// `"v1"`.
    V1,
}

/// `POST /quote`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteRequest {
    /// Jupiter's id for this request.
    pub request_id: String,
    /// The quote's id; a later `/swap` names it.
    pub quote_id: String,
    /// The mint the taker pays in.
    pub token_in: String,
    /// Atoms, as a decimal string: the input for `exactIn`, the output for `exactOut`.
    pub amount: String,
    /// The mint the taker receives.
    pub token_out: String,
    /// See [`QuoteType`].
    pub quote_type: QuoteType,
    /// Always [`Protocol::V1`].
    pub protocol: Protocol,
    /// The taker, once signed in; absent for an anonymous quote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taker: Option<String>,
    /// Jupiter's suggested compute unit price, micro-lamports per CU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_prioritization_fees: Option<u64>,
    /// Jupiter's fee in basis points, at most 10,000.
    pub fee_bps: u16,
    /// Whether a side is wrapped SOL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_wsol: Option<bool>,
    /// Who receives the output, when not the taker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<String>,
    /// `internal` or `integrator`, set by Jupiter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// The answer to a quote the maker makes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    /// Echo of the request.
    pub request_id: String,
    /// Echo of the request.
    pub quote_id: String,
    /// Echo of the request.
    pub token_in: String,
    /// Atoms the taker pays.
    pub amount_in: String,
    /// Echo of the request.
    pub token_out: String,
    /// Echo of the request.
    pub quote_type: QuoteType,
    /// Echo of the request.
    pub protocol: Protocol,
    /// Atoms the taker receives.
    pub amount_out: String,
    /// The maker's wallet, base58.
    pub maker: String,
    /// The compute unit price the fill transaction should use, micro-lamports per CU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prioritization_fee_to_use: Option<u64>,
    /// Echo of the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taker: Option<String>,
    /// Echo of the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<String>,
    /// Echo of the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// `POST /swap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapRequest {
    /// Jupiter's id for this request.
    pub request_id: String,
    /// The quote this transaction fills.
    pub quote_id: String,
    /// The taker-signed versioned transaction, base64.
    pub transaction: String,
    /// `internal` or `integrator`, set by Jupiter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// A rejection reason Jupiter knows (`enums.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RejectionReason {
    /// The maker cannot cover the output.
    InsufficientBalance,
    /// The maker cannot pay for an account creation.
    InsufficientBalanceForAtaCreation,
    /// The taker's signature does not verify.
    SignatureVerificationFailed,
    /// Anything Jupiter should treat as abuse.
    BotActivityDetected,
}

/// What became of a swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SwapState {
    /// Co-signed and sent. The prototype answers it only in tests and behind `acceptance`.
    Accepted,
    /// Refused; `rejectionReason` says why.
    Rejected,
    /// Refused with a reason Jupiter knows.
    RejectedWithReason(RejectionReason),
}

/// The answer to a swap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapResponse {
    /// Echo of the request.
    pub quote_id: String,
    /// See [`SwapState`].
    pub state: SwapState,
    /// Deprecated in the schema; the prototype never has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_signature: Option<String>,
    /// Why the swap was refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_reason: Option<String>,
    /// Echo of the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// The body of a 4xx or 5xx answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponse {
    /// What went wrong.
    pub message: String,
}
