//! Local demo: two maker services and a taker CLI flow against quotebook in LiteSVM.
//!
//! 1. LiteSVM with the program (`make program` first), a series of two 6-decimal SPL mints,
//!    two makers with approved delegates, and a taker (`tests/common`).
//! 2. Each maker runs the WebSocket service on localhost, pricing with market-maker-rs from
//!    a configured mid with a different risk aversion, so the two asks differ.
//! 3. The taker asks both for a firm quote to buy, verifies each (signature and every term
//!    the program checks), ranks them, and settles the best through the SDK.
//!
//! Run with `cargo run -p quotebook-maker --example demo`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    missing_docs
)]

#[path = "../tests/common/mod.rs"]
mod common;

use std::str::FromStr;
use std::sync::Arc;

use common::{Chain, NOW};
use quotebook_maker::client::{self, Expectation, SignedQuote};
use quotebook_maker::issuer::NonceIssuer;
use quotebook_maker::keys::MakerKey;
use quotebook_maker::pricing::PricingParams;
use quotebook_maker::protocol::{QuoteRequest, TakerSide};
use quotebook_maker::server::{FixedInventory, FixedMid, FixedTime, MakerService, ServiceConfig};
use quotebook_sdk::core::{BaseLots, Seconds, Side, UnixTimestamp};
use rust_decimal::Decimal;
use solana_address::Address;
use solana_signer::Signer;

/// What the demo did, for the test and the printout.
#[derive(Debug)]
pub struct Report {
    pub quotes: Vec<(Address, u64)>,
    pub winner: Address,
    pub price_ticks: u64,
    pub taker_base_delta: i128,
    pub taker_quote_delta: i128,
    pub fills: u64,
}

pub async fn run() -> Report {
    // 1. Chain.
    let mut chain = Chain::new();
    let makers = [chain.maker(), chain.maker()];
    let taker = chain.taker();
    let market = chain.market;

    // 2. One service per maker, on localhost. These keys have never signed, so their
    // issuers start fresh at the on-chain watermark.
    let d = |v: &str| Decimal::from_str(v).unwrap();
    let mut urls = Vec::new();
    for (i, maker) in makers.iter().enumerate() {
        let window = chain.window(&maker.key.pubkey());
        let config = ServiceConfig {
            market,
            pricing: PricingParams {
                risk_aversion: if i == 0 { d("0.1") } else { d("0.5") },
                volatility: d("0.6"),
                time_to_terminal_ms: 3_600_000,
                order_intensity: d("1.5"),
                max_deviation: d("0.05"),
            },
            ttl: Seconds::new(30),
            max_size: BaseLots::new(1_000),
        };
        let service = MakerService::new(
            MakerKey::new(maker.key.insecure_clone()),
            config,
            NonceIssuer::fresh(window.window_base()),
            Box::new(FixedMid(d("25.00"))),
            Box::new(FixedInventory(Decimal::ZERO)),
            Box::new(FixedTime(UnixTimestamp::new(NOW))),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        urls.push(format!("ws://{}", listener.local_addr().unwrap()));
        tokio::spawn(Arc::new(service).serve(listener));
    }

    // 3. The taker: request, verify, rank, settle the best.
    let size = BaseLots::new(40);
    let request = QuoteRequest {
        request_id: 1,
        series: market.series.to_string(),
        taker: taker.key.pubkey().to_string(),
        taker_side: TakerSide::Buy,
        size_lots: size.get(),
    };
    let expected = Expectation {
        market,
        taker: taker.key.pubkey(),
        taker_side: TakerSide::Buy,
        size,
        now: UnixTimestamp::new(NOW),
    };
    let mut quotes: Vec<SignedQuote> = Vec::new();
    for url in &urls {
        let q = client::request_quote(url, &request).await.unwrap();
        client::verify(&q, &expected).unwrap();
        quotes.push(q);
    }
    let summary = quotes
        .iter()
        .map(|q| (q.quote.terms().maker, q.quote.terms().price.get()))
        .collect();
    let best = client::rank(quotes, TakerSide::Buy)[0];
    let winner = best.quote.terms().maker;
    assert_eq!(best.quote.terms().side, Side::Ask);
    let maker = makers.iter().find(|m| m.key.pubkey() == winner).unwrap();

    let (b0, q0) = (chain.amount(&taker.base), chain.amount(&taker.quote));
    chain.settle(&best, maker, &taker, size).unwrap();
    let (b1, q1) = (chain.amount(&taker.base), chain.amount(&taker.quote));
    let window = chain.window(&winner);
    assert!(window.check(best.quote.terms().nonce).is_err());
    let state = quotebook_sdk::pda::maker_state(&common::PROGRAM_ID, &winner).0;
    let state = chain.svm.get_account(&state).unwrap();
    let fills = quotebook_sdk::decode::maker_state(&common::PROGRAM_ID, &state.owner, &state.data)
        .unwrap()
        .fills;

    Report {
        quotes: summary,
        winner,
        price_ticks: best.quote.terms().price.get(),
        taker_base_delta: i128::from(b1) - i128::from(b0),
        taker_quote_delta: i128::from(q1) - i128::from(q0),
        fills,
    }
}

#[allow(dead_code)]
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();
    let r = run().await;
    for (maker, ticks) in &r.quotes {
        println!("quote from {maker}: {ticks} ticks");
    }
    println!(
        "settled with {}: {} ticks, taker base {:+} atoms, quote {:+} atoms",
        r.winner, r.price_ticks, r.taker_base_delta, r.taker_quote_delta
    );
}
