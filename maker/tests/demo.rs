//! Milestone 6 done-when: the local demo requests, receives and settles a quote priced by
//! market-maker-rs, end to end.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    missing_docs
)]

#[path = "../examples/demo.rs"]
mod demo;

#[tokio::test(flavor = "multi_thread")]
async fn test_demo_settles_the_best_of_two_makers() {
    let r = demo::run().await;
    assert_eq!(r.quotes.len(), 2);
    // The two risk aversions give two different asks; the lower one wins.
    let best = r.quotes.iter().map(|(_, t)| *t).min().unwrap();
    assert_eq!(r.price_ticks, best);
    assert!(r.quotes.iter().any(|(_, t)| *t > best));
    assert_eq!(
        r.winner,
        r.quotes.iter().find(|(_, t)| *t == best).unwrap().0
    );
    // 40 lots of 10,000 base atoms; the taker pays price × 40 × 100 quote atoms.
    assert_eq!(r.taker_base_delta, 400_000);
    assert_eq!(r.taker_quote_delta, -(i128::from(r.price_ticks) * 40 * 100));
    // Around the 25.00 mid: 25 quote tokens per base token = 2,500 ticks.
    assert!(
        r.price_ticks > 2_500 && r.price_ticks < 2_625,
        "{}",
        r.price_ticks
    );
}
