//! I10 / ADR 0001: immutable per-series taker fees and `withdraw_fees`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    clippy::disallowed_macros,
    clippy::result_large_err,
    missing_docs
)]

mod common;

use common::*;
use quotebook_core::event::FillEvent;
use quotebook_core::fee::fee;
use quotebook_core::{QuoteLotsPerBaseLot, QuoteTerms, QuotebookError, Side};
use quotebook_sdk::events::{fill_events, Commitment, Extraction, TransactionLogs};
use solana_address::Address;
use solana_instruction::error::InstructionError;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_signer::Signer;

const FEE_BPS: u16 = 100;

fn fee_market(spec: MarketSpec) -> Market {
    Market::with(MarketSpec {
        fee_bps: Some(FEE_BPS),
        ..spec
    })
}

fn priced(m: &Market, side: Side) -> QuoteTerms {
    QuoteTerms {
        side,
        price: QuoteLotsPerBaseLot::new(1_000),
        ..m.terms()
    }
}

/// The five balances: maker base, maker quote, taker base, taker quote, fee account.
fn five(m: &Market) -> [u64; 5] {
    let [a, b, c, d] = m.balances();
    [a, b, c, d, m.env.token_amount(&m.fee_account.unwrap())]
}

fn allowance(m: &Market, account: &Address) -> u64 {
    u64::from_le_bytes(m.env.data(account)[121..129].try_into().unwrap())
}

fn event_of(meta: &litesvm::types::TransactionMetadata) -> FillEvent {
    let x = fill_events(
        &PROGRAM_ID,
        &TransactionLogs {
            succeeded: true,
            commitment: Commitment::Finalized,
            logs: &meta.logs,
        },
        Commitment::Finalized,
    )
    .unwrap();
    let Extraction::Complete(v) = x else {
        panic!("{x:?}")
    };
    assert_eq!(v.len(), 1);
    v[0].event
}

#[test]
fn test_v1_series_has_no_fee_and_settles_with_thirteen_accounts() {
    let mut m = Market::new();
    assert_eq!(m.env.series(&m.series).fee_bps(), 0);
    assert!(m.fee_account.is_none());
    let meta = m.settle(m.terms(), 4).unwrap();
    assert_eq!(event_of(&meta).fee, 0);
}

#[test]
fn test_create_series_stores_the_fee_and_caps_it() {
    let m = fee_market(MarketSpec::default());
    assert_eq!(m.env.series(&m.series).fee_bps(), FEE_BPS);
    let mut env = Env::new();
    env.init_config();
    let (base, quote) = (env.spl_mint(), env.spl_mint());
    let series = series_address(&base, &quote, 0);
    let args = SeriesArgs {
        fee_bps: Some(101),
        ..SeriesArgs::default()
    };
    let ix = create_series_ix(
        &env.admin.pubkey(),
        &env.payer.pubkey(),
        &series,
        &base,
        &quote,
        args.encode(),
    );
    let admin = env.admin.insecure_clone();
    assert_ix_err(env.send(&[ix], &[&admin]), qb(QuotebookError::FeeTooHigh));
    // Some(0) is accepted and stored as 0.
    let args = SeriesArgs {
        fee_bps: Some(0),
        ..SeriesArgs::default()
    };
    let ix = create_series_ix(
        &admin.pubkey(),
        &env.payer.pubkey(),
        &series,
        &base,
        &quote,
        args.encode(),
    );
    env.send(&[ix], &[&admin]).unwrap();
    assert_eq!(env.series(&series).fee_bps(), 0);
}

#[test]
fn test_fee_moves_exactly_on_both_sides_and_token_programs() {
    for (base_2022, quote_2022) in [(false, false), (true, true)] {
        for side in [Side::Bid, Side::Ask] {
            let mut m = fee_market(MarketSpec {
                base_2022,
                quote_2022,
                ..MarketSpec::default()
            });
            let t = priced(&m, side);
            let before = five(&m);
            let (mq_allow, mb_allow) =
                (allowance(&m, &m.maker_quote), allowance(&m, &m.maker_base));
            let meta = m.settle(t, 10).unwrap();
            let base = 10 * 1_000;
            let quote = 1_000 * 10 * 10;
            let f = fee(quote, FEE_BPS).unwrap();
            assert_eq!(f, 1_000);
            let after = five(&m);
            match side {
                Side::Bid => {
                    // Maker buys base: pays `quote` in total, `quote - f` to the taker, `f` to fees.
                    assert_eq!(after[0], before[0] + base);
                    assert_eq!(after[1], before[1] - quote);
                    assert_eq!(after[2], before[2] - base);
                    assert_eq!(after[3], before[3] + quote - f);
                    assert_eq!(allowance(&m, &m.maker_quote), mq_allow - quote);
                }
                Side::Ask => {
                    // Maker sells base: receives `quote`; the taker pays `quote + f`.
                    assert_eq!(after[0], before[0] - base);
                    assert_eq!(after[1], before[1] + quote);
                    assert_eq!(after[2], before[2] + base);
                    assert_eq!(after[3], before[3] - quote - f);
                    assert_eq!(allowance(&m, &m.maker_base), mb_allow - base);
                }
            }
            assert_eq!(after[4], before[4] + f);
            let e = event_of(&meta);
            assert_eq!((e.quote_atoms, e.fee), (quote, f));
        }
    }
}

#[test]
fn test_fee_rounds_down_to_zero_on_dust() {
    let mut m = fee_market(MarketSpec::default());
    // price 3, 4 lots: quote 120 atoms, 120 × 100 / 10,000 = 1.2, floor 1; 3 lots: 90, fee 0.
    let before = five(&m);
    let meta = m.settle(m.terms(), 3).unwrap();
    assert_eq!(event_of(&meta).fee, 0);
    assert_eq!(five(&m)[4], before[4]);
}

#[test]
fn test_fee_account_rejections() {
    let missing = |m: &mut Market| {
        let maker = m.maker.insecure_clone();
        let pre = m.precompile(m.terms(), &maker);
        let mut ix = m.settle_ix(0, 4);
        ix.accounts.pop();
        m.send_as_taker(&[pre, ix])
    };
    let mut m = fee_market(MarketSpec::default());
    // The program returns `ProgramError::NotEnoughAccountKeys`, reported by the runtime under
    // its deprecated name (as in init_config.rs).
    #[allow(deprecated)]
    let not_enough = InstructionError::NotEnoughAccountKeys;
    assert_ix_err_at(missing(&mut m), 1, not_enough);

    let with_fee_account = |m: &mut Market, account: Address, writable: bool| {
        let maker = m.maker.insecure_clone();
        let pre = m.precompile(m.terms(), &maker);
        let mut ix = m.settle_ix(0, 4);
        *ix.accounts.last_mut().unwrap() = if writable {
            AccountMeta::new(account, false)
        } else {
            AccountMeta::new_readonly(account, false)
        };
        m.send_as_taker(&[pre, ix])
    };
    let bad = qb(QuotebookError::InvalidVaultOrTokenAccount);
    // Wrong mint: a Config-owned account of the base mint.
    let mut m = fee_market(MarketSpec::default());
    let (bp, bm) = (m.base_program, m.base_mint);
    let wrong_mint = m
        .env
        .token_account(bp, &bm, &config_address(), 0, None, &[]);
    assert_ix_err_at(with_fee_account(&mut m, wrong_mint, true), 1, bad.clone());
    // Wrong owner: the quote mint, owned by the taker.
    let (qp, qm, taker) = (m.quote_program, m.quote_mint, m.taker.pubkey());
    let wrong_owner = m.env.token_account(qp, &qm, &taker, 0, None, &[]);
    assert_ix_err_at(with_fee_account(&mut m, wrong_owner, true), 1, bad.clone());
    // Wrong token program: right bytes, owned by another program.
    let foreign = Keypair::new().pubkey();
    let data = token_account_data(&qm, &config_address(), 0, None, 1, &[]);
    m.env.set(foreign, Address::new_from_array([9; 32]), data);
    assert_ix_err_at(with_fee_account(&mut m, foreign, true), 1, bad);
    // Not writable.
    let fa = m.fee_account.unwrap();
    assert_ix_err_at(
        with_fee_account(&mut m, fa, false),
        1,
        InstructionError::Immutable,
    );
    // Token-2022 with MemoTransfer (required memos) on the receiving fee account.
    let mut m = fee_market(MarketSpec {
        base_2022: true,
        quote_2022: true,
        ..MarketSpec::default()
    });
    let (qp, qm) = (m.quote_program, m.quote_mint);
    let memo = m
        .env
        .token_account(qp, &qm, &config_address(), 0, None, &tlv(8, &[1]));
    assert_ix_err_at(
        with_fee_account(&mut m, memo, true),
        1,
        qb(QuotebookError::AccountExtensionUnsupported),
    );
}

#[test]
fn test_ask_total_overflow_is_rejected_before_any_transfer() {
    let mut m = fee_market(MarketSpec::default());
    // quote = price × 1 × 10 just below u64::MAX; quote + floor(quote / 100) overflows.
    let price = u64::MAX / 10 - 1;
    let t = QuoteTerms {
        side: Side::Ask,
        price: QuoteLotsPerBaseLot::new(price),
        ..m.terms()
    };
    let before = five(&m);
    assert_ix_err_at(m.settle(t, 1), 1, qb(QuotebookError::Overflow));
    assert_eq!(five(&m), before);
}

#[test]
fn test_bid_second_transfer_failure_rolls_everything_back() {
    let mut m = fee_market(MarketSpec::default());
    let t = priced(&m, Side::Bid);
    let quote = 1_000 * 10 * 10;
    let f = fee(quote, FEE_BPS).unwrap();
    // The maker's allowance is strictly between `quote - f` and `quote`: the transfer of the
    // proceeds succeeds inside the instruction and leaves 1 atom of allowance, so the delegate
    // stays set, and the fee transfer then fails in the token program. (At exactly `quote - f`
    // SPL Token clears the delegate and the second transfer fails with `OwnerMismatch`.)
    let (qm, owner, delegate) = (
        m.quote_mint,
        m.maker.pubkey(),
        delegate_address(&m.maker.pubkey()),
    );
    let data = token_account_data(&qm, &owner, FUNDS, Some((&delegate, quote - f + 1)), 1, &[]);
    let mq = m.maker_quote;
    let qp = m.quote_program;
    m.env.set(mq, qp, data);
    let before = five(&m);
    let state_before = m.env.data(&maker_state_address(&m.maker.pubkey()));
    let allowance_before = allowance(&m, &m.maker_quote);
    let r = m.settle(t, 10);
    // SPL Token `InsufficientFunds` (1): the delegate's remaining allowance is below the fee.
    assert_ix_err_at(r, 1, InstructionError::Custom(1));
    assert_eq!(five(&m), before);
    assert_eq!(allowance(&m, &m.maker_quote), allowance_before);
    assert_eq!(
        m.env.data(&maker_state_address(&m.maker.pubkey())),
        state_before
    );
}

fn withdraw_ix(
    admin: &Address,
    config: &Address,
    fee_account: &Address,
    destination: &Address,
    mint: &Address,
    program: &Address,
    amount: u64,
) -> Instruction {
    let mut data = vec![8u8];
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new_readonly(*config, false),
            AccountMeta::new(*fee_account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(*program, false),
        ],
    )
}

#[test]
fn test_withdraw_fees_happy_path_and_rejections() {
    let mut m = fee_market(MarketSpec::default());
    m.settle(priced(&m, Side::Bid), 10).unwrap();
    let fa = m.fee_account.unwrap();
    let collected = m.env.token_amount(&fa);
    assert_eq!(collected, 1_000);
    let (qm, qp) = (m.quote_mint, m.quote_program);
    let stranger = Keypair::new().pubkey();
    let dest = m.env.token_account(qp, &qm, &stranger, 0, None, &[]);
    let admin = m.env.admin.insecure_clone();
    let cfg = config_address();
    let send = |m: &mut Market, signer: &Keypair, ix: Instruction| m.env.send(&[ix], &[signer]);

    // Signer is not the admin.
    let other = Keypair::new();
    m.env.svm.airdrop(&other.pubkey(), 1_000_000_000).unwrap();
    let ix = withdraw_ix(&other.pubkey(), &cfg, &fa, &dest, &qm, &qp, 1);
    assert_ix_err(send(&mut m, &other, ix), qb(QuotebookError::NotAuthority));
    // Wrong config: the series account in its place.
    let ix = withdraw_ix(&admin.pubkey(), &m.series, &fa, &dest, &qm, &qp, 1);
    assert!(send(&mut m, &admin, ix).is_err());
    // Source not owned by the Config PDA.
    let not_config = m.env.token_account(qp, &qm, &stranger, 5_000, None, &[]);
    let ix = withdraw_ix(&admin.pubkey(), &cfg, &not_config, &dest, &qm, &qp, 1);
    assert_ix_err(
        send(&mut m, &admin, ix),
        qb(QuotebookError::InvalidVaultOrTokenAccount),
    );
    // Wrong mint: the base mint with the quote fee account.
    let bm = m.base_mint;
    let ix = withdraw_ix(&admin.pubkey(), &cfg, &fa, &dest, &bm, &qp, 1);
    assert_ix_err(
        send(&mut m, &admin, ix),
        qb(QuotebookError::InvalidVaultOrTokenAccount),
    );
    // Above the balance, and zero.
    let ix = withdraw_ix(&admin.pubkey(), &cfg, &fa, &dest, &qm, &qp, collected + 1);
    assert_ix_err(
        send(&mut m, &admin, ix),
        qb(QuotebookError::InsufficientFees),
    );
    let ix = withdraw_ix(&admin.pubkey(), &cfg, &fa, &dest, &qm, &qp, 0);
    assert_ix_err(
        send(&mut m, &admin, ix),
        qb(QuotebookError::InvalidInstructionData),
    );

    // The admin withdraws everything to an account of another owner.
    let ix = withdraw_ix(&admin.pubkey(), &cfg, &fa, &dest, &qm, &qp, collected);
    send(&mut m, &admin, ix).unwrap();
    assert_eq!(m.env.token_amount(&fa), 0);
    assert_eq!(m.env.token_amount(&dest), collected);
}
