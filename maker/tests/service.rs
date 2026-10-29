//! The service's answers and the taker's checks, without a network: every refusal, the
//! issuance bound through the service, and every verification failure.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    missing_docs
)]

use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use quotebook_core::{BaseLots, Nonce, Quote, Seconds, Side, UnixTimestamp, WINDOW_BITS};
use quotebook_maker::client::{self, ClientError, Expectation, SignedQuote};
use quotebook_maker::issuer::NonceIssuer;
use quotebook_maker::keys::MakerKey;
use quotebook_maker::market::MarketInfo;
use quotebook_maker::pricing::PricingParams;
use quotebook_maker::protocol::{from_hex, QuoteRequest, RejectReason, ServerMessage, TakerSide};
use quotebook_maker::server::{
    ConfigError, FixedInventory, FixedMid, MakerService, PriceFeed, ServiceConfig, TimeSource,
};
use rust_decimal::Decimal;
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;

const NOW: i64 = 1_700_000_000;

#[derive(Clone)]
struct Clock(Arc<AtomicI64>);

impl TimeSource for Clock {
    fn now(&self) -> UnixTimestamp {
        UnixTimestamp::new(self.0.load(Ordering::SeqCst))
    }
}

struct NoMid;

impl PriceFeed for NoMid {
    fn mid(&self) -> Option<Decimal> {
        None
    }
}

fn d(v: &str) -> Decimal {
    Decimal::from_str(v).unwrap()
}

fn market() -> MarketInfo {
    MarketInfo {
        program_id: Address::new_from_array([0x51; 32]),
        deployment: [7; 16],
        series: Address::new_from_array([9; 32]),
        base_decimals: 6,
        quote_decimals: 6,
        base_lot_size: 10_000,
        quote_lot_size: 100,
        max_quote_ttl: Seconds::new(60),
        min_size: BaseLots::new(2),
    }
}

fn config(max_deviation: &str) -> ServiceConfig {
    ServiceConfig {
        market: market(),
        pricing: PricingParams {
            risk_aversion: d("0.1"),
            volatility: d("0.6"),
            time_to_terminal_ms: 3_600_000,
            order_intensity: d("1.5"),
            max_deviation: d(max_deviation),
        },
        ttl: Seconds::new(30),
        max_size: BaseLots::new(100),
    }
}

fn service(
    clock: &Clock,
    feed: Box<dyn PriceFeed>,
    max_deviation: &str,
) -> (MakerService, Keypair) {
    let key = Keypair::new();
    let s = MakerService::new(
        MakerKey::new(key.insecure_clone()),
        config(max_deviation),
        NonceIssuer::new(Nonce::new(0)),
        feed,
        Box::new(FixedInventory(Decimal::ZERO)),
        Box::new(clock.clone()),
    )
    .unwrap();
    (s, key)
}

fn clock() -> Clock {
    Clock(Arc::new(AtomicI64::new(NOW)))
}

fn request(id: u64, side: TakerSide, size: u64, taker: &Address) -> QuoteRequest {
    QuoteRequest {
        request_id: id,
        series: market().series.to_string(),
        taker: taker.to_string(),
        taker_side: side,
        size_lots: size,
    }
}

fn signed(answer: ServerMessage) -> SignedQuote {
    match answer {
        ServerMessage::Quote(m) => SignedQuote {
            quote: Quote::decode(&from_hex(&m.quote).unwrap()).unwrap(),
            signature: from_hex(&m.signature).unwrap().try_into().unwrap(),
        },
        ServerMessage::Reject(r) => panic!("rejected: {r:?}"),
    }
}

fn reason(answer: ServerMessage) -> RejectReason {
    match answer {
        ServerMessage::Reject(r) => r.reason,
        ServerMessage::Quote(_) => panic!("quoted"),
    }
}

fn expectation(taker: &Address, side: TakerSide, size: u64, now: i64) -> Expectation {
    Expectation {
        program_id: market().program_id,
        deployment: market().deployment,
        series: market().series,
        taker: *taker,
        taker_side: side,
        size: BaseLots::new(size),
        now: UnixTimestamp::new(now),
        max_quote_ttl: Seconds::new(60),
    }
}

#[test]
fn test_service_quotes_both_sides_rounded_toward_the_maker() {
    let c = clock();
    let (s, key) = service(&c, Box::new(FixedMid(d("25"))), "0.05");
    let taker = Keypair::new().pubkey();
    let ask = signed(s.respond(&request(1, TakerSide::Buy, 10, &taker)));
    let bid = signed(s.respond(&request(2, TakerSide::Sell, 10, &taker)));
    for (q, side) in [(&ask, TakerSide::Buy), (&bid, TakerSide::Sell)] {
        client::verify(q, &expectation(&taker, side, 10, NOW)).unwrap();
        assert_eq!(q.quote.terms().maker, key.pubkey());
        assert_eq!(q.quote.terms().taker, taker);
        assert_eq!(q.quote.terms().expires_at.get(), NOW + 30);
    }
    assert_eq!(ask.quote.terms().side, Side::Ask);
    assert_eq!(bid.quote.terms().side, Side::Bid);
    assert!(bid.quote.terms().price < ask.quote.terms().price);
    // Consecutive nonces.
    assert_eq!(ask.quote.terms().nonce, Nonce::new(0));
    assert_eq!(bid.quote.terms().nonce, Nonce::new(1));
}

#[test]
fn test_service_refusals() {
    let c = clock();
    let (s, _) = service(&c, Box::new(FixedMid(d("25"))), "0.05");
    let taker = Keypair::new().pubkey();
    let mut other_series = request(1, TakerSide::Buy, 10, &taker);
    other_series.series = Address::new_from_array([8; 32]).to_string();
    assert_eq!(reason(s.respond(&other_series)), RejectReason::BadRequest);
    let mut garbage = request(1, TakerSide::Buy, 10, &taker);
    garbage.taker = "not-an-address".into();
    assert_eq!(reason(s.respond(&garbage)), RejectReason::BadRequest);
    assert_eq!(
        reason(s.respond(&request(1, TakerSide::Buy, 1, &taker))),
        RejectReason::Size
    );
    assert_eq!(
        reason(s.respond(&request(1, TakerSide::Buy, 101, &taker))),
        RejectReason::Size
    );

    let (no_mid, _) = service(&c, Box::new(NoMid), "0.05");
    assert_eq!(
        reason(no_mid.respond(&request(1, TakerSide::Buy, 10, &taker))),
        RejectReason::Pricing
    );
    // A price bound tighter than the model's half spread rejects every quote.
    let (tight, _) = service(&c, Box::new(FixedMid(d("25"))), "0.0000001");
    assert_eq!(
        reason(tight.respond(&request(1, TakerSide::Buy, 10, &taker))),
        RejectReason::Pricing
    );
}

#[test]
fn test_service_config_is_checked() {
    let c = clock();
    let mut cfg = config("0.05");
    cfg.ttl = Seconds::new(61);
    let r = MakerService::new(
        MakerKey::new(Keypair::new()),
        cfg,
        NonceIssuer::new(Nonce::new(0)),
        Box::new(FixedMid(d("25"))),
        Box::new(FixedInventory(Decimal::ZERO)),
        Box::new(c.clone()),
    );
    assert_eq!(r.unwrap_err(), ConfigError::Ttl);
    let mut cfg = config("0.05");
    cfg.max_size = BaseLots::new(1);
    let r = MakerService::new(
        MakerKey::new(Keypair::new()),
        cfg,
        NonceIssuer::new(Nonce::new(0)),
        Box::new(FixedMid(d("25"))),
        Box::new(FixedInventory(Decimal::ZERO)),
        Box::new(c.clone()),
    );
    assert_eq!(r.unwrap_err(), ConfigError::Size);
}

/// PLAN M6: the service refuses to stamp a nonce below the floor of the quote 16,384 nonces
/// back, and resumes once the clock passes it.
#[test]
fn test_service_refuses_past_the_issuance_bound() {
    let c = clock();
    let (s, _) = service(&c, Box::new(FixedMid(d("25"))), "0.05");
    let taker = Keypair::new().pubkey();
    for i in 0..WINDOW_BITS {
        signed(s.respond(&request(i, TakerSide::Buy, 10, &taker)));
    }
    let r = request(WINDOW_BITS, TakerSide::Buy, 10, &taker);
    assert_eq!(reason(s.respond(&r)), RejectReason::RateLimited);
    // The floor is expiry (NOW + 30) + MAX_CLOCK_SKEW (10).
    c.0.store(NOW + 39, Ordering::SeqCst);
    assert_eq!(reason(s.respond(&r)), RejectReason::RateLimited);
    c.0.store(NOW + 40, Ordering::SeqCst);
    let q = signed(s.respond(&r));
    assert_eq!(q.quote.terms().nonce, Nonce::new(WINDOW_BITS));
}

#[test]
fn test_client_verify_rejects_every_mismatch() {
    let c = clock();
    let (s, _) = service(&c, Box::new(FixedMid(d("25"))), "0.05");
    let taker = Keypair::new().pubkey();
    let q = signed(s.respond(&request(1, TakerSide::Buy, 10, &taker)));
    let ok = expectation(&taker, TakerSide::Buy, 10, NOW);
    client::verify(&q, &ok).unwrap();

    let mut tampered = q;
    tampered.signature[0] ^= 1;
    assert_eq!(
        client::verify(&tampered, &ok),
        Err(ClientError::BadSignature)
    );

    let cases: [(Expectation, &str); 7] = [
        (
            Expectation {
                program_id: Address::new_from_array([1; 32]),
                ..ok
            },
            "another program",
        ),
        (
            Expectation {
                deployment: [0; 16],
                ..ok
            },
            "another deployment",
        ),
        (
            Expectation {
                series: Address::new_from_array([1; 32]),
                ..ok
            },
            "another series",
        ),
        (
            Expectation {
                taker: Keypair::new().pubkey(),
                ..ok
            },
            "bound to another taker",
        ),
        (
            Expectation {
                taker_side: TakerSide::Sell,
                ..ok
            },
            "wrong side",
        ),
        (
            Expectation {
                size: BaseLots::new(11),
                ..ok
            },
            "smaller than requested",
        ),
        (
            Expectation {
                now: UnixTimestamp::new(NOW + 30),
                ..ok
            },
            "outside its time window",
        ),
    ];
    for (exp, why) in cases {
        assert_eq!(client::verify(&q, &exp), Err(ClientError::BadQuote(why)));
    }
}

#[test]
fn test_client_rank_orders_by_price_for_each_side() {
    let c = clock();
    let taker = Keypair::new().pubkey();
    let mut asks = Vec::new();
    let mut bids = Vec::new();
    for mid in ["24", "26", "25"] {
        let (s, _) = service(&c, Box::new(FixedMid(d(mid))), "0.05");
        asks.push(signed(s.respond(&request(1, TakerSide::Buy, 10, &taker))));
        bids.push(signed(s.respond(&request(2, TakerSide::Sell, 10, &taker))));
    }
    let asks: Vec<u64> = client::rank(asks, TakerSide::Buy)
        .iter()
        .map(|q| q.quote.terms().price.get())
        .collect();
    let bids: Vec<u64> = client::rank(bids, TakerSide::Sell)
        .iter()
        .map(|q| q.quote.terms().price.get())
        .collect();
    assert!(asks.windows(2).all(|w| w[0] <= w[1]), "{asks:?}");
    assert!(bids.windows(2).all(|w| w[0] >= w[1]), "{bids:?}");
}

#[test]
fn test_maker_key_debug_shows_no_secret() {
    let key = Keypair::new();
    let secret = key.to_bytes();
    let shown = format!("{:?}", MakerKey::new(key.insecure_clone()));
    assert!(shown.contains(&key.pubkey().to_string()), "{shown}");
    assert!(!shown.contains(&format!("{:?}", &secret[..32])), "{shown}");
}
