//! The V1 webhook over HTTP: `POST /quote`, `POST /swap` and `GET /tokens` (Webhook API).
//!
//! - `/quote` answers within its deadline: the configured one (200 ms by default in the
//!   shipped config) and never beyond Jupiter's `x-request-timeout` header, read as
//!   milliseconds. Pricing runs on a blocking thread; past the deadline the answer is a 404,
//!   which Jupiter treats as a decline with no penalty. Every decline is a 404, except a
//!   malformed request, which is a 400.
//! - `/swap` never answers `accepted`: a fill that passes every check is answered `rejected`,
//!   with the reason that the prototype never co-signs, and is logged. Only the crate's own
//!   tests, and the `acceptance` feature's canned answers, produce `accepted`; neither signs.
//! - Nothing here holds a key, sends a transaction or calls Jupiter.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::api::{ErrorResponse, QuoteRequest, SwapRequest, SwapResponse, SwapState};
use crate::desk::{Commit, Decline, Desk, SwapDecision};

/// Jupiter's deadline header on `/quote`.
pub const REQUEST_TIMEOUT_HEADER: &str = "x-request-timeout";

/// Why the prototype answers `rejected` to a fill it would have co-signed.
pub const NEVER_SIGNS: &str =
    "prototype: the fill passed every check, but this webhook never co-signs";

/// The router's shared state.
#[derive(Debug)]
pub struct AppState {
    desk: Arc<Desk>,
    quote_deadline: Duration,
    commit: Commit,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorResponse {
            message: message.into(),
        }),
    )
        .into_response()
}

/// The webhook. It never accepts a swap.
pub fn router(desk: Arc<Desk>, quote_deadline: Duration) -> Router {
    build(AppState {
        desk,
        quote_deadline,
        commit: Commit::DryRun,
    })
}

fn build(state: AppState) -> Router {
    Router::new()
        .route("/quote", post(quote))
        .route("/swap", post(swap))
        .route("/tokens", get(tokens))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "not found") })
        .with_state(Arc::new(state))
}

/// `min(configured, x-request-timeout)`; an unreadable header leaves the configured one.
fn deadline(configured: Duration, headers: &HeaderMap) -> Duration {
    headers
        .get(REQUEST_TIMEOUT_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(configured, |ms| configured.min(Duration::from_millis(ms)))
}

async fn quote(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    payload: Result<Json<QuoteRequest>, JsonRejection>,
) -> Response {
    let Json(req) = match payload {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    let limit = deadline(state.quote_deadline, &headers);
    let desk = Arc::clone(&state.desk);
    let quote_id = req.quote_id.clone();
    let job = tokio::task::spawn_blocking(move || desk.quote(&req));
    match tokio::time::timeout(limit, job).await {
        Ok(Ok(Ok(answer))) => (StatusCode::OK, Json(answer)).into_response(),
        Ok(Ok(Err(Decline::BadRequest(why)))) => error(StatusCode::BAD_REQUEST, why),
        Ok(Ok(Err(decline))) => {
            tracing::info!(quote_id = ?quote_id, reason = %decline, "quote declined");
            error(StatusCode::NOT_FOUND, decline.to_string())
        }
        Ok(Err(join)) => {
            tracing::error!(quote_id = ?quote_id, error = %join, "quote task failed");
            error(StatusCode::NOT_FOUND, "declined")
        }
        Err(_) => {
            tracing::warn!(quote_id = ?quote_id, deadline_ms = limit.as_millis(), "quote deadline passed");
            error(StatusCode::NOT_FOUND, "deadline passed")
        }
    }
}

fn swap_answer(req: &SwapRequest, state: SwapState, reason: Option<String>) -> Response {
    (
        StatusCode::OK,
        Json(SwapResponse {
            quote_id: req.quote_id.clone(),
            state,
            tx_signature: None,
            rejection_reason: reason,
            source: req.source.clone(),
        }),
    )
        .into_response()
}

#[cfg(feature = "acceptance")]
fn canned(req: &SwapRequest) -> Option<Response> {
    use crate::api::RejectionReason;
    // The magic ids of the toolkit's acceptance suite (`tests/suites/acceptance/test.ts`),
    // answered as its sample server does. Nothing is signed.
    Some(match req.request_id.as_str() {
        "629bddf3-0038-43a6-8956-f5433d6b1191" => swap_answer(req, SwapState::Accepted, None),
        "00000000-0000-0000-0000-000000000001" => swap_answer(
            req,
            SwapState::Rejected,
            Some("acceptance: simulated rejection".into()),
        ),
        "00000000-0000-0000-0000-000000000002" => error(
            StatusCode::BAD_REQUEST,
            "acceptance: simulated malformed request",
        ),
        "00000000-0000-0000-0000-000000000003" => swap_answer(
            req,
            SwapState::RejectedWithReason(RejectionReason::InsufficientBalance),
            None,
        ),
        "00000000-0000-0000-0000-000000000004" => swap_answer(
            req,
            SwapState::RejectedWithReason(RejectionReason::SignatureVerificationFailed),
            None,
        ),
        _ => return None,
    })
}

async fn swap(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<SwapRequest>, JsonRejection>,
) -> Response {
    let Json(req) = match payload {
        Ok(p) => p,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.body_text()),
    };
    #[cfg(feature = "acceptance")]
    if let Some(answer) = canned(&req) {
        return answer;
    }
    let desk = Arc::clone(&state.desk);
    let commit = state.commit;
    let decided = {
        let req = req.clone();
        tokio::task::spawn_blocking(move || desk.swap(&req, commit)).await
    };
    match decided {
        Ok(SwapDecision::WouldSign { validated, .. }) => match commit {
            Commit::Reserve => swap_answer(&req, SwapState::Accepted, None),
            Commit::DryRun => {
                tracing::info!(
                    quote_id = ?req.quote_id,
                    fee_lamports = validated.fee_lamports,
                    expire_at = validated.expire_at,
                    "swap would be co-signed; the prototype never signs"
                );
                swap_answer(&req, SwapState::Rejected, Some(NEVER_SIGNS.into()))
            }
        },
        Ok(SwapDecision::Reject(r)) => {
            tracing::info!(quote_id = ?req.quote_id, reason = %r.message, "swap rejected");
            match r.known {
                Some(known) => {
                    swap_answer(&req, SwapState::RejectedWithReason(known), Some(r.message))
                }
                None => swap_answer(&req, SwapState::Rejected, Some(r.message)),
            }
        }
        Err(join) => {
            tracing::error!(quote_id = ?req.quote_id, error = %join, "swap task failed");
            swap_answer(&req, SwapState::Rejected, Some("internal error".into()))
        }
    }
}

async fn tokens(State(state): State<Arc<AppState>>) -> Json<Vec<String>> {
    Json(state.desk.tokens())
}

/// A router whose valid swaps are reserved and answered `accepted`: the crate's own tests
/// only. It signs nothing either.
#[cfg(test)]
pub(crate) fn router_accepting(desk: Arc<Desk>, quote_deadline: Duration) -> Router {
    build(AppState {
        desk,
        quote_deadline,
        commit: Commit::Reserve,
    })
}
