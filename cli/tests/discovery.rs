//! I10 / ADR 0002: taker discovery over the maker registry, against the program in LiteSVM, and
//! the identity binding: a quote is accepted only from the maker of the profile its endpoint
//! came from, and only from an allowlisted `quote.maker`.

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

use common::{setup, LiteRpc, NOW, PROGRAM_ID};
use quotebook_cli::maker::{prepare, refresh, MakerSettings, Prepared};
use quotebook_cli::taker::{self, MakerFilter};
use quotebook_cli::{admin, CliError};
use quotebook_core::{BaseLots, Seconds, UnixTimestamp};
use quotebook_maker::client::ClientError;
use quotebook_maker::pricing::PricingParams;
use quotebook_maker::protocol::TakerSide;
use quotebook_maker::server::TimeSource;
use rust_decimal::Decimal;
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;

#[derive(Clone)]
struct Clock(Arc<AtomicI64>);

impl TimeSource for Clock {
    fn now(&self) -> UnixTimestamp {
        UnixTimestamp::new(self.0.load(Ordering::SeqCst))
    }
}

struct Maker {
    key: Keypair,
    prepared: Prepared,
    url: String,
}

fn settings(
    series: Address,
    maker_base: Address,
    maker_quote: Address,
    mid: &str,
) -> MakerSettings {
    let d = |v: &str| Decimal::from_str(v).unwrap();
    MakerSettings {
        program_id: PROGRAM_ID,
        series,
        maker_base,
        maker_quote,
        mid: d(mid),
        pricing: PricingParams {
            risk_aversion: d("0.1"),
            volatility: d("0.6"),
            time_to_terminal_ms: 3_600_000,
            order_intensity: d("1.5"),
            max_deviation: d("0.05"),
            min_half_spread_bps: 5,
            max_half_spread_bps: 100,
            max_skew_bps: 100,
            model_units: quotebook_maker::pricing::ModelUnits::Relative,
        },
        ttl: Seconds::new(30),
        max_size: BaseLots::new(1_000),
        max_ttl_any_series: Seconds::new(60),
    }
}

/// A maker on chain (init, funded accounts, approved delegate) with its service listening on a
/// local `ws://` port.
#[allow(clippy::too_many_arguments)]
fn start_maker(
    rpc: &mut LiteRpc,
    runtime: &tokio::runtime::Runtime,
    clock: &Clock,
    payer: &Keypair,
    series: Address,
    mints: (Address, Address),
    key: Keypair,
    mid: &str,
) -> Maker {
    let maker = key.pubkey();
    if rpc.svm.get_account(&maker).is_none() {
        rpc.svm.airdrop(&maker, 1_000_000_000).unwrap();
        admin::init_maker(rpc, &PROGRAM_ID, &key, payer).unwrap();
    }
    let base = rpc.token_account(&mints.0, &maker, 1_000_000_000);
    let quote = rpc.token_account(&mints.1, &maker, 1_000_000_000);
    admin::approve(rpc, &PROGRAM_ID, &key, payer, &base, 1_000_000_000).unwrap();
    let prepared = prepare(
        rpc,
        &settings(series, base, quote, mid),
        key.insecure_clone(),
        Box::new(clock.clone()),
    )
    .unwrap();
    let url = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(Arc::clone(&prepared.service).serve(listener));
        url
    });
    Maker { key, prepared, url }
}

fn profile_address(maker: &Address) -> Address {
    quotebook_sdk::pda::profile(&PROGRAM_ID, maker).0
}

fn mismatch(e: &CliError) -> &'static str {
    match e {
        CliError::Quote(ClientError::Mismatch(m)) => m,
        other => panic!("expected a maker mismatch, got {other:?}"),
    }
}

const NOT_PROFILE: &str = "quote.maker is not the profile's maker";
const NOT_ALLOWED: &str = "quote.maker is not on the allowlist";

#[test]
fn test_discovery_lists_valid_profiles_and_binds_quotes_to_them() {
    let mut s = setup();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let clock = Clock(Arc::new(AtomicI64::new(NOW)));
    let mints = (s.base_mint, s.quote_mint);
    let payer = s.payer.insecure_clone();
    // A quotes around 25, B cheaper around 24, D is a valid maker the taker does not allow.
    let a = start_maker(
        &mut s.rpc,
        &runtime,
        &clock,
        &payer,
        s.series,
        mints,
        s.maker.insecure_clone(),
        "25",
    );
    let b = start_maker(
        &mut s.rpc,
        &runtime,
        &clock,
        &payer,
        s.series,
        mints,
        Keypair::new(),
        "24",
    );
    let d = start_maker(
        &mut s.rpc,
        &runtime,
        &clock,
        &payer,
        s.series,
        mints,
        Keypair::new(),
        "23",
    );
    // Past every cold start.
    let until = [&a, &b, &d]
        .iter()
        .map(|m| m.prepared.cold_start_until.get())
        .max()
        .unwrap();
    s.rpc.set_clock(until);
    clock.0.store(until, Ordering::SeqCst);
    for m in [&a, &b, &d] {
        assert!(refresh(&mut s.rpc, &m.prepared).unwrap());
    }

    // Profiles: A, B and D publish; C's account is forged (a non-zero name tail).
    for (m, endpoint, name) in [
        (&a, "wss://a.example/rfq", "Desk A"),
        (&b, "wss://b.example/rfq", "Desk B"),
        (&d, "wss://d.example/rfq", ""),
    ] {
        admin::set_profile(&mut s.rpc, &PROGRAM_ID, &m.key, &payer, endpoint, name).unwrap();
    }
    let c_key = Keypair::new();
    s.rpc.svm.airdrop(&c_key.pubkey(), 1_000_000_000).unwrap();
    admin::set_profile(
        &mut s.rpc,
        &PROGRAM_ID,
        &c_key,
        &payer,
        "wss://c.example",
        "",
    )
    .unwrap();
    let c_profile = profile_address(&c_key.pubkey());
    let mut forged = s.rpc.svm.get_account(&c_profile).unwrap();
    assert_eq!(forged.data.len(), 208);
    forged.data[207] = b'x'; // the last byte of the name tail
    s.rpc.svm.set_account(c_profile, forged).unwrap();
    // A valid profile's bytes copied to another address do not decode either (wrong PDA).
    let copied = Address::new_from_array([0x77; 32]);
    let a_account = s
        .rpc
        .svm
        .get_account(&profile_address(&a.key.pubkey()))
        .unwrap();
    s.rpc.svm.set_account(copied, a_account).unwrap();

    // Discovery without an allowlist: every valid profile, the invalid ones reported.
    let all = taker::discover(&mut s.rpc, &PROGRAM_ID, None).unwrap();
    let mut makers = vec![a.key.pubkey(), b.key.pubkey(), d.key.pubkey()];
    makers.sort();
    assert_eq!(
        all.listings.iter().map(|l| l.maker).collect::<Vec<_>>(),
        makers
    );
    let mut invalid = vec![c_profile, copied];
    invalid.sort();
    assert_eq!(all.invalid, invalid);
    assert_eq!(all.not_allowed, 0);
    let listed_a = all
        .listings
        .iter()
        .find(|l| l.maker == a.key.pubkey())
        .unwrap();
    assert_eq!(
        (listed_a.endpoint.as_str(), listed_a.name.as_str()),
        ("wss://a.example/rfq", "Desk A")
    );

    // With an allowlist of A and B, D is left out.
    let allow = [a.key.pubkey(), b.key.pubkey()];
    let allowed = taker::discover(&mut s.rpc, &PROGRAM_ID, Some(&allow)).unwrap();
    assert_eq!(allowed.listings.len(), 2);
    assert_eq!(allowed.not_allowed, 1);
    assert!(allowed.listings.iter().all(|l| allow.contains(&l.maker)));

    // The listed endpoints stand for these local services (no TLS in the test).
    let local = |maker: &Address| {
        [&a, &b, &d]
            .into_iter()
            .find(|m| m.key.pubkey() == *maker)
            .unwrap()
            .url
            .clone()
    };
    let taker_key = Keypair::new();
    let size = BaseLots::new(10);
    let request = taker::request(1, &s.series, &taker_key.pubkey(), TakerSide::Buy, size);
    let choose =
        |rpc: &mut LiteRpc, endpoints: Vec<(Address, String)>, allow: Option<&[Address]>| {
            let asked = runtime.block_on(taker::ask_each(&endpoints, &request));
            taker::choose(
                rpc,
                &PROGRAM_ID,
                &s.series,
                asked,
                &taker_key.pubkey(),
                TakerSide::Buy,
                size,
                allow,
            )
        };

    // Honest endpoints: both quotes check, and the cheaper one (B) wins the buy.
    let honest: Vec<_> = allowed
        .listings
        .iter()
        .map(|l| (l.maker, local(&l.maker)))
        .collect();
    let choice = choose(&mut s.rpc, honest, Some(&allow));
    assert!(choice.rejected.is_empty(), "{:?}", choice.rejected);
    let (winner, answer, _) = choice.best.unwrap();
    assert_eq!(winner, b.key.pubkey());
    assert_eq!(answer.signed.quote.terms().maker, b.key.pubkey());

    // A's profile whose endpoint serves B's valid quote: rejected, with and without --allow.
    for allow in [None, Some(&allow[..])] {
        let choice = choose(&mut s.rpc, vec![(a.key.pubkey(), b.url.clone())], allow);
        assert!(choice.best.is_none());
        assert_eq!(choice.rejected.len(), 1);
        assert_eq!(mismatch(&choice.rejected[0].1), NOT_PROFILE);
    }
    // An allowlisted profile (A) whose endpoint serves an unallowlisted maker's (D) quote.
    let choice = choose(
        &mut s.rpc,
        vec![(a.key.pubkey(), d.url.clone())],
        Some(&allow),
    );
    assert_eq!(mismatch(&choice.rejected[0].1), NOT_PROFILE);
    // D's own endpoint, honest, still refused by an allowlist without D: --allow binds the
    // signed quote.maker, not only the profile list.
    let choice = choose(
        &mut s.rpc,
        vec![(d.key.pubkey(), d.url.clone())],
        Some(&allow),
    );
    assert!(choice.best.is_none());
    assert_eq!(mismatch(&choice.rejected[0].1), NOT_ALLOWED);

    // Direct mode (--maker URL --allow): the allowlist applies to quote.maker too.
    let answer = runtime.block_on(taker::ask(&d.url, &request)).unwrap();
    let filter = MakerFilter {
        expected: None,
        allow: Some(&allow),
    };
    let err = taker::check(
        &mut s.rpc,
        &PROGRAM_ID,
        &s.series,
        &answer,
        &taker_key.pubkey(),
        TakerSide::Buy,
        size,
        &filter,
    )
    .unwrap_err();
    assert_eq!(mismatch(&err), NOT_ALLOWED);
    // And the same answer passes with no filter: the rejection is the filter's alone.
    taker::check(
        &mut s.rpc,
        &PROGRAM_ID,
        &s.series,
        &answer,
        &taker_key.pubkey(),
        TakerSide::Buy,
        size,
        &MakerFilter::default(),
    )
    .unwrap();
}

#[test]
fn test_profiles_are_published_and_closed_through_the_cli() {
    let mut s = setup();
    let payer = s.payer.insecure_clone();
    let maker = s.maker.insecure_clone();
    // The text rules are checked before sending.
    assert!(matches!(
        admin::set_profile(
            &mut s.rpc,
            &PROGRAM_ID,
            &maker,
            &payer,
            "ws://plain.example",
            ""
        ),
        Err(CliError::Args(_))
    ));
    admin::set_profile(
        &mut s.rpc,
        &PROGRAM_ID,
        &maker,
        &payer,
        "wss://m.example",
        "M",
    )
    .unwrap();
    assert_eq!(
        taker::discover(&mut s.rpc, &PROGRAM_ID, None)
            .unwrap()
            .listings
            .len(),
        1
    );
    let dest = Keypair::new().pubkey();
    admin::close_profile(&mut s.rpc, &PROGRAM_ID, &maker, &payer, &dest).unwrap();
    assert!(taker::discover(&mut s.rpc, &PROGRAM_ID, None)
        .unwrap()
        .listings
        .is_empty());
    assert!(s.rpc.svm.get_account(&dest).unwrap().lamports > 0);
}
