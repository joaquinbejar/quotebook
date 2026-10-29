//! `init_maker`, `cancel_all` and `cancel_nonce`: happy paths, nonce semantics through the
//! program, and every account-validation rule with a forged or swapped account.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::panic_in_result_fn,
    clippy::result_large_err,
    missing_docs
)]

mod common;

use common::*;
use quotebook_core::{Nonce, QuotebookError, WINDOW_BITS};
use solana_address::Address;
use solana_instruction::error::InstructionError;
use solana_instruction::AccountMeta;
use solana_keypair::Keypair;
use solana_signer::Signer;

#[test]
fn test_init_maker_happy_path_writes_state() {
    let mut env = Env::new();
    let maker = env.maker();
    let m = env.maker_state(&maker.pubkey());
    assert_eq!(m.hdr.tag, 3);
    assert_eq!(m.hdr.version, 1);
    assert_eq!(
        m.hdr.bump,
        Address::find_program_address(&[b"maker", maker.pubkey().as_ref()], &PROGRAM_ID).1
    );
    assert_eq!(m.maker, maker.pubkey());
    assert_eq!(m.fills, 0);
    assert_eq!(m.delegate_bump, delegate_bump(&maker.pubkey()));
    assert_eq!(m.nonces.window_base(), Nonce::new(0));
    assert_eq!(m.nonces.used_count(), 0);
    assert_eq!(env.data(&maker_state_address(&maker.pubkey())).len(), 2_120);
}

#[test]
fn test_init_maker_twice_is_rejected() {
    let mut env = Env::new();
    let maker = env.maker();
    let ix = init_maker_ix(&maker.pubkey(), &env.payer.pubkey());
    assert_ix_err(
        env.send(&[ix], &[&maker]),
        InstructionError::AccountAlreadyInitialized,
    );
}

#[test]
fn test_init_maker_prefunded_pda_succeeds() {
    let mut env = Env::new();
    let maker = Keypair::new();
    let grief = env.svm.minimum_balance_for_rent_exemption(0);
    env.svm
        .airdrop(&maker_state_address(&maker.pubkey()), grief)
        .unwrap();
    let ix = init_maker_ix(&maker.pubkey(), &env.payer.pubkey());
    env.send(&[ix], &[&maker]).unwrap();
    assert_eq!(env.maker_state(&maker.pubkey()).maker, maker.pubkey());
}

#[test]
fn test_init_maker_not_signer_is_rejected() {
    let mut env = Env::new();
    let maker = Keypair::new();
    let mut ix = init_maker_ix(&maker.pubkey(), &env.payer.pubkey());
    ix.accounts[0] = AccountMeta::new_readonly(maker.pubkey(), false);
    assert_ix_err(env.send(&[ix], &[]), qb(QuotebookError::MissingSignature));
}

/// The signer creates a state at another maker's PDA (or any other address).
#[test]
fn test_init_maker_wrong_address_is_rejected() {
    let mut env = Env::new();
    let maker = Keypair::new();
    let victim = Keypair::new().pubkey();
    for wrong in [maker_state_address(&victim), Keypair::new().pubkey()] {
        let mut ix = init_maker_ix(&maker.pubkey(), &env.payer.pubkey());
        ix.accounts[2] = AccountMeta::new(wrong, false);
        assert_ix_err(env.send(&[ix], &[&maker]), qb(QuotebookError::InvalidPda));
    }
}

#[test]
fn test_init_maker_not_writable_and_bad_args_are_rejected() {
    let mut env = Env::new();
    let maker = Keypair::new();
    let mut ix = init_maker_ix(&maker.pubkey(), &env.payer.pubkey());
    ix.accounts[2] = AccountMeta::new_readonly(maker_state_address(&maker.pubkey()), false);
    assert_ix_err(env.send(&[ix], &[&maker]), InstructionError::Immutable);

    let mut ix = init_maker_ix(&maker.pubkey(), &env.payer.pubkey());
    ix.data.push(0);
    assert_ix_err(
        env.send(&[ix], &[&maker]),
        qb(QuotebookError::InvalidInstructionData),
    );

    let mut ix = init_maker_ix(&maker.pubkey(), &env.payer.pubkey());
    ix.accounts[3] = AccountMeta::new_readonly(Keypair::new().pubkey(), false);
    assert_ix_err(
        env.send(&[ix], &[&maker]),
        InstructionError::IncorrectProgramId,
    );
}

fn cancel(env: &mut Env, tag: u8, maker: &Keypair, value: u64) -> Result<(), InstructionError> {
    let ix = cancel_ix(
        tag,
        &maker.pubkey(),
        &maker_state_address(&maker.pubkey()),
        value,
    );
    match env.send(&[ix], &[maker]) {
        Ok(_) => Ok(()),
        Err(f) => {
            let s = format!("{:?}", f.err);
            let code = s
                .strip_prefix("InstructionError(0, Custom(")
                .and_then(|r| r.strip_suffix("))"))
                .and_then(|c| c.parse::<u32>().ok());
            Err(code
                .map(InstructionError::Custom)
                .unwrap_or_else(|| panic!("unexpected error {s}")))
        }
    }
}

#[test]
fn test_cancel_all_and_cancel_nonce_follow_the_window() {
    let mut env = Env::new();
    let maker = env.maker();
    let me = maker.pubkey();

    cancel(&mut env, tag::CANCEL_NONCE, &maker, 5).unwrap();
    assert_eq!(
        cancel(&mut env, tag::CANCEL_NONCE, &maker, 5),
        Err(qb(QuotebookError::NonceUsed))
    );
    assert_eq!(env.maker_state(&me).nonces.used_count(), 1);

    cancel(&mut env, tag::CANCEL_ALL, &maker, 100).unwrap();
    let m = env.maker_state(&me);
    assert_eq!(m.nonces.window_base(), Nonce::new(100));
    assert_eq!(m.nonces.last_cancel_all(), Nonce::new(100));
    assert_eq!(m.nonces.used_count(), 0);
    assert_eq!(
        cancel(&mut env, tag::CANCEL_NONCE, &maker, 99),
        Err(qb(QuotebookError::NonceCancelled))
    );
    for same_or_lower in [100, 50] {
        assert_eq!(
            cancel(&mut env, tag::CANCEL_ALL, &maker, same_or_lower),
            Err(qb(QuotebookError::WatermarkNotIncreasing))
        );
    }

    // A nonce beyond the window slides it.
    cancel(&mut env, tag::CANCEL_NONCE, &maker, 100 + WINDOW_BITS + 9).unwrap();
    let m = env.maker_state(&me);
    assert_eq!(m.nonces.window_base(), Nonce::new(110));
    assert_eq!(
        cancel(&mut env, tag::CANCEL_NONCE, &maker, 105),
        Err(qb(QuotebookError::NonceOutOfWindow))
    );
}

#[test]
fn test_cancel_not_signer_is_rejected() {
    let mut env = Env::new();
    let maker = env.maker();
    for t in [tag::CANCEL_ALL, tag::CANCEL_NONCE] {
        let mut ix = cancel_ix(t, &maker.pubkey(), &maker_state_address(&maker.pubkey()), 1);
        ix.accounts[0] = AccountMeta::new_readonly(maker.pubkey(), false);
        assert_ix_err(env.send(&[ix], &[]), qb(QuotebookError::MissingSignature));
    }
}

/// A maker cancels against someone else's state: their own key signs, the victim's state is
/// passed.
#[test]
fn test_cancel_other_makers_state_is_rejected() {
    let mut env = Env::new();
    let victim = env.maker();
    let attacker = env.maker();
    for t in [tag::CANCEL_ALL, tag::CANCEL_NONCE] {
        let ix = cancel_ix(
            t,
            &attacker.pubkey(),
            &maker_state_address(&victim.pubkey()),
            u64::MAX,
        );
        assert_ix_err(
            env.send(&[ix], &[&attacker]),
            qb(QuotebookError::NotAuthority),
        );
    }
    assert_eq!(
        env.maker_state(&victim.pubkey()).nonces.window_base(),
        Nonce::new(0)
    );
}

/// Copies the signer's own state to `owner`-owned data at a fresh address, after `edit`.
fn forged_state(
    env: &mut Env,
    maker: &Address,
    owner: Address,
    edit: impl FnOnce(&mut Vec<u8>),
) -> Address {
    let mut data = env.data(&maker_state_address(maker));
    edit(&mut data);
    let forged = Keypair::new().pubkey();
    env.set(forged, owner, data);
    forged
}

#[test]
fn test_cancel_forged_state_is_rejected() {
    let mut env = Env::new();
    let maker = env.maker();
    let me = maker.pubkey();
    let cases = [
        (
            "owned by another program",
            forged_state(&mut env, &me, LOADER, |_| {}),
            qb(QuotebookError::InvalidAccountOwner),
        ),
        (
            "quotebook-owned copy, not the PDA",
            forged_state(&mut env, &me, PROGRAM_ID, |_| {}),
            qb(QuotebookError::InvalidPda),
        ),
        (
            "wrong tag",
            forged_state(&mut env, &me, PROGRAM_ID, |d| d[0] = 1),
            InstructionError::InvalidAccountData,
        ),
        (
            "wrong version",
            forged_state(&mut env, &me, PROGRAM_ID, |d| d[1] = 2),
            InstructionError::InvalidAccountData,
        ),
        (
            "wrong length",
            forged_state(&mut env, &me, PROGRAM_ID, |d| d.truncate(2_112)),
            InstructionError::InvalidAccountData,
        ),
    ];
    for (name, state, err) in cases {
        for t in [tag::CANCEL_ALL, tag::CANCEL_NONCE] {
            let ix = cancel_ix(t, &me, &state, 7);
            let r = env.send(&[ix], &[&maker]);
            assert!(r.is_err(), "{name} accepted");
            assert_ix_err(r, err.clone());
        }
    }
}

/// The real PDA with a stored bump that is not the canonical one.
#[test]
fn test_cancel_tampered_bump_is_rejected() {
    let mut env = Env::new();
    let maker = env.maker();
    let me = maker.pubkey();
    let address = maker_state_address(&me);
    let mut data = env.data(&address);
    data[2] = data[2].wrapping_sub(1);
    env.set(address, PROGRAM_ID, data);
    let ix = cancel_ix(tag::CANCEL_ALL, &me, &address, 7);
    assert_ix_err(env.send(&[ix], &[&maker]), qb(QuotebookError::InvalidPda));
}

#[test]
fn test_cancel_not_writable_and_bad_args_are_rejected() {
    let mut env = Env::new();
    let maker = env.maker();
    let me = maker.pubkey();
    for t in [tag::CANCEL_ALL, tag::CANCEL_NONCE] {
        let mut ix = cancel_ix(t, &me, &maker_state_address(&me), 7);
        ix.accounts[1] = AccountMeta::new_readonly(maker_state_address(&me), false);
        assert_ix_err(env.send(&[ix], &[&maker]), InstructionError::Immutable);

        let mut ix = cancel_ix(t, &me, &maker_state_address(&me), 7);
        ix.data.pop();
        assert_ix_err(
            env.send(&[ix], &[&maker]),
            qb(QuotebookError::InvalidInstructionData),
        );
    }
}
