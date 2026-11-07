//! Shared test support: a fake clock, spot and chain, the desk's configuration, and a builder
//! of fill transactions against the Order Engine's published account layout
//! (`idls/order_engine.json` at toolkit `3aa4fce`). Test keys are generated here; none is
//! stored anywhere.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::integer_division,
    missing_docs
)]

mod desk;
mod http;
mod validator;

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use quotebook_maker::pricing::PricingParams;
use rust_decimal::Decimal;
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;

use crate::api::{Protocol, QuoteRequest, QuoteResponse, QuoteType, SwapRequest};
use crate::desk::{
    ChainError, ChainView, Clock, Desk, DeskConfig, Landed, Limits, Market, Snapshot, Spent, Spot,
    SpotSource, Start, TxId,
};
use crate::pricing::PricingInputs;
use crate::programs::{
    ComputeBudget, Fill, ASSOCIATED_TOKEN, COMPUTE_BUDGET, LIGHTHOUSE, ORDER_ENGINE, SYSTEM, TOKEN,
};
use crate::tx::{CompiledInstruction, Header, LookupTable, Message, Transaction, Version};
use crate::validate::FeeBudget;

pub const NOW: i64 = 1_794_000_000;
pub const MAKER: Address = Address::new_from_array([7; 32]);
pub const BASE: Address = Address::new_from_array([11; 32]);
pub const QUOTE: Address = Address::new_from_array([12; 32]);
pub const MAKER_BASE: Address = Address::new_from_array([21; 32]);
pub const MAKER_QUOTE: Address = Address::new_from_array([22; 32]);
pub const TAKER_BASE: Address = Address::new_from_array([31; 32]);
pub const TAKER_QUOTE: Address = Address::new_from_array([32; 32]);
pub const MID: i64 = 150;
pub const SOL: u64 = 20_000_000;
pub const FLOOR: u64 = 10_000_000;

#[derive(Clone)]
pub struct TestClock(pub Arc<AtomicI64>);

impl TestClock {
    pub fn new() -> Self {
        Self(Arc::new(AtomicI64::new(NOW)))
    }
    pub fn set(&self, t: i64) {
        self.0.store(t, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A mid published at the clock's time, optionally slow to answer.
pub struct TestSpot {
    pub clock: TestClock,
    pub delay_ms: Arc<AtomicU64>,
    pub age: Arc<AtomicI64>,
}

impl SpotSource for TestSpot {
    fn spot(&self, index: usize) -> Option<Spot> {
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
        (index == 0).then(|| Spot {
            mid: Decimal::from(MID),
            published_at: self.clock.now() - self.age.load(Ordering::SeqCst),
        })
    }
}

/// A chain whose snapshot tests edit.
pub struct FakeChain {
    pub snapshot: Mutex<Snapshot>,
    pub history: Mutex<Result<Vec<Spent>, ChainError>>,
    pub fail: Mutex<bool>,
}

impl FakeChain {
    pub fn new(clock: i64) -> Self {
        Self {
            snapshot: Mutex::new(Snapshot {
                slot: 1,
                clock,
                sol_lamports: SOL,
                balances: vec![(BASE, 10_000_000_000), (QUOTE, 2_000_000_000)],
                landed: Vec::new(),
            }),
            history: Mutex::new(Ok(Vec::new())),
            fail: Mutex::new(false),
        }
    }

    /// Moves to a later slot at `clock`.
    pub fn at(&self, clock: i64) {
        let mut s = self.snapshot.lock().unwrap();
        s.slot += 1;
        s.clock = clock;
    }

    pub fn land(&self, id: TxId, fee: u64, at: i64, failed: bool) {
        self.snapshot.lock().unwrap().landed.push(Landed {
            id,
            fee_lamports: fee,
            at,
            failed,
        });
    }
}

impl ChainView for FakeChain {
    fn snapshot(&self, _tracked: &[TxId]) -> Result<Snapshot, ChainError> {
        if *self.fail.lock().unwrap() {
            return Err(ChainError("down".into()));
        }
        Ok(self.snapshot.lock().unwrap().clone())
    }
    fn history_since(&self, _since: i64) -> Result<Vec<Spent>, ChainError> {
        self.history.lock().unwrap().clone()
    }
}

pub fn budget() -> FeeBudget {
    FeeBudget {
        max_cu_limit: 400_000,
        max_cu_price: 1_000_000,
        // 3 signatures and the largest limit at the largest price: 15,000 + 400,000.
        max_fee_lamports: 415_000,
        lamports_per_signature: 5_000,
    }
}

pub fn halt_path(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("jupiter-rfq-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

pub fn config(name: &str) -> DeskConfig {
    DeskConfig {
        maker: MAKER,
        markets: vec![Market {
            base_mint: BASE,
            quote_mint: QUOTE,
            maker_base_account: MAKER_BASE,
            maker_quote_account: MAKER_QUOTE,
            pricing: PricingInputs {
                base_decimals: 9,
                quote_decimals: 6,
                params: PricingParams::relative_defaults(),
            },
            max_base_out: 5_000_000_000,
            max_quote_out: 1_000_000_000,
            target_base_atoms: 10_000_000_000,
        }],
        limits: Limits {
            max_quotes: 100,
            quote_life_secs: 55,
            min_landing_secs: 20,
            max_snapshot_age_secs: 60,
            max_spot_age_secs: 30,
            max_open_swaps: 4,
            max_fills_per_hour: 10,
            max_fees_per_hour: 2_000_000,
            sol_floor: FLOOR,
            halt_file: halt_path(name),
        },
        budget: budget(),
    }
}

pub struct Rig {
    pub clock: TestClock,
    pub delay_ms: Arc<AtomicU64>,
    pub spot_age: Arc<AtomicI64>,
    pub chain: FakeChain,
    pub desk: Arc<Desk>,
}

pub fn rig_with(cfg: DeskConfig, start: Start) -> Rig {
    let clock = TestClock::new();
    let delay_ms = Arc::new(AtomicU64::new(0));
    let spot_age = Arc::new(AtomicI64::new(0));
    let desk = Arc::new(Desk::new(
        cfg,
        Box::new(TestSpot {
            clock: clock.clone(),
            delay_ms: Arc::clone(&delay_ms),
            age: Arc::clone(&spot_age),
        }),
        Box::new(clock.clone()),
        start,
    ));
    let chain = FakeChain::new(NOW);
    desk.reconcile(&chain).unwrap();
    Rig {
        clock,
        delay_ms,
        spot_age,
        chain,
        desk,
    }
}

/// A fresh desk with its first snapshot applied.
pub fn rig(name: &str) -> Rig {
    rig_with(config(name), Start::Fresh)
}

pub fn quote_request(
    id: &str,
    taker: &Keypair,
    side_buy_base: bool,
    qt: QuoteType,
    amount: u64,
) -> QuoteRequest {
    let (token_in, token_out) = if side_buy_base {
        // The taker buys base: it pays quote.
        (QUOTE, BASE)
    } else {
        (BASE, QUOTE)
    };
    QuoteRequest {
        request_id: format!("req-{id}"),
        quote_id: id.to_owned(),
        token_in: token_in.to_string(),
        amount: amount.to_string(),
        token_out: token_out.to_string(),
        quote_type: qt,
        protocol: Protocol::V1,
        taker: Some(taker.pubkey().to_string()),
        suggested_prioritization_fees: Some(10_000),
        fee_bps: 2,
        is_wsol: None,
        receiver: None,
        source: None,
    }
}

/// One instruction before compilation.
#[derive(Clone)]
pub struct Ix {
    pub program: Address,
    pub accounts: Vec<Address>,
    pub data: Vec<u8>,
}

/// A fill transaction for a quote, valid by default; tests then forge it.
#[derive(Clone)]
pub struct FillTx {
    pub fee_payer: Address,
    /// Signers after the fee payer, in order; the taker signs, others stay zero.
    pub other_signers: Vec<Address>,
    pub instructions: Vec<Ix>,
    pub lookups: Vec<LookupTable>,
}

impl FillTx {
    /// The canonical fill of `quote` by `taker`, expiring 55 s after `NOW`.
    pub fn for_quote(quote: &QuoteResponse, taker: &Keypair) -> Self {
        let token_in: Address = quote.token_in.parse().unwrap();
        let token_out: Address = quote.token_out.parse().unwrap();
        let (taker_in, maker_in, taker_out, maker_out) = if token_in == QUOTE {
            (TAKER_QUOTE, MAKER_QUOTE, TAKER_BASE, MAKER_BASE)
        } else {
            (TAKER_BASE, MAKER_BASE, TAKER_QUOTE, MAKER_QUOTE)
        };
        let fill = Fill {
            input_amount: quote.amount_in.parse().unwrap(),
            output_amount: quote.amount_out.parse().unwrap(),
            expire_at: NOW + 55,
            fee_bps: 2,
            flags: match quote.quote_type {
                QuoteType::ExactIn => 0,
                QuoteType::ExactOut => 1,
            },
            integrator_fee_bps: 0,
        };
        let t = taker.pubkey();
        Self {
            fee_payer: MAKER,
            other_signers: vec![t],
            instructions: vec![
                Ix {
                    program: COMPUTE_BUDGET,
                    accounts: vec![],
                    data: ComputeBudget::SetComputeUnitLimit(200_000).encode(),
                },
                Ix {
                    program: COMPUTE_BUDGET,
                    accounts: vec![],
                    data: ComputeBudget::SetComputeUnitPrice(10_000).encode(),
                },
                Ix {
                    program: ASSOCIATED_TOKEN,
                    accounts: vec![t, taker_out, t, token_out, SYSTEM, TOKEN],
                    data: vec![1],
                },
                Ix {
                    program: ORDER_ENGINE,
                    accounts: vec![
                        t, MAKER, taker_in, maker_in, taker_out, maker_out, token_in, TOKEN,
                        token_out, TOKEN, SYSTEM,
                    ],
                    data: fill.encode(),
                },
            ],
            lookups: Vec::new(),
        }
    }

    pub fn fill_index(&self) -> usize {
        self.instructions
            .iter()
            .position(|i| i.program == ORDER_ENGINE)
            .unwrap()
    }

    pub fn edit_fill(&mut self, f: impl FnOnce(&mut Fill)) {
        let i = self.fill_index();
        let mut fill = Fill::decode(&self.instructions[i].data).unwrap();
        f(&mut fill);
        self.instructions[i].data = fill.encode();
    }

    pub fn lighthouse(discriminator: u8) -> Ix {
        Ix {
            program: LIGHTHOUSE,
            accounts: vec![BASE],
            data: vec![discriminator, 0, 0],
        }
    }

    /// Compiles and signs with `taker` (the maker's slot stays zero).
    pub fn build(&self, taker: &Keypair) -> Transaction {
        let mut keys = vec![self.fee_payer];
        for s in &self.other_signers {
            if !keys.contains(s) {
                keys.push(*s);
            }
        }
        let signers = keys.len();
        let intern = |keys: &mut Vec<Address>, a: &Address| -> u8 {
            if let Some(i) = keys.iter().position(|k| k == a) {
                return i as u8;
            }
            keys.push(*a);
            (keys.len() - 1) as u8
        };
        let mut compiled = Vec::new();
        for ix in &self.instructions {
            let program_index = intern(&mut keys, &ix.program);
            let accounts = ix.accounts.iter().map(|a| intern(&mut keys, a)).collect();
            compiled.push(CompiledInstruction {
                program_index,
                accounts,
                data: ix.data.clone(),
            });
        }
        let message = Message {
            version: Version::V0,
            header: Header {
                num_required_signatures: signers as u8,
                num_readonly_signed: 0,
                num_readonly_unsigned: 0,
            },
            account_keys: keys.clone(),
            recent_blockhash: [9; 32],
            instructions: compiled,
            lookups: self.lookups.clone(),
        };
        let bytes = message.serialize();
        let mut signatures = vec![[0u8; 64]; signers];
        if let Some(i) = keys.iter().position(|k| *k == taker.pubkey()) {
            if i < signers {
                let sig = taker.sign_message(&bytes);
                signatures[i] = <[u8; 64]>::try_from(sig.as_ref()).unwrap();
            }
        }
        let tx = Transaction {
            signatures,
            message,
            message_bytes: bytes,
        };
        // Through the wire and back, so the parser sees exactly what Jupiter would send.
        Transaction::from_base64(&tx.to_base64()).unwrap()
    }

    pub fn request(&self, quote_id: &str, taker: &Keypair) -> SwapRequest {
        SwapRequest {
            request_id: format!("swap-{quote_id}"),
            quote_id: quote_id.to_owned(),
            transaction: self.build(taker).to_base64(),
            source: None,
        }
    }
}
