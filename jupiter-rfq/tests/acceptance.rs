//! The toolkit's offline acceptance suite (`tests/suites/acceptance/test.ts` at `3aa4fce`),
//! replayed in-process: its eight cases, with its payloads and its expectations, against the
//! shipped `config/acceptance.json`. The suite itself could not be installed under the approved
//! conditions (its `package-lock.json` is out of sync with `package.json`, so `npm ci` refuses,
//! and the pinned alternative needs `pnpm`), so this is the stand-in. The `/swap` cases go
//! through the `acceptance` feature's canned answers, as docs/design/jupiter-rfq.md says.

#![cfg(feature = "acceptance")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros,
    missing_docs
)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use jupiter_rfq::config::{ConfigFile, StaticChain, StaticSpot};
use jupiter_rfq::desk::{Desk, Start, SystemClock};
use jupiter_rfq::ledger::MemoryLedger;
use serde_json::{json, Value};
use tower::ServiceExt;

const ACCEPTANCE: &str = include_str!("fixtures/acceptance.json");

fn app() -> axum::Router {
    let loaded = ConfigFile::from_json(include_str!("../config/acceptance.json"))
        .unwrap()
        .load()
        .unwrap();
    let desk = Arc::new(Desk::new(
        loaded.desk.clone(),
        Box::new(StaticSpot::new(loaded.mids.clone(), Box::new(SystemClock))),
        Box::new(SystemClock),
        Box::new(MemoryLedger::new()),
        Start::Fresh,
    ));
    let chain = StaticChain::new(loaded.wallet.0, loaded.wallet.1, Box::new(SystemClock));
    desk.reconcile(&chain).unwrap();
    jupiter_rfq::server::router(desk, loaded.quote_deadline)
}

async fn send(req: Request<Body>) -> (StatusCode, Value) {
    let res = app().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post(path: &str, body: &Value) -> (StatusCode, Value) {
    send(
        Request::post(path)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap(),
    )
    .await
}

fn fixtures() -> Value {
    serde_json::from_str(ACCEPTANCE).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn quote_exact_in() {
    let payload = fixtures()["quote_exact_in"].clone();
    let (s, r) = post("/quote", &payload).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    for k in [
        "quoteId",
        "requestId",
        "tokenIn",
        "tokenOut",
        "quoteType",
        "taker",
    ] {
        assert_eq!(r[k], payload[k], "{k}");
    }
    assert!(r["maker"].is_string());
    assert_eq!(r["amountIn"], payload["amount"]);
    assert!(r["amountOut"].as_str().unwrap().parse::<u64>().unwrap() > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn quote_exact_out() {
    let mut payload = fixtures()["quote_exact_in"].clone();
    // As the suite: exact-out, MINT_A (wSOL) in, MINT_B (USDC) out.
    payload["quoteType"] = json!("exactOut");
    payload["tokenIn"] = json!("So11111111111111111111111111111111111111112");
    payload["tokenOut"] = json!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
    let (s, r) = post("/quote", &payload).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert_eq!(r["quoteType"], "exactOut");
    assert_eq!(r["taker"], payload["taker"]);
    assert_eq!(r["amountOut"], payload["amount"]);
    assert!(r["amountIn"].as_str().unwrap().parse::<u64>().unwrap() > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn quote_unsupported_pair_is_404() {
    let (s, _) = post("/quote", &fixtures()["quote_unsupported"]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn swap_cases() {
    let swap = fixtures()["swap"].clone();
    let (s, r) = post("/swap", &swap).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["quoteId"], swap["quoteId"]);
    assert_eq!(r["state"], "accepted");

    let with_id = |id: &str| {
        let mut p = swap.clone();
        p["requestId"] = json!(id);
        p
    };
    let (_, r) = post("/swap", &with_id("00000000-0000-0000-0000-000000000001")).await;
    assert_eq!(r["state"], "rejected");
    assert!(r["rejectionReason"].as_str().is_some_and(|s| !s.is_empty()));
    let (_, r) = post("/swap", &with_id("00000000-0000-0000-0000-000000000003")).await;
    assert_eq!(
        r["state"],
        json!({ "rejectedWithReason": "insufficientBalance" })
    );
    let (_, r) = post("/swap", &with_id("00000000-0000-0000-0000-000000000004")).await;
    assert_eq!(
        r["state"],
        json!({ "rejectedWithReason": "signatureVerificationFailed" })
    );

    // Outside the magic ids, the same transaction meets the real validator: its quote id was
    // never quoted here, and its payload is the older 3 bytes.
    let (_, r) = post("/swap", &with_id("11111111-2222-3333-4444-555555555555")).await;
    assert_eq!(r["state"], "rejected");
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens() {
    let (s, r) = send(Request::get("/tokens").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        r,
        json!([
            "So11111111111111111111111111111111111111112",
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
        ])
    );
}
