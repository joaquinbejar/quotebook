//! `init_config`: happy path, the upgrade-authority gate and every account-validation rule.

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
use quotebook_core::QuotebookError;
use solana_instruction::error::InstructionError;
use solana_instruction::AccountMeta;
use solana_keypair::Keypair;
use solana_signer::Signer;

#[test]
fn test_init_config_happy_path_writes_config() {
    let mut env = Env::new();
    env.init_config();
    let config = env.config();
    assert_eq!(config.hdr.tag, 1);
    assert_eq!(config.hdr.version, 1);
    assert_eq!(
        config.hdr.bump,
        solana_address::Address::find_program_address(&[b"config"], &PROGRAM_ID).1
    );
    assert_eq!(config.admin, env.admin.pubkey());
    assert_eq!(config.series_count, 0);
    assert_eq!(config.deployment_id, DEPLOYMENT);
    let account = env.svm.get_account(&config_address()).unwrap();
    assert_eq!(account.owner, PROGRAM_ID);
    assert_eq!(account.data.len(), 64);
}

#[test]
fn test_init_config_twice_is_rejected() {
    let mut env = Env::new();
    env.init_config();
    let ix = init_config_ix(&env.admin.pubkey(), &env.payer.pubkey(), DEPLOYMENT);
    let admin = env.admin.insecure_clone();
    assert_ix_err(
        env.send(&[ix], &[&admin]),
        InstructionError::AccountAlreadyInitialized,
    );
}

#[test]
fn test_init_config_prefunded_pda_succeeds() {
    let mut env = Env::new();
    // Anyone can send lamports to the config address before it exists (at least the rent
    // exemption of an empty account, or the transfer itself fails).
    let grief = env.svm.minimum_balance_for_rent_exemption(0);
    env.svm.airdrop(&config_address(), grief).unwrap();
    env.init_config();
    assert_eq!(env.config().admin, env.admin.pubkey());
    let account = env.svm.get_account(&config_address()).unwrap();
    assert_eq!(
        account.lamports,
        env.svm.minimum_balance_for_rent_exemption(64)
    );
}

#[test]
fn test_init_config_non_authority_admin_is_rejected() {
    let mut env = Env::new();
    let attacker = Keypair::new();
    let ix = init_config_ix(&attacker.pubkey(), &env.payer.pubkey(), DEPLOYMENT);
    assert_ix_err(
        env.send(&[ix], &[&attacker]),
        qb(QuotebookError::NotAuthority),
    );
}

#[test]
fn test_init_config_immutable_program_is_rejected() {
    let mut env = Env::new();
    env.set_upgrade_authority(None);
    let ix = init_config_ix(&env.admin.pubkey(), &env.payer.pubkey(), DEPLOYMENT);
    let admin = env.admin.insecure_clone();
    assert_ix_err(env.send(&[ix], &[&admin]), qb(QuotebookError::NotAuthority));
}

#[test]
fn test_init_config_admin_not_signer_is_rejected() {
    let mut env = Env::new();
    let mut ix = init_config_ix(&env.admin.pubkey(), &env.payer.pubkey(), DEPLOYMENT);
    ix.accounts[0] = AccountMeta::new_readonly(env.admin.pubkey(), false);
    assert_ix_err(env.send(&[ix], &[]), qb(QuotebookError::MissingSignature));
}

/// A `ProgramData` look-alike with the right authority, owned by another program.
#[test]
fn test_init_config_forged_program_data_owner_is_rejected() {
    let mut env = Env::new();
    let forged = Keypair::new().pubkey();
    let data = env.data(&program_data_address());
    env.set(forged, PROGRAM_ID, data);
    let mut ix = init_config_ix(&env.admin.pubkey(), &env.payer.pubkey(), DEPLOYMENT);
    ix.accounts[3] = AccountMeta::new_readonly(forged, false);
    let admin = env.admin.insecure_clone();
    assert_ix_err(
        env.send(&[ix], &[&admin]),
        qb(QuotebookError::InvalidAccountOwner),
    );
}

/// A loader-owned account with the right authority, at an address that is not this
/// program's `ProgramData`.
#[test]
fn test_init_config_forged_program_data_address_is_rejected() {
    let mut env = Env::new();
    let forged = Keypair::new().pubkey();
    let data = env.data(&program_data_address());
    env.set(forged, LOADER, data);
    let mut ix = init_config_ix(&env.admin.pubkey(), &env.payer.pubkey(), DEPLOYMENT);
    ix.accounts[3] = AccountMeta::new_readonly(forged, false);
    let admin = env.admin.insecure_clone();
    assert_ix_err(env.send(&[ix], &[&admin]), qb(QuotebookError::InvalidPda));
}

#[test]
fn test_init_config_config_at_wrong_address_is_rejected() {
    let mut env = Env::new();
    let mut ix = init_config_ix(&env.admin.pubkey(), &env.payer.pubkey(), DEPLOYMENT);
    ix.accounts[2] = AccountMeta::new(Keypair::new().pubkey(), false);
    let admin = env.admin.insecure_clone();
    assert_ix_err(env.send(&[ix], &[&admin]), qb(QuotebookError::InvalidPda));
}

#[test]
fn test_init_config_config_not_writable_is_rejected() {
    let mut env = Env::new();
    let mut ix = init_config_ix(&env.admin.pubkey(), &env.payer.pubkey(), DEPLOYMENT);
    ix.accounts[2] = AccountMeta::new_readonly(config_address(), false);
    let admin = env.admin.insecure_clone();
    assert_ix_err(env.send(&[ix], &[&admin]), InstructionError::Immutable);
}

#[test]
fn test_init_config_wrong_system_program_is_rejected() {
    let mut env = Env::new();
    let mut ix = init_config_ix(&env.admin.pubkey(), &env.payer.pubkey(), DEPLOYMENT);
    ix.accounts[4] = AccountMeta::new_readonly(Keypair::new().pubkey(), false);
    let admin = env.admin.insecure_clone();
    assert_ix_err(
        env.send(&[ix], &[&admin]),
        InstructionError::IncorrectProgramId,
    );
}

#[test]
fn test_init_config_bad_deployment_id_is_rejected() {
    let mut env = Env::new();
    let admin = env.admin.insecure_clone();
    let ix = init_config_ix(&env.admin.pubkey(), &env.payer.pubkey(), [0; 16]);
    assert_ix_err(
        env.send(&[ix], &[&admin]),
        qb(QuotebookError::InvalidInstructionData),
    );
    let mut ix = init_config_ix(&env.admin.pubkey(), &env.payer.pubkey(), DEPLOYMENT);
    ix.data.pop();
    assert_ix_err(
        env.send(&[ix], &[&admin]),
        qb(QuotebookError::InvalidInstructionData),
    );
}

#[test]
fn test_unknown_tag_and_missing_accounts_are_rejected() {
    let mut env = Env::new();
    let mut ix = init_config_ix(&env.admin.pubkey(), &env.payer.pubkey(), DEPLOYMENT);
    let admin = env.admin.insecure_clone();
    for unassigned in [8u8, 255] {
        ix.data = vec![unassigned];
        assert_ix_err(
            env.send(&[ix.clone()], &[&admin]),
            qb(QuotebookError::InvalidInstruction),
        );
    }
    ix.data = vec![];
    assert_ix_err(
        env.send(&[ix.clone()], &[&admin]),
        qb(QuotebookError::InvalidInstruction),
    );
    let mut ix = init_config_ix(&env.admin.pubkey(), &env.payer.pubkey(), DEPLOYMENT);
    ix.accounts.truncate(4);
    // The program returns `ProgramError::NotEnoughAccountKeys`, which the runtime reports as
    // this (deprecated) variant.
    #[allow(deprecated)]
    assert_ix_err(
        env.send(&[ix], &[&admin]),
        InstructionError::NotEnoughAccountKeys,
    );
}
