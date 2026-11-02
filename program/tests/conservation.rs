//! Stateful property test of the program (docs/PLAN-improvements.md, I6).
//!
//! Random sequences of `settle` (both sides, SPL Token and Token-2022), `cancel_all`,
//! `cancel_nonce` and `set_series_paused` run across 2 makers, 2 takers and 2 series. Each
//! fill is either valid or carries one mutation: a flipped signature byte, a quote for the
//! other series, an expired quote, a replayed or cancelled nonce, an over-size fill, a quote
//! bound to the other taker, or a swapped token account.
//!
//! A pure Rust model gets each step before it runs and **predicts** its outcome: success, or
//! the exact error code. The test asserts that the observed outcome equals the prediction, so a
//! regression that rejects every valid `settle` fails at once. After every step it checks
//! against the model:
//! - conservation: the total of each mint across all token accounts is constant;
//! - a successful fill moves exactly `fill × base_lot` and `price × fill × quote_lot`, and
//!   nothing else changes;
//! - delegate allowances drop by exactly the maker's outflow;
//! - the nonce window (base, last cancel_all, used set) and `fills` equal the model's;
//! - a rejected transaction leaves every program and token account byte-identical. The fee
//!   payer's lamports are excluded from that: a rejected transaction still pays its signature
//!   fee, which the model charges to the payer explicitly (5,000 lamports per signature,
//!   precompile signatures included).
//!
//! `QUOTEBOOK_PROPTEST_CASES` sets the number of sequences (default 24 in `make pre-push`;
//! `make fuzz-long` runs 10,000). The run fails if any (side, token program) pair never filled.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    clippy::disallowed_macros,
    clippy::panic_in_result_fn,
    clippy::result_large_err,
    clippy::too_many_lines,
    missing_docs
)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use common::*;
use litesvm::types::{FailedTransactionMetadata, TransactionMetadata};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed, TestRunner};
use quotebook_core::{
    BaseLots, Nonce, Quote, QuoteLotsPerBaseLot, QuoteTerms, QuotebookError as E, SeriesId, Side,
    UnixTimestamp, WINDOW_BITS,
};
use solana_address::Address;
use solana_instruction::error::InstructionError;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::TransactionError;

const BASE_LOT: u64 = 1_000;
const QUOTE_LOT: u64 = 10;
const SIG_FEE: u64 = 5_000;

// ---------------------------------------------------------------------------------------
// The model.

/// The nonce rules of docs/PLAN.md 4.2, as in quotebook-core/tests/nonce_window_model.rs.
#[derive(Default, Clone)]
struct NonceModel {
    base: u64,
    last_cancel_all: u64,
    used: BTreeSet<u64>,
}

impl NonceModel {
    fn check(&self, n: u64) -> Result<(), E> {
        if n < self.base {
            return Err(if n < self.last_cancel_all {
                E::NonceCancelled
            } else {
                E::NonceOutOfWindow
            });
        }
        if self.used.contains(&n) {
            return Err(E::NonceUsed);
        }
        Ok(())
    }

    fn slide(&mut self, new_base: u64) {
        self.base = new_base;
        self.used = self.used.split_off(&new_base);
    }

    fn consume(&mut self, n: u64) -> Result<(), E> {
        self.check(n)?;
        if n - self.base >= WINDOW_BITS {
            self.slide(n - (WINDOW_BITS - 1));
        }
        self.used.insert(n);
        Ok(())
    }

    fn cancel_all(&mut self, m: u64) -> Result<(), E> {
        if m <= self.base {
            return Err(E::WatermarkNotIncreasing);
        }
        self.slide(m);
        self.last_cancel_all = m;
        Ok(())
    }

    fn next_fresh(&self) -> u64 {
        self.used
            .iter()
            .next_back()
            .map_or(self.base, |n| n + 1)
            .max(self.base)
    }
}

/// What a step is predicted to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Success,
    /// Instruction `ix` failed with quotebook error `code`.
    Err {
        ix: u8,
        code: u32,
    },
    /// The ed25519 precompile (instruction 0) rejected the signature.
    Precompile,
}

fn err(ix: u8, e: E) -> Outcome {
    Outcome::Err { ix, code: e.code() }
}

// ---------------------------------------------------------------------------------------
// Generated operations.

#[derive(Debug, Clone, Copy)]
enum NoncePick {
    Fresh,
    /// The k-th used nonce (wrapping), or fresh if none.
    Reuse(u8),
    /// `base - 1 - k`, saturating at 0.
    Below(u8),
    /// `base + WINDOW_BITS + k`: forces a slide.
    Beyond(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mutation {
    None,
    FlipSigByte(u8),
    OtherSeries,
    Expired,
    OverSize,
    ZeroFill,
    OtherTaker,
    SwapTakerAccount,
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Fill {
        series: usize,
        maker: usize,
        taker: usize,
        ask: bool,
        price: u64,
        size: u64,
        fill: u64,
        bind_taker: bool,
        nonce: NoncePick,
        mutation: Mutation,
    },
    CancelAll {
        maker: usize,
        nonce: NoncePick,
    },
    CancelNonce {
        maker: usize,
        nonce: NoncePick,
    },
    Pause {
        series: usize,
        paused: bool,
    },
}

fn nonce_pick() -> impl Strategy<Value = NoncePick> {
    prop_oneof![
        6 => Just(NoncePick::Fresh),
        3 => any::<u8>().prop_map(NoncePick::Reuse),
        1 => any::<u8>().prop_map(NoncePick::Below),
        1 => any::<u16>().prop_map(NoncePick::Beyond),
    ]
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        8 => Just(Mutation::None),
        1 => any::<u8>().prop_map(Mutation::FlipSigByte),
        1 => Just(Mutation::OtherSeries),
        1 => Just(Mutation::Expired),
        1 => Just(Mutation::OverSize),
        1 => Just(Mutation::ZeroFill),
        1 => Just(Mutation::OtherTaker),
        1 => Just(Mutation::SwapTakerAccount),
    ]
}

fn fill_op(
    series: impl Strategy<Value = usize>,
    ask: impl Strategy<Value = bool>,
    nonce: impl Strategy<Value = NoncePick>,
    mutation: impl Strategy<Value = Mutation>,
) -> impl Strategy<Value = Op> {
    (
        series,
        0usize..2,
        0usize..2,
        ask,
        1u64..1_000,
        1u64..50,
        any::<u64>(),
        any::<bool>(),
        nonce,
        mutation,
    )
        .prop_map(
            |(series, maker, taker, ask, price, size, f, bind_taker, nonce, mutation)| Op::Fill {
                series,
                maker,
                taker,
                ask,
                price,
                size,
                fill: 1 + f % size,
                bind_taker,
                nonce,
                mutation,
            },
        )
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        12 => fill_op(0usize..2, any::<bool>(), nonce_pick(), mutation()),
        2 => (0usize..2, nonce_pick()).prop_map(|(maker, nonce)| Op::CancelAll { maker, nonce }),
        2 => (0usize..2, nonce_pick()).prop_map(|(maker, nonce)| Op::CancelNonce { maker, nonce }),
        1 => (0usize..2, any::<bool>()).prop_map(|(series, paused)| Op::Pause { series, paused }),
    ]
}

/// A random sequence plus one valid fill per (side, series) at random positions, so every
/// sequence offers a successful fill on each side and token program.
fn sequence() -> impl Strategy<Value = Vec<Op>> {
    let forced = |series: usize, ask: bool| {
        fill_op(
            Just(series),
            Just(ask),
            Just(NoncePick::Fresh),
            Just(Mutation::None),
        )
    };
    (
        prop::collection::vec(op(), 8..40),
        [
            forced(0, false),
            forced(0, true),
            forced(1, false),
            forced(1, true),
        ],
        prop::collection::vec(any::<prop::sample::Index>(), 4),
    )
        .prop_map(|(mut ops, forced, at)| {
            for (f, i) in forced.into_iter().zip(at) {
                let pos = i.index(ops.len() + 1);
                ops.insert(pos, f);
            }
            ops
        })
}

// ---------------------------------------------------------------------------------------
// The world.

struct SeriesW {
    address: Address,
    base_mint: Address,
    quote_mint: Address,
    program: Address,
    paused: bool,
}

/// A token account the model tracks.
#[derive(Clone)]
struct TokenModel {
    mint: Address,
    amount: u64,
    allowance: u64,
}

struct World {
    env: Env,
    series: Vec<SeriesW>,
    makers: Vec<Keypair>,
    takers: Vec<Keypair>,
    /// `accounts[(owner index, series, is_quote)]`; owners 0..2 makers, 2..4 takers.
    accounts: BTreeMap<(usize, usize, bool), Address>,
    tokens: BTreeMap<Address, TokenModel>,
    nonces: Vec<NonceModel>,
    fills: Vec<u64>,
    totals: BTreeMap<Address, u64>,
}

impl World {
    fn new() -> Self {
        let mut env = Env::new();
        env.set_clock(NOW);
        env.init_config();
        let mut series = Vec::new();
        for program in [token_program(), token_2022_program()] {
            let base_mint = Keypair::new().pubkey();
            env.set(base_mint, program, mint_data(&[]));
            let quote_mint = Keypair::new().pubkey();
            env.set(quote_mint, program, mint_data(&[]));
            let (address, _) = env.create_series_with(&base_mint, &quote_mint);
            series.push(SeriesW {
                address,
                base_mint,
                quote_mint,
                program,
                paused: false,
            });
        }
        let makers = vec![env.maker(), env.maker()];
        let takers = vec![Keypair::new(), Keypair::new()];
        for t in &takers {
            env.svm.airdrop(&t.pubkey(), 1_000_000_000).unwrap();
        }
        let mut accounts = BTreeMap::new();
        let mut tokens = BTreeMap::new();
        let mut totals = BTreeMap::new();
        for owner in 0..4 {
            let (key, delegate) = if owner < 2 {
                let k = makers[owner].pubkey();
                (k, Some(delegate_address(&k)))
            } else {
                (takers[owner - 2].pubkey(), None)
            };
            for (s, sw) in series.iter().enumerate() {
                for is_quote in [false, true] {
                    let mint = if is_quote {
                        sw.quote_mint
                    } else {
                        sw.base_mint
                    };
                    let allowance = if delegate.is_some() { FUNDS } else { 0 };
                    let address = env.token_account(
                        sw.program,
                        &mint,
                        &key,
                        FUNDS,
                        delegate.as_ref().map(|d| (d, FUNDS)),
                        &[],
                    );
                    accounts.insert((owner, s, is_quote), address);
                    tokens.insert(
                        address,
                        TokenModel {
                            mint,
                            amount: FUNDS,
                            allowance,
                        },
                    );
                    *totals.entry(mint).or_insert(0) += FUNDS;
                }
            }
        }
        Self {
            env,
            series,
            makers,
            takers,
            accounts,
            tokens,
            nonces: vec![NonceModel::default(), NonceModel::default()],
            fills: vec![0, 0],
            totals,
        }
    }

    fn pick(&self, maker: usize, p: NoncePick) -> u64 {
        let m = &self.nonces[maker];
        match p {
            NoncePick::Fresh => m.next_fresh(),
            NoncePick::Reuse(k) => {
                if m.used.is_empty() {
                    m.next_fresh()
                } else {
                    *m.used.iter().nth(usize::from(k) % m.used.len()).unwrap()
                }
            }
            NoncePick::Below(k) => m.base.saturating_sub(1 + u64::from(k)),
            NoncePick::Beyond(k) => m.base + WINDOW_BITS + u64::from(k),
        }
    }

    /// Every account whose bytes a rejected transaction must leave unchanged.
    fn watched(&self) -> Vec<Address> {
        let mut v: Vec<Address> = self.tokens.keys().copied().collect();
        v.push(config_address());
        v.extend(self.series.iter().map(|s| s.address));
        v.extend(self.makers.iter().map(|m| maker_state_address(&m.pubkey())));
        v
    }

    fn snapshot(&self) -> BTreeMap<Address, Vec<u8>> {
        self.watched()
            .into_iter()
            .map(|a| (a, self.env.data(&a)))
            .collect()
    }

    /// Lamports of every account that must not pay or receive any: the watched program and
    /// token accounts, and every signer other than the fee payer (makers, takers, admin).
    fn other_lamports(&self) -> BTreeMap<Address, u64> {
        let mut v = self.watched();
        v.extend(self.makers.iter().map(Signer::pubkey));
        v.extend(self.takers.iter().map(Signer::pubkey));
        v.push(self.env.admin.pubkey());
        v.into_iter().map(|a| (a, self.lamports(&a))).collect()
    }

    fn lamports(&self, a: &Address) -> u64 {
        self.env.svm.get_account(a).map_or(0, |acc| acc.lamports)
    }

    fn settle_ix(
        &self,
        s: usize,
        maker: usize,
        taker: usize,
        fill: u64,
        swap: bool,
    ) -> Instruction {
        let sw = &self.series[s];
        let m = self.makers[maker].pubkey();
        let mut taker_base = self.accounts[&(2 + taker, s, false)];
        if swap {
            // The taker's base account of the other series: right owner, wrong mint.
            taker_base = self.accounts[&(2 + taker, 1 - s, false)];
        }
        let mut data = vec![tag::SETTLE, 0];
        data.extend_from_slice(&fill.to_le_bytes());
        Instruction::new_with_bytes(
            PROGRAM_ID,
            &data,
            vec![
                AccountMeta::new_readonly(self.takers[taker].pubkey(), true),
                AccountMeta::new_readonly(sw.address, false),
                AccountMeta::new(maker_state_address(&m), false),
                AccountMeta::new_readonly(delegate_address(&m), false),
                AccountMeta::new(self.accounts[&(maker, s, false)], false),
                AccountMeta::new(self.accounts[&(maker, s, true)], false),
                AccountMeta::new(taker_base, false),
                AccountMeta::new(self.accounts[&(2 + taker, s, true)], false),
                AccountMeta::new_readonly(sw.base_mint, false),
                AccountMeta::new_readonly(sw.quote_mint, false),
                AccountMeta::new_readonly(sw.program, false),
                AccountMeta::new_readonly(sw.program, false),
                AccountMeta::new_readonly(INSTRUCTIONS, false),
            ],
        )
    }

    fn precompile(&self, terms: QuoteTerms, signer: &Keypair, flip: Option<u8>) -> Instruction {
        let quote = Quote::new(terms).unwrap();
        let sig = signer.sign_message(&quote.encode());
        let mut sig: [u8; 64] = sig.as_ref().try_into().unwrap();
        if let Some(i) = flip {
            sig[usize::from(i) % 64] ^= 0x01;
        }
        let data = quotebook_core::sigverify::instruction_data(&quote, &sig).to_vec();
        Instruction::new_with_bytes(ED25519, &data, vec![])
    }
}

/// The observed outcome of a transaction.
fn observe(r: &Result<TransactionMetadata, FailedTransactionMetadata>) -> Outcome {
    match r {
        Ok(_) => Outcome::Success,
        Err(f) => match &f.err {
            TransactionError::InstructionError(ix, InstructionError::Custom(code)) => {
                if *ix == 0 && *code < 100 {
                    // Precompile errors use small codes; quotebook's start at 100.
                    Outcome::Precompile
                } else {
                    Outcome::Err {
                        ix: *ix,
                        code: *code,
                    }
                }
            }
            TransactionError::InstructionError(0, _) => Outcome::Precompile,
            other => panic!(
                "unexpected transaction error {other:?}\n{}",
                f.meta.pretty_logs()
            ),
        },
    }
}

/// Counters across the whole run.
#[derive(Default, Debug)]
struct Stats {
    /// Successful fills per (ask, series).
    fills: BTreeMap<(bool, usize), usize>,
    outcomes: BTreeMap<String, usize>,
}

static STATS: Mutex<Option<Stats>> = Mutex::new(None);

fn record(ask_series: Option<(bool, usize)>, outcome: Outcome) {
    let mut g = STATS.lock().unwrap();
    let s = g.get_or_insert_with(Stats::default);
    if let (Some(k), Outcome::Success) = (ask_series, outcome) {
        *s.fills.entry(k).or_default() += 1;
    }
    let key = match outcome {
        Outcome::Success => "Success".to_string(),
        Outcome::Precompile => "Precompile".to_string(),
        Outcome::Err { code, .. } => format!("{:?}", E::from_code(code)),
    };
    *s.outcomes.entry(key).or_default() += 1;
}

/// Runs one sequence; returns a description of the first disagreement.
fn run(ops: &[Op]) -> Result<(), TestCaseError> {
    let mut w = World::new();
    let payer = w.env.payer.pubkey();
    for (step, op) in ops.iter().enumerate() {
        let before = w.snapshot();
        let payer_before = w.lamports(&payer);
        let others_before = w.other_lamports();
        let mut expected_tokens = w.tokens.clone();
        let mut next_nonces = w.nonces.clone();
        let mut next_fills = w.fills.clone();
        let mut next_paused: Vec<bool> = w.series.iter().map(|s| s.paused).collect();
        let now = NOW;

        let (predicted, result, signatures, kind) = match *op {
            Op::Fill {
                series: s,
                maker,
                taker,
                ask,
                price,
                size,
                fill,
                bind_taker,
                nonce,
                mutation,
            } => {
                let n = w.pick(maker, nonce);
                let bound = if mutation == Mutation::OtherTaker {
                    Some(1 - taker)
                } else if bind_taker {
                    Some(taker)
                } else {
                    None
                };
                let quote_series = if mutation == Mutation::OtherSeries {
                    1 - s
                } else {
                    s
                };
                let (issued, expires) = if mutation == Mutation::Expired {
                    (now - 20, now - 1)
                } else {
                    (now, now + 30)
                };
                let fill = match mutation {
                    Mutation::OverSize => size + 1,
                    Mutation::ZeroFill => 0,
                    _ => fill,
                };
                let terms = QuoteTerms {
                    program_id: PROGRAM_ID,
                    deployment: DEPLOYMENT,
                    maker: w.makers[maker].pubkey(),
                    series: SeriesId::new(w.series[quote_series].address),
                    taker: bound.map_or(Quote::OPEN_TAKER, |t| w.takers[t].pubkey()),
                    side: if ask { Side::Ask } else { Side::Bid },
                    price: QuoteLotsPerBaseLot::new(price),
                    size: BaseLots::new(size),
                    issued_at: UnixTimestamp::new(issued),
                    expires_at: UnixTimestamp::new(expires),
                    nonce: Nonce::new(n),
                };
                let flip = match mutation {
                    Mutation::FlipSigByte(i) => Some(i),
                    _ => None,
                };
                let swap = mutation == Mutation::SwapTakerAccount;
                // Prediction, in settle's check order.
                let predicted = if flip.is_some() {
                    Outcome::Precompile
                } else if quote_series != s {
                    err(1, E::WrongSeries)
                } else if w.series[s].paused {
                    err(1, E::SeriesPaused)
                } else if bound.is_some_and(|t| t != taker) {
                    err(1, E::WrongTaker)
                } else if expires <= now {
                    err(1, E::QuoteExpired)
                } else if fill < 1 || fill > size {
                    err(1, E::FillSizeInvalid)
                } else if let Err(e) = next_nonces[maker].consume(n) {
                    err(1, e)
                } else if swap {
                    err(1, E::InvalidVaultOrTokenAccount)
                } else {
                    let base = fill * BASE_LOT;
                    let quote = price * fill * QUOTE_LOT;
                    let mb = w.accounts[&(maker, s, false)];
                    let mq = w.accounts[&(maker, s, true)];
                    let tb = w.accounts[&(2 + taker, s, false)];
                    let tq = w.accounts[&(2 + taker, s, true)];
                    let t = &mut expected_tokens;
                    if ask {
                        t.get_mut(&mb).unwrap().amount -= base;
                        t.get_mut(&mb).unwrap().allowance -= base;
                        t.get_mut(&tb).unwrap().amount += base;
                        t.get_mut(&tq).unwrap().amount -= quote;
                        t.get_mut(&mq).unwrap().amount += quote;
                    } else {
                        t.get_mut(&tb).unwrap().amount -= base;
                        t.get_mut(&mb).unwrap().amount += base;
                        t.get_mut(&mq).unwrap().amount -= quote;
                        t.get_mut(&mq).unwrap().allowance -= quote;
                        t.get_mut(&tq).unwrap().amount += quote;
                    }
                    next_fills[maker] += 1;
                    Outcome::Success
                };
                let maker_kp = w.makers[maker].insecure_clone();
                let taker_kp = w.takers[taker].insecure_clone();
                let pre = w.precompile(terms, &maker_kp, flip);
                let ix = w.settle_ix(s, maker, taker, fill, swap);
                let r = w.env.send(&[pre, ix], &[&taker_kp]);
                // payer + taker + the precompile's one signature.
                (predicted, r, 3, Some((ask, s)))
            }
            Op::CancelAll { maker, nonce } => {
                let n = w.pick(maker, nonce);
                let predicted = match next_nonces[maker].cancel_all(n) {
                    Ok(()) => Outcome::Success,
                    Err(e) => err(0, e),
                };
                let m = w.makers[maker].insecure_clone();
                let ix = cancel_ix(
                    tag::CANCEL_ALL,
                    &m.pubkey(),
                    &maker_state_address(&m.pubkey()),
                    n,
                );
                (predicted, w.env.send(&[ix], &[&m]), 2, None)
            }
            Op::CancelNonce { maker, nonce } => {
                let n = w.pick(maker, nonce);
                let predicted = match next_nonces[maker].consume(n) {
                    Ok(()) => Outcome::Success,
                    Err(e) => err(0, e),
                };
                let m = w.makers[maker].insecure_clone();
                let ix = cancel_ix(
                    tag::CANCEL_NONCE,
                    &m.pubkey(),
                    &maker_state_address(&m.pubkey()),
                    n,
                );
                (predicted, w.env.send(&[ix], &[&m]), 2, None)
            }
            Op::Pause { series: s, paused } => {
                next_paused[s] = paused;
                let admin = w.env.admin.insecure_clone();
                let ix =
                    set_series_paused_ix(&admin.pubkey(), &w.series[s].address, u8::from(paused));
                (Outcome::Success, w.env.send(&[ix], &[&admin]), 2, None)
            }
        };

        let observed = observe(&result);
        let logs = match &result {
            Ok(m) => m.pretty_logs(),
            Err(f) => f.meta.pretty_logs(),
        };
        prop_assert_eq!(
            observed,
            predicted,
            "step {} {:?}: observed {:?}, model predicted {:?}\n{}",
            step,
            op,
            observed,
            predicted,
            logs
        );
        record(kind, observed);

        // The fee payer pays the signature fees, success or not; nobody else's lamports move.
        let fee = SIG_FEE * signatures;
        prop_assert_eq!(
            payer_before - w.lamports(&payer),
            fee,
            "step {}: payer fee",
            step
        );
        prop_assert_eq!(
            &w.other_lamports(),
            &others_before,
            "step {}: lamports of an account other than the payer moved",
            step
        );

        let after = w.snapshot();
        if observed == Outcome::Success {
            w.tokens = expected_tokens;
            w.nonces = next_nonces;
            w.fills = next_fills;
            for (s, p) in next_paused.into_iter().enumerate() {
                w.series[s].paused = p;
            }
        } else {
            prop_assert!(
                before == after,
                "step {}: a rejected transaction changed state",
                step
            );
        }

        // Token accounts: amount and allowance as modelled, every other byte unchanged.
        for (a, t) in &w.tokens {
            let d = &after[a];
            let amount = u64::from_le_bytes(d[64..72].try_into().unwrap());
            let allowance = u64::from_le_bytes(d[121..129].try_into().unwrap());
            prop_assert_eq!(amount, t.amount, "step {}: amount of {}", step, a);
            prop_assert_eq!(allowance, t.allowance, "step {}: allowance of {}", step, a);
            let (b, n) = (&before[a], d);
            prop_assert!(
                b[..64] == n[..64] && b[72..121] == n[72..121] && b[129..] == n[129..],
                "step {}: bytes outside amount/allowance changed in {}",
                step,
                a
            );
        }
        // Conservation per mint.
        let mut totals: BTreeMap<Address, u64> = BTreeMap::new();
        for t in w.tokens.values() {
            *totals.entry(t.mint).or_default() += t.amount;
        }
        prop_assert_eq!(&totals, &w.totals, "step {}: conservation", step);
        // Maker state and series flags.
        for (i, m) in w.makers.iter().enumerate() {
            let st = w.env.maker_state(&m.pubkey());
            let model = &w.nonces[i];
            prop_assert_eq!(st.fills, w.fills[i], "step {}: fills of maker {}", step, i);
            prop_assert_eq!(
                st.nonces.window_base().get(),
                model.base,
                "step {}: base",
                step
            );
            prop_assert_eq!(
                st.nonces.last_cancel_all().get(),
                model.last_cancel_all,
                "step {}: last_cancel_all",
                step
            );
            prop_assert_eq!(
                st.nonces.used_count() as usize,
                model.used.len(),
                "step {}: used",
                step
            );
            for n in &model.used {
                prop_assert_eq!(st.nonces.check(Nonce::new(*n)), Err(E::NonceUsed));
            }
        }
        for s in &w.series {
            prop_assert_eq!(w.env.series(&s.address).paused != 0, s.paused);
        }
    }
    Ok(())
}

fn cases() -> u32 {
    std::env::var("QUOTEBOOK_PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24)
}

#[test]
fn settle_sequences_match_the_model_and_conserve_tokens() {
    let mut runner = TestRunner::new(Config {
        cases: cases(),
        rng_seed: RngSeed::Fixed(0x0B0C_0006_C0A5_0001),
        failure_persistence: None,
        ..Config::default()
    });
    runner
        .run(&sequence(), |ops| run(&ops))
        .unwrap_or_else(|e| panic!("{e}"));
    let stats = STATS.lock().unwrap().take().unwrap_or_default();
    println!("conservation outcomes: {:#?}", stats.outcomes);
    println!("successful fills per (ask, series): {:?}", stats.fills);
    for ask in [false, true] {
        for s in 0..2 {
            assert!(
                stats.fills.get(&(ask, s)).copied().unwrap_or(0) > 0,
                "no successful fill for ask={ask} on series {s}"
            );
        }
    }
}
