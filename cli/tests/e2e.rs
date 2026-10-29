//! The CLI's admin, maker and taker logic end to end over the `Rpc` trait, against the program
//! in LiteSVM: no network. The binaries only parse arguments around these functions.

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

use common::{LiteRpc, GENESIS, NOW, PROGRAM_ID};
use quotebook_cli::maker::{prepare, refresh, MakerSettings};
use quotebook_cli::{admin, market, taker, CliError};
use quotebook_core::instruction::CreateSeriesArgs;
use quotebook_core::{BaseLots, QuotebookError, Seconds, UnixTimestamp, MAX_CLOCK_SKEW};
use quotebook_maker::client::ClientError;
use quotebook_maker::pricing::PricingParams;
use quotebook_maker::protocol::{RejectReason, TakerSide};
use quotebook_maker::server::TimeSource;
use rust_decimal::Decimal;
use solana_keypair::Keypair;
use solana_signer::Signer;

#[derive(Clone)]
struct Clock(Arc<AtomicI64>);

impl TimeSource for Clock {
    fn now(&self) -> UnixTimestamp {
        UnixTimestamp::new(self.0.load(Ordering::SeqCst))
    }
}

#[test]
fn test_cli_end_to_end_over_rpc() {
    let admin_key = Keypair::new();
    let payer = Keypair::new();
    let mut rpc = LiteRpc::new(&admin_key.pubkey(), &payer.pubkey());

    // Admin: init with the genesis-hash deployment id, a series, then a maker.
    let (deployment, _) = admin::init_config(&mut rpc, &PROGRAM_ID, &admin_key, &payer).unwrap();
    assert_eq!(
        deployment,
        quotebook_sdk::deployment_id_from_genesis_hash(&GENESIS)
    );
    assert_eq!(
        market::config(&mut rpc, &PROGRAM_ID).unwrap().admin,
        admin_key.pubkey()
    );
    // A second init and a non-authority init fail.
    assert!(admin::init_config(&mut rpc, &PROGRAM_ID, &admin_key, &payer).is_err());
    let base_mint = rpc.mint();
    let quote_mint = rpc.mint();
    let args = CreateSeriesArgs {
        base_lot_size: 10_000,
        quote_lot_size: 100,
        max_quote_ttl: 60,
        min_size: 1,
    };
    let (series, _) = admin::create_series(
        &mut rpc,
        &PROGRAM_ID,
        &admin_key,
        &payer,
        &base_mint,
        &quote_mint,
        &args,
    )
    .unwrap();
    let m = market::market(&mut rpc, &PROGRAM_ID, &series).unwrap();
    assert_eq!(
        (m.base_decimals, m.quote_decimals, m.base_lot_size),
        (6, 6, 10_000)
    );

    let maker_key = Keypair::new();
    rpc.svm.airdrop(&maker_key.pubkey(), 1_000_000_000).unwrap();
    admin::init_maker(&mut rpc, &PROGRAM_ID, &maker_key, &payer).unwrap();
    let maker_base = rpc.token_account(&base_mint, &maker_key.pubkey(), 1_000_000_000);
    let maker_quote = rpc.token_account(&quote_mint, &maker_key.pubkey(), 1_000_000_000);
    admin::approve(
        &mut rpc,
        &PROGRAM_ID,
        &maker_key,
        &payer,
        &maker_base,
        1_000_000_000,
    )
    .unwrap();

    // Maker: prepare resumes fail-closed.
    let d = |v: &str| Decimal::from_str(v).unwrap();
    let settings = MakerSettings {
        program_id: PROGRAM_ID,
        series,
        maker_base,
        maker_quote,
        mid: d("25"),
        pricing: PricingParams {
            risk_aversion: d("0.1"),
            volatility: d("0.6"),
            time_to_terminal_ms: 3_600_000,
            order_intensity: d("1.5"),
            max_deviation: d("0.05"),
        },
        ttl: Seconds::new(30),
        max_size: BaseLots::new(1_000),
        max_ttl_any_series: Seconds::new(60),
    };
    let clock = Clock(Arc::new(AtomicI64::new(NOW)));
    let prepared = prepare(
        &mut rpc,
        &settings,
        maker_key.insecure_clone(),
        Box::new(clock.clone()),
    )
    .unwrap();
    assert_eq!(
        prepared.cold_start_until.get(),
        NOW + 60 + MAX_CLOCK_SKEW.get()
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let url = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(Arc::clone(&prepared.service).serve(listener));
        url
    });

    // Taker: during the cold start the maker refuses.
    let taker_key = Keypair::new();
    rpc.svm.airdrop(&taker_key.pubkey(), 1_000_000_000).unwrap();
    let taker_base = rpc.token_account(&base_mint, &taker_key.pubkey(), 1_000_000_000);
    let taker_quote = rpc.token_account(&quote_mint, &taker_key.pubkey(), 1_000_000_000);
    let size = BaseLots::new(40);
    let request = taker::request(1, &series, &taker_key.pubkey(), TakerSide::Buy, size);
    match runtime.block_on(taker::ask(&url, &request)) {
        Err(CliError::Quote(ClientError::Rejected { reason, .. })) => {
            assert_eq!(reason, RejectReason::RateLimited)
        }
        other => panic!("expected a cold-start refusal, got {other:?}"),
    }
    // Refresh does nothing before the cold start ends, then succeeds.
    assert!(!refresh(&mut rpc, &prepared).unwrap());
    let until = prepared.cold_start_until.get();
    rpc.set_clock(until);
    clock.0.store(until, Ordering::SeqCst);
    assert!(refresh(&mut rpc, &prepared).unwrap());

    // Taker: request, check against chain, settle.
    let answer = runtime.block_on(taker::ask(&url, &request)).unwrap();
    assert_eq!(
        (answer.maker_base, answer.maker_quote),
        (maker_base, maker_quote)
    );
    taker::check(
        &mut rpc,
        &PROGRAM_ID,
        &series,
        &answer,
        &taker_key.pubkey(),
        TakerSide::Buy,
        size,
    )
    .unwrap();
    // The same quote checked for a bigger size is refused before sending.
    let err = taker::check(
        &mut rpc,
        &PROGRAM_ID,
        &series,
        &answer,
        &taker_key.pubkey(),
        TakerSide::Buy,
        BaseLots::new(41),
    );
    assert!(matches!(
        err,
        Err(CliError::Quote(ClientError::WouldFail(
            QuotebookError::FillSizeInvalid
        )))
    ));
    let (b0, q0) = (rpc.amount(&taker_base), rpc.amount(&taker_quote));
    taker::settle(
        &mut rpc,
        &PROGRAM_ID,
        &answer,
        &taker_key,
        &taker_key,
        &taker_base,
        &taker_quote,
        size,
    )
    .unwrap();
    let price = answer.signed.quote.terms().price.get();
    assert_eq!(rpc.amount(&taker_base), b0 + 400_000);
    assert_eq!(rpc.amount(&taker_quote), q0 - price * 40 * 100);
    assert_eq!(
        market::maker_state(&mut rpc, &PROGRAM_ID, &maker_key.pubkey())
            .unwrap()
            .fills,
        1
    );
    // Replaying the same answer fails on chain.
    assert!(taker::settle(
        &mut rpc,
        &PROGRAM_ID,
        &answer,
        &taker_key,
        &taker_key,
        &taker_base,
        &taker_quote,
        size
    )
    .is_err());

    // The maker cancels everything below nonce 100.
    admin::cancel_all(
        &mut rpc,
        &PROGRAM_ID,
        &maker_key,
        &payer,
        quotebook_core::Nonce::new(100),
    )
    .unwrap();
    admin::cancel_nonce(
        &mut rpc,
        &PROGRAM_ID,
        &maker_key,
        &payer,
        quotebook_core::Nonce::new(150),
    )
    .unwrap();
    let window = market::maker_state(&mut rpc, &PROGRAM_ID, &maker_key.pubkey())
        .unwrap()
        .nonces;
    assert_eq!(window.window_base(), quotebook_core::Nonce::new(100));
    assert_eq!(window.highest_used(), Some(quotebook_core::Nonce::new(150)));

    // Admin: pause, then hand over the admin role.
    admin::set_paused(&mut rpc, &PROGRAM_ID, &admin_key, &payer, &series, true).unwrap();
    assert_eq!(
        market::series(&mut rpc, &PROGRAM_ID, &series)
            .unwrap()
            .paused,
        1
    );
    let new_admin = Keypair::new();
    admin::set_admin(
        &mut rpc,
        &PROGRAM_ID,
        &admin_key,
        &payer,
        &new_admin.pubkey(),
    )
    .unwrap();
    assert_eq!(
        market::config(&mut rpc, &PROGRAM_ID).unwrap().admin,
        new_admin.pubkey()
    );
}

#[test]
fn test_cli_init_config_requires_the_upgrade_authority() {
    let authority = Keypair::new();
    let payer = Keypair::new();
    let mut rpc = LiteRpc::new(&authority.pubkey(), &payer.pubkey());
    let err = admin::init_config(&mut rpc, &PROGRAM_ID, &Keypair::new(), &payer).unwrap_err();
    assert!(
        err.to_string()
            .contains(&format!("Custom({})", QuotebookError::NotAuthority.code())),
        "{err}"
    );
}

#[test]
fn test_cli_keypair_errors_name_the_path_only() {
    let err = quotebook_cli::keys::load("/nonexistent/maker.json").unwrap_err();
    assert_eq!(
        err.to_string(),
        "cannot read keypair file /nonexistent/maker.json"
    );
}

/// A real transport error from `solana-rpc-client` against a loopback port that refuses the
/// connection: neither the error nor `Debug` may show the URL or its API key.
#[test]
fn test_cli_rpc_errors_never_show_the_url() {
    use quotebook_cli::rpc::{Cluster, Rpc};
    // Bind and drop to get a port nobody listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("http://127.0.0.1:{port}/?api-key=SECRET-API-KEY-123");
    let mut cluster = Cluster::new(&url);
    let debug = format!("{cluster:?}");
    assert!(
        !debug.contains("SECRET") && !debug.contains("127.0.0.1"),
        "{debug}"
    );
    let err = cluster.genesis_hash().unwrap_err().to_string();
    assert!(!err.contains("SECRET"), "{err}");
    assert!(!err.contains(&format!("127.0.0.1:{port}")), "{err}");
    let err = cluster
        .account(&solana_sdk_ids::system_program::ID)
        .unwrap_err()
        .to_string();
    assert!(!err.contains("SECRET"), "{err}");
}
