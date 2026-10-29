//! Local demo: two maker services and a taker CLI flow against quotebook in LiteSVM.
//!
//! 1. LiteSVM with the program (`make program` first), a series of two 6/6-decimal SPL mints,
//!    two makers with approved delegates, and a taker.
//! 2. Each maker runs the WebSocket service on localhost, pricing with market-maker-rs from
//!    a configured mid with a different risk aversion, so the two asks differ.
//! 3. The taker asks both for a firm quote to buy, verifies each (signature and every term
//!    the program checks), ranks them, and settles the best through the SDK.
//!
//! Run with `cargo run -p quotebook-maker --example demo`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    missing_docs
)]

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use litesvm::LiteSVM;
use quotebook_maker::client::{self, Expectation, SignedQuote};
use quotebook_maker::issuer::NonceIssuer;
use quotebook_maker::keys::MakerKey;
use quotebook_maker::market::MarketInfo;
use quotebook_maker::pricing::PricingParams;
use quotebook_maker::protocol::{QuoteRequest, TakerSide};
use quotebook_maker::server::{FixedInventory, FixedMid, FixedTime, MakerService, ServiceConfig};
use quotebook_sdk::core::instruction::CreateSeriesArgs;
use quotebook_sdk::core::{BaseLots, Nonce, Seconds, Side, UnixTimestamp};
use quotebook_sdk::{decode, ix, pda, SettleAccounts};
use rust_decimal::Decimal;
use solana_account::Account;
use solana_address::Address;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

pub const PROGRAM_ID: Address = Address::new_from_array([0x51; 32]);
pub const NOW: i64 = 1_700_000_000;
const FUNDS: u64 = 1_000_000_000_000;

fn token_program() -> Address {
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        .parse()
        .unwrap()
}

struct Vm {
    svm: LiteSVM,
    payer: Keypair,
}

impl Vm {
    fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) {
        self.svm.expire_blockhash();
        let mut all = vec![&self.payer];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(
            ixs,
            Some(&self.payer.pubkey()),
            &all,
            self.svm.latest_blockhash(),
        );
        if let Err(f) = self.svm.send_transaction(tx) {
            panic!("{:?}\n{}", f.err, f.meta.pretty_logs());
        }
    }

    fn set(&mut self, address: Address, data: Vec<u8>) {
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        let account = Account {
            lamports,
            data,
            owner: token_program(),
            executable: false,
            rent_epoch: 0,
        };
        self.svm.set_account(address, account).unwrap();
    }

    fn mint(&mut self) -> Address {
        let address = Keypair::new().pubkey();
        let mut d = vec![0u8; 82];
        d[44] = 6;
        d[45] = 1;
        self.set(address, d);
        address
    }

    fn token_account(&mut self, mint: &Address, owner: &Address) -> Address {
        let address = Keypair::new().pubkey();
        let mut d = vec![0u8; 165];
        d[0..32].copy_from_slice(mint.as_ref());
        d[32..64].copy_from_slice(owner.as_ref());
        d[64..72].copy_from_slice(&FUNDS.to_le_bytes());
        d[108] = 1;
        self.set(address, d);
        address
    }

    fn amount(&self, account: &Address) -> u64 {
        let data = self.svm.get_account(account).unwrap().data;
        u64::from_le_bytes(data[64..72].try_into().unwrap())
    }
}

/// What the demo did, for the test and the printout.
#[derive(Debug)]
pub struct Report {
    pub quotes: Vec<(Address, u64)>,
    pub winner: Address,
    pub price_ticks: u64,
    pub taker_base_delta: i128,
    pub taker_quote_delta: i128,
}

struct MakerSetup {
    key: Keypair,
    base: Address,
    quote: Address,
}

pub async fn run() -> Report {
    // 1. Chain: program, config, series, makers, taker.
    let mut svm = LiteSVM::new();
    let so =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/deploy/quotebook_program.so");
    svm.add_program_from_file(PROGRAM_ID, &so)
        .unwrap_or_else(|e| panic!("load {}: {e}; run `make program` first", so.display()));
    let payer = Keypair::new();
    let admin = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    svm.airdrop(&admin.pubkey(), 1_000_000_000).unwrap();
    let mut clock: solana_clock::Clock = svm.get_sysvar();
    clock.unix_timestamp = NOW;
    svm.set_sysvar(&clock);
    let program_data = pda::program_data(&PROGRAM_ID).0;
    let mut pd = svm.get_account(&program_data).unwrap();
    pd.data[12] = 1;
    pd.data[13..45].copy_from_slice(admin.pubkey().as_ref());
    svm.set_account(program_data, pd).unwrap();
    let mut vm = Vm { svm, payer };
    let payer = vm.payer.pubkey();

    let deployment = quotebook_sdk::deployment_id_from_genesis_hash(&[42; 32]);
    vm.send(
        &[ix::init_config(
            &PROGRAM_ID,
            &admin.pubkey(),
            &payer,
            deployment,
        )],
        &[&admin],
    );
    let base_mint = vm.mint();
    let quote_mint = vm.mint();
    // 1 base lot = 0.01 base token; 1 quote lot = 0.0001 quote token.
    let args = CreateSeriesArgs {
        base_lot_size: 10_000,
        quote_lot_size: 100,
        max_quote_ttl: 60,
        min_size: 1,
    };
    vm.send(
        &[ix::create_series(
            &PROGRAM_ID,
            &admin.pubkey(),
            &payer,
            &base_mint,
            &quote_mint,
            0,
            &args,
        )],
        &[&admin],
    );
    let series = pda::series(&PROGRAM_ID, &base_mint, &quote_mint, 0).0;
    let s = vm.svm.get_account(&series).unwrap();
    let s = decode::series(&PROGRAM_ID, &s.owner, &s.data).unwrap();

    let mut makers = Vec::new();
    for _ in 0..2 {
        let key = Keypair::new();
        vm.svm.airdrop(&key.pubkey(), 1_000_000_000).unwrap();
        vm.send(
            &[ix::init_maker(&PROGRAM_ID, &key.pubkey(), &payer)],
            &[&key],
        );
        let base = vm.token_account(&base_mint, &key.pubkey());
        let quote = vm.token_account(&quote_mint, &key.pubkey());
        // The taker buys, so the maker pays base: approve its delegate on the base account.
        vm.send(
            &[ix::approve_delegate(
                &PROGRAM_ID,
                &token_program(),
                &base,
                &key.pubkey(),
                FUNDS,
            )],
            &[&key],
        );
        makers.push(MakerSetup { key, base, quote });
    }
    let taker = Keypair::new();
    vm.svm.airdrop(&taker.pubkey(), 1_000_000_000).unwrap();
    let taker_base = vm.token_account(&base_mint, &taker.pubkey());
    let taker_quote = vm.token_account(&quote_mint, &taker.pubkey());

    // 2. One service per maker, on localhost.
    let market = MarketInfo {
        program_id: PROGRAM_ID,
        deployment: s.deployment_id,
        series,
        base_decimals: 6,
        quote_decimals: 6,
        base_lot_size: s.base_lot_size.get(),
        quote_lot_size: s.quote_lot_size.get(),
        max_quote_ttl: s.max_quote_ttl,
        min_size: s.min_size,
    };
    let d = |v: &str| Decimal::from_str(v).unwrap();
    let mut urls = Vec::new();
    for (i, maker) in makers.iter().enumerate() {
        let state = pda::maker_state(&PROGRAM_ID, &maker.key.pubkey()).0;
        let state = vm.svm.get_account(&state).unwrap();
        let state = decode::maker_state(&PROGRAM_ID, &state.owner, &state.data).unwrap();
        let config = ServiceConfig {
            market,
            pricing: PricingParams {
                risk_aversion: if i == 0 { d("0.1") } else { d("0.5") },
                volatility: d("0.6"),
                time_to_terminal_ms: 3_600_000,
                order_intensity: d("1.5"),
                max_deviation: d("0.05"),
            },
            ttl: Seconds::new(30),
            max_size: BaseLots::new(1_000),
        };
        let service = MakerService::new(
            MakerKey::new(maker.key.insecure_clone()),
            config,
            NonceIssuer::new(state.nonces.window_base()),
            Box::new(FixedMid(d("25.00"))),
            Box::new(FixedInventory(Decimal::ZERO)),
            Box::new(FixedTime(UnixTimestamp::new(NOW))),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        urls.push(format!("ws://{}", listener.local_addr().unwrap()));
        tokio::spawn(Arc::new(service).serve(listener));
    }

    // 3. The taker: request, verify, rank, settle the best.
    let size = BaseLots::new(40);
    let request = QuoteRequest {
        request_id: 1,
        series: series.to_string(),
        taker: taker.pubkey().to_string(),
        taker_side: TakerSide::Buy,
        size_lots: size.get(),
    };
    let expected = Expectation {
        program_id: PROGRAM_ID,
        deployment,
        series,
        taker: taker.pubkey(),
        taker_side: TakerSide::Buy,
        size,
        now: UnixTimestamp::new(NOW),
        max_quote_ttl: s.max_quote_ttl,
    };
    let mut quotes: Vec<SignedQuote> = Vec::new();
    for url in &urls {
        let q = client::request_quote(url, &request).await.unwrap();
        client::verify(&q, &expected).unwrap();
        quotes.push(q);
    }
    let summary = quotes
        .iter()
        .map(|q| (q.quote.terms().maker, q.quote.terms().price.get()))
        .collect();
    let best = client::rank(quotes, TakerSide::Buy)[0];
    let winner = best.quote.terms().maker;
    assert_eq!(best.quote.terms().side, Side::Ask);
    let m = makers.iter().find(|m| m.key.pubkey() == winner).unwrap();

    let accounts = SettleAccounts {
        taker: taker.pubkey(),
        base_mint,
        quote_mint,
        base_token_program: token_program(),
        quote_token_program: token_program(),
        maker_base: m.base,
        maker_quote: m.quote,
        taker_base,
        taker_quote,
    };
    let (b0, q0) = (vm.amount(&taker_base), vm.amount(&taker_quote));
    let pair = quotebook_sdk::settle_instructions(
        &PROGRAM_ID,
        &best.quote,
        &best.signature,
        &accounts,
        size,
        0,
    );
    vm.send(&pair, &[&taker]);
    let (b1, q1) = (vm.amount(&taker_base), vm.amount(&taker_quote));
    let state = pda::maker_state(&PROGRAM_ID, &winner).0;
    let state = vm.svm.get_account(&state).unwrap();
    let state = decode::maker_state(&PROGRAM_ID, &state.owner, &state.data).unwrap();
    assert_eq!(state.fills, 1);
    assert!(state
        .nonces
        .check(Nonce::new(best.quote.terms().nonce.get()))
        .is_err());

    Report {
        quotes: summary,
        winner,
        price_ticks: best.quote.terms().price.get(),
        taker_base_delta: i128::from(b1) - i128::from(b0),
        taker_quote_delta: i128::from(q1) - i128::from(q0),
    }
}

#[allow(dead_code)]
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();
    let r = run().await;
    for (maker, ticks) in &r.quotes {
        println!("quote from {maker}: {ticks} ticks");
    }
    println!(
        "settled with {}: {} ticks, taker base {:+} atoms, quote {:+} atoms",
        r.winner, r.price_ticks, r.taker_base_delta, r.taker_quote_delta
    );
}
