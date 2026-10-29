//! Property tests for the quote encoding: `decode(encode(q)) == q` at every alignment, `decode`
//! accepts exactly the outputs of `encode`, and invalid side bytes are rejected.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    clippy::panic_in_result_fn,
    clippy::disallowed_macros,
    missing_docs
)]

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use quotebook_core::{
    Address, BaseLots, Nonce, Quote, QuoteLotsPerBaseLot, QuoteTerms, QuotebookError, SeriesId,
    Side, UnixTimestamp, QUOTE_DOMAIN,
};

/// Cases per property. `QUOTEBOOK_PROPTEST_CASES` overrides the default.
fn config() -> Config {
    let cases = std::env::var("QUOTEBOOK_PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x0B0C_0001_5EED_0001),
        failure_persistence: None,
        ..Config::default()
    }
}

fn address() -> impl Strategy<Value = Address> {
    any::<[u8; 32]>().prop_map(Address::new_from_array)
}

fn quote() -> impl Strategy<Value = Quote> {
    (
        (address(), any::<[u8; 16]>(), address(), address()),
        (
            prop_oneof![Just(Quote::OPEN_TAKER), address()],
            prop_oneof![Just(Side::Bid), Just(Side::Ask)],
        ),
        (1..=u64::MAX, 1..=u64::MAX),
        (any::<i64>(), any::<i64>(), any::<u64>()),
    )
        .prop_map(
            |(
                (program_id, deployment, maker, series),
                (taker, side),
                (price, size),
                (issued_at, expires_at, nonce),
            )| {
                Quote::new(QuoteTerms {
                    program_id,
                    deployment,
                    maker,
                    series: SeriesId::new(series),
                    taker,
                    side,
                    price: QuoteLotsPerBaseLot::new(price),
                    size: BaseLots::new(size),
                    issued_at: UnixTimestamp::new(issued_at),
                    expires_at: UnixTimestamp::new(expires_at),
                    nonce: Nonce::new(nonce),
                })
                .unwrap()
            },
        )
}

proptest! {
    #![proptest_config(config())]

    /// Round trip at offsets 0 to 15 of a larger buffer, so the message is read unaligned as
    /// it is from precompile instruction data.
    #[test]
    fn test_quote_decode_encode_round_trips_at_any_offset(
        q in quote(),
        offset in 0usize..16,
        filler in any::<u8>(),
    ) {
        let bytes = q.encode();
        let mut buf = vec![filler; offset + Quote::LEN + 3];
        buf[offset..offset + Quote::LEN].copy_from_slice(&bytes);
        prop_assert_eq!(Quote::decode(&buf[offset..offset + Quote::LEN]), Ok(q));
        prop_assert_eq!(&bytes[..16], &QUOTE_DOMAIN);
    }

    /// Any side byte but 0 or 1 is rejected.
    #[test]
    fn test_quote_decode_invalid_side_is_rejected(q in quote(), side in 2u8..) {
        let mut bytes = q.encode();
        bytes[160] = side;
        prop_assert_eq!(Quote::decode(&bytes), Err(QuotebookError::MalformedQuote));
    }

    /// Canonicality: whatever 208 bytes `decode` accepts re-encode to the same bytes, so two
    /// different byte strings never decode to the same quote.
    #[test]
    fn test_quote_decode_accepts_only_canonical_bytes(
        q in quote(),
        flips in proptest::collection::vec((0usize..Quote::LEN, any::<u8>()), 0..4),
    ) {
        let mut bytes = q.encode();
        for (i, v) in flips {
            bytes[i] = v;
        }
        if let Ok(decoded) = Quote::decode(&bytes) {
            prop_assert_eq!(decoded.encode(), bytes);
        }
    }

    /// Fully random bytes behind a valid domain tag: decode never panics, and accepts only
    /// canonical encodings.
    #[test]
    fn test_quote_decode_random_bytes(tail in proptest::collection::vec(any::<u8>(), 192)) {
        let mut bytes = [0u8; Quote::LEN];
        bytes[..16].copy_from_slice(&QUOTE_DOMAIN);
        bytes[16..].copy_from_slice(&tail);
        if let Ok(decoded) = Quote::decode(&bytes) {
            prop_assert_eq!(decoded.encode(), bytes);
        }
    }
}
