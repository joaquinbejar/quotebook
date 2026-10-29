//! A restarted maker must not evict a still-valid quote it signed before the restart. Run
//! against the real program in LiteSVM: first the hazard with a naive fresh issuer, then the
//! fail-closed `NonceIssuer::resume`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    missing_docs
)]

mod common;

use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use common::{Chain, Maker, NOW};
use quotebook_core::{
    BaseLots, Nonce, QuotebookError, Seconds, UnixTimestamp, MAX_CLOCK_SKEW, WINDOW_BITS,
};
use quotebook_maker::client::SignedQuote;
use quotebook_maker::issuer::NonceIssuer;
use quotebook_maker::keys::MakerKey;
use quotebook_maker::pricing::PricingParams;
use quotebook_maker::protocol::{from_hex, QuoteRequest, RejectReason, ServerMessage, TakerSide};
use quotebook_maker::server::{FixedInventory, FixedMid, MakerService, ServiceConfig, TimeSource};
use quotebook_sdk::core::Quote;
use rust_decimal::Decimal;
use solana_signer::Signer;

#[derive(Clone)]
struct Clock(Arc<AtomicI64>);

impl TimeSource for Clock {
    fn now(&self) -> UnixTimestamp {
        UnixTimestamp::new(self.0.load(Ordering::SeqCst))
    }
}

fn service(chain: &Chain, maker: &Maker, issuer: NonceIssuer, clock: &Clock) -> MakerService {
    let d = |v: &str| Decimal::from_str(v).unwrap();
    MakerService::new(
        MakerKey::new(maker.key.insecure_clone()),
        ServiceConfig {
            market: chain.market,
            pricing: PricingParams {
                risk_aversion: d("0.1"),
                volatility: d("0.6"),
                time_to_terminal_ms: 3_600_000,
                order_intensity: d("1.5"),
                max_deviation: d("0.05"),
            },
            ttl: Seconds::new(30),
            max_size: BaseLots::new(1_000),
            maker_base: maker.base,
            maker_quote: maker.quote,
        },
        issuer,
        Box::new(FixedMid(d("25"))),
        Box::new(FixedInventory(Decimal::ZERO)),
        Box::new(clock.clone()),
    )
    .unwrap()
}

fn quote(
    service: &MakerService,
    chain: &Chain,
    taker: &common::Taker,
    id: u64,
) -> Result<SignedQuote, RejectReason> {
    let request = QuoteRequest {
        request_id: id,
        series: chain.market.series.to_string(),
        taker: taker.key.pubkey().to_string(),
        taker_side: TakerSide::Buy,
        size_lots: 1,
    };
    match service.respond(&request) {
        ServerMessage::Quote(m) => Ok(SignedQuote {
            quote: Quote::decode(&from_hex(&m.quote).unwrap()).unwrap(),
            signature: from_hex(&m.signature).unwrap().try_into().unwrap(),
        }),
        ServerMessage::Reject(r) => Err(r.reason),
    }
}

/// Before the restart, the service signs quote Q0 (nonce 0, live until NOW + 30) and the taker
/// holds it unfilled. Returns the chain, the maker, the taker and Q0.
fn before_restart() -> (Chain, Maker, common::Taker, SignedQuote) {
    let mut chain = Chain::new();
    let maker = chain.maker();
    let taker = chain.taker();
    let clock = Clock(Arc::new(AtomicI64::new(NOW)));
    let window = chain.window(&maker.key.pubkey());
    let old = service(
        &chain,
        &maker,
        NonceIssuer::fresh(window.window_base()),
        &clock,
    );
    let q0 = quote(&old, &chain, &taker, 1).unwrap();
    assert_eq!(q0.quote.terms().nonce, Nonce::new(0));
    (chain, maker, taker, q0)
}

/// The hazard: a naive restart with a fresh issuer (here starting a window ahead) signs a
/// nonce that evicts Q0 while Q0 is still valid, and the program cannot tell.
#[test]
fn test_naive_restart_evicts_a_live_quote() {
    let (mut chain, maker, taker, q0) = before_restart();
    let clock = Clock(Arc::new(AtomicI64::new(NOW + 1)));
    let naive = service(
        &chain,
        &maker,
        NonceIssuer::fresh(Nonce::new(WINDOW_BITS)),
        &clock,
    );
    let qm = quote(&naive, &chain, &taker, 2).unwrap();
    chain.set_clock(NOW + 2);
    chain.settle(&qm, &maker, &taker, BaseLots::new(1)).unwrap();
    // Q0 is within its lifetime, but its nonce has been slid out.
    assert!(q0
        .quote
        .check_time(UnixTimestamp::new(NOW + 3), Seconds::new(60))
        .is_ok());
    chain.set_clock(NOW + 3);
    let err = chain
        .settle(&q0, &maker, &taker, BaseLots::new(1))
        .unwrap_err();
    assert!(
        err.contains(&format!(
            "Custom({})",
            QuotebookError::NonceOutOfWindow.code()
        )),
        "{err}"
    );
}

/// The fix: `resume` reads the on-chain window and refuses every quote until all quotes
/// signed before the restart have expired, so none can be evicted while live.
#[test]
fn test_resumed_issuer_cannot_evict_a_live_quote() {
    let (mut chain, maker, taker, q0) = before_restart();
    let started_at = NOW + 1;
    let clock = Clock(Arc::new(AtomicI64::new(started_at)));
    let window = chain.window(&maker.key.pubkey());
    let issuer =
        NonceIssuer::resume(&window, UnixTimestamp::new(started_at), Seconds::new(60)).unwrap();
    let resumed = service(&chain, &maker, issuer, &clock);
    let until = started_at + 60 + MAX_CLOCK_SKEW.get();

    // During the cold start nothing is signed, at any time before `until`.
    for t in [started_at, NOW + 30, until - 1] {
        clock.0.store(t, Ordering::SeqCst);
        assert_eq!(
            quote(&resumed, &chain, &taker, 3).unwrap_err(),
            RejectReason::RateLimited
        );
    }
    // Meanwhile Q0 still fills normally.
    chain.set_clock(NOW + 5);
    chain.settle(&q0, &maker, &taker, BaseLots::new(1)).unwrap();

    // At the end of the cold start it still refuses until refreshed with a window read from
    // then on; the refresh picks up Q0's fill during the wait.
    clock.0.store(until, Ordering::SeqCst);
    assert_eq!(
        quote(&resumed, &chain, &taker, 4).unwrap_err(),
        RejectReason::RateLimited
    );
    let window = chain.window(&maker.key.pubkey());
    assert!(resumed
        .refresh_window(&window, UnixTimestamp::new(until - 1))
        .is_err());
    resumed
        .refresh_window(&window, UnixTimestamp::new(until))
        .unwrap();
    // It quotes again, above everything used on chain; every quote signed before the
    // restart has expired by then.
    let next = quote(&resumed, &chain, &taker, 4).unwrap();
    assert_eq!(next.quote.terms().nonce, Nonce::new(1));
    assert!(q0
        .quote
        .check_time(
            UnixTimestamp::new(until - MAX_CLOCK_SKEW.get()),
            Seconds::new(60)
        )
        .is_err());
    chain.set_clock(until);
    chain
        .settle(&next, &maker, &taker, BaseLots::new(1))
        .unwrap();
}

/// With Q0 left unfilled, the first nonce the resumed issuer can sign comes only after Q0 is
/// dead on chain.
#[test]
fn test_resumed_issuer_waits_out_an_unfilled_quote() {
    let (mut chain, maker, taker, q0) = before_restart();
    let started_at = NOW + 1;
    let window = chain.window(&maker.key.pubkey());
    let mut issuer =
        NonceIssuer::resume(&window, UnixTimestamp::new(started_at), Seconds::new(60)).unwrap();
    let until = started_at + 60 + MAX_CLOCK_SKEW.get();
    assert!(issuer
        .issue(
            UnixTimestamp::new(until - 1),
            UnixTimestamp::new(until + 29)
        )
        .is_err());
    issuer
        .refresh(
            &chain.window(&maker.key.pubkey()),
            UnixTimestamp::new(until),
        )
        .unwrap();
    issuer
        .issue(UnixTimestamp::new(until), UnixTimestamp::new(until + 30))
        .unwrap();
    // A fill of anything signed from `until` happens at a chain time >= until - skew, when Q0
    // (expires NOW + 30) is already expired.
    chain.set_clock(until - MAX_CLOCK_SKEW.get());
    let err = chain
        .settle(&q0, &maker, &taker, BaseLots::new(1))
        .unwrap_err();
    assert!(
        err.contains(&format!("Custom({})", QuotebookError::QuoteExpired.code())),
        "{err}"
    );
}

/// Codex's reproduction against the program: a pre-restart nonce 1 is live; a fresh issuer
/// starting at 2 at the same timestamp signs through 16,385; the program accepts 16,385 and
/// then rejects the still-live nonce 1 with `NonceOutOfWindow` (121). A resumed issuer signs
/// nothing at that timestamp.
#[test]
fn test_codex_restart_regression() {
    let mut chain = Chain::new();
    let maker = chain.maker();
    let taker = chain.taker();
    let clock = Clock(Arc::new(AtomicI64::new(NOW)));
    let window = chain.window(&maker.key.pubkey());
    let old = service(&chain, &maker, NonceIssuer::fresh(Nonce::new(1)), &clock);
    let q1 = quote(&old, &chain, &taker, 1).unwrap();
    assert_eq!(q1.quote.terms().nonce, Nonce::new(1));

    let naive = service(&chain, &maker, NonceIssuer::fresh(Nonce::new(2)), &clock);
    let mut last = None;
    for id in 2..=16_385u64 {
        last = Some(quote(&naive, &chain, &taker, id).unwrap());
    }
    let q16385 = last.unwrap();
    assert_eq!(q16385.quote.terms().nonce, Nonce::new(16_385));
    chain
        .settle(&q16385, &maker, &taker, BaseLots::new(1))
        .unwrap();
    let err = chain
        .settle(&q1, &maker, &taker, BaseLots::new(1))
        .unwrap_err();
    assert!(err.contains("Custom(121)"), "{err}");
    assert_eq!(QuotebookError::NonceOutOfWindow.code(), 121);

    // The same restart through `resume` refuses at that timestamp.
    let issuer = NonceIssuer::resume(&window, UnixTimestamp::new(NOW), Seconds::new(60)).unwrap();
    let resumed = service(&chain, &maker, issuer, &clock);
    assert_eq!(
        quote(&resumed, &chain, &taker, 99).unwrap_err(),
        RejectReason::RateLimited
    );
}

/// Codex's repro of the refresh gap, against the program: a live issuer signs nonces 0, 1, 2;
/// the maker cancels nonce 16,384 on chain (base moves to 1, nonce 1 still valid); a refresh
/// of the live issuer is refused, so its next nonce is 3, not 16,385, and nonce 1 still
/// fills afterwards.
#[test]
fn test_codex_refresh_regression() {
    let mut chain = Chain::new();
    let maker = chain.maker();
    let taker = chain.taker();
    let clock = Clock(Arc::new(AtomicI64::new(NOW)));
    let live = service(&chain, &maker, NonceIssuer::fresh(Nonce::new(0)), &clock);
    let quotes: Vec<SignedQuote> = (0..3)
        .map(|id| quote(&live, &chain, &taker, id).unwrap())
        .collect();
    let key = maker.key.insecure_clone();
    chain
        .send(
            &[quotebook_sdk::ix::cancel_nonce(
                &common::PROGRAM_ID,
                &key.pubkey(),
                Nonce::new(WINDOW_BITS),
            )],
            &[&key],
        )
        .unwrap();
    let window = chain.window(&maker.key.pubkey());
    assert_eq!(window.window_base(), Nonce::new(1));
    assert!(live
        .refresh_window(&window, UnixTimestamp::new(NOW))
        .is_err());
    let next = quote(&live, &chain, &taker, 3).unwrap();
    assert_eq!(next.quote.terms().nonce, Nonce::new(3));
    chain
        .settle(&next, &maker, &taker, BaseLots::new(1))
        .unwrap();
    chain
        .settle(&quotes[1], &maker, &taker, BaseLots::new(1))
        .unwrap();
}
