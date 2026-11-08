//! D5's writer cycle: the cap on contracts written across expiries, the order of the steps, the
//! drain before any mint, the finalized confirmations, and the reads it plans from. The chain
//! reads use the live devnet images of `fixtures/devnet_accounts.json`.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::indexing_slicing,
    clippy::integer_division,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_used,
    missing_docs
)]

use std::collections::HashMap;

use metalbook_core::accounts::Writer;
use metalbook_core::{OptionKind, SeriesState, Underlying};
use metalbook_sdk::pda;
use quotebook_cli::rpc::Rpc;
use quotebook_cli::CliError;
use quotebook_demo::chain::{CLOCK_SYSVAR, SYSVAR_PROGRAM, TOKEN_PROGRAM};
use quotebook_demo::config::{DemoConfig, SeriesPair};
use quotebook_demo::cycle::{
    long_account, plan, read_view, run_cycle, with_pairs, BookSeries, CycleIo, MetalSeries,
    Patience, Step, View, LONG_APPROVAL, USDC_APPROVAL,
};
use quotebook_demo::host::associated_token_account;
use quotebook_demo::writer_cap::Position;
use quotebook_maker::protocol::{from_hex, StatusMessage};
use quotebook_maker::risk::TokenAccountState;
use serde_json::Value;
use solana_account::Account;
use solana_address::Address;

const CONFIG: &str = include_str!("../config/devnet.json");

/// Thursday 2026-11-12 12:00Z.
const THURSDAY: i64 = 1_794_484_800;
/// Series 1's expiry, Friday 2026-11-13 21:00Z.
const NOV13: i64 = 1_794_603_600;
const NOV20: i64 = NOV13 + 7 * 86_400;
const WINDOW_S: u32 = 1_800;
const MAKER: Address = Address::new_from_array([0x42; 32]);
const MAX_LONG: u64 = 2;

fn demo() -> DemoConfig {
    DemoConfig::from_json(CONFIG).unwrap()
}

fn xag_feed() -> [u8; 32] {
    demo()
        .underlying(Underlying::Xag)
        .map(|u| u.feed_id)
        .unwrap()
}

/// A 1 oz XAG capped call (cap 10% above the strike), strike in whole USD.
fn call(tag: u8, strike_usd: u64, expiry: i64, state: SeriesState) -> MetalSeries {
    let address = Address::new_from_array([tag; 32]);
    let strike = strike_usd * 1_000_000;
    MetalSeries {
        address,
        long_mint: pda::long_mint(&demo().metalbook_program, &address).0,
        state,
        underlying: Underlying::Xag,
        kind: OptionKind::Call,
        strike,
        cap: strike + strike / 10,
        multiplier: 1_000,
        expiry,
        window_s: WINDOW_S,
        max_payout: strike / 10,
        usdc_mint: demo().usdc_mint,
        feed_id: xag_feed(),
    }
}

fn book_for(tag: u8, m: &MetalSeries) -> BookSeries {
    BookSeries {
        address: Address::new_from_array([tag; 32]),
        index: u64::from(tag),
        base_mint: m.long_mint,
        quote_mint: demo().usdc_mint,
        base_lot: 1,
        quote_lot: 10_000,
        fee_bps: 0,
    }
}

fn tok(amount: u64, delegated: u64) -> Option<TokenAccountState> {
    Some(TokenAccountState {
        amount,
        delegated_amount: delegated,
    })
}

fn pair(m: &MetalSeries, b: &BookSeries) -> SeriesPair {
    SeriesPair {
        metalbook_series: m.address,
        quotebook_series: b.address,
    }
}

fn position(m: &MetalSeries, written: u64, claimed: bool) -> Position {
    Position {
        address: pda::position(&demo().metalbook_program, &m.address, &MAKER).0,
        series: m.address,
        written,
        claimed,
    }
}

/// Nov 13 paired and tradable, 14 USDC with 4 approved, nothing written.
fn view() -> View {
    let nov13 = call(1, 50, NOV13, SeriesState::Active);
    let b13 = book_for(1, &nov13);
    View {
        maker: MAKER,
        usdc_mint: demo().usdc_mint,
        usdc_account: associated_token_account(&MAKER, &demo().usdc_mint, &TOKEN_PROGRAM),
        slot: 10,
        now: THURSDAY,
        spot: Some(50_400_000),
        metal: vec![nov13],
        book: vec![b13],
        pairs: vec![pair(&nov13, &b13)],
        positions: Vec::new(),
        usdc: tok(14_000_000, USDC_APPROVAL),
        longs: vec![(nov13.long_mint, tok(0, LONG_APPROVAL))],
    }
}

fn mints(steps: &[Step]) -> Vec<(Address, u64)> {
    steps
        .iter()
        .filter_map(|s| match s {
            Step::Mint { metal, contracts } => Some((*metal, *contracts)),
            _ => None,
        })
        .collect()
}

#[test]
fn the_first_cycle_mints_up_to_the_band_funding_and_cap() {
    let v = view();
    let p = plan(&v, &demo(), MAX_LONG).unwrap();
    assert_eq!(mints(&p.steps), vec![(v.metal[0].address, 2)]);
    // Both approvals are already at their amounts: a mint alone does not touch them.
    assert!(!p.steps.iter().any(|s| matches!(s, Step::Approve { .. })));
    assert_eq!(p.pairs, v.pairs);

    // 8 USDC writes one 5 USDC contract, not two.
    let mut poor = view();
    poor.usdc = tok(8_000_000, USDC_APPROVAL);
    assert_eq!(
        mints(&plan(&poor, &demo(), MAX_LONG).unwrap().steps),
        vec![(v.metal[0].address, 1)]
    );

    // Holding 2 long tokens, the band is full.
    let mut full = view();
    full.longs = vec![(v.metal[0].long_mint, tok(2, LONG_APPROVAL))];
    let p = plan(&full, &demo(), MAX_LONG).unwrap();
    assert!(mints(&p.steps).is_empty());
    assert!(p.notes.iter().any(|n| n.contains("band")));
}

#[test]
fn sell_then_roll_refuses_a_third_mint() {
    // Two written and both sold: inventory 0, premiums and cash available, the band would allow
    // two more. The cap counts contracts written, so nothing is minted.
    let mut v = view();
    let nov13 = v.metal[0];
    v.positions = vec![position(&nov13, 2, false)];
    v.usdc = tok(9_000_000, USDC_APPROVAL);
    v.longs = vec![(nov13.long_mint, tok(0, 0))];
    let p = plan(&v, &demo(), MAX_LONG).unwrap();
    assert!(mints(&p.steps).is_empty(), "{:?}", p.steps);
    assert!(p.notes.iter().any(|n| n.contains("writer cap is full")));
    // Nothing held and nothing minted: no approval on an empty account, so no halt at all.
    assert!(p.halted().is_empty(), "{:?}", p.steps);

    // One written: room for exactly one more.
    v.positions = vec![position(&nov13, 1, false)];
    assert_eq!(
        mints(&plan(&v, &demo(), MAX_LONG).unwrap().steps),
        vec![(nov13.address, 1)]
    );
}

fn two_expiries(nov13_state: SeriesState, claimed: bool) -> View {
    let mut v = view();
    let mut nov13 = v.metal[0];
    nov13.state = nov13_state;
    let nov20 = call(2, 50, NOV20, SeriesState::Active);
    let b20 = book_for(2, &nov20);
    v.metal = vec![nov13, nov20];
    v.book.push(b20);
    v.pairs.push(pair(&nov20, &b20));
    v.positions = vec![position(&nov13, 2, claimed)];
    v.longs = vec![(nov13.long_mint, tok(0, 0)), (nov20.long_mint, tok(0, 0))];
    v.now = NOV13 + 3_600;
    v
}

#[test]
fn an_open_expiry_blocks_the_next_one_until_its_claim_is_finalized() {
    // Nov 13 expired with 2 open: Nov 20 writes nothing.
    let v = two_expiries(SeriesState::Expired, false);
    let p = plan(&v, &demo(), MAX_LONG).unwrap();
    assert!(mints(&p.steps).is_empty(), "{:?}", p.steps);

    // Settled, the claim not yet finalized (the finalized view still shows it open): the claim is
    // planned, and Nov 20 still writes nothing in the same run.
    let v = two_expiries(SeriesState::Settled, false);
    let p = plan(&v, &demo(), MAX_LONG).unwrap();
    assert!(p.steps.contains(&Step::Claim {
        metal: v.metal[0].address
    }));
    assert!(mints(&p.steps).is_empty(), "{:?}", p.steps);

    // The claim finalized: Nov 20 writes 2, and Nov 13 leaves the configuration.
    let v = two_expiries(SeriesState::Settled, true);
    let p = plan(&v, &demo(), MAX_LONG).unwrap();
    assert_eq!(mints(&p.steps), vec![(v.metal[1].address, 2)]);
    assert!(!p.steps.iter().any(|s| matches!(s, Step::Claim { .. })));
    assert_eq!(p.pairs, vec![v.pairs[1]]);
    // The new long account gets its approval with the mint, and the USDC one is unchanged.
    assert!(p.steps.contains(&Step::Approve {
        account: long_account(&MAKER, &v.metal[1].long_mint),
        amount: LONG_APPROVAL,
    }));
}

#[test]
fn after_settlement_it_claims_redeems_and_revokes_then_drops_the_pair() {
    let mut v = two_expiries(SeriesState::Settled, false);
    let nov13 = v.metal[0];
    v.longs[0] = (nov13.long_mint, tok(1, 1));
    let p = plan(&v, &demo(), MAX_LONG).unwrap();
    let account = long_account(&MAKER, &nov13.long_mint);
    let order: Vec<Step> = p.steps.clone();
    assert_eq!(
        order[..3],
        [
            Step::Claim {
                metal: nov13.address
            },
            Step::Redeem {
                metal: nov13.address,
                contracts: 1
            },
            Step::Revoke { account },
        ]
    );
    assert!(p.pairs.contains(&v.pairs[0]), "kept while work remains");

    // All done: the pair leaves.
    v.positions = vec![position(&nov13, 2, true)];
    v.longs[0] = (nov13.long_mint, tok(0, 0));
    assert!(!plan(&v, &demo(), MAX_LONG)
        .unwrap()
        .pairs
        .contains(&v.pairs[0]));
}

#[test]
fn a_new_expiry_is_paired_at_the_strike_nearest_spot_once() {
    let mut v = view();
    let strikes = [48u64, 50, 52];
    let mut series: Vec<MetalSeries> = strikes
        .iter()
        .enumerate()
        .map(|(i, s)| {
            call(
                10 + u8::try_from(i).unwrap(),
                *s,
                NOV20,
                SeriesState::Active,
            )
        })
        .collect();
    // A put, a 2 oz call and an XAU call at the same expiry are never chosen.
    let mut put = call(20, 50, NOV20, SeriesState::Active);
    put.kind = OptionKind::Put;
    let mut big = call(21, 50, NOV20, SeriesState::Active);
    big.multiplier = 2_000;
    let mut gold = call(22, 50, NOV20, SeriesState::Active);
    gold.underlying = Underlying::Xau;
    series.extend([put, big, gold]);
    v.metal.extend(series.clone());
    v.spot = Some(51_200_000);

    // No quotebook series yet: create one, for the 52 strike (nearest 51.2), and pair nothing.
    let p = plan(&v, &demo(), MAX_LONG).unwrap();
    let chosen = series[2];
    assert_eq!(
        p.creates(),
        vec![Step::CreateSeries {
            metal: chosen.address,
            long_mint: chosen.long_mint
        }]
    );
    assert_eq!(p.pairs, v.pairs);

    // Once it exists, the pair is added and the next run keeps it whatever spot does.
    v.book.push(book_for(30, &chosen));
    let p = plan(&v, &demo(), MAX_LONG).unwrap();
    assert!(p.creates().is_empty());
    assert_eq!(p.pairs.len(), 2);
    assert_eq!(p.pairs[1].metalbook_series, chosen.address);
    let mut later = v.clone();
    later.pairs = p.pairs.clone();
    later.spot = Some(48_100_000);
    assert_eq!(plan(&later, &demo(), MAX_LONG).unwrap().pairs, p.pairs);

    // Without a fresh spot, an unpaired expiry waits.
    let mut stale = v.clone();
    stale.book.pop();
    stale.spot = None;
    let p = plan(&stale, &demo(), MAX_LONG).unwrap();
    assert!(p.creates().is_empty());
    assert!(p.notes.iter().any(|n| n.contains("no fresh XAG spot")));
}

#[test]
fn an_unreadable_series_under_an_open_position_plans_nothing() {
    let mut v = view();
    v.positions = vec![Position {
        address: Address::new_from_array([9; 32]),
        series: Address::new_from_array([8; 32]),
        written: 1,
        claimed: false,
    }];
    assert!(plan(&v, &demo(), MAX_LONG).is_err());
}

#[test]
fn the_configuration_is_rewritten_with_its_pairs_only() {
    let v = two_expiries(SeriesState::Active, false);
    let text = with_pairs(CONFIG, &v.pairs).unwrap();
    let back = DemoConfig::from_json(&text).unwrap();
    assert_eq!(back.pairs, v.pairs);
    assert_eq!(back.underlyings, demo().underlyings);
    assert!(with_pairs("[]", &v.pairs).is_err());
}

// --- run_cycle against a fake world -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    Read,
    Status,
    Clock(i64),
    Halt,
    Send(Step),
    Pause,
}

/// A world in memory: the view each read returns, which changes when a step is sent; a maker
/// whose newest quote expires at `newest`; a finalized Clock that advances 5 s a pause.
struct World {
    view: View,
    clock: i64,
    newest: Option<i64>,
    halted: bool,
    maker_down: bool,
    fail_read: bool,
    apply_sends: bool,
    events: Vec<Event>,
}

impl World {
    fn new(view: View) -> Self {
        Self {
            clock: view.now,
            view,
            newest: None,
            halted: false,
            maker_down: false,
            fail_read: false,
            apply_sends: true,
            events: Vec::new(),
        }
    }

    fn sends(&self) -> Vec<Step> {
        self.events
            .iter()
            .filter_map(|e| match e {
                Event::Send(s) => Some(*s),
                _ => None,
            })
            .collect()
    }
}

impl CycleIo for World {
    fn read(&mut self) -> Result<View, CliError> {
        self.events.push(Event::Read);
        if self.fail_read {
            return Err(CliError::Rpc("getProgramAccounts down".into()));
        }
        let mut v = self.view.clone();
        v.now = self.clock;
        Ok(v)
    }
    fn status(&mut self) -> Result<StatusMessage, String> {
        self.events.push(Event::Status);
        if self.maker_down {
            return Err("connection refused".into());
        }
        Ok(StatusMessage {
            request_id: 1,
            halted: self.halted,
            open_quotes: 1,
            next_nonce: 9,
            newest_expires_at: self.newest,
            snapshot_age_secs: Some(3),
            spot_ages: Vec::new(),
        })
    }
    fn finalized_clock(&mut self) -> Result<i64, CliError> {
        self.events.push(Event::Clock(self.clock));
        Ok(self.clock)
    }
    fn halt(&mut self) -> Result<bool, CliError> {
        self.events.push(Event::Halt);
        let created = !self.halted;
        self.halted = true;
        Ok(created)
    }
    fn send(&mut self, step: &Step, _: &View) -> Result<String, CliError> {
        self.events.push(Event::Send(*step));
        if self.apply_sends {
            let v = &mut self.view;
            match *step {
                Step::Mint { metal, contracts } => {
                    match v.positions.iter_mut().find(|p| p.series == metal) {
                        Some(p) => p.written += contracts,
                        None => {
                            let m = *v.metal.iter().find(|m| m.address == metal).unwrap();
                            v.positions.push(position(&m, contracts, false));
                        }
                    }
                    let m = *v.metal.iter().find(|m| m.address == metal).unwrap();
                    let entry = v.longs.iter_mut().find(|(l, _)| *l == m.long_mint).unwrap();
                    let t = entry.1.get_or_insert_default();
                    t.amount += contracts;
                    let u = v.usdc.as_mut().unwrap();
                    u.amount -= contracts * m.max_payout;
                }
                Step::Approve { account, amount } => {
                    if account == v.usdc_account {
                        v.usdc.as_mut().unwrap().delegated_amount = amount;
                    } else {
                        for (mint, t) in &mut v.longs {
                            if long_account(&MAKER, mint) == account {
                                t.get_or_insert_default().delegated_amount = amount;
                            }
                        }
                    }
                }
                Step::CreateSeries { metal, .. } => {
                    let m = *v.metal.iter().find(|m| m.address == metal).unwrap();
                    v.book.push(book_for(40, &m));
                }
                Step::Claim { metal } => {
                    for p in v.positions.iter_mut().filter(|p| p.series == metal) {
                        p.claimed = true;
                    }
                }
                Step::Redeem { .. } | Step::Revoke { .. } => {}
            }
        }
        Ok(format!("sig{}", self.events.len()))
    }
    fn pause(&mut self) {
        self.events.push(Event::Pause);
        self.clock += 5;
    }
}

const PATIENCE: Patience = Patience {
    drain_polls: 30,
    confirm_polls: 5,
};

#[test]
fn a_mint_with_a_live_quote_waits_for_the_drain() {
    let mut world = World::new(view());
    // A quote signed just before the halt expires 40 s from now.
    world.newest = Some(THURSDAY + 40);
    let outcome = run_cycle(&mut world, &demo(), MAX_LONG, PATIENCE).unwrap();
    assert!(outcome.halt_created);
    assert_eq!(
        world.sends(),
        vec![Step::Mint {
            metal: view().metal[0].address,
            contracts: 2
        }]
    );

    let halt = world.events.iter().position(|e| *e == Event::Halt).unwrap();
    let first_send = world
        .events
        .iter()
        .position(|e| matches!(e, Event::Send(_)))
        .unwrap();
    assert!(halt < first_send, "halt before any send");
    // The last finalized Clock read before the send is at or past the newest expiry, and every
    // Clock read before it that was short of it was followed by a wait.
    let clocks: Vec<i64> = world.events[..first_send]
        .iter()
        .filter_map(|e| match e {
            Event::Clock(c) => Some(*c),
            _ => None,
        })
        .collect();
    assert!(*clocks.last().unwrap() >= THURSDAY + 40, "{clocks:?}");
    assert!(clocks.len() > 1, "it waited");
    // A read after the drain re-plans before sending.
    assert_eq!(world.events[first_send - 1], Event::Read);
    assert!(!outcome.config_changed);
    assert_eq!(outcome.sent.len(), 1);
}

#[test]
fn a_maker_that_does_not_answer_or_never_drains_gets_nothing_sent() {
    let mut world = World::new(view());
    world.maker_down = true;
    let failure = run_cycle(&mut world, &demo(), MAX_LONG, PATIENCE).unwrap_err();
    assert!(failure.error.to_string().contains("drain cannot be judged"));
    assert!(failure.halt_created, "the halt stays");
    assert!(world.sends().is_empty());

    let mut world = World::new(view());
    world.newest = Some(THURSDAY + 10_000);
    let failure = run_cycle(&mut world, &demo(), MAX_LONG, PATIENCE).unwrap_err();
    assert!(failure.error.to_string().contains("not drained"));
    assert!(world.sends().is_empty());
}

#[test]
fn a_halt_someone_else_made_is_left_alone() {
    // A revoke or an incident left the halt file: nothing in steps 3 to 5 is sent.
    let mut world = World::new(view());
    world.halted = true;
    let failure = run_cycle(&mut world, &demo(), MAX_LONG, PATIENCE).unwrap_err();
    assert!(failure.error.to_string().contains("already halted"));
    assert!(!failure.halt_created);
    assert!(world.sends().is_empty());
}

#[test]
fn a_failed_writer_read_refuses() {
    // The reads fail: nothing is planned, halted or sent.
    let mut world = World::new(view());
    world.fail_read = true;
    let failure = run_cycle(&mut world, &demo(), MAX_LONG, PATIENCE).unwrap_err();
    assert!(!failure.halt_created);
    assert_eq!(world.events, vec![Event::Read]);

    // And the real reader fails on a failed getProgramAccounts of the positions.
    let mut chain = Chain::with_series();
    chain.fail_writers = true;
    assert!(read_view(&mut chain, &demo(), &MAKER).is_err());
}

#[test]
fn an_effect_never_finalized_fails_with_what_was_sent() {
    let mut world = World::new(view());
    world.apply_sends = false;
    let failure = run_cycle(&mut world, &demo(), MAX_LONG, PATIENCE).unwrap_err();
    assert!(failure
        .error
        .to_string()
        .contains("not confirmed at finalized"));
    assert_eq!(failure.sent.len(), 1);
    assert!(failure.halt_created);
}

#[test]
fn a_new_series_is_created_without_a_halt_then_paired_and_minted() {
    let mut v = view();
    let nov20 = call(2, 50, NOV20, SeriesState::Active);
    v.metal.push(nov20);
    v.longs.push((nov20.long_mint, None));
    // Nov 13 already holds its band: only Nov 20 mints.
    v.longs[0] = (v.metal[0].long_mint, tok(2, LONG_APPROVAL));
    v.positions = vec![position(&v.metal[0], 0, false)];
    let mut world = World::new(v);
    let outcome = run_cycle(&mut world, &demo(), MAX_LONG, PATIENCE).unwrap();
    let sends = world.sends();
    assert!(matches!(sends[0], Step::CreateSeries { metal, .. } if metal == nov20.address));
    let halt = world.events.iter().position(|e| *e == Event::Halt).unwrap();
    let create = world
        .events
        .iter()
        .position(|e| matches!(e, Event::Send(Step::CreateSeries { .. })))
        .unwrap();
    assert!(create < halt, "create_series needs no halt");
    assert!(sends.contains(&Step::Mint {
        metal: nov20.address,
        contracts: 2
    }));
    assert!(sends.contains(&Step::Approve {
        account: long_account(&MAKER, &nov20.long_mint),
        amount: LONG_APPROVAL,
    }));
    assert!(outcome.config_changed);
    assert_eq!(outcome.pairs.len(), 2);
}

#[test]
fn nothing_due_means_no_halt() {
    let mut v = view();
    v.longs = vec![(v.metal[0].long_mint, tok(2, LONG_APPROVAL))];
    let mut world = World::new(v);
    let outcome = run_cycle(&mut world, &demo(), MAX_LONG, PATIENCE).unwrap();
    assert!(!outcome.halt_created);
    assert!(!world.events.contains(&Event::Halt));
    assert!(world.sends().is_empty());
}

// --- read_view against a fake chain -------------------------------------------------------------

fn fixture(name: &str) -> (Address, Address, Vec<u8>) {
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/devnet_accounts.json")).unwrap();
    let a = &fixtures[name];
    (
        a["address"].as_str().unwrap().parse().unwrap(),
        a["owner"].as_str().unwrap().parse().unwrap(),
        from_hex(a["data_hex"].as_str().unwrap()).unwrap(),
    )
}

fn account(owner: Address, data: Vec<u8>) -> Account {
    Account {
        lamports: 1_000_000,
        data,
        owner,
        executable: false,
        rent_epoch: 0,
    }
}

fn token_account(
    mint: Address,
    owner: Address,
    amount: u64,
    delegate: Option<(Address, u64)>,
) -> Account {
    let mut d = vec![0u8; 165];
    d[0..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    if let Some((delegate, allowance)) = delegate {
        d[72..76].copy_from_slice(&1u32.to_le_bytes());
        d[76..108].copy_from_slice(delegate.as_ref());
        d[121..129].copy_from_slice(&allowance.to_le_bytes());
    }
    d[108] = 1;
    account(TOKEN_PROGRAM, d)
}

/// Reads at `finalized` only: `account` (confirmed) is an error, so a value seen only at
/// `confirmed` can never reach the plan.
#[derive(Default)]
struct Chain {
    accounts: HashMap<Address, Account>,
    programs: Vec<(Address, Account)>,
    fail_writers: bool,
}

impl Chain {
    /// Series 1 (Nov 13, strike 50) and its quotebook series, at Thursday, with 14 USDC.
    fn with_series() -> Self {
        let demo = demo();
        let mut c = Self::default();
        let mut clock = vec![0u8; 40];
        clock[0..8].copy_from_slice(&10u64.to_le_bytes());
        clock[32..40].copy_from_slice(&THURSDAY.to_le_bytes());
        c.accounts
            .insert(CLOCK_SYSVAR, account(SYSVAR_PROGRAM, clock));
        let (series, owner, data) = fixture("series_xag_50");
        c.programs.push((series, account(owner, data)));
        let (book, owner, data) = fixture("quotebook_series_1");
        c.programs.push((book, account(owner, data)));
        let delegate = quotebook_sdk::pda::delegate(&demo.quotebook_program, &MAKER).0;
        c.accounts.insert(
            associated_token_account(&MAKER, &demo.usdc_mint, &TOKEN_PROGRAM),
            token_account(
                demo.usdc_mint,
                MAKER,
                14_000_000,
                Some((delegate, 4_000_000)),
            ),
        );
        c
    }

    fn writer(&mut self, series: &Address, written: u64, claimed: bool) {
        let program = demo().metalbook_program;
        let mut w = <Writer as bytemuck::Zeroable>::zeroed();
        w.tag = 4;
        w.version = metalbook_core::accounts::VERSION;
        w.claimed = u8::from(claimed);
        w.series = series.to_bytes();
        w.owner = MAKER.to_bytes();
        w.written = written;
        self.programs.push((
            pda::position(&program, series, &MAKER).0,
            account(program, bytemuck::bytes_of(&w).to_vec()),
        ));
    }
}

impl Rpc for Chain {
    fn account(&mut self, _: &Address) -> Result<Option<Account>, CliError> {
        Err(CliError::Rpc("the cycle reads at finalized only".into()))
    }
    fn latest_blockhash(&mut self) -> Result<solana_hash::Hash, CliError> {
        Err(CliError::Rpc("a read sends nothing".into()))
    }
    fn send(
        &mut self,
        _: &solana_transaction::Transaction,
    ) -> Result<solana_signature::Signature, CliError> {
        panic!("a read sends nothing");
    }
    fn genesis_hash(&mut self) -> Result<solana_hash::Hash, CliError> {
        Err(CliError::Rpc("unused".into()))
    }
    fn finalized_accounts(
        &mut self,
        addresses: &[Address],
    ) -> Result<(u64, Vec<Option<Account>>), CliError> {
        Ok((
            10,
            addresses
                .iter()
                .map(|a| self.accounts.get(a).cloned())
                .collect(),
        ))
    }
    fn finalized_program_accounts(
        &mut self,
        program_id: &Address,
        data_size: u64,
        memcmp: &[(usize, Vec<u8>)],
    ) -> Result<Vec<(Address, Account)>, CliError> {
        if self.fail_writers && data_size == 80 {
            return Err(CliError::Rpc("getProgramAccounts down".into()));
        }
        Ok(self
            .programs
            .iter()
            .filter(|(_, a)| {
                a.owner == *program_id
                    && a.data.len() as u64 == data_size
                    && memcmp
                        .iter()
                        .all(|(o, b)| a.data.get(*o..*o + b.len()) == Some(b.as_slice()))
            })
            .cloned()
            .collect())
    }
}

#[test]
fn the_reader_sees_the_live_series_and_plans_the_first_mint() {
    let mut chain = Chain::with_series();
    let v = read_view(&mut chain, &demo(), &MAKER).unwrap();
    assert_eq!(v.now, THURSDAY);
    assert_eq!(v.metal.len(), 1);
    let m = v.metal[0];
    assert_eq!(m.address, demo().pairs[0].metalbook_series);
    assert_eq!(
        (m.strike, m.max_payout, m.expiry),
        (50_000_000, 5_000_000, NOV13)
    );
    assert!(m.eligible(&demo(), THURSDAY));
    assert_eq!(v.book.len(), 1);
    assert!(v.book[0].trades(&m.long_mint, &demo().usdc_mint));
    assert_eq!(v.usdc.unwrap().amount, 14_000_000);
    assert_eq!(v.long(&m.long_mint), None, "no long account yet");
    // No feed account in this chain: no spot, which only matters for a new expiry.
    assert_eq!(v.spot, None);
    let p = plan(&v, &demo(), MAX_LONG).unwrap();
    assert_eq!(mints(&p.steps), vec![(m.address, 2)]);
    assert!(p.steps.contains(&Step::Approve {
        account: long_account(&MAKER, &m.long_mint),
        amount: LONG_APPROVAL,
    }));
}

#[test]
fn the_reader_counts_positions_at_finalized() {
    let series = demo().pairs[0].metalbook_series;
    let mut chain = Chain::with_series();
    chain.writer(&series, 2, false);
    let v = read_view(&mut chain, &demo(), &MAKER).unwrap();
    let e = v.exposure().unwrap();
    assert_eq!((e.contracts, e.committed), (2, 10_000_000));
    assert!(mints(&plan(&v, &demo(), MAX_LONG).unwrap().steps).is_empty());
}

/// Nov 20 listed at strikes 48 and 52, no pair, a contract written on 48 by a run that failed.
fn stranded() -> (View, MetalSeries, MetalSeries) {
    let mut v = view();
    v.pairs.clear();
    let a = call(7, 48, NOV13, SeriesState::Active);
    let b = call(8, 52, NOV13, SeriesState::Active);
    v.metal = vec![a, b];
    v.book = vec![book_for(7, &a), book_for(8, &b)];
    v.positions = vec![position(&a, 1, false)];
    v.longs = vec![(a.long_mint, tok(1, 2)), (b.long_mint, tok(0, 0))];
    v.spot = Some(51_900_000);
    (v, a, b)
}

#[test]
fn a_run_that_minted_and_failed_keeps_its_strike() {
    let (mut v, a, _) = stranded();
    let p = plan(&v, &demo(), MAX_LONG).unwrap();
    assert_eq!(p.pairs.len(), 1);
    assert_eq!(
        p.pairs[0].metalbook_series, a.address,
        "not the nearest, 52"
    );
    // No spot is needed to find it.
    v.spot = None;
    let p = plan(&v, &demo(), MAX_LONG).unwrap();
    assert_eq!(p.pairs[0].metalbook_series, a.address);
}

#[test]
fn settlement_covers_a_position_whose_pair_was_never_recorded() {
    let (mut v, mut a, _) = stranded();
    a.state = SeriesState::Settled;
    v.metal[0] = a;
    v.positions = vec![position(&a, 1, true)];
    let steps = plan(&v, &demo(), MAX_LONG).unwrap().steps;
    assert!(steps.contains(&Step::Redeem {
        metal: a.address,
        contracts: 1
    }));
    assert!(steps.contains(&Step::Revoke {
        account: long_account(&MAKER, &a.long_mint)
    }));
}
