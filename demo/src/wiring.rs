//! D3's wiring, end to end without a network: the service [`crate::host::service`] builds, over
//! [`crate::chain::RpcChainView`] on a fake RPC and D2's [`crate::feed::Board`] on a fake feed.
//! It checks that the cold start is judged on the finalized Clock, that the limits read the
//! snapshot's balances and allowances, that a used nonce releases its reservation, and that
//! `Status` reports each spot's age.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use metalbook_core::Underlying;
use pyth_lite::RECEIVER_PROGRAM_ID;
use quotebook_core::{BaseLots, Nonce, NonceWindow, Seconds, UnixTimestamp};
use quotebook_maker::market::MarketInfo;
use quotebook_maker::protocol::{
    from_hex, QuoteRequest, RejectReason, ServerMessage, StatusMessage, TakerSide,
};
use quotebook_maker::server::{MakerService, Start, TimeSource};
use rust_decimal::Decimal;
use solana_address::Address;
use solana_keypair::Keypair;

use crate::chain::tests::{
    account, approve, chain, clock, delegate, maker_state, token_account, FakeRpc, BASE,
    BASE_ACCOUNT, MAKER, PROGRAM, USDC, USDC_ACCOUNT,
};
use crate::chain::{RpcChainView, CLOCK_SYSVAR, SYSVAR_PROGRAM, TOKEN_PROGRAM};
use crate::config::{DemoConfig, SeriesPair, UnderlyingConfig};
use crate::feed::Board;
use crate::host::{self, HostSettings, Plan, PlannedMarket};
use crate::test_support::{capped_call, update, Update, FEED};

/// Thursday 2026-11-05 18:13:20Z: the market is open.
const NOW: i64 = 1_793_900_000;
const EXPIRY: i64 = 1_794_603_600;
const TAKER: &str = "3ZBj1qBpdoH5NrMTwAHq2fY6Hbo5M1GURxaP9y8bsqLs";
const SETTINGS: &str = include_str!("../config/devnet-maker.json");

#[derive(Clone)]
struct TestClock(Arc<AtomicI64>);

impl TimeSource for TestClock {
    fn now(&self) -> UnixTimestamp {
        UnixTimestamp::new(self.0.load(Ordering::SeqCst))
    }
}

struct Rig {
    time: Arc<AtomicI64>,
    board: Arc<Board>,
    service: MakerService,
    quotebook_series: Address,
    metalbook_series: Address,
}

fn demo() -> DemoConfig {
    let listed = capped_call(1, 52, EXPIRY);
    DemoConfig {
        metalbook_program: Address::new_from_array([90; 32]),
        quotebook_program: PROGRAM,
        usdc_mint: USDC,
        underlyings: vec![UnderlyingConfig {
            underlying: Underlying::Xag,
            feed_account: Address::new_from_array([93; 32]),
            feed_id: FEED,
            vol: Decimal::new(30, 2),
            rate: Decimal::new(4, 2),
            carry: Decimal::ZERO,
        }],
        pairs: vec![SeriesPair {
            metalbook_series: listed.metalbook_series,
            quotebook_series: listed.quotebook_series,
        }],
    }
}

/// The service over `view`, resumed at `NOW` with the window of `view`'s first snapshot.
fn rig(view: &RpcChainView<FakeRpc>) -> Rig {
    let listed = capped_call(1, 52, EXPIRY);
    let time = Arc::new(AtomicI64::new(NOW));
    let clock = TestClock(Arc::clone(&time));
    let board = Arc::new(Board::new(&demo(), Box::new(clock.clone())));
    board.set_series(&[listed]);
    let plan = Plan {
        maker: MAKER,
        markets: vec![PlannedMarket {
            pair: demo().pairs[0],
            market: MarketInfo {
                program_id: PROGRAM,
                deployment: [3; 16],
                series: listed.quotebook_series,
                base_decimals: 0,
                quote_decimals: 6,
                base_lot_size: 1,
                quote_lot_size: 10_000,
                max_quote_ttl: Seconds::new(60),
                min_size: BaseLots::new(1),
            },
            base_mint: BASE,
            maker_base: BASE_ACCOUNT,
        }],
        maker_quote: USDC_ACCOUNT,
    };
    use quotebook_maker::risk::ChainView;
    let first = view.snapshot().unwrap();
    let service = host::service(
        Keypair::new(),
        &plan,
        &HostSettings::from_json(SETTINGS).unwrap(),
        &board,
        &view.balances(),
        Start::Resume {
            window: Box::new(first.window),
            started_at: UnixTimestamp::new(NOW),
        },
        PathBuf::from("/nonexistent/quotebook-demo-halt"),
        Box::new(clock),
    )
    .unwrap();
    Rig {
        time,
        board,
        service,
        quotebook_series: listed.quotebook_series,
        metalbook_series: listed.metalbook_series,
    }
}

impl Rig {
    /// Moves every clock (the service's, the board's and the chain's) to `t`, with a fresh spot.
    fn at(&self, rpc: &mut FakeRpc, t: i64) {
        self.time.store(t, Ordering::SeqCst);
        rpc.slot += 1;
        rpc.accounts
            .insert(CLOCK_SYSVAR, account(SYSVAR_PROGRAM, clock(rpc.slot, t)));
        self.board
            .ingest(
                Underlying::Xag,
                &RECEIVER_PROGRAM_ID,
                &update(Update::at(t)),
            )
            .unwrap()
            .unwrap();
    }

    fn ask(&self, id: u64, side: TakerSide, taker: &str) -> ServerMessage {
        self.service.respond(&QuoteRequest {
            request_id: id,
            series: self.quotebook_series.to_string(),
            taker: taker.to_owned(),
            taker_side: side,
            size_lots: 1,
        })
    }

    fn status(&self) -> StatusMessage {
        match self.service.status(9) {
            ServerMessage::Status(s) => s,
            other => panic!("status: {other:?}"),
        }
    }
}

fn refusal(m: &ServerMessage) -> RejectReason {
    match m {
        ServerMessage::Reject(r) => r.reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn nonce(m: &ServerMessage) -> u64 {
    match m {
        ServerMessage::Quote(q) => {
            let bytes = from_hex(&q.quote).unwrap();
            quotebook_core::Quote::decode(&bytes)
                .unwrap()
                .terms()
                .nonce
                .get()
        }
        other => panic!("expected a quote, got {other:?}"),
    }
}

/// Puts the maker's long-token account in the fake chain with `amount` and an allowance.
fn hold_long(rpc: &mut FakeRpc, amount: u64, allowance: u64) {
    let mut data = token_account(BASE, MAKER, amount);
    approve(&mut data, delegate(), allowance);
    rpc.accounts
        .insert(BASE_ACCOUNT, account(TOKEN_PROGRAM, data));
}

/// The view owns the fake chain, so the test edits it through the view's lock.
fn with_rpc(view: &RpcChainView<FakeRpc>, f: impl FnOnce(&mut FakeRpc)) {
    view.with_rpc(f);
}

#[test]
fn the_hosted_service_quotes_only_what_the_finalized_snapshot_allows() {
    let view =
        RpcChainView::new(chain(NonceWindow::new()), PROGRAM, MAKER, host_watched()).unwrap();
    let rig = rig(&view);

    // Before any snapshot is applied: nothing.
    with_rpc(&view, |rpc| rig.at(rpc, NOW + 5));
    assert_eq!(
        refusal(&rig.ask(1, TakerSide::Buy, TAKER)),
        RejectReason::RateLimited
    );

    // A snapshot inside the cold start (60 s TTL + 10 s) still quotes nothing.
    rig.service.reconcile(&view).unwrap();
    assert_eq!(
        refusal(&rig.ask(2, TakerSide::Buy, TAKER)),
        RejectReason::RateLimited
    );

    // Past the cold start on the finalized Clock: the limits now read the snapshot. The maker
    // holds no long token, so the band (max_short 0) refuses an ask.
    with_rpc(&view, |rpc| rig.at(rpc, NOW + 71));
    rig.service.reconcile(&view).unwrap();
    assert_eq!(
        refusal(&rig.ask(3, TakerSide::Buy, TAKER)),
        RejectReason::RiskLimit
    );
    assert_eq!(
        refusal(&rig.ask(
            4,
            TakerSide::Buy,
            &Address::new_from_array([5; 32]).to_string()
        )),
        RejectReason::TakerNotAllowed
    );

    // Two long tokens with an allowance of 2: one ask is signed. A bid would take the position
    // to 3, above max_long 2.
    with_rpc(&view, |rpc| {
        hold_long(rpc, 2, 2);
        rig.at(rpc, NOW + 80);
    });
    rig.service.reconcile(&view).unwrap();
    let quote = rig.ask(5, TakerSide::Buy, TAKER);
    let n = nonce(&quote);
    assert_eq!(
        refusal(&rig.ask(6, TakerSide::Sell, TAKER)),
        RejectReason::RiskLimit
    );
    let status = rig.status();
    assert_eq!(status.open_quotes, 1);
    assert_eq!(status.spot_ages.len(), 1);
    assert_eq!(status.spot_ages[0].series, rig.quotebook_series.to_string());
    assert_eq!(status.spot_ages[0].age_secs, Some(0));
    assert!(status.snapshot_age_secs.is_some());

    // The nonce is seen used at finalized: the reservation is released.
    with_rpc(&view, |rpc| {
        let mut window = NonceWindow::new();
        window.consume(Nonce::new(n)).unwrap();
        rpc.accounts.insert(
            crate::chain::tests::maker_state_address(),
            account(PROGRAM, maker_state(MAKER, window)),
        );
        hold_long(rpc, 1, 1);
        rig.at(rpc, NOW + 90);
    });
    rig.service.reconcile(&view).unwrap();
    assert_eq!(rig.status().open_quotes, 0);

    // A failed read refuses everything and releases nothing.
    with_rpc(&view, |rpc| rpc.fail = true);
    assert!(rig.service.reconcile(&view).is_err());
    assert_eq!(
        refusal(&rig.ask(7, TakerSide::Buy, TAKER)),
        RejectReason::RiskUnavailable
    );
    let _ = rig.metalbook_series;
}

fn host_watched() -> Vec<crate::chain::WatchedAccount> {
    vec![
        crate::chain::tests::watched(USDC_ACCOUNT, USDC),
        crate::chain::tests::watched(BASE_ACCOUNT, BASE),
    ]
}
