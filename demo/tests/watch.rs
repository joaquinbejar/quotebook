//! D4's watcher against a fake chain: every alert kind, the `stale-spot` closures, re-alert
//! timing, delivery and the rotated output. The series and both feeds are the live devnet images
//! of `fixtures/devnet_accounts.json`, with the feeds' `publish_time` set per test.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::unwrap_used,
    missing_docs
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bytemuck::Zeroable;
use metalbook_core::accounts::{Calendar, Config, Writer, VERSION};
use metalbook_sdk::pda;
use quotebook_cli::rpc::Rpc;
use quotebook_cli::CliError;
use quotebook_demo::chain::{CLOCK_SYSVAR, SYSVAR_PROGRAM, TOKEN_PROGRAM};
use quotebook_demo::config::DemoConfig;
use quotebook_demo::host::associated_token_account;
use quotebook_demo::logging::RotatingFile;
use quotebook_demo::watch::{
    alert_log_line, deliver, deliver_within, judge_spot, last_taker_run, listing_expected,
    log_alert, pass, Alert, AlertBook, Limits, LineOut, Pass, SpotVerdict, StatusSource,
    TakerOutcome, WatchState, Watched, REALERT_SECS,
};
use quotebook_demo::writer_cap::{read_exposure, Exposure};
use quotebook_maker::protocol::{from_hex, StatusMessage};
use serde_json::Value;
use solana_account::Account;
use solana_address::Address;

const CONFIG: &str = include_str!("../config/devnet.json");

/// Thursday 2026-11-12 12:00Z: the market is open, a day and a half before series 1's expiry.
const THURSDAY: i64 = 1_794_484_800;
/// Series 1's expiry, Friday 2026-11-13 21:00Z, and its settlement window.
const EXPIRY: i64 = 1_794_603_600;
const WINDOW_S: u32 = 1_800;
/// Wednesday 2026-11-11's daily break, 22:00 to 23:00Z.
const BREAK_START: i64 = 1_794_434_400;
const BREAK_END: i64 = BREAK_START + 3_600;

const MAKER: Address = Address::new_from_array([0x42; 32]);
const PAYER: Address = Address::new_from_array([0x43; 32]);
const TAKER: Address = Address::new_from_array([0x44; 32]);

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

fn feed(name: &str, publish_time: i64) -> (Address, Account) {
    let (address, owner, mut data) = fixture(name);
    data[93..101].copy_from_slice(&publish_time.to_le_bytes());
    data[101..109].copy_from_slice(&(publish_time - 1).to_le_bytes());
    (address, account(owner, data))
}

/// A chain in memory: the accounts, the writer positions, and which reads fail.
#[derive(Default, Clone)]
struct Chain {
    slot: u64,
    accounts: HashMap<Address, Account>,
    writers: Vec<(Address, Account)>,
    calls: usize,
    fail_call: Option<usize>,
    fail_writers: bool,
}

impl Rpc for Chain {
    fn account(&mut self, _: &Address) -> Result<Option<Account>, CliError> {
        Err(CliError::Rpc("the watcher reads at finalized only".into()))
    }
    fn latest_blockhash(&mut self) -> Result<solana_hash::Hash, CliError> {
        Err(CliError::Rpc("the watcher sends nothing".into()))
    }
    fn send(
        &mut self,
        _: &solana_transaction::Transaction,
    ) -> Result<solana_signature::Signature, CliError> {
        panic!("the watcher sends nothing");
    }
    fn genesis_hash(&mut self) -> Result<solana_hash::Hash, CliError> {
        Err(CliError::Rpc("unused".into()))
    }
    fn finalized_accounts(
        &mut self,
        addresses: &[Address],
    ) -> Result<(u64, Vec<Option<Account>>), CliError> {
        self.calls += 1;
        if self.fail_call == Some(self.calls) {
            return Err(CliError::Rpc("down".into()));
        }
        Ok((
            self.slot,
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
        if self.fail_writers {
            return Err(CliError::Rpc("getProgramAccounts down".into()));
        }
        Ok(self
            .writers
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

#[derive(Clone)]
struct Maker(Result<StatusMessage, String>);

impl StatusSource for Maker {
    fn status(&mut self) -> Result<StatusMessage, String> {
        self.0.clone()
    }
}

fn healthy_status() -> StatusMessage {
    StatusMessage {
        request_id: 1,
        halted: false,
        open_quotes: 0,
        next_nonce: 1,
        newest_expires_at: None,
        snapshot_age_secs: Some(3),
        spot_ages: Vec::new(),
    }
}

struct World {
    demo: DemoConfig,
    chain: Chain,
    maker: Maker,
    watched: Watched,
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "quotebook-demo-watch-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

impl World {
    /// Everything healthy at `now`: spots just published, 14 USDC with 4 approved, 2 long tokens
    /// approved, a funded payer and taker, no position, the maker answering.
    fn at(name: &str, now: i64) -> Self {
        let demo = DemoConfig::from_json(CONFIG).unwrap();
        let mut chain = Chain {
            slot: 5_000,
            ..Chain::default()
        };
        let mut clock = vec![0u8; 40];
        clock[0..8].copy_from_slice(&5_000u64.to_le_bytes());
        clock[32..40].copy_from_slice(&now.to_le_bytes());
        chain
            .accounts
            .insert(CLOCK_SYSVAR, account(SYSVAR_PROGRAM, clock));
        let mut payer = account(SYSVAR_PROGRAM, Vec::new());
        payer.lamports = 980_000_000;
        payer.owner = Address::default();
        chain.accounts.insert(PAYER, payer);

        let mut config = Config::zeroed();
        config.tag = 1;
        config.version = VERSION;
        config.window_s = WINDOW_S;
        chain.accounts.insert(
            pda::config(&demo.metalbook_program).0,
            account(demo.metalbook_program, bytemuck::bytes_of(&config).to_vec()),
        );
        let dir = scratch(name);
        let world = Self {
            chain,
            maker: Maker(Ok(healthy_status())),
            watched: Watched {
                demo: demo.clone(),
                maker: MAKER,
                payer: PAYER,
                taker: Some(TAKER),
                halt_file: dir.join("HALT"),
                taker_log: Some(dir.join("trades.jsonl")),
                limits: Limits::default(),
            },
            demo,
        };
        let mut world = world;
        world.calendar(&[EXPIRY]);
        for f in ["feed_xau", "feed_xag"] {
            let (address, a) = feed(f, now - 30);
            world.chain.accounts.insert(address, a);
        }
        let (series, owner, data) = fixture("series_xag_50");
        world.chain.accounts.insert(series, account(owner, data));
        world.usdc(14_000_000, 4_000_000);
        world.long(0, 2);
        world.taker_usdc(8_770_000);
        world
    }

    fn calendar(&mut self, expiries: &[i64]) {
        let mut c = Calendar::zeroed();
        c.tag = 2;
        c.version = VERSION;
        c.underlying = 1;
        c.count = u8::try_from(expiries.len()).unwrap();
        c.expiries[..expiries.len()].copy_from_slice(expiries);
        self.chain.accounts.insert(
            pda::calendar(&self.demo.metalbook_program, 1).0,
            account(self.demo.metalbook_program, bytemuck::bytes_of(&c).to_vec()),
        );
    }

    fn delegate(&self) -> Address {
        quotebook_sdk::pda::delegate(&self.demo.quotebook_program, &MAKER).0
    }

    fn usdc_account(&self) -> Address {
        associated_token_account(&MAKER, &self.demo.usdc_mint, &TOKEN_PROGRAM)
    }

    fn long_mint(&self) -> Address {
        let (series, _, _) = fixture("series_xag_50");
        pda::long_mint(&self.demo.metalbook_program, &series).0
    }

    fn long_account(&self) -> Address {
        associated_token_account(&MAKER, &self.long_mint(), &TOKEN_PROGRAM)
    }

    fn usdc(&mut self, amount: u64, allowance: u64) {
        let a = token_account(
            self.demo.usdc_mint,
            MAKER,
            amount,
            Some((self.delegate(), allowance)),
        );
        self.chain.accounts.insert(self.usdc_account(), a);
    }

    fn long(&mut self, amount: u64, allowance: u64) {
        let a = token_account(
            self.long_mint(),
            MAKER,
            amount,
            Some((self.delegate(), allowance)),
        );
        self.chain.accounts.insert(self.long_account(), a);
    }

    fn taker_usdc(&mut self, amount: u64) {
        let address = associated_token_account(&TAKER, &self.demo.usdc_mint, &TOKEN_PROGRAM);
        self.chain.accounts.insert(
            address,
            token_account(self.demo.usdc_mint, TAKER, amount, None),
        );
    }

    fn set_clock(&mut self, now: i64) {
        let mut clock = vec![0u8; 40];
        clock[0..8].copy_from_slice(&5_000u64.to_le_bytes());
        clock[32..40].copy_from_slice(&now.to_le_bytes());
        self.chain
            .accounts
            .insert(CLOCK_SYSVAR, account(SYSVAR_PROGRAM, clock));
    }

    fn spots_at(&mut self, publish_time: i64) {
        for f in ["feed_xau", "feed_xag"] {
            let (address, a) = feed(f, publish_time);
            self.chain.accounts.insert(address, a);
        }
    }

    /// A position of `owner` in series 1.
    fn writer(&mut self, owner: Address, written: u64, claimed: bool) {
        let (series, _, _) = fixture("series_xag_50");
        let mut w = Writer::zeroed();
        w.tag = 4;
        w.version = VERSION;
        w.claimed = u8::from(claimed);
        w.series = series.to_bytes();
        w.owner = owner.to_bytes();
        w.written = written;
        let address = pda::position(&self.demo.metalbook_program, &series, &owner).0;
        self.chain.writers.push((
            address,
            account(self.demo.metalbook_program, bytemuck::bytes_of(&w).to_vec()),
        ));
    }

    fn run(&mut self, state: &mut WatchState) -> Pass {
        self.chain.calls = 0;
        pass(&mut self.chain, &mut self.maker, &self.watched, state, 1)
    }

    fn once(&mut self) -> Pass {
        self.run(&mut WatchState::default())
    }
}

fn kinds(p: &Pass) -> Vec<&'static str> {
    let mut k: Vec<_> = p.alerts.iter().map(|a| a.kind).collect();
    k.sort_unstable();
    k
}

fn only<'a>(p: &'a Pass, kind: &str) -> &'a Alert {
    let found: Vec<_> = p.alerts.iter().filter(|a| a.kind == kind).collect();
    assert_eq!(found.len(), 1, "{kind}: {:?}", p.alerts);
    found[0]
}

#[test]
fn a_healthy_demo_raises_nothing_and_observes_every_scope() {
    let mut w = World::at("healthy", THURSDAY);
    let p = w.once();
    assert_eq!(kinds(&p), Vec::<&str>::new(), "{:?}", p.alerts);
    assert_eq!(p.now, THURSDAY, "the finalized Clock, not the host's");
    for scope in [
        "rpc",
        "balance",
        "rpc:tokens",
        "inventory",
        "allowance",
        "writer",
        "collateral",
        "spot",
        "maker",
        "status",
        "halt",
        "series",
        "taker",
    ] {
        assert!(
            p.observed.iter().any(|s| s == scope),
            "{scope} not observed"
        );
    }
    let line = &p.lines[0]["pass"];
    assert_eq!(line["quotable"], true);
    assert_eq!(line["exposure"]["contracts"], 0);
    assert_eq!(line["accounts"][0]["amount"], 14_000_000);
    assert!(!p.lines.iter().any(|l| l.to_string().contains("api.devnet")));
}

#[test]
fn balance_fires_under_the_floor() {
    let mut w = World::at("balance", THURSDAY);
    w.chain.accounts.get_mut(&PAYER).unwrap().lamports = 299_999_999;
    let p = w.once();
    assert_eq!(kinds(&p), vec!["balance"]);
    assert_eq!(only(&p, "balance").subject, PAYER.to_string());
    w.chain.accounts.remove(&PAYER);
    assert_eq!(
        kinds(&w.once()),
        vec!["balance"],
        "a missing payer has nothing"
    );
}

#[test]
fn inventory_fires_for_the_maker_and_the_taker() {
    let mut w = World::at("inventory", THURSDAY);
    w.usdc(999_999, 4_000_000);
    w.taker_usdc(500_000);
    let p = w.once();
    let subjects: Vec<_> = p
        .alerts
        .iter()
        .filter(|a| a.kind == "inventory")
        .map(|a| a.subject.clone())
        .collect();
    assert_eq!(subjects.len(), 2, "{:?}", p.alerts);
    assert!(subjects.contains(&w.usdc_account().to_string()));
    // A long floor of 0 never fires, since the band starts at 0; a floor of 1 does.
    w.usdc(14_000_000, 4_000_000);
    w.taker_usdc(8_770_000);
    w.watched.limits.min_long = 1;
    assert_eq!(
        only(&w.once(), "inventory").subject,
        w.long_account().to_string()
    );
}

#[test]
fn allowance_fires_under_a_fifth_of_the_approval() {
    let mut w = World::at("allowance", THURSDAY);
    w.usdc(14_000_000, 800_000);
    w.long(0, 1);
    assert_eq!(
        kinds(&w.once()),
        Vec::<&str>::new(),
        "exactly 20% and 50% do not fire"
    );
    w.usdc(14_000_000, 799_999);
    w.long(2, 0);
    let p = w.once();
    assert_eq!(kinds(&p), vec!["allowance", "allowance"]);
    // An approval to another delegate counts as none.
    let other = token_account(
        w.demo.usdc_mint,
        MAKER,
        14_000_000,
        Some((PAYER, 4_000_000)),
    );
    let usdc = w.usdc_account();
    w.chain.accounts.insert(usdc, other);
    w.long(0, 2);
    assert_eq!(only(&w.once(), "allowance").subject, usdc.to_string());
}

#[test]
fn collateral_fires_only_when_the_cap_has_room_for_the_next_contract() {
    // One contract of series 1 locks (55 − 50) USDC × 1 oz = 5 USDC.
    let mut w = World::at("collateral", THURSDAY);
    w.usdc(4_999_999, 4_000_000);
    let p = w.once();
    assert_eq!(kinds(&p), vec!["collateral"]);
    w.usdc(5_000_000, 4_000_000);
    assert_eq!(kinds(&w.once()), Vec::<&str>::new());
    // Two contracts written: the cap blocks the next one anyway, so low USDC is expected.
    w.usdc(3_600_000, 4_000_000);
    w.writer(MAKER, 2, false);
    assert_eq!(kinds(&w.once()), Vec::<&str>::new());
}

#[test]
fn maker_down_fires_when_status_has_no_answer() {
    let mut w = World::at("down", THURSDAY);
    w.maker = Maker(Err("no answer within 2 s".into()));
    let p = w.once();
    assert_eq!(kinds(&p), vec!["maker-down"]);
    assert!(
        !p.observed.iter().any(|s| s == "status"),
        "risk-unavailable is not judged"
    );
    assert!(p.observed.iter().any(|s| s == "maker"));
}

#[test]
fn risk_unavailable_fires_on_an_old_or_missing_snapshot() {
    let mut w = World::at("risk", THURSDAY);
    let mut s = healthy_status();
    s.snapshot_age_secs = Some(60);
    w.maker = Maker(Ok(s.clone()));
    assert_eq!(kinds(&w.once()), Vec::<&str>::new(), "60 s is the limit");
    s.snapshot_age_secs = Some(61);
    w.maker = Maker(Ok(s.clone()));
    assert_eq!(kinds(&w.once()), vec!["risk-unavailable"]);
    s.snapshot_age_secs = None;
    w.maker = Maker(Ok(s));
    assert_eq!(kinds(&w.once()), vec!["risk-unavailable"]);
}

#[test]
fn halted_fires_on_the_halt_file_or_a_halted_service() {
    let mut w = World::at("halted", THURSDAY);
    std::fs::write(&w.watched.halt_file, b"").unwrap();
    assert_eq!(kinds(&w.once()), vec!["halted"]);
    std::fs::remove_file(&w.watched.halt_file).unwrap();
    let mut s = healthy_status();
    s.halted = true;
    w.maker = Maker(Ok(s));
    assert_eq!(kinds(&w.once()), vec!["halted"]);
}

#[test]
fn writer_cap_counts_open_positions_and_fails_closed() {
    let mut w = World::at("cap", THURSDAY);
    w.writer(MAKER, 2, false);
    w.writer(MAKER, 5, true);
    w.writer(PAYER, 9, false);
    let e = read_exposure(&mut w.chain, &w.demo.metalbook_program, &MAKER).unwrap();
    assert_eq!(
        e,
        Exposure {
            contracts: 2,
            committed: 10_000_000,
            positions: 2
        },
        "a claimed position and another owner's do not count"
    );
    assert_eq!(
        kinds(&w.once()),
        Vec::<&str>::new(),
        "2 contracts and 10 USDC are within the cap"
    );
    w.chain.writers.clear();
    w.writer(MAKER, 3, false);
    let p = w.once();
    assert!(kinds(&p).contains(&"writer-cap"), "{:?}", p.alerts);
    w.chain.writers.clear();
    w.chain.fail_writers = true;
    let p = w.once();
    assert!(only(&p, "writer-cap").message.contains("fails closed"));
}

#[test]
fn the_cap_has_room_only_within_both_limits() {
    let e = Exposure {
        contracts: 1,
        committed: 5_200_000,
        positions: 1,
    };
    assert!(e.room_for(5_200_000));
    assert!(!e.room_for(5_200_001));
    let full = Exposure {
        contracts: 2,
        committed: 1,
        positions: 2,
    };
    assert!(!full.room_for(1));
    assert!(!full.has_room());
    assert!(!full.over_cap());
}

#[test]
fn stale_spot_fires_outside_a_closure() {
    let mut w = World::at("stale", THURSDAY);
    w.spots_at(THURSDAY - 600);
    assert_eq!(kinds(&w.once()), Vec::<&str>::new(), "600 s is the limit");
    w.spots_at(THURSDAY - 601);
    assert_eq!(kinds(&w.once()), vec!["stale-spot", "stale-spot"]);
    w.spots_at(THURSDAY + 31);
    assert_eq!(
        kinds(&w.once()),
        vec!["stale-spot", "stale-spot"],
        "ahead of the clock"
    );
}

#[test]
fn a_broken_feed_fires() {
    let mut w = World::at("broken", THURSDAY);
    let (address, mut a) = feed("feed_xag", THURSDAY - 30);
    a.owner = PAYER;
    w.chain.accounts.insert(address, a);
    let p = w.once();
    assert!(only(&p, "stale-spot").message.contains("no usable spot"));
    w.chain.accounts.remove(&address);
    assert_eq!(kinds(&w.once()), vec!["stale-spot"]);
}

#[test]
fn a_closure_writes_one_expected_line_and_no_alert() {
    // The feed's last update just before the daily break, the clock inside it.
    let mut w = World::at("closure", BREAK_START + 1_800);
    w.spots_at(BREAK_START - 1);
    let mut state = WatchState::default();
    let p = w.run(&mut state);
    assert_eq!(kinds(&p), Vec::<&str>::new());
    let expected: Vec<_> = p
        .lines
        .iter()
        .filter(|l| l["expected"] == "stale-spot")
        .collect();
    assert_eq!(expected.len(), 2, "one per feed");
    assert_eq!(expected[0]["closure_start"], BREAK_START);
    assert_eq!(expected[0]["closure_end"], BREAK_END);
    // The next pass in the same closure writes nothing again.
    w.set_clock(BREAK_START + 2_400);
    let p = w.run(&mut state);
    assert!(!p.lines.iter().any(|l| l["expected"] == "stale-spot"));
    assert_eq!(kinds(&p), Vec::<&str>::new());
    // Quiet until 600 s after the reopening, then a normal alert.
    w.set_clock(BREAK_END + 599);
    assert_eq!(kinds(&w.run(&mut state)), Vec::<&str>::new());
    w.set_clock(BREAK_END + 600);
    assert_eq!(kinds(&w.run(&mut state)), vec!["stale-spot", "stale-spot"]);
}

#[test]
fn an_alert_already_firing_when_a_closure_starts_keeps_firing() {
    let mut w = World::at("firing", BREAK_START + 60);
    // Stale since BREAK_START − 1: it was already alerting before the break.
    w.spots_at(BREAK_START - 601);
    assert_eq!(kinds(&w.once()), vec!["stale-spot", "stale-spot"]);
}

#[test]
fn only_the_listed_windows_count() {
    // Thursday 15:00Z with the feed quiet for an hour: a holiday is not listed, so it alerts.
    assert_eq!(
        judge_spot(THURSDAY, THURSDAY + 3 * 3_600),
        SpotVerdict::Stale
    );
    // The weekend is listed: a feed that stopped at Friday's close is expected all weekend.
    let friday_close = 1_794_607_200; // 2026-11-13 22:00Z
    let sunday = friday_close + 40 * 3_600;
    assert!(
        matches!(judge_spot(friday_close - 1, sunday), SpotVerdict::Expected(c) if c.start == friday_close)
    );
}

#[test]
fn no_series_fires_only_when_the_calendar_expects_one() {
    // In series 1's settlement window, with the next expiry a week later: the known gap, silent.
    let in_window = EXPIRY - i64::from(WINDOW_S) + 60;
    let mut w = World::at("noseries", in_window);
    w.calendar(&[EXPIRY, EXPIRY + 7 * 86_400]);
    let p = w.once();
    assert_eq!(p.lines[0]["pass"]["quotable"], false);
    assert_eq!(kinds(&p), Vec::<&str>::new());
    // An expiry two days out should be listed by now.
    w.calendar(&[EXPIRY, EXPIRY + 2 * 86_400]);
    assert_eq!(kinds(&w.once()), vec!["no-series"]);
    // A full writer cap blocks the next series on purpose.
    w.writer(MAKER, 2, false);
    assert_eq!(kinds(&w.once()), Vec::<&str>::new());
    w.chain.writers.clear();
    // A closed market never expects one: Friday 22:30Z, the weekend.
    w.set_clock(EXPIRY + 5_400);
    w.spots_at(EXPIRY + 3_000);
    assert_eq!(kinds(&w.once()), Vec::<&str>::new());
}

#[test]
fn the_listing_span_runs_from_three_days_before_to_the_window() {
    let e = EXPIRY;
    assert!(!listing_expected(&[e], WINDOW_S, e - 3 * 86_400 - 1));
    assert!(listing_expected(&[e], WINDOW_S, e - 3 * 86_400));
    assert!(listing_expected(
        &[e],
        WINDOW_S,
        e - i64::from(WINDOW_S) - 1
    ));
    assert!(!listing_expected(&[e], WINDOW_S, e - i64::from(WINDOW_S)));
    assert!(!listing_expected(&[], WINDOW_S, e));
}

fn write_log(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
}

#[test]
fn fill_error_reads_the_taker_logs_last_complete_line() {
    let mut w = World::at("fill", THURSDAY);
    let log = w.watched.taker_log.clone().unwrap();
    assert_eq!(kinds(&w.once()), Vec::<&str>::new(), "no log yet is silent");
    write_log(&log, "{\"time\":1,\"outcome\":\"filled\"}\n{\"time\":2,\"outcome\":\"failed\",\"reason\":\"Rejected: RiskLimit\\u0007\",\"slot\":9}\n");
    let p = w.once();
    let a = only(&p, "fill-error");
    assert!(
        a.message.contains("at 2 failed: Rejected: RiskLimit"),
        "{}",
        a.message
    );
    assert!(
        !a.message.contains('\u{7}'),
        "control characters are dropped"
    );
    write_log(&log, "{\"time\":2,\"outcome\":\"failed\"}\n{\"time\":3,\"outcome\":\"skipped\",\"reason\":\"no inventory\"}\n");
    assert_eq!(
        kinds(&w.once()),
        Vec::<&str>::new(),
        "a skip is not a failure"
    );
    // A line still being written is not read.
    write_log(
        &log,
        "{\"time\":3,\"outcome\":\"filled\"}\n{\"time\":4,\"outc",
    );
    assert_eq!(
        last_taker_run(&log).unwrap().unwrap().outcome,
        TakerOutcome::Filled
    );
    write_log(&log, "not json\n");
    assert_eq!(kinds(&w.once()), vec!["fill-error"]);
}

#[test]
fn a_failed_read_is_an_rpc_alert_that_observes_nothing_it_missed() {
    let mut w = World::at("rpc", THURSDAY);
    w.chain.fail_call = Some(1);
    let p = w.once();
    assert_eq!(kinds(&p), vec!["rpc"]);
    assert_eq!(
        p.observed,
        vec!["maker", "status", "halt"],
        "no chain scope"
    );
    assert_eq!(p.now, 1, "the host's clock");
    w.chain.fail_call = Some(2);
    let p = w.once();
    assert_eq!(only(&p, "rpc").scope, "rpc:tokens");
    assert!(!p
        .observed
        .iter()
        .any(|s| s == "inventory" || s == "allowance"));
}

fn a(kind: &'static str, subject: &str) -> Alert {
    Alert {
        kind,
        scope: kind.to_owned(),
        subject: subject.to_owned(),
        message: format!("{kind} {subject}"),
    }
}

#[test]
fn an_active_alert_is_delivered_again_after_an_hour() {
    let mut book = AlertBook::default();
    let x = a("balance", "payer");
    let observed = vec!["balance".to_owned()];
    let (due, _) = book.update(std::slice::from_ref(&x), &observed, 1_000, REALERT_SECS);
    assert_eq!(due, vec![x.clone()]);
    book.delivered(&x, 1_000);
    let (due, _) = book.update(
        std::slice::from_ref(&x),
        &observed,
        1_000 + REALERT_SECS - 1,
        REALERT_SECS,
    );
    assert!(due.is_empty());
    let (due, _) = book.update(
        std::slice::from_ref(&x),
        &observed,
        1_000 + REALERT_SECS,
        REALERT_SECS,
    );
    assert_eq!(due, vec![x.clone()]);
    // Resolved only once its scope was observed without it.
    let (_, resolved) = book.update(&[], &[], 9_000, REALERT_SECS);
    assert!(resolved.is_empty());
    let (_, resolved) = book.update(&[], &observed, 9_001, REALERT_SECS);
    assert_eq!(resolved, vec![("balance", "payer".to_owned())]);
    assert_eq!(book.active(), 0);
}

#[test]
fn a_failed_delivery_is_retried_at_the_next_pass() {
    let mut book = AlertBook::default();
    let x = a("halted", "maker");
    let (due, _) = book.update(std::slice::from_ref(&x), &[], 10, REALERT_SECS);
    assert_eq!(due.len(), 1);
    // Not delivered: due again at once.
    let (due, _) = book.update(std::slice::from_ref(&x), &[], 70, REALERT_SECS);
    assert_eq!(due.len(), 1);
    // Unobserved and unreported, it stays active, marked last known.
    book.delivered(&x, 70);
    let (due, _) = book.update(&[], &[], 70 + REALERT_SECS, REALERT_SECS);
    assert!(due[0]
        .message
        .ends_with("(last known: not observed this pass)"));
}

#[test]
fn the_alert_command_gets_the_alert_in_its_environment() {
    let dir = scratch("deliver");
    let out = dir.join("alerts.log");
    let cmd = format!(
        "printf '%s|%s|%s\\n' \"$QUOTEBOOK_ALERT_KIND\" \"$QUOTEBOOK_ALERT_SUBJECT\" \"$QUOTEBOOK_ALERT_MESSAGE\" >> '{}'",
        out.display()
    );
    let x = Alert {
        kind: "maker-down",
        scope: "maker".into(),
        subject: "142G".into(),
        message: "no answer; $(touch pwned)".into(),
    };
    assert!(deliver(Some(&cmd), &x));
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "maker-down|142G|no answer; $(touch pwned)\n"
    );
    assert!(
        !dir.join("pwned").exists(),
        "the message is data, never run"
    );
    assert!(!deliver(Some("exit 3"), &x));
    assert!(deliver(None, &x));
}

#[test]
fn the_output_rotates_by_size_without_splitting_a_line() {
    let dir = scratch("rotate");
    let path = dir.join("watch.jsonl");
    let mut out = LineOut::File(RotatingFile::open(&path, 100, 2).unwrap());
    for i in 0..20 {
        out.line(&serde_json::json!({"pass": i, "pad": "xxxxxxxxxxxxxxxxxxxx"}))
            .unwrap();
    }
    let mut files = 0;
    for p in [
        path.clone(),
        dir.join("watch.jsonl.1"),
        dir.join("watch.jsonl.2"),
    ] {
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.len() <= 100);
        for line in text.lines() {
            serde_json::from_str::<Value>(line).unwrap();
        }
        files += 1;
    }
    assert_eq!(files, 3);
    assert!(
        !dir.join("watch.jsonl.3").exists(),
        "only 2 old files are kept"
    );
}

#[test]
fn the_alert_log_is_one_line_per_alert_and_rotates() {
    let x = Alert {
        kind: "maker-down",
        scope: "maker".into(),
        subject: "142G".into(),
        message: "no answer\nforged line\r".into(),
    };
    // 2026-11-13T21:05:09Z.
    assert_eq!(
        alert_log_line(&x, 1_794_603_909),
        "2026-11-13T21:05:09Z maker-down 142G no answer forged line \n"
    );
    let dir = scratch("alert-log");
    let path = dir.join("alerts.log");
    let line_len = u64::try_from(alert_log_line(&x, 1_794_603_909).len()).unwrap();
    let mut log = RotatingFile::open(&path, line_len * 2, 2).unwrap();
    for i in 0..7 {
        assert!(log_alert(&mut log, &x, 1_794_603_909 + i));
    }
    let lines = |p: &std::path::Path| std::fs::read_to_string(p).unwrap().lines().count();
    // Two lines a file, two old files kept: the oldest are dropped, no line is split.
    assert_eq!(lines(&path), 1);
    assert_eq!(lines(&dir.join("alerts.log.1")), 2);
    assert_eq!(lines(&dir.join("alerts.log.2")), 2);
    assert!(!dir.join("alerts.log.3").exists());
}

fn resolved_after(w: &mut World, book: &mut AlertBook, at: i64) -> Vec<(&'static str, String)> {
    let p = w.once();
    book.update(&p.alerts, &p.observed, at, REALERT_SECS).1
}

#[test]
fn a_failed_read_does_not_resolve_collateral() {
    let mut book = AlertBook::default();
    let mut w = World::at("collateral-read", THURSDAY);
    w.usdc(4_999_999, 4_000_000);
    let p = w.once();
    let (due, _) = book.update(&p.alerts, &p.observed, 100, REALERT_SECS);
    assert!(due.iter().any(|a| a.kind == "collateral"));
    // The writers cannot be read.
    w.chain.fail_writers = true;
    let resolved = resolved_after(&mut w, &mut book, 160);
    assert!(
        !resolved.iter().any(|(k, _)| *k == "collateral"),
        "{resolved:?}"
    );
    w.chain.fail_writers = false;
    // The token accounts cannot be read: the second RPC read of the pass.
    w.chain.fail_call = Some(2);
    let resolved = resolved_after(&mut w, &mut book, 220);
    assert!(
        !resolved.iter().any(|(k, _)| *k == "collateral"),
        "{resolved:?}"
    );
    w.chain.fail_call = None;
    // Read again with enough USDC: now it resolves.
    w.usdc(14_000_000, 4_000_000);
    let resolved = resolved_after(&mut w, &mut book, 280);
    assert!(
        resolved.iter().any(|(k, _)| *k == "collateral"),
        "{resolved:?}"
    );
}

#[test]
fn a_failed_status_does_not_resolve_a_service_halt() {
    let mut book = AlertBook::default();
    let mut w = World::at("halted-read", THURSDAY);
    let mut s = healthy_status();
    s.halted = true;
    w.maker = Maker(Ok(s));
    let p = w.once();
    let (due, _) = book.update(&p.alerts, &p.observed, 100, REALERT_SECS);
    assert!(due.iter().any(|a| a.kind == "halted"));
    w.maker = Maker(Err("no answer within 2 s".into()));
    let resolved = resolved_after(&mut w, &mut book, 160);
    assert!(
        !resolved.iter().any(|(k, _)| *k == "halted"),
        "{resolved:?}"
    );
    w.maker = Maker(Ok(healthy_status()));
    let resolved = resolved_after(&mut w, &mut book, 220);
    assert!(resolved.iter().any(|(k, _)| *k == "halted"), "{resolved:?}");
}

#[test]
fn an_rpc_outage_still_reports_the_maker_and_the_halt_file() {
    let mut w = World::at("outage", THURSDAY);
    w.chain.fail_call = Some(1);
    w.maker = Maker(Err("no answer within 2 s".into()));
    std::fs::write(&w.watched.halt_file, b"").unwrap();
    let p = w.once();
    let mut got = kinds(&p);
    got.sort_unstable();
    assert_eq!(got, vec!["halted", "maker-down", "rpc"]);
}

#[test]
fn a_hung_alert_command_is_killed() {
    let x = a("balance", "payer");
    let started = std::time::Instant::now();
    assert!(!deliver_within(
        Some("sleep 30"),
        &x,
        std::time::Duration::from_millis(100)
    ));
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}
