//! The maker's token accounts come with the quote answer, unsigned. A maker service that sends
//! the wrong ones must be caught by `taker::check` before anything is sent, and the program
//! would have rejected the settle anyway.

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

use common::{setup, Setup, NOW, PROGRAM_ID};
use quotebook_cli::maker::{prepare, refresh, MakerSettings};
use quotebook_cli::{taker, CliError};
use quotebook_core::{BaseLots, QuotebookError, Seconds, UnixTimestamp, MAX_CLOCK_SKEW};
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

/// Runs a maker service that sends `maker_base` / `maker_quote` with its quotes, past its cold
/// start, asks it for a 40-lot buy, and returns the taker's check and, if the check is
/// skipped, what the program says.
fn ask_with(
    s: &mut Setup,
    maker_base: Address,
    maker_quote: Address,
) -> (Result<(), CliError>, Result<(), CliError>) {
    let d = |v: &str| Decimal::from_str(v).unwrap();
    let settings = MakerSettings {
        program_id: PROGRAM_ID,
        series: s.series,
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
    let start = s.rpc.svm.get_sysvar::<solana_clock::Clock>().unix_timestamp;
    let clock = Clock(Arc::new(AtomicI64::new(start)));
    let prepared = prepare(
        &mut s.rpc,
        &settings,
        s.maker.insecure_clone(),
        Box::new(clock.clone()),
    )
    .unwrap();
    let until = start + 60 + MAX_CLOCK_SKEW.get();
    s.rpc.set_clock(until);
    clock.0.store(until, Ordering::SeqCst);
    assert!(refresh(&mut s.rpc, &prepared).unwrap());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let url = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(Arc::clone(&prepared.service).serve(listener));
        url
    });
    let taker_key = Keypair::new();
    s.rpc
        .svm
        .airdrop(&taker_key.pubkey(), 1_000_000_000)
        .unwrap();
    let taker_base = s
        .rpc
        .token_account(&s.base_mint, &taker_key.pubkey(), 1_000_000_000);
    let taker_quote = s
        .rpc
        .token_account(&s.quote_mint, &taker_key.pubkey(), 1_000_000_000);
    let size = BaseLots::new(40);
    let request = taker::request(1, &s.series, &taker_key.pubkey(), TakerSide::Buy, size);
    let answer = runtime.block_on(taker::ask(&url, &request)).unwrap();
    let check = taker::check(
        &mut s.rpc,
        &PROGRAM_ID,
        &s.series,
        &answer,
        &taker_key.pubkey(),
        TakerSide::Buy,
        size,
    )
    .map(|_| ());
    let program = taker::settle(
        &mut s.rpc,
        &PROGRAM_ID,
        &answer,
        &taker_key,
        &taker_key,
        &taker_base,
        &taker_quote,
        size,
    )
    .map(|_| ());
    (check, program)
}

fn account_error(r: &Result<(), CliError>, why: &str) {
    match r {
        Err(CliError::Account(m)) => assert!(m.contains(why), "{m}"),
        other => panic!("expected an account error containing {why:?}, got {other:?}"),
    }
}

fn program_error(r: &Result<(), CliError>, e: QuotebookError) {
    let m = r.as_ref().unwrap_err().to_string();
    assert!(m.contains(&format!("Custom({})", e.code())), "{m}");
}

#[test]
fn test_correct_accounts_pass_and_settle() {
    let mut s = setup();
    let (base, quote) = (s.maker_base, s.maker_quote);
    let (check, program) = ask_with(&mut s, base, quote);
    check.unwrap();
    program.unwrap();
}

/// Another holder's account of the right mint (here a third party's).
#[test]
fn test_substituted_owner_is_caught() {
    let mut s = setup();
    let base_mint = s.base_mint;
    let stranger = s
        .rpc
        .token_account(&base_mint, &Keypair::new().pubkey(), 1_000_000_000);
    let quote = s.maker_quote;
    let (check, program) = ask_with(&mut s, stranger, quote);
    account_error(&check, "not owned by the quote's maker");
    program_error(&program, QuotebookError::InvalidVaultOrTokenAccount);
}

/// The maker's own two accounts, swapped.
#[test]
fn test_swapped_accounts_are_caught() {
    let mut s = setup();
    let (base, quote) = (s.maker_base, s.maker_quote);
    let (check, program) = ask_with(&mut s, quote, base);
    account_error(&check, "holds another mint");
    program_error(&program, QuotebookError::InvalidVaultOrTokenAccount);
}

/// The maker's own base account, but one its delegate is not approved on: the program
/// accepts every account check and the token program fails the transfer.
#[test]
fn test_unapproved_paying_account_is_caught() {
    let mut s = setup();
    let base_mint = s.base_mint;
    let maker = s.maker.pubkey();
    let unapproved = s.rpc.token_account(&base_mint, &maker, 1_000_000_000);
    let quote = s.maker_quote;
    let (check, program) = ask_with(&mut s, unapproved, quote);
    account_error(&check, "delegate PDA is not approved");
    assert!(program.is_err());
}

/// An account that does not exist.
#[test]
fn test_missing_account_is_caught() {
    let mut s = setup();
    let base = s.maker_base;
    let (check, program) = ask_with(&mut s, base, Keypair::new().pubkey());
    account_error(&check, "does not exist");
    assert!(program.is_err());
}

#[test]
fn test_start_clock_is_now() {
    // The fixture starts at NOW; the cold start is measured from the chain's clock.
    let s = setup();
    assert_eq!(
        s.rpc.svm.get_sysvar::<solana_clock::Clock>().unix_timestamp,
        NOW
    );
}
