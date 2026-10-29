//! `create_series`: happy paths (SPL Token and Token-2022), the mint and extension rules and
//! every account-validation rule, each with a forged or swapped account.

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
use solana_address::Address;
use solana_instruction::error::InstructionError;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_signer::Signer;

fn ready() -> Env {
    let mut env = Env::new();
    env.init_config();
    env
}

/// `create_series` for `base`/`quote` at the next index, with `edit` applied to the
/// instruction before sending.
fn try_series(
    env: &mut Env,
    base: &Address,
    quote: &Address,
    edit: impl FnOnce(&mut Instruction),
) -> Result<litesvm::types::TransactionMetadata, litesvm::types::FailedTransactionMetadata> {
    let index = env.config().series_count;
    let mut ix = create_series_ix(
        &env.admin.pubkey(),
        &env.payer.pubkey(),
        &series_address(base, quote, index),
        base,
        quote,
        SeriesArgs::default().encode(),
    );
    edit(&mut ix);
    let admin = env.admin.insecure_clone();
    env.send(&[ix], &[&admin])
}

#[test]
fn test_create_series_happy_path_writes_series() {
    let mut env = ready();
    let base = env.spl_mint();
    let quote = env.spl_mint();
    let (address, _) = env.create_series_with(&base, &quote);
    let s = env.series(&address);
    assert_eq!(s.hdr.tag, 2);
    assert_eq!(s.hdr.version, 1);
    assert_eq!(s.series_index, 0);
    assert_eq!(s.deployment_id, DEPLOYMENT);
    assert_eq!(s.base_mint, base);
    assert_eq!(s.quote_mint, quote);
    assert_eq!(s.base_lot_size.get(), 1_000);
    assert_eq!(s.quote_lot_size.get(), 10);
    assert_eq!(s.max_quote_ttl.get(), 60);
    assert_eq!(s.min_size.get(), 1);
    assert_eq!((s.paused, s.base_is_2022, s.quote_is_2022), (0, 0, 0));
    assert_eq!(env.config().series_count, 1);
    assert_eq!(env.data(&address).len(), 144);
}

#[test]
fn test_create_series_same_pair_twice_gets_index_one() {
    let mut env = ready();
    let base = env.spl_mint();
    let quote = env.spl_mint();
    env.create_series_with(&base, &quote);
    let (second, _) = env.create_series_with(&base, &quote);
    assert_eq!(env.series(&second).series_index, 1);
    assert_eq!(env.config().series_count, 2);
}

#[test]
fn test_create_series_prefunded_pda_succeeds() {
    let mut env = ready();
    let base = env.spl_mint();
    let quote = env.spl_mint();
    let grief = env.svm.minimum_balance_for_rent_exemption(0);
    env.svm
        .airdrop(&series_address(&base, &quote, 0), grief)
        .unwrap();
    let (address, _) = env.create_series_with(&base, &quote);
    assert_eq!(env.series(&address).base_mint, base);
}

#[test]
fn test_create_series_token_2022_mints_are_flagged() {
    let mut env = ready();
    let base = env.mint_2022(&[]);
    let quote = env.mint_2022(&tlv(18, &[0; 64]));
    let (address, _) = env.create_series_with(&base, &quote);
    let s = env.series(&address);
    assert_eq!((s.base_is_2022, s.quote_is_2022), (1, 1));
}

/// Display and metadata extensions pass, including a non-frozen default account state.
#[test]
fn test_create_series_allowed_extensions_pass() {
    let mut env = ready();
    let quote = env.spl_mint();
    for ext in [
        tlv(3, &[0; 32]),  // MintCloseAuthority
        tlv(6, &[1]),      // DefaultAccountState = Initialized
        tlv(10, &[0; 52]), // InterestBearingConfig
        tlv(18, &[0; 64]), // MetadataPointer
        tlv(19, &[0; 90]), // TokenMetadata
        tlv(20, &[0; 64]), // GroupPointer
        tlv(21, &[0; 80]), // TokenGroup
        tlv(22, &[0; 64]), // GroupMemberPointer
        tlv(23, &[0; 72]), // TokenGroupMember
        tlv(25, &[0; 56]), // ScaledUiAmount
        [tlv(18, &[0; 64]), tlv(19, &[7; 10])].concat(),
    ] {
        let base = env.mint_2022(&ext);
        try_series(&mut env, &base, &quote, |_| {}).unwrap();
    }
}

#[test]
fn test_create_series_rejected_extensions_fail() {
    let mut env = ready();
    let quote = env.spl_mint();
    for (name, ext) in [
        ("TransferFeeConfig", tlv(1, &[0; 108])),
        ("ConfidentialTransferMint", tlv(4, &[0; 65])),
        ("DefaultAccountState = Frozen", tlv(6, &[2])),
        ("NonTransferable", tlv(9, &[])),
        ("CpiGuard (account-only)", tlv(11, &[0])),
        ("PermanentDelegate", tlv(12, &[0; 32])),
        ("TransferHook", tlv(14, &[0; 64])),
        ("ConfidentialTransferFeeConfig", tlv(16, &[0; 129])),
        ("ConfidentialMintBurn", tlv(24, &[0; 196])),
        ("Pausable", tlv(26, &[0; 33])),
        ("PermissionedBurn", tlv(28, &[0; 32])),
        ("unknown type", tlv(999, &[])),
        (
            "rejected after an allowed one",
            [tlv(18, &[0; 64]), tlv(14, &[0; 64])].concat(),
        ),
    ] {
        let base = env.mint_2022(&ext);
        let r = try_series(&mut env, &base, &quote, |_| {});
        assert!(r.is_err(), "{name} accepted");
        assert_ix_err(r, qb(QuotebookError::MintExtensionUnsupported));
    }
}

#[test]
fn test_create_series_malformed_mints_fail() {
    let mut env = ready();
    let quote = env.spl_mint();
    let mut cases: Vec<(&str, Address, Vec<u8>)> = Vec::new();
    let t22 = token_2022_program();
    let spl = token_program();
    let mut nonzero_padding = mint_data(&tlv(18, &[0; 64]));
    nonzero_padding[100] = 1;
    let mut wrong_type = mint_data(&tlv(18, &[0; 64]));
    wrong_type[165] = 2;
    let mut truncated = mint_data(&tlv(18, &[0; 64]));
    truncated.truncate(truncated.len() - 1);
    let mut short_header = mint_data(&tlv(18, &[0; 64]));
    short_header.extend_from_slice(&[18, 0, 4]);
    let mut uninitialized = mint_data(&[]);
    uninitialized[45] = 0;
    let frozen_len = mint_data(&tlv(6, &[2, 0]));
    cases.push(("non-zero padding", t22, nonzero_padding));
    cases.push(("account type Account", t22, wrong_type));
    cases.push(("value past the end", t22, truncated));
    cases.push(("half a TLV header", t22, short_header));
    cases.push(("uninitialized", spl, uninitialized));
    cases.push((
        "SPL Token with extra bytes",
        spl,
        mint_data(&tlv(18, &[0; 64])),
    ));
    cases.push(("Token-2022 between base and extended", t22, vec![0; 120]));
    cases.push(("multisig length", t22, {
        let mut d = mint_data(&tlv(18, &[0; 64]));
        d.resize(355, 0);
        d
    }));
    cases.push(("DefaultAccountState with two bytes", t22, frozen_len));
    cases.push(("empty data", spl, vec![]));
    for (name, owner, data) in cases {
        let base = Keypair::new().pubkey();
        env.set(base, owner, data);
        let r = try_series(&mut env, &base, &quote, |_| {});
        assert!(r.is_err(), "{name} accepted");
        assert_ix_err(r, qb(QuotebookError::InvalidMint));
    }
}

#[test]
fn test_create_series_mint_owned_by_other_program_fails() {
    let mut env = ready();
    let quote = env.spl_mint();
    let base = Keypair::new().pubkey();
    env.set(base, PROGRAM_ID, mint_data(&[]));
    assert_ix_err(
        try_series(&mut env, &base, &quote, |_| {}),
        qb(QuotebookError::InvalidMint),
    );
}

#[test]
fn test_create_series_same_mint_twice_fails() {
    let mut env = ready();
    let mint = env.spl_mint();
    assert_ix_err(
        try_series(&mut env, &mint, &mint, |_| {}),
        qb(QuotebookError::InvalidMint),
    );
}

#[test]
fn test_create_series_admin_not_signer_fails() {
    let mut env = ready();
    let base = env.spl_mint();
    let quote = env.spl_mint();
    let index = env.config().series_count;
    let mut ix = create_series_ix(
        &env.admin.pubkey(),
        &env.payer.pubkey(),
        &series_address(&base, &quote, index),
        &base,
        &quote,
        SeriesArgs::default().encode(),
    );
    ix.accounts[0] = AccountMeta::new_readonly(env.admin.pubkey(), false);
    assert_ix_err(env.send(&[ix], &[]), qb(QuotebookError::MissingSignature));
}

#[test]
fn test_create_series_other_admin_fails() {
    let mut env = ready();
    let base = env.spl_mint();
    let quote = env.spl_mint();
    let attacker = Keypair::new();
    let ix = create_series_ix(
        &attacker.pubkey(),
        &env.payer.pubkey(),
        &series_address(&base, &quote, 0),
        &base,
        &quote,
        SeriesArgs::default().encode(),
    );
    assert_ix_err(
        env.send(&[ix], &[&attacker]),
        qb(QuotebookError::NotAuthority),
    );
}

/// A config account naming the attacker as admin, at an address the attacker controls.
fn forged_config(env: &mut Env, owner: Address, tag: u8, admin: &Address) -> Address {
    let mut data = env.data(&config_address());
    data[0] = tag;
    data[8..40].copy_from_slice(admin.as_ref());
    let forged = Keypair::new().pubkey();
    env.set(forged, owner, data);
    forged
}

#[test]
fn test_create_series_forged_config_fails() {
    let mut env = ready();
    let base = env.spl_mint();
    let quote = env.spl_mint();
    let attacker = Keypair::new();
    let cases = [
        (
            "owned by another program",
            forged_config(&mut env, LOADER, 1, &attacker.pubkey()),
            qb(QuotebookError::InvalidAccountOwner),
        ),
        (
            "quotebook-owned, not the config PDA",
            forged_config(&mut env, PROGRAM_ID, 1, &attacker.pubkey()),
            qb(QuotebookError::InvalidPda),
        ),
        (
            "wrong tag",
            forged_config(&mut env, PROGRAM_ID, 3, &attacker.pubkey()),
            InstructionError::InvalidAccountData,
        ),
    ];
    for (name, config, err) in cases {
        let mut ix = create_series_ix(
            &attacker.pubkey(),
            &env.payer.pubkey(),
            &series_address(&base, &quote, 0),
            &base,
            &quote,
            SeriesArgs::default().encode(),
        );
        ix.accounts[2] = AccountMeta::new(config, false);
        let r = env.send(&[ix], &[&attacker]);
        assert!(r.is_err(), "{name} accepted");
        assert_ix_err(r, err);
    }
}

#[test]
fn test_create_series_config_wrong_length_fails() {
    let mut env = ready();
    let base = env.spl_mint();
    let quote = env.spl_mint();
    let mut data = env.data(&config_address());
    data.push(0);
    env.set(config_address(), PROGRAM_ID, data);
    // Built by hand: `try_series` reads the series count from the (now 65-byte) config.
    let ix = create_series_ix(
        &env.admin.pubkey(),
        &env.payer.pubkey(),
        &series_address(&base, &quote, 0),
        &base,
        &quote,
        SeriesArgs::default().encode(),
    );
    let admin = env.admin.insecure_clone();
    assert_ix_err(
        env.send(&[ix], &[&admin]),
        InstructionError::InvalidAccountData,
    );
}

#[test]
fn test_create_series_config_not_writable_fails() {
    let mut env = ready();
    let base = env.spl_mint();
    let quote = env.spl_mint();
    assert_ix_err(
        try_series(&mut env, &base, &quote, |ix| {
            ix.accounts[2] = AccountMeta::new_readonly(config_address(), false);
        }),
        InstructionError::Immutable,
    );
}

#[test]
fn test_create_series_series_at_wrong_address_fails() {
    let mut env = ready();
    let base = env.spl_mint();
    let quote = env.spl_mint();
    for wrong in [
        series_address(&base, &quote, 1),
        series_address(&quote, &base, 0),
        Keypair::new().pubkey(),
    ] {
        assert_ix_err(
            try_series(&mut env, &base, &quote, |ix| {
                ix.accounts[3] = AccountMeta::new(wrong, false);
            }),
            qb(QuotebookError::InvalidPda),
        );
    }
}

#[test]
fn test_create_series_series_not_writable_fails() {
    let mut env = ready();
    let base = env.spl_mint();
    let quote = env.spl_mint();
    let series = series_address(&base, &quote, 0);
    assert_ix_err(
        try_series(&mut env, &base, &quote, |ix| {
            ix.accounts[3] = AccountMeta::new_readonly(series, false);
        }),
        InstructionError::Immutable,
    );
}

#[test]
fn test_create_series_wrong_system_program_fails() {
    let mut env = ready();
    let base = env.spl_mint();
    let quote = env.spl_mint();
    assert_ix_err(
        try_series(&mut env, &base, &quote, |ix| {
            ix.accounts[6] = AccountMeta::new_readonly(Keypair::new().pubkey(), false);
        }),
        InstructionError::IncorrectProgramId,
    );
}

#[test]
fn test_create_series_bad_arguments_fail() {
    let mut env = ready();
    let base = env.spl_mint();
    let quote = env.spl_mint();
    let d = SeriesArgs::default();
    for args in [
        SeriesArgs {
            base_lot_size: 0,
            ..d
        }
        .encode(),
        SeriesArgs {
            quote_lot_size: 0,
            ..d
        }
        .encode(),
        SeriesArgs {
            max_quote_ttl: 0,
            ..d
        }
        .encode(),
        SeriesArgs {
            max_quote_ttl: -1,
            ..d
        }
        .encode(),
        SeriesArgs { min_size: 0, ..d }.encode(),
        d.encode()[..31].to_vec(),
        [d.encode(), vec![0]].concat(),
    ] {
        assert_ix_err(
            try_series(&mut env, &base, &quote, |ix| {
                ix.data = [vec![tag::CREATE_SERIES], args].concat();
            }),
            qb(QuotebookError::InvalidInstructionData),
        );
    }
}
