//! Two series under one maker key, on the real program in LiteSVM: no nonce is issued twice, a
//! fill is reconciled from the chain, and `cancel_all` kills a signed quote.

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

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Mutex;

use common::{Chain, Maker, Taker, NOW, PROGRAM_ID};
use quotebook_core::{BaseLots, Nonce, QuotebookError, Seconds, UnixTimestamp};
use quotebook_maker::client::SignedQuote;
use quotebook_maker::keys::MakerKey;
use quotebook_maker::market::MarketInfo;
use quotebook_maker::pricing::{ModelUnits, PricingParams};
use quotebook_maker::protocol::{from_hex, QuoteRequest, ServerMessage, TakerSide};
use quotebook_maker::risk::{
    settle_count, ChainError, ChainSnapshot, ChainView, MarketLimits, RiskConfig, TokenAccountState,
};
use quotebook_maker::server::{
    FixedInventory, FixedMid, FixedTime, MakerService, MarketEntry, ServiceConfig, Start,
};
use quotebook_sdk::core::instruction::CreateSeriesArgs;
use quotebook_sdk::core::Quote;
use quotebook_sdk::{decode, ix, pda};
use rust_decimal::Decimal;
use solana_address::Address;
use solana_signer::Signer;

struct Reads(Mutex<ChainSnapshot>);

impl ChainView for Reads {
    fn snapshot(&self) -> Result<ChainSnapshot, ChainError> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn fill_times_since(
        &self,
        _since: UnixTimestamp,
    ) -> Result<Vec<Option<UnixTimestamp>>, ChainError> {
        Ok(Vec::new())
    }
}

/// The chain as a finalized read would see it, at slot `slot`.
fn read(chain: &Chain, maker: &Maker, slot: u64, clock: i64) -> ChainSnapshot {
    let account = |a: &Address| {
        let amount = chain.amount(a);
        TokenAccountState {
            amount,
            delegated_amount: amount,
        }
    };
    ChainSnapshot {
        slot,
        clock: UnixTimestamp::new(clock),
        window: chain.window(&maker.key.pubkey()),
        accounts: vec![
            (maker.base, account(&maker.base)),
            (maker.quote, account(&maker.quote)),
        ],
    }
}

fn second_series(chain: &mut Chain) -> MarketInfo {
    let args = CreateSeriesArgs {
        base_lot_size: 10_000,
        quote_lot_size: 100,
        max_quote_ttl: 60,
        min_size: 1,
        fee_bps: None,
    };
    let admin = chain.admin.insecure_clone();
    let payer = chain.payer.pubkey();
    let (base, quote) = (chain.base_mint, chain.quote_mint);
    chain
        .send(
            &[ix::create_series(
                &PROGRAM_ID,
                &admin.pubkey(),
                &payer,
                &base,
                &quote,
                1,
                &args,
            )],
            &[&admin],
        )
        .unwrap();
    let series = pda::series(&PROGRAM_ID, &base, &quote, 1).0;
    let s = chain.svm.get_account(&series).unwrap();
    decode::series(&PROGRAM_ID, &s.owner, &s.data).unwrap();
    MarketInfo {
        series,
        ..chain.market
    }
}

fn entry(market: MarketInfo, maker: &Maker) -> MarketEntry {
    let d = |v: &str| Decimal::from_str(v).unwrap();
    MarketEntry {
        config: ServiceConfig {
            market,
            pricing: PricingParams {
                risk_aversion: d("0.1"),
                volatility: d("0.6"),
                time_to_terminal_ms: 3_600_000,
                order_intensity: d("1.5"),
                max_deviation: d("0.05"),
                min_half_spread_bps: 5,
                max_half_spread_bps: 100,
                max_skew_bps: 100,
                model_units: ModelUnits::Relative,
            },
            ttl: Seconds::new(30),
            max_size: BaseLots::new(1_000),
            maker_base: maker.base,
            maker_quote: maker.quote,
        },
        limits: MarketLimits {
            max_notional: u64::MAX,
            max_long: u64::MAX,
            max_short: u64::MAX,
        },
        feed: Box::new(FixedMid(d("25"))),
        inventory: Box::new(FixedInventory(Decimal::ZERO)),
    }
}

fn setup() -> (Chain, Maker, Taker, MarketInfo, MakerService) {
    let mut chain = Chain::new();
    let second = second_series(&mut chain);
    let maker = chain.maker();
    let taker = chain.taker();
    let dir = std::env::temp_dir().join(format!("qb-multi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let service = MakerService::multi(
        MakerKey::new(maker.key.insecure_clone()),
        vec![entry(chain.market, &maker), entry(second, &maker)],
        Start::Fresh(chain.window(&maker.key.pubkey()).window_base()),
        RiskConfig {
            max_open_quotes: 100,
            max_fills_per_hour: 100,
            max_snapshot_age: Seconds::new(60),
            allowed_takers: vec![taker.key.pubkey()],
            halt_file: dir.join("no-such-halt-file"),
        },
        Box::new(FixedTime(UnixTimestamp::new(NOW))),
    )
    .unwrap();
    (chain, maker, taker, second, service)
}

fn quote(service: &MakerService, series: &Address, taker: &Taker) -> SignedQuote {
    let answer = service.respond(&QuoteRequest {
        request_id: 1,
        series: series.to_string(),
        taker: taker.key.pubkey().to_string(),
        taker_side: TakerSide::Buy,
        size_lots: 1,
    });
    match answer {
        ServerMessage::Quote(m) => SignedQuote {
            quote: Quote::decode(&from_hex(&m.quote).unwrap()).unwrap(),
            signature: from_hex(&m.signature).unwrap().try_into().unwrap(),
        },
        other => panic!("not a quote: {other:?}"),
    }
}

fn open_quotes(service: &MakerService) -> u32 {
    match service.status(1) {
        ServerMessage::Status(m) => m.open_quotes,
        other => panic!("not a status: {other:?}"),
    }
}

#[test]
fn test_two_series_share_nonces_and_a_fill_reconciles() {
    let (mut chain, maker, taker, second, service) = setup();
    let reads = Reads(Mutex::new(read(&chain, &maker, 1, NOW)));
    service.reconcile(&reads).unwrap();

    let mut nonces = HashSet::new();
    let mut quotes = Vec::new();
    for i in 0..6 {
        let series = if i % 2 == 0 {
            chain.market.series
        } else {
            second.series
        };
        let q = quote(&service, &series, &taker);
        assert!(nonces.insert(q.quote.terms().nonce.get()), "nonce reused");
        quotes.push(q);
    }
    assert_eq!(open_quotes(&service), 6);

    // A fill on the second series, through the real program.
    let filled = quotes.remove(1);
    let balance = chain.amount(&maker.base);
    chain
        .settle(&filled, &maker, &taker, BaseLots::new(1))
        .unwrap();
    assert_eq!(chain.amount(&maker.base), balance - 10_000);

    // Seen on a later snapshot, it releases only that reservation.
    *reads.0.lock().unwrap() = read(&chain, &maker, 2, NOW + 1);
    service.reconcile(&reads).unwrap();
    assert_eq!(open_quotes(&service), 5);

    // The real instructions of a fill count as one.
    let pair = chain.settle_pair(&quotes[0], &maker, &taker, BaseLots::new(1));
    assert_eq!(
        settle_count(
            &PROGRAM_ID,
            pair.iter().map(|i| (&i.program_id, i.data.as_slice())),
        ),
        1
    );
}

#[test]
fn test_cancel_all_kills_a_signed_quote_and_releases_it() {
    let (mut chain, maker, taker, _second, service) = setup();
    let reads = Reads(Mutex::new(read(&chain, &maker, 1, NOW)));
    service.reconcile(&reads).unwrap();

    let q = quote(&service, &chain.market.series, &taker);
    assert_eq!(open_quotes(&service), 1);

    let next = match service.status(1) {
        ServerMessage::Status(m) => m.next_nonce,
        other => panic!("not a status: {other:?}"),
    };
    let key = maker.key.insecure_clone();
    chain
        .send(
            &[ix::cancel_all(&PROGRAM_ID, &key.pubkey(), Nonce::new(next))],
            &[&key],
        )
        .unwrap();
    assert_eq!(
        chain
            .window(&maker.key.pubkey())
            .check(q.quote.terms().nonce),
        Err(QuotebookError::NonceCancelled)
    );
    assert!(chain.settle(&q, &maker, &taker, BaseLots::new(1)).is_err());

    *reads.0.lock().unwrap() = read(&chain, &maker, 2, NOW + 1);
    service.reconcile(&reads).unwrap();
    assert_eq!(open_quotes(&service), 0);
}
