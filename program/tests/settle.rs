//! `settle`: fills on SPL Token and Token-2022, replay, the ed25519 precompile binding, and
//! every account and quote rule with a forged or swapped input.

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
    missing_docs
)]

mod common;

use common::*;
use quotebook_core::{BaseLots, Nonce, QuoteLotsPerBaseLot, QuotebookError, Side, UnixTimestamp};
use solana_instruction::error::InstructionError;
use solana_instruction::AccountMeta;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// Settle at index 1 failed with a quotebook error.
#[track_caller]
fn fails(
    r: Result<litesvm::types::TransactionMetadata, litesvm::types::FailedTransactionMetadata>,
    e: QuotebookError,
) {
    assert_ix_err_at(r, 1, qb(e));
}

#[test]
fn test_settle_bid_moves_both_legs_and_consumes_the_nonce() {
    let mut m = Market::new();
    let before = m.balances();
    // Bid: the maker buys 4 lots of base at 3 ticks. base = 4 × 1,000; quote = 3 × 4 × 10.
    m.settle(m.terms(), 4).unwrap();
    let after = m.balances();
    assert_eq!(after[0], before[0] + 4_000); // maker base
    assert_eq!(after[1], before[1] - 120); // maker quote
    assert_eq!(after[2], before[2] - 4_000); // taker base
    assert_eq!(after[3], before[3] + 120); // taker quote
    let state = m.env.maker_state(&m.maker.pubkey());
    assert_eq!(state.fills, 1);
    assert_eq!(
        state.nonces.check(Nonce::new(1)),
        Err(QuotebookError::NonceUsed)
    );
}

#[test]
fn test_settle_ask_moves_both_legs() {
    let mut m = Market::new();
    let before = m.balances();
    let mut t = m.terms();
    t.side = Side::Ask;
    m.settle(t, 10).unwrap();
    let after = m.balances();
    assert_eq!(after[0], before[0] - 10_000);
    assert_eq!(after[1], before[1] + 300);
    assert_eq!(after[2], before[2] + 10_000);
    assert_eq!(after[3], before[3] - 300);
}

#[test]
fn test_settle_partial_fill_then_replay_is_rejected() {
    let mut m = Market::new();
    m.settle(m.terms(), 1).unwrap();
    // The remaining 9 lots are void: the nonce is spent.
    fails(m.settle(m.terms(), 9), QuotebookError::NonceUsed);
}

#[test]
fn test_settle_open_quote_any_taker_bound_quote_only_its_taker() {
    let mut m = Market::new();
    let mut t = m.terms();
    t.taker = m.taker.pubkey();
    t.nonce = Nonce::new(2);
    m.settle(t, 1).unwrap();

    let mut t = m.terms();
    t.taker = Keypair::new().pubkey();
    t.nonce = Nonce::new(3);
    fails(m.settle(t, 1), QuotebookError::WrongTaker);
}

#[test]
fn test_settle_maker_as_taker_is_self_trade() {
    let mut m = Market::new();
    m.taker = m.maker.insecure_clone();
    m.taker_base = m.maker_base;
    m.taker_quote = m.maker_quote;
    fails(m.settle(m.terms(), 1), QuotebookError::SelfTrade);
}

#[test]
fn test_settle_two_quotes_in_one_transaction() {
    let mut m = Market::new();
    let maker = m.maker.insecure_clone();
    let mut t2 = m.terms();
    t2.nonce = Nonce::new(2);
    t2.side = Side::Ask;
    let ixs = [
        m.precompile(m.terms(), &maker),
        m.precompile(t2, &maker),
        m.settle_ix(0, 2),
        m.settle_ix(1, 2),
    ];
    let before = m.balances();
    m.send_as_taker(&ixs).unwrap();
    // A bid and an ask of the same size and price cancel out.
    assert_eq!(m.balances(), before);
    assert_eq!(m.env.maker_state(&m.maker.pubkey()).fills, 2);
}

#[test]
fn test_settle_after_cancel_is_rejected() {
    let mut m = Market::new();
    let maker = m.maker.insecure_clone();
    let state = maker_state_address(&maker.pubkey());
    m.env
        .send(
            &[cancel_ix(tag::CANCEL_NONCE, &maker.pubkey(), &state, 1)],
            &[&maker],
        )
        .unwrap();
    fails(m.settle(m.terms(), 1), QuotebookError::NonceUsed);
    m.env
        .send(
            &[cancel_ix(tag::CANCEL_ALL, &maker.pubkey(), &state, 50)],
            &[&maker],
        )
        .unwrap();
    let mut t = m.terms();
    t.nonce = Nonce::new(49);
    fails(m.settle(t, 1), QuotebookError::NonceCancelled);
}

// ---- the precompile binding (docs/PLAN.md, 4.1) ----

#[test]
fn test_settle_sig_index_not_ed25519_or_not_before_is_rejected() {
    let mut m = Market::new();
    let maker = m.maker.insecure_clone();
    let pre = m.precompile(m.terms(), &maker);
    // Index 1 is settle itself; index 2 does not exist.
    for index in [1u8, 2, 200] {
        let ix = m.settle_ix(index, 1);
        fails(
            m.send_as_taker(&[pre.clone(), ix]),
            QuotebookError::MissingSigVerify,
        );
    }
    // Precompile after settle.
    let ix = m.settle_ix(1, 1);
    assert_ix_err_at(
        m.send_as_taker(&[ix, pre.clone()]),
        0,
        qb(QuotebookError::MissingSigVerify),
    );
    // Index 0 is another program's instruction.
    let other = solana_instruction::Instruction::new_with_bytes(
        solana_sdk_ids::compute_budget::ID,
        &[2, 0x40, 0x0D, 0x03, 0x00],
        vec![],
    );
    let ix = m.settle_ix(0, 1);
    assert_ix_err_at(
        m.send_as_taker(&[other, pre, ix]),
        2,
        qb(QuotebookError::MissingSigVerify),
    );
}

/// Precompile data with two valid signatures over two quotes.
#[test]
fn test_settle_two_signatures_in_one_precompile_is_rejected() {
    let mut m = Market::new();
    let maker = m.maker.insecure_clone();
    let one = m.precompile(m.terms(), &maker).data;
    let mut t2 = m.terms();
    t2.nonce = Nonce::new(2);
    let two = m.precompile(t2, &maker).data;
    // Layout: [2, 0, offsets A, offsets B, A's key|sig|msg, B's key|sig|msg].
    let body_a = &one[16..];
    let body_b = &two[16..];
    let base_a = 2 + 2 * 14;
    let base_b = base_a + body_a.len();
    let offsets = |base: usize| {
        let mut o = Vec::new();
        for v in [base + 32, 0xFFFF, base, 0xFFFF, base + 96, 208, 0xFFFF] {
            o.extend_from_slice(&(v as u16).to_le_bytes());
        }
        o
    };
    let data = [
        vec![2, 0],
        offsets(base_a),
        offsets(base_b),
        body_a.to_vec(),
        body_b.to_vec(),
    ]
    .concat();
    let pre = solana_instruction::Instruction::new_with_bytes(ED25519, &data, vec![]);
    let ix = m.settle_ix(0, 1);
    fails(
        m.send_as_taker(&[pre, ix]),
        QuotebookError::MalformedSigVerify,
    );
}

/// Indexes that resolve to the precompile itself but are not `0xFFFF`.
#[test]
fn test_settle_explicit_instruction_index_is_rejected() {
    let mut m = Market::new();
    let maker = m.maker.insecure_clone();
    for field in [1usize, 3, 6] {
        let mut pre = m.precompile(m.terms(), &maker);
        let at = 2 + 2 * field;
        pre.data[at..at + 2].copy_from_slice(&0u16.to_le_bytes());
        let ix = m.settle_ix(0, 1);
        fails(
            m.send_as_taker(&[pre, ix]),
            QuotebookError::SigVerifyNotSelfContained,
        );
    }
}

/// The classic attack: the precompile verifies a genuine quote held in *another*
/// instruction, while its own data holds a forged quote at the offsets settle would read.
#[test]
fn test_settle_precompile_verifying_another_instruction_is_rejected() {
    let mut m = Market::new();
    let maker = m.maker.insecure_clone();
    let genuine = m.precompile(m.terms(), &maker);
    let mut forged_terms = m.terms();
    forged_terms.price = QuoteLotsPerBaseLot::new(1);
    forged_terms.nonce = Nonce::new(9);
    let mut forged = m.precompile(forged_terms, &Keypair::new());
    // Same layout, but every index points at instruction 0, the genuine precompile.
    for field in [1usize, 3, 6] {
        let at = 2 + 2 * field;
        forged.data[at..at + 2].copy_from_slice(&0u16.to_le_bytes());
    }
    let ix = m.settle_ix(1, 1);
    assert_ix_err_at(
        m.send_as_taker(&[genuine, forged, ix]),
        2,
        qb(QuotebookError::SigVerifyNotSelfContained),
    );
}

/// A forged second quote appended to genuine precompile data is never what settle reads.
#[test]
fn test_settle_reads_the_signed_bytes_not_an_appended_copy() {
    let mut m = Market::new();
    let maker = m.maker.insecure_clone();
    let mut pre = m.precompile(m.terms(), &maker);
    let mut forged = m.terms();
    forged.price = QuoteLotsPerBaseLot::new(1);
    pre.data
        .extend_from_slice(&quotebook_core::Quote::new(forged).unwrap().encode());
    let before = m.balances();
    let ix = m.settle_ix(0, 2);
    m.send_as_taker(&[pre.clone(), ix]).unwrap();
    // Price 3, not the appended 1: quote = 3 × 2 × 10.
    assert_eq!(m.balances()[1], before[1] - 60);

    // Pointing the message at the appended copy fails the precompile itself.
    let mut pre2 = m.precompile(
        {
            let mut t = m.terms();
            t.nonce = Nonce::new(2);
            t
        },
        &maker,
    );
    pre2.data
        .extend_from_slice(&quotebook_core::Quote::new(forged).unwrap().encode());
    pre2.data[10..12].copy_from_slice(&320u16.to_le_bytes());
    let ix = m.settle_ix(0, 2);
    assert!(m.send_as_taker(&[pre2, ix]).is_err());
}

/// The attacker signs a quote naming the victim as maker with the attacker's own key.
#[test]
fn test_settle_quote_signed_by_another_key_is_rejected() {
    let mut m = Market::new();
    let pre = m.precompile(m.terms(), &Keypair::new());
    let ix = m.settle_ix(0, 1);
    fails(m.send_as_taker(&[pre, ix]), QuotebookError::SignerMismatch);
}

/// A genuine signature over 207 bytes of the quote.
#[test]
fn test_settle_message_one_byte_short_is_rejected() {
    let mut m = Market::new();
    let maker = m.maker.insecure_clone();
    let quote = quotebook_core::Quote::new(m.terms()).unwrap();
    let msg = &quote.encode()[..207];
    let sig = maker.sign_message(msg);
    let mut data = vec![1, 0];
    for v in [48u16, 0xFFFF, 16, 0xFFFF, 112, 207, 0xFFFF] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    data.extend_from_slice(maker.pubkey().as_ref());
    data.extend_from_slice(sig.as_ref());
    data.extend_from_slice(msg);
    let pre = solana_instruction::Instruction::new_with_bytes(ED25519, &data, vec![]);
    let ix = m.settle_ix(0, 1);
    fails(
        m.send_as_taker(&[pre, ix]),
        QuotebookError::QuoteLengthMismatch,
    );
}

#[test]
fn test_settle_wrong_instructions_sysvar_is_rejected() {
    let mut m = Market::new();
    let maker = m.maker.insecure_clone();
    let pre = m.precompile(m.terms(), &maker);
    let mut ix = m.settle_ix(0, 1);
    ix.accounts[12] = AccountMeta::new_readonly(solana_sdk_ids::sysvar::clock::ID, false);
    fails(m.send_as_taker(&[pre, ix]), QuotebookError::InvalidSysvar);
}

// ---- quote binding ----

#[test]
fn test_settle_quote_for_another_program_series_or_deployment_is_rejected() {
    let mut m = Market::new();
    let mut t = m.terms();
    t.program_id = Keypair::new().pubkey();
    fails(m.settle(t, 1), QuotebookError::WrongProgram);

    let mut t = m.terms();
    t.series = quotebook_core::SeriesId::new(Keypair::new().pubkey());
    fails(m.settle(t, 1), QuotebookError::WrongSeries);

    let mut t = m.terms();
    t.deployment = *b"other-deployment";
    fails(m.settle(t, 1), QuotebookError::WrongSeries);
}

/// The same keys, program and series on another deployment (another cluster): only the
/// series' deployment id differs, and the quote signed for the first one does not fill.
#[test]
fn test_settle_quote_from_another_deployment_with_identical_addresses_is_rejected() {
    let mut m = Market::new();
    let mut data = m.env.data(&m.series);
    data[16..32].copy_from_slice(b"mainnet-beta-xyz");
    m.env.set(m.series, PROGRAM_ID, data);
    fails(m.settle(m.terms(), 1), QuotebookError::WrongSeries);
}

#[test]
fn test_settle_second_series_of_the_same_pair_is_not_the_first() {
    let mut m = Market::new();
    let (other, _) = m.env.create_series_with(&m.base_mint, &m.quote_mint);
    let maker = m.maker.insecure_clone();
    let pre = m.precompile(m.terms(), &maker);
    let mut ix = m.settle_ix(0, 1);
    ix.accounts[1] = AccountMeta::new_readonly(other, false);
    fails(m.send_as_taker(&[pre, ix]), QuotebookError::WrongSeries);
}

// ---- time and size ----

#[test]
fn test_settle_time_window() {
    let mut m = Market::new();
    let mut t = m.terms();
    t.issued_at = UnixTimestamp::new(NOW - 30);
    t.expires_at = UnixTimestamp::new(NOW);
    fails(m.settle(t, 1), QuotebookError::QuoteExpired);

    let mut t = m.terms();
    t.expires_at = UnixTimestamp::new(NOW + 61);
    fails(m.settle(t, 1), QuotebookError::QuoteTtlTooLong);

    let mut t = m.terms();
    t.issued_at = UnixTimestamp::new(NOW + 11);
    t.expires_at = UnixTimestamp::new(NOW + 40);
    fails(m.settle(t, 1), QuotebookError::QuoteTtlTooLong);

    // At the skew limit it fills.
    let mut t = m.terms();
    t.issued_at = UnixTimestamp::new(NOW + 10);
    t.expires_at = UnixTimestamp::new(NOW + 70);
    m.settle(t, 1).unwrap();

    // The on-chain clock moves past expiry.
    let mut t = m.terms();
    t.nonce = Nonce::new(2);
    m.env.set_clock(NOW + 30);
    fails(m.settle(t, 1), QuotebookError::QuoteExpired);
}

#[test]
fn test_settle_fill_size_bounds() {
    let mut m = Market::new();
    fails(m.settle(m.terms(), 0), QuotebookError::FillSizeInvalid);
    fails(m.settle(m.terms(), 11), QuotebookError::FillSizeInvalid);
    m.settle(m.terms(), 10).unwrap();
}

#[test]
fn test_settle_amount_overflow_is_rejected() {
    let mut m = Market::new();
    let mut t = m.terms();
    t.price = QuoteLotsPerBaseLot::new(u64::MAX / 10);
    fails(m.settle(t, 10), QuotebookError::Overflow);
    let mut t = m.terms();
    t.size = BaseLots::new(u64::MAX);
    t.nonce = Nonce::new(2);
    fails(m.settle(t, u64::MAX / 999), QuotebookError::Overflow);
}

#[test]
fn test_settle_paused_series_is_rejected() {
    let mut m = Market::new();
    let admin = m.env.admin.insecure_clone();
    m.env
        .send(
            &[set_series_paused_ix(&admin.pubkey(), &m.series, 1)],
            &[&admin],
        )
        .unwrap();
    fails(m.settle(m.terms(), 1), QuotebookError::SeriesPaused);
    m.env
        .send(
            &[set_series_paused_ix(&admin.pubkey(), &m.series, 0)],
            &[&admin],
        )
        .unwrap();
    m.settle(m.terms(), 1).unwrap();
}

// ---- forged and swapped accounts ----

/// Settles the default quote with `edit` applied to the settle instruction.
fn settle_edited(
    m: &mut Market,
    edit: impl FnOnce(&mut solana_instruction::Instruction),
) -> Result<litesvm::types::TransactionMetadata, litesvm::types::FailedTransactionMetadata> {
    let maker = m.maker.insecure_clone();
    let pre = m.precompile(m.terms(), &maker);
    let mut ix = m.settle_ix(0, 1);
    edit(&mut ix);
    m.send_as_taker(&[pre, ix])
}

#[test]
fn test_settle_taker_not_signer_is_rejected() {
    let mut m = Market::new();
    let maker = m.maker.insecure_clone();
    let pre = m.precompile(m.terms(), &maker);
    let mut ix = m.settle_ix(0, 1);
    ix.accounts[0] = AccountMeta::new_readonly(m.taker.pubkey(), false);
    fails(
        m.env.send(&[pre, ix], &[]),
        QuotebookError::MissingSignature,
    );
}

#[test]
fn test_settle_forged_series_is_rejected() {
    let mut m = Market::new();
    let data = m.env.data(&m.series);
    let elsewhere = Keypair::new().pubkey();
    m.env.set(elsewhere, PROGRAM_ID, data.clone());
    let foreign = Keypair::new().pubkey();
    m.env.set(foreign, LOADER, data.clone());
    let mut tagged = data;
    tagged[0] = 1;
    let wrong_tag = Keypair::new().pubkey();
    m.env.set(wrong_tag, PROGRAM_ID, tagged);
    for (series, err) in [
        (elsewhere, qb(QuotebookError::InvalidPda)),
        (foreign, qb(QuotebookError::InvalidAccountOwner)),
        (wrong_tag, InstructionError::InvalidAccountData),
    ] {
        assert_ix_err_at(
            settle_edited(&mut m, |ix| {
                ix.accounts[1] = AccountMeta::new_readonly(series, false)
            }),
            1,
            err,
        );
    }
}

#[test]
fn test_settle_forged_or_foreign_maker_state_is_rejected() {
    let mut m = Market::new();
    let other = m.env.maker();
    let me = m.maker.pubkey();
    let copy = Keypair::new().pubkey();
    let data = m.env.data(&maker_state_address(&me));
    m.env.set(copy, PROGRAM_ID, data);
    for (state, err) in [
        (
            maker_state_address(&other.pubkey()),
            qb(QuotebookError::NotAuthority),
        ),
        (copy, qb(QuotebookError::InvalidPda)),
    ] {
        assert_ix_err_at(
            settle_edited(&mut m, |ix| ix.accounts[2] = AccountMeta::new(state, false)),
            1,
            err,
        );
    }
    assert_ix_err_at(
        settle_edited(&mut m, |ix| {
            ix.accounts[2] = AccountMeta::new_readonly(maker_state_address(&me), false)
        }),
        1,
        InstructionError::Immutable,
    );
}

#[test]
fn test_settle_wrong_delegate_is_rejected() {
    let mut m = Market::new();
    let other = m.env.maker();
    for delegate in [delegate_address(&other.pubkey()), Keypair::new().pubkey()] {
        assert_ix_err_at(
            settle_edited(&mut m, |ix| {
                ix.accounts[3] = AccountMeta::new_readonly(delegate, false)
            }),
            1,
            qb(QuotebookError::InvalidPda),
        );
    }
}

#[test]
fn test_settle_wrong_mint_or_token_program_is_rejected() {
    let mut m = Market::new();
    let (base, quote) = (m.base_mint, m.quote_mint);
    assert_ix_err_at(
        settle_edited(&mut m, |ix| {
            ix.accounts[8] = AccountMeta::new_readonly(quote, false);
            ix.accounts[9] = AccountMeta::new_readonly(base, false);
        }),
        1,
        qb(QuotebookError::InvalidMint),
    );
    assert_ix_err_at(
        settle_edited(&mut m, |ix| {
            ix.accounts[10] = AccountMeta::new_readonly(token_2022_program(), false)
        }),
        1,
        InstructionError::IncorrectProgramId,
    );
    assert_ix_err_at(
        settle_edited(&mut m, |ix| {
            ix.accounts[11] = AccountMeta::new_readonly(PROGRAM_ID, false)
        }),
        1,
        InstructionError::IncorrectProgramId,
    );
}

#[test]
fn test_settle_swapped_or_forged_token_accounts_are_rejected() {
    let mut m = Market::new();
    let me = m.maker.pubkey();
    let taker = m.taker.pubkey();
    // Another holder's account of the right mint, the wrong mint, another program, frozen.
    let strangers_base = m.env.token_account(
        token_program(),
        &m.base_mint,
        &Keypair::new().pubkey(),
        FUNDS,
        None,
        &[],
    );
    let makers_wrong_mint =
        m.env
            .token_account(token_program(), &m.quote_mint, &me, FUNDS, None, &[]);
    let foreign = Keypair::new().pubkey();
    m.env.set(
        foreign,
        PROGRAM_ID,
        token_account_data(&m.base_mint, &taker, FUNDS, None, 1, &[]),
    );
    let frozen = Keypair::new().pubkey();
    m.env.set(
        frozen,
        token_program(),
        token_account_data(&m.base_mint, &taker, FUNDS, None, 2, &[]),
    );
    let (maker_base, taker_base) = (m.maker_base, m.taker_base);
    for (slot, account) in [
        (4usize, strangers_base),
        (4, makers_wrong_mint),
        (6, maker_base), // the taker passes the maker's account as its own
        (4, taker_base),
        (6, foreign),
        (6, frozen),
    ] {
        assert_ix_err_at(
            settle_edited(&mut m, |ix| {
                ix.accounts[slot] = AccountMeta::new(account, false)
            }),
            1,
            qb(QuotebookError::InvalidVaultOrTokenAccount),
        );
    }
    let taker_quote = m.taker_quote;
    assert_ix_err_at(
        settle_edited(&mut m, |ix| {
            ix.accounts[7] = AccountMeta::new_readonly(taker_quote, false)
        }),
        1,
        InstructionError::Immutable,
    );
}

/// A failed transfer (allowance revoked) rolls the whole settle back, nonce included.
#[test]
fn test_settle_failed_transfer_leaves_no_trace() {
    let mut m = Market::new();
    let mut data = m.env.data(&m.maker_quote);
    data[72..76].copy_from_slice(&0u32.to_le_bytes());
    data[121..129].copy_from_slice(&0u64.to_le_bytes());
    m.env.set(m.maker_quote, token_program(), data);
    let before = m.balances();
    assert!(m.settle(m.terms(), 1).is_err());
    assert_eq!(m.balances(), before);
    let state = m.env.maker_state(&m.maker.pubkey());
    assert_eq!(state.fills, 0);
    assert_eq!(state.nonces.check(Nonce::new(1)), Ok(()));
}

// ---- Token-2022 ----

fn immutable_owner() -> Vec<u8> {
    tlv(7, &[])
}

#[test]
fn test_settle_token_2022_both_sides() {
    let spec = MarketSpec {
        base_2022: true,
        maker_base_ext: immutable_owner(),
        taker_base_ext: immutable_owner(),
        ..MarketSpec::default()
    };
    for side in [Side::Bid, Side::Ask] {
        let mut m = Market::with(spec.clone());
        let mut t = m.terms();
        t.side = side;
        let before = m.balances();
        m.settle(t, 2).unwrap();
        assert_ne!(m.balances(), before);
    }
    let mut m = Market::with(MarketSpec {
        base_2022: true,
        quote_2022: true,
        ..MarketSpec::default()
    });
    m.settle(m.terms(), 2).unwrap();
}

#[test]
fn test_settle_token_2022_cpi_guard_on_taker_payer_is_rejected() {
    // Bid: the taker pays base.
    let mut m = Market::with(MarketSpec {
        base_2022: true,
        taker_base_ext: tlv(11, &[1]),
        ..MarketSpec::default()
    });
    fails(
        m.settle(m.terms(), 1),
        QuotebookError::AccountExtensionUnsupported,
    );
    // Disabled CPI Guard is fine.
    let mut m = Market::with(MarketSpec {
        base_2022: true,
        taker_base_ext: tlv(11, &[0]),
        ..MarketSpec::default()
    });
    m.settle(m.terms(), 1).unwrap();
}

/// CPI Guard does not stop a delegate transfer, so the maker's paying account may have it,
/// and a receiving account may too.
#[test]
fn test_settle_token_2022_cpi_guard_elsewhere_is_allowed() {
    let mut m = Market::with(MarketSpec {
        base_2022: true,
        quote_2022: true,
        maker_quote_ext: tlv(11, &[1]),
        taker_quote_ext: tlv(11, &[1]),
        ..MarketSpec::default()
    });
    m.settle(m.terms(), 1).unwrap();
}

#[test]
fn test_settle_token_2022_required_memo_on_receiver_is_rejected() {
    // Bid: the maker receives base, the taker receives quote.
    for spec in [
        MarketSpec {
            base_2022: true,
            maker_base_ext: tlv(8, &[1]),
            ..MarketSpec::default()
        },
        MarketSpec {
            quote_2022: true,
            taker_quote_ext: tlv(8, &[1]),
            ..MarketSpec::default()
        },
    ] {
        let mut m = Market::with(spec);
        fails(
            m.settle(m.terms(), 1),
            QuotebookError::AccountExtensionUnsupported,
        );
    }
    // A memo requirement on a paying account does not apply to outgoing transfers.
    let mut m = Market::with(MarketSpec {
        base_2022: true,
        taker_base_ext: tlv(8, &[1]),
        ..MarketSpec::default()
    });
    m.settle(m.terms(), 1).unwrap();
}

#[test]
fn test_settle_token_2022_malformed_account_extensions_are_rejected() {
    let mut truncated = tlv(7, &[]);
    truncated.extend_from_slice(&[11, 0, 5]);
    let mut m = Market::with(MarketSpec {
        base_2022: true,
        taker_base_ext: truncated,
        ..MarketSpec::default()
    });
    fails(
        m.settle(m.terms(), 1),
        QuotebookError::InvalidVaultOrTokenAccount,
    );
}
