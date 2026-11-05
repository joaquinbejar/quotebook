//! D2's first step: the published 0.1.0 layouts decode live devnet accounts (`fixtures/README.md`).

#![allow(
    clippy::disallowed_macros,
    clippy::indexing_slicing,
    clippy::unwrap_used,
    missing_docs
)]

use std::sync::Arc;

use metalbook_core::{OptionKind, SeriesState, Underlying};
use metalbook_sdk::{decode, pda};
use pyth_lite::{PriceUpdate, VerificationLevel, RECEIVER_PROGRAM_ID};
use quotebook_cli::rpc::Rpc;
use quotebook_cli::CliError;
use quotebook_core::UnixTimestamp;
use quotebook_demo::config::DemoConfig;
use quotebook_demo::feed::Board;
use quotebook_demo::reader::poll_once;
use quotebook_demo::series::{check_pair, classify, SeriesSkip};
use quotebook_maker::protocol::from_hex;
use quotebook_maker::server::{FixedTime, PriceFeed};
use rust_decimal::Decimal;
use serde_json::Value;
use solana_account::Account;
use solana_address::Address;

const CONFIG: &str = include_str!("../config/devnet.json");

fn account(name: &str) -> (Address, Address, Vec<u8>) {
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/devnet_accounts.json")).unwrap();
    let a = &fixtures[name];
    (
        a["address"].as_str().unwrap().parse().unwrap(),
        a["owner"].as_str().unwrap().parse().unwrap(),
        from_hex(a["data_hex"].as_str().unwrap()).unwrap(),
    )
}

#[test]
fn the_published_layout_decodes_the_live_series() {
    let config = DemoConfig::from_json(CONFIG).unwrap();
    let (address, owner, data) = account("series_xag_50");
    assert_eq!(data.len(), 504);
    let series = decode::series(&config.metalbook_program, &owner, &data).unwrap();
    assert_eq!(
        SeriesState::try_from(series.state).unwrap(),
        SeriesState::Active
    );
    let terms = series.terms().unwrap();
    assert_eq!(terms.key().underlying(), Underlying::Xag);
    assert_eq!(terms.key().kind(), OptionKind::Call);
    assert_eq!(terms.key().strike().get(), 50_000_000);
    assert_eq!(terms.cap().get(), 55_000_000);
    assert_eq!(terms.key().expiry().get(), 1_794_603_600);
    let (long_mint, _) = pda::long_mint(&config.metalbook_program, &address);
    assert_eq!(Address::new_from_array(series.long_mint), long_mint);
}

#[test]
fn the_devnet_config_accepts_the_live_series() {
    let config = DemoConfig::from_json(CONFIG).unwrap();
    let (_, owner, data) = account("series_xag_50");
    let pair = config.pairs[0];
    let listed = classify(&config, &pair, &owner, &data).unwrap();
    assert_eq!(listed.metalbook_series, pair.metalbook_series);
    assert_eq!(listed.quotebook_series, pair.quotebook_series);
    assert!(listed.window_s > 0);
}

/// `Series.state` is the fifth byte: tag, version, bump, long mint bump, state.
const STATE_OFFSET: usize = 4;

#[test]
fn a_series_failing_a_check_is_skipped() {
    let config = DemoConfig::from_json(CONFIG).unwrap();
    let (_, owner, data) = account("series_xag_50");
    let pair = config.pairs[0];

    let mut settled = data.clone();
    settled[STATE_OFFSET] = 2;
    assert_eq!(
        classify(&config, &pair, &owner, &settled),
        Err(SeriesSkip::NotActive)
    );

    let wrong_owner = Address::new_from_array([3; 32]);
    assert!(matches!(
        classify(&config, &pair, &wrong_owner, &data),
        Err(SeriesSkip::Decode(_))
    ));

    let mut other = pair;
    other.metalbook_series = Address::new_from_array([4; 32]);
    assert_eq!(
        classify(&config, &other, &owner, &data),
        Err(SeriesSkip::WrongLongMint)
    );

    let mut usdc = config.clone();
    usdc.usdc_mint = Address::new_from_array([5; 32]);
    assert_eq!(
        classify(&usdc, &pair, &owner, &data),
        Err(SeriesSkip::WrongCollateral)
    );

    let mut feed = config.clone();
    feed.underlyings
        .iter_mut()
        .for_each(|u| u.feed_id = [6; 32]);
    assert_eq!(
        classify(&feed, &pair, &owner, &data),
        Err(SeriesSkip::WrongFeed)
    );

    let mut none = config.clone();
    none.underlyings.clear();
    assert_eq!(
        classify(&none, &pair, &owner, &data),
        Err(SeriesSkip::NoUnderlying)
    );
}

#[test]
fn a_pair_must_trade_the_long_mint_against_usdc() {
    let config = DemoConfig::from_json(CONFIG).unwrap();
    let long = Address::new_from_array([7; 32]);
    let other = Address::new_from_array([8; 32]);
    let usdc = config.usdc_mint;
    assert_eq!(check_pair(&config, &long, &long, &usdc), Ok(()));
    assert_eq!(
        check_pair(&config, &long, &other, &usdc),
        Err(SeriesSkip::WrongPair)
    );
    assert_eq!(
        check_pair(&config, &long, &long, &other),
        Err(SeriesSkip::WrongPair)
    );
}

#[test]
fn the_published_layout_decodes_the_live_feeds() {
    let config = DemoConfig::from_json(CONFIG).unwrap();
    for (name, underlying, exponent) in [
        ("feed_xau", Underlying::Xau, -3),
        ("feed_xag", Underlying::Xag, -5),
    ] {
        let (address, owner, data) = account(name);
        let update = PriceUpdate::load(&owner.to_bytes(), &data, &RECEIVER_PROGRAM_ID).unwrap();
        assert_eq!(update.verification_level(), VerificationLevel::Full);
        let m = update.message();
        let metal = config.underlying(underlying).unwrap();
        assert_eq!(metal.feed_account, address);
        assert_eq!(m.feed_id().to_bytes(), metal.feed_id);
        assert_eq!(m.exponent(), exponent);
        assert!(m.price() > 0);
    }
}

/// The accounts of `fixtures/devnet_accounts.json`, over the `Rpc` trait.
struct FixtureRpc;

impl Rpc for FixtureRpc {
    fn account(&mut self, address: &Address) -> Result<Option<Account>, CliError> {
        for name in [
            "series_xag_50",
            "quotebook_series_1",
            "feed_xau",
            "feed_xag",
        ] {
            let (a, owner, data) = account(name);
            if a == *address {
                return Ok(Some(Account {
                    lamports: 1,
                    data,
                    owner,
                    executable: false,
                    rent_epoch: 0,
                }));
            }
        }
        Ok(None)
    }

    fn latest_blockhash(&mut self) -> Result<solana_hash::Hash, CliError> {
        Err(CliError::Rpc("unused".into()))
    }

    fn send(
        &mut self,
        _: &solana_transaction::Transaction,
    ) -> Result<solana_signature::Signature, CliError> {
        Err(CliError::Rpc("unused".into()))
    }

    fn genesis_hash(&mut self) -> Result<solana_hash::Hash, CliError> {
        Err(CliError::Rpc("unused".into()))
    }
}

fn publish_time(name: &str) -> i64 {
    let (_, owner, data) = account(name);
    PriceUpdate::load(&owner.to_bytes(), &data, &RECEIVER_PROGRAM_ID)
        .unwrap()
        .message()
        .publish_time()
        .get()
}

#[test]
fn one_round_over_the_live_accounts_gives_a_mid() {
    let config = DemoConfig::from_json(CONFIG).unwrap();
    let now = publish_time("feed_xag").max(publish_time("feed_xau"));
    let board = Arc::new(Board::new(
        &config,
        Box::new(FixedTime(UnixTimestamp::new(now))),
    ));
    let pair = config.pairs[0];
    assert_eq!(board.feed(pair.metalbook_series).mid(), None);

    poll_once(&mut FixtureRpc, &config, &board);

    let mid = board.feed(pair.metalbook_series).mid().unwrap();
    // A 1 oz capped call of strike 50, cap 55.
    assert!(mid > Decimal::ZERO && mid < Decimal::new(5, 0), "{mid}");
}
