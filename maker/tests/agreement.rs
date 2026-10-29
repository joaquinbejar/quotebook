//! `client::verify` and the program agree on every rule `settle` applies to a quote's own
//! terms. Each row signs a quote (or sets up a taker) that breaks one rule, checks it with
//! `verify`, then settles it on the real program in LiteSVM: the program must fail with
//! exactly the error `verify` predicted, and accept exactly what `verify` accepts. Rules about
//! on-chain state (nonces, pause, balances) are outside `verify` by design.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    clippy::disallowed_macros,
    missing_docs
)]

mod common;

use common::{Chain, Taker, NOW};
use quotebook_core::{
    BaseLots, Nonce, Quote, QuoteLotsPerBaseLot, QuoteTerms, QuotebookError, SeriesId, Side,
    UnixTimestamp,
};
use quotebook_maker::client::{self, ClientError, Expectation, SignedQuote};
use quotebook_maker::keys::MakerKey;
use quotebook_maker::protocol::TakerSide;
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// One rule: how to break it, and the error both sides must report (`None`: both accept).
struct Row {
    name: &'static str,
    terms: fn(&mut QuoteTerms),
    fill: u64,
    self_trade: bool,
    now: i64,
    expected: Option<QuotebookError>,
}

fn row(name: &'static str, terms: fn(&mut QuoteTerms), expected: Option<QuotebookError>) -> Row {
    Row {
        name,
        terms,
        fill: 5,
        self_trade: false,
        now: NOW,
        expected,
    }
}

fn rows() -> Vec<Row> {
    use QuotebookError::*;
    vec![
        row("valid, bound to the taker", |_| {}, None),
        row("valid, open quote", |t| t.taker = Quote::OPEN_TAKER, None),
        row(
            "another program",
            |t| t.program_id = Address::new_from_array([7; 32]),
            Some(WrongProgram),
        ),
        row(
            "another series",
            |t| t.series = SeriesId::new(Address::new_from_array([7; 32])),
            Some(WrongSeries),
        ),
        row(
            "another deployment",
            |t| t.deployment = *b"other-deployment",
            Some(WrongSeries),
        ),
        Row {
            self_trade: true,
            ..row(
                "the maker as taker",
                |t| t.taker = Quote::OPEN_TAKER,
                Some(SelfTrade),
            )
        },
        row(
            "bound to another taker",
            |t| t.taker = Address::new_from_array([7; 32]),
            Some(WrongTaker),
        ),
        Row {
            now: NOW + 30,
            ..row("expired", |_| {}, Some(QuoteExpired))
        },
        row(
            "lifetime above the series TTL",
            |t| t.expires_at = UnixTimestamp::new(NOW + 61),
            Some(QuoteTtlTooLong),
        ),
        row(
            "issued beyond the clock skew",
            |t| {
                t.issued_at = UnixTimestamp::new(NOW + 11);
                t.expires_at = UnixTimestamp::new(NOW + 40);
            },
            Some(QuoteTtlTooLong),
        ),
        row(
            "issued at the clock skew",
            |t| {
                t.issued_at = UnixTimestamp::new(NOW + 10);
                t.expires_at = UnixTimestamp::new(NOW + 40);
            },
            None,
        ),
        Row {
            fill: 11,
            ..row("fill above the quote size", |_| {}, Some(FillSizeInvalid))
        },
        Row {
            fill: 0,
            ..row(
                "fill below the series minimum",
                |_| {},
                Some(FillSizeInvalid),
            )
        },
        row(
            "quote amount overflows u64",
            |t| t.price = QuoteLotsPerBaseLot::new(u64::MAX / 100),
            Some(Overflow),
        ),
    ]
}

#[test]
fn test_verify_and_program_agree_on_every_quote_rule() {
    for r in rows() {
        let mut chain = Chain::new();
        let maker = chain.maker();
        let taker = if r.self_trade {
            Taker {
                key: maker.key.insecure_clone(),
                base: maker.base,
                quote: maker.quote,
            }
        } else {
            chain.taker()
        };
        let market = chain.market;
        let mut terms = QuoteTerms {
            program_id: market.program_id,
            deployment: market.deployment,
            maker: maker.key.pubkey(),
            series: SeriesId::new(market.series),
            taker: taker.key.pubkey(),
            side: Side::Ask,
            price: QuoteLotsPerBaseLot::new(2_500),
            size: BaseLots::new(10),
            issued_at: UnixTimestamp::new(NOW),
            expires_at: UnixTimestamp::new(NOW + 30),
            nonce: Nonce::new(1),
        };
        (r.terms)(&mut terms);
        let quote = Quote::new(terms).unwrap();
        let signed = SignedQuote {
            quote,
            signature: MakerKey::new(maker.key.insecure_clone()).sign(&quote),
        };
        let expectation = Expectation {
            market,
            taker: taker.key.pubkey(),
            taker_side: TakerSide::Buy,
            size: BaseLots::new(r.fill),
            now: UnixTimestamp::new(r.now),
        };
        let client = client::verify(&signed, &expectation);
        chain.set_clock(r.now);
        // The taker always passes the market's own series account; the SDK takes it from
        // the quote, so point it back at the real one.
        let mut pair = chain.settle_pair(&signed, &maker, &taker, BaseLots::new(r.fill));
        pair[1].accounts[1].pubkey = market.series;
        let key = taker.key.insecure_clone();
        let program = chain.send(&pair, &[&key]);
        match r.expected {
            None => {
                assert_eq!(client, Ok(()), "{}: verify", r.name);
                assert_eq!(program, Ok(()), "{}: program", r.name);
            }
            Some(e) => {
                assert_eq!(client, Err(ClientError::WouldFail(e)), "{}: verify", r.name);
                let err = program.expect_err(r.name);
                assert!(
                    err.contains(&format!("Custom({}))", e.code())),
                    "{}: program said {err}, verify said {e:?}",
                    r.name
                );
            }
        }
    }
}

/// A signature by another key: `verify` reports it, and the ed25519 precompile fails the
/// transaction before `settle` runs.
#[test]
fn test_verify_and_precompile_agree_on_a_foreign_signature() {
    let mut chain = Chain::new();
    let maker = chain.maker();
    let taker = chain.taker();
    let market = chain.market;
    let quote = Quote::new(QuoteTerms {
        program_id: market.program_id,
        deployment: market.deployment,
        maker: maker.key.pubkey(),
        series: SeriesId::new(market.series),
        taker: taker.key.pubkey(),
        side: Side::Ask,
        price: QuoteLotsPerBaseLot::new(2_500),
        size: BaseLots::new(10),
        issued_at: UnixTimestamp::new(NOW),
        expires_at: UnixTimestamp::new(NOW + 30),
        nonce: Nonce::new(1),
    })
    .unwrap();
    let signed = SignedQuote {
        quote,
        signature: MakerKey::new(Keypair::new()).sign(&quote),
    };
    let expectation = Expectation {
        market,
        taker: taker.key.pubkey(),
        taker_side: TakerSide::Buy,
        size: BaseLots::new(5),
        now: UnixTimestamp::new(NOW),
    };
    assert_eq!(
        client::verify(&signed, &expectation),
        Err(ClientError::BadSignature)
    );
    let err = chain
        .settle(&signed, &maker, &taker, BaseLots::new(5))
        .unwrap_err();
    assert!(err.contains("InstructionError(0,"), "{err}");
}
