//! `set_config` and `set_series_paused`: happy paths and every account-validation rule with
//! a forged or swapped account.

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
use quotebook_core::QuotebookError;
use solana_address::Address;
use solana_instruction::error::InstructionError;
use solana_instruction::AccountMeta;
use solana_keypair::Keypair;
use solana_signer::Signer;

fn ready() -> Env {
    let mut env = Env::new();
    env.init_config();
    env
}

#[test]
fn test_set_config_hands_over_admin() {
    let mut env = ready();
    let old = env.admin.insecure_clone();
    let new = Keypair::new();
    env.send(&[set_config_ix(&old.pubkey(), &new.pubkey())], &[&old])
        .unwrap();
    assert_eq!(env.config().admin, new.pubkey());
    assert_eq!(env.config().deployment_id, DEPLOYMENT);

    // The old admin is out, the new one can create series.
    let base = env.spl_mint();
    let quote = env.spl_mint();
    let ix = create_series_ix(
        &old.pubkey(),
        &env.payer.pubkey(),
        &series_address(&base, &quote, 0),
        &base,
        &quote,
        SeriesArgs::default().encode(),
    );
    assert_ix_err(env.send(&[ix], &[&old]), qb(QuotebookError::NotAuthority));
    env.admin = new;
    env.create_series_with(&base, &quote);
}

#[test]
fn test_set_config_rejections() {
    let mut env = ready();
    let admin = env.admin.insecure_clone();
    let attacker = Keypair::new();
    assert_ix_err(
        env.send(
            &[set_config_ix(&attacker.pubkey(), &attacker.pubkey())],
            &[&attacker],
        ),
        qb(QuotebookError::NotAuthority),
    );
    let mut ix = set_config_ix(&admin.pubkey(), &attacker.pubkey());
    ix.accounts[0] = AccountMeta::new_readonly(admin.pubkey(), false);
    assert_ix_err(env.send(&[ix], &[]), qb(QuotebookError::MissingSignature));
    assert_ix_err(
        env.send(
            &[set_config_ix(
                &admin.pubkey(),
                &Address::new_from_array([0; 32]),
            )],
            &[&admin],
        ),
        qb(QuotebookError::InvalidInstructionData),
    );
    let mut ix = set_config_ix(&admin.pubkey(), &attacker.pubkey());
    ix.data.pop();
    assert_ix_err(
        env.send(&[ix], &[&admin]),
        qb(QuotebookError::InvalidInstructionData),
    );
    let mut ix = set_config_ix(&admin.pubkey(), &attacker.pubkey());
    ix.accounts[1] = AccountMeta::new_readonly(config_address(), false);
    assert_ix_err(env.send(&[ix], &[&admin]), InstructionError::Immutable);
    assert_eq!(env.config().admin, admin.pubkey());
}

/// A quotebook-owned copy of the config naming the attacker as admin.
#[test]
fn test_set_config_forged_config_is_rejected() {
    let mut env = ready();
    let attacker = Keypair::new();
    let mut data = env.data(&config_address());
    data[8..40].copy_from_slice(attacker.pubkey().as_ref());
    let forged = Keypair::new().pubkey();
    env.set(forged, PROGRAM_ID, data.clone());
    let foreign = Keypair::new().pubkey();
    env.set(foreign, LOADER, data);
    for (config, err) in [
        (forged, qb(QuotebookError::InvalidPda)),
        (foreign, qb(QuotebookError::InvalidAccountOwner)),
    ] {
        let mut ix = set_config_ix(&attacker.pubkey(), &attacker.pubkey());
        ix.accounts[1] = AccountMeta::new(config, false);
        assert_ix_err(env.send(&[ix], &[&attacker]), err);
    }
}

#[test]
fn test_set_series_paused_toggles() {
    let mut env = ready();
    let (series, _) = env.create_series();
    let admin = env.admin.insecure_clone();
    env.send(
        &[set_series_paused_ix(&admin.pubkey(), &series, 1)],
        &[&admin],
    )
    .unwrap();
    assert_eq!(env.series(&series).paused, 1);
    env.send(
        &[set_series_paused_ix(&admin.pubkey(), &series, 0)],
        &[&admin],
    )
    .unwrap();
    assert_eq!(env.series(&series).paused, 0);
}

#[test]
fn test_set_series_paused_rejections() {
    let mut env = ready();
    let (series, _) = env.create_series();
    let admin = env.admin.insecure_clone();
    let attacker = Keypair::new();
    assert_ix_err(
        env.send(
            &[set_series_paused_ix(&attacker.pubkey(), &series, 1)],
            &[&attacker],
        ),
        qb(QuotebookError::NotAuthority),
    );
    let mut ix = set_series_paused_ix(&admin.pubkey(), &series, 1);
    ix.accounts[0] = AccountMeta::new_readonly(admin.pubkey(), false);
    assert_ix_err(env.send(&[ix], &[]), qb(QuotebookError::MissingSignature));
    for bad in [2u8, 255] {
        assert_ix_err(
            env.send(
                &[set_series_paused_ix(&admin.pubkey(), &series, bad)],
                &[&admin],
            ),
            qb(QuotebookError::InvalidInstructionData),
        );
    }
    let mut ix = set_series_paused_ix(&admin.pubkey(), &series, 1);
    ix.accounts[2] = AccountMeta::new_readonly(series, false);
    assert_ix_err(env.send(&[ix], &[&admin]), InstructionError::Immutable);

    // A forged config naming the attacker, and a forged series copy.
    let mut data = env.data(&config_address());
    data[8..40].copy_from_slice(attacker.pubkey().as_ref());
    let forged_config = Keypair::new().pubkey();
    env.set(forged_config, PROGRAM_ID, data);
    let mut ix = set_series_paused_ix(&attacker.pubkey(), &series, 1);
    ix.accounts[1] = AccountMeta::new_readonly(forged_config, false);
    assert_ix_err(
        env.send(&[ix], &[&attacker]),
        qb(QuotebookError::InvalidPda),
    );

    let copy = Keypair::new().pubkey();
    let data = env.data(&series);
    env.set(copy, PROGRAM_ID, data);
    assert_ix_err(
        env.send(
            &[set_series_paused_ix(&admin.pubkey(), &copy, 1)],
            &[&admin],
        ),
        qb(QuotebookError::InvalidPda),
    );
    let mut wrong_tag = env.data(&series);
    wrong_tag[0] = 3;
    let tagged = Keypair::new().pubkey();
    env.set(tagged, PROGRAM_ID, wrong_tag);
    assert_ix_err(
        env.send(
            &[set_series_paused_ix(&admin.pubkey(), &tagged, 1)],
            &[&admin],
        ),
        InstructionError::InvalidAccountData,
    );
    assert_eq!(env.series(&series).paused, 0);
}
