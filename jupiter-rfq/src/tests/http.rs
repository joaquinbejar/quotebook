//! The routes: status codes, bodies, the `/quote` deadline, and `/swap` answering `rejected`
//! to a valid fill outside tests.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::Value;
use solana_keypair::Keypair;
use tower::ServiceExt;

use super::*;
use crate::api::QuoteType;
use crate::server::{router, router_accepting, NEVER_SIGNS};

async fn call(app: Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn post(path: &str, body: &impl serde::Serialize) -> Request<Body> {
    Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_quote_and_errors() {
    let r = rig("http");
    let app = router(Arc::clone(&r.desk), Duration::from_millis(200));
    let (s, body) = call(
        app.clone(),
        Request::get("/tokens").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!([BASE.to_string(), QUOTE.to_string()])
    );

    let taker = Keypair::new();
    let req = quote_request("q", &taker, false, QuoteType::ExactIn, 1_000_000_000);
    let (s, body) = call(app.clone(), post("/quote", &req)).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["quoteId"], "q");
    assert_eq!(body["amountIn"], "1000000000");
    assert_eq!(body["quoteType"], "exactIn");
    assert_eq!(body["protocol"], "v1");
    assert_eq!(body["maker"], MAKER.to_string());
    assert_eq!(body["prioritizationFeeToUse"], 10_000);

    let mut unsupported = req.clone();
    unsupported.token_out = Address::new_from_array([55; 32]).to_string();
    let (s, body) = call(app.clone(), post("/quote", &unsupported)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(body["message"].is_string());

    let (s, _) = call(
        app.clone(),
        Request::post("/quote")
            .header("content-type", "application/json")
            .body(Body::from("{\"requestId\": 1}"))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let mut bad = req.clone();
    bad.amount = "1.5".into();
    let (s, _) = call(app.clone(), post("/quote", &bad)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, _) = call(app, Request::get("/nothing").body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_quote_is_declined_at_its_deadline() {
    let r = rig("deadline");
    let taker = Keypair::new();
    let req = quote_request("q", &taker, false, QuoteType::ExactIn, 1_000_000_000);
    r.delay_ms.store(400, Ordering::SeqCst);

    // The configured deadline.
    let app = router(Arc::clone(&r.desk), Duration::from_millis(100));
    let started = Instant::now();
    let (s, body) = call(app, post("/quote", &req)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["message"], "deadline passed");
    assert!(
        started.elapsed() < Duration::from_millis(350),
        "{:?}",
        started.elapsed()
    );

    // Jupiter's header lowers it, never raises it.
    let app = router(Arc::clone(&r.desk), Duration::from_millis(2_000));
    let mut http = post("/quote", &req);
    http.headers_mut()
        .insert("x-request-timeout", "50".parse().unwrap());
    let started = Instant::now();
    let (s, _) = call(app.clone(), http).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(
        started.elapsed() < Duration::from_millis(350),
        "{:?}",
        started.elapsed()
    );

    // Fast enough: answered.
    r.delay_ms.store(0, Ordering::SeqCst);
    let mut http = post("/quote", &req);
    http.headers_mut()
        .insert("x-request-timeout", "5000".parse().unwrap());
    let (s, _) = call(app, http).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_valid_fill_is_rejected_because_the_prototype_never_signs() {
    let r = rig("swap-http");
    let taker = Keypair::new();
    let q = r
        .desk
        .quote(&quote_request(
            "q",
            &taker,
            true,
            QuoteType::ExactOut,
            1_000_000_000,
        ))
        .unwrap();
    let tx = FillTx::for_quote(&q, &taker);
    let req = tx.request("q", &taker);

    let app = router(Arc::clone(&r.desk), Duration::from_millis(200));
    let (s, body) = call(app.clone(), post("/swap", &req)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["quoteId"], "q");
    assert_eq!(body["state"], "rejected");
    assert_eq!(body["rejectionReason"], NEVER_SIGNS);
    assert!(body.get("txSignature").is_none());
    // Nothing was reserved or consumed.
    assert_eq!(r.desk.status().open_swaps, 0);

    // A forged signature: Jupiter's known reason, in enums.rs's shape.
    let mut forged = tx.build(&taker);
    forged.signatures[1] = [3; 64];
    let mut bad = req.clone();
    bad.transaction = forged.to_base64();
    let (_, body) = call(app, post("/swap", &bad)).await;
    assert_eq!(
        body["state"],
        serde_json::json!({ "rejectedWithReason": "signatureVerificationFailed" })
    );

    // Only the crate's tests can make it answer `accepted`; it still signs nothing.
    let accepting = router_accepting(Arc::clone(&r.desk), Duration::from_millis(200));
    let (_, body) = call(accepting.clone(), post("/swap", &req)).await;
    assert_eq!(body["state"], "accepted");
    assert!(body.get("txSignature").is_none());
    assert_eq!(r.desk.status().open_swaps, 1);
    let (_, body) = call(accepting, post("/swap", &tx.request("q", &taker))).await;
    assert_eq!(
        body["state"], "accepted",
        "a retry of the same message is idempotent"
    );
    assert_eq!(r.desk.status().open_swaps, 1);
}
