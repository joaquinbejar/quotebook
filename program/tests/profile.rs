//! I10 / ADR 0002: the maker registry, `set_profile` and `close_profile`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::result_large_err,
    missing_docs
)]

mod common;

use common::*;
use litesvm::types::{FailedTransactionMetadata, TransactionMetadata};
use quotebook_core::accounts::{new_hdr, MakerProfile};
use quotebook_core::profile::ProfileArgs;
use quotebook_core::QuotebookError;
use solana_address::Address;
use solana_instruction::error::InstructionError;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_signer::Signer;

fn profile_address(maker: &Address) -> Address {
    Address::find_program_address(&[b"profile", maker.as_ref()], &PROGRAM_ID).0
}

fn set_ix(
    maker: &Address,
    payer: &Address,
    profile: &Address,
    endpoint: &[u8],
    name: &[u8],
) -> Instruction {
    let mut data = vec![9u8, u8::try_from(endpoint.len()).unwrap()];
    data.extend_from_slice(endpoint);
    data.push(u8::try_from(name.len()).unwrap());
    data.extend_from_slice(name);
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new_readonly(*maker, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new(*profile, false),
            AccountMeta::new_readonly(SYSTEM, false),
        ],
    )
}

fn close_ix(maker: &Address, profile: &Address, destination: &Address) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &[10u8],
        vec![
            AccountMeta::new_readonly(*maker, true),
            AccountMeta::new(*profile, false),
            AccountMeta::new(*destination, false),
        ],
    )
}

fn funded(env: &mut Env) -> Keypair {
    let k = Keypair::new();
    env.svm.airdrop(&k.pubkey(), 1_000_000_000).unwrap();
    k
}

fn set(
    env: &mut Env,
    maker: &Keypair,
    profile: &Address,
    endpoint: &[u8],
    name: &[u8],
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    let ix = set_ix(
        &maker.pubkey(),
        &env.payer.pubkey(),
        profile,
        endpoint,
        name,
    );
    env.send(&[ix], &[maker])
}

fn read(env: &Env, profile: &Address) -> MakerProfile {
    bytemuck::pod_read_unaligned(&env.data(profile))
}

#[test]
fn test_create_update_close_and_recreate() {
    let mut env = Env::new();
    let maker = funded(&mut env);
    let p = profile_address(&maker.pubkey());
    set(&mut env, &maker, &p, b"wss://a.example/rfq", b"Desk A").unwrap();
    let got = read(&env, &p);
    assert_eq!(got.maker, maker.pubkey());
    assert_eq!(
        &got.endpoint[..usize::from(got.endpoint_len)],
        b"wss://a.example/rfq"
    );
    assert_eq!(&got.name[..usize::from(got.name_len)], b"Desk A");
    // A shorter update zeroes the old tails.
    set(&mut env, &maker, &p, b"wss://b.io", b"").unwrap();
    let got = read(&env, &p);
    assert_eq!(&got.endpoint[..10], b"wss://b.io");
    assert!(got.endpoint[10..].iter().all(|b| *b == 0) && got.name.iter().all(|b| *b == 0));
    quotebook_core::profile::check_canonical(&got).unwrap();
    // Close returns the rent; the account disappears.
    let dest = Keypair::new().pubkey();
    let rent = env.svm.get_account(&p).unwrap().lamports;
    env.send(&[close_ix(&maker.pubkey(), &p, &dest)], &[&maker])
        .unwrap();
    assert!(env.svm.get_account(&p).is_none_or(|a| a.lamports == 0));
    assert_eq!(env.svm.get_account(&dest).unwrap().lamports, rent);
    // Re-publishing after a close works.
    set(&mut env, &maker, &p, b"wss://c.io", b"again").unwrap();
    assert_eq!(read(&env, &p).name_len, 5);
}

#[test]
fn test_create_over_a_prefunded_pda() {
    let mut env = Env::new();
    let maker = funded(&mut env);
    let p = profile_address(&maker.pubkey());
    let grief = env.svm.minimum_balance_for_rent_exemption(0);
    env.svm.airdrop(&p, grief).unwrap();
    set(&mut env, &maker, &p, b"wss://a.io", b"").unwrap();
    assert_eq!(read(&env, &p).maker, maker.pubkey());
}

#[test]
fn test_only_the_maker_writes_or_closes_its_profile() {
    let mut env = Env::new();
    let (a, b) = (funded(&mut env), funded(&mut env));
    let pa = profile_address(&a.pubkey());
    // B cannot create A's profile, nor write it once it exists.
    assert_ix_err(
        set(&mut env, &b, &pa, b"wss://evil.io", b""),
        qb(QuotebookError::InvalidPda),
    );
    set(&mut env, &a, &pa, b"wss://a.io", b"").unwrap();
    assert_ix_err(
        set(&mut env, &b, &pa, b"wss://evil.io", b""),
        qb(QuotebookError::InvalidPda),
    );
    // Unauthorized close: B signs for A's profile.
    let dest = Keypair::new().pubkey();
    assert_ix_err(
        env.send(&[close_ix(&b.pubkey(), &pa, &dest)], &[&b]),
        qb(QuotebookError::InvalidPda),
    );
    assert_eq!(read(&env, &pa).maker, a.pubkey());
}

#[test]
fn test_substituted_profiles_are_rejected_on_update_and_close() {
    let mut env = Env::new();
    let maker = funded(&mut env);
    let other = Keypair::new().pubkey();
    let p = profile_address(&maker.pubkey());
    let bump = Address::find_program_address(&[b"profile", maker.pubkey().as_ref()], &PROGRAM_ID).1;
    let forged = |tag: u8, version: u8, stored: Address| {
        let mut prof: MakerProfile = bytemuck::Zeroable::zeroed();
        prof.hdr = new_hdr::<MakerProfile>(bump);
        prof.hdr.tag = tag;
        prof.hdr.version = version;
        prof.maker = stored;
        let args = ProfileArgs::new(b"wss://x.io", b"").unwrap();
        args.write_to(&mut prof);
        bytemuck::bytes_of(&prof).to_vec()
    };
    let dest = Keypair::new().pubkey();
    let cases: [(Vec<u8>, InstructionError); 3] = [
        (
            forged(3, 1, maker.pubkey()),
            InstructionError::InvalidAccountData,
        ), // wrong tag
        (
            forged(4, 2, maker.pubkey()),
            InstructionError::InvalidAccountData,
        ), // wrong version
        (forged(4, 1, other), qb(QuotebookError::NotAuthority)), // wrong maker
    ];
    for (data, err) in cases {
        env.set(p, PROGRAM_ID, data);
        assert_ix_err(set(&mut env, &maker, &p, b"wss://y.io", b""), err.clone());
        assert_ix_err(
            env.send(&[close_ix(&maker.pubkey(), &p, &dest)], &[&maker]),
            err,
        );
    }
    // An account at the PDA that quotebook does not own: update tries to create and fails;
    // close rejects the owner.
    env.set(
        p,
        Address::new_from_array([9; 32]),
        forged(4, 1, maker.pubkey()),
    );
    assert!(set(&mut env, &maker, &p, b"wss://y.io", b"").is_err());
    assert_ix_err(
        env.send(&[close_ix(&maker.pubkey(), &p, &dest)], &[&maker]),
        qb(QuotebookError::InvalidAccountOwner),
    );
}

#[test]
fn test_invalid_text_is_rejected() {
    let mut env = Env::new();
    let maker = funded(&mut env);
    let p = profile_address(&maker.pubkey());
    let bad = qb(QuotebookError::InvalidProfile);
    assert_ix_err(set(&mut env, &maker, &p, b"ws://a.io", b""), bad.clone());
    assert_ix_err(set(&mut env, &maker, &p, b"wss://a b", b""), bad.clone());
    assert_ix_err(set(&mut env, &maker, &p, b"wss://a.io", &[b'n'; 33]), bad);
    // A length byte that disagrees with the data.
    let mut ix = set_ix(&maker.pubkey(), &env.payer.pubkey(), &p, b"wss://a.io", b"");
    ix.data.push(0);
    assert_ix_err(
        env.send(&[ix], &[&maker]),
        qb(QuotebookError::InvalidInstructionData),
    );
}

#[test]
fn test_settle_never_takes_the_profile() {
    let m = Market::new();
    let p = profile_address(&m.maker.pubkey());
    let ix = m.settle_ix(0, 1);
    assert_eq!(ix.accounts.len(), 13);
    assert!(ix.accounts.iter().all(|a| a.pubkey != p));
}

#[test]
fn test_set_profile_meta_and_account_matrix() {
    let mut env = Env::new();
    let maker = funded(&mut env);
    let p = profile_address(&maker.pubkey());
    let base = set_ix(&maker.pubkey(), &env.payer.pubkey(), &p, b"wss://a.io", b"");

    // Create path. Maker not a signer.
    let mut ix = base.clone();
    ix.accounts[0].is_signer = false;
    env.rejected(ix, &[], qb(QuotebookError::MissingSignature));
    // A payer that does not sign (another funded key, so the fee payer stays separate).
    let payer2 = funded(&mut env);
    let mut ix = set_ix(&maker.pubkey(), &payer2.pubkey(), &p, b"wss://a.io", b"");
    ix.accounts[1].is_signer = false;
    env.rejected(ix, &[&maker], qb(QuotebookError::MissingSignature));
    // A read-only payer.
    let mut ix = set_ix(&maker.pubkey(), &payer2.pubkey(), &p, b"wss://a.io", b"");
    ix.accounts[1].is_writable = false;
    env.rejected(ix, &[&maker, &payer2], InstructionError::Immutable);
    // A read-only profile.
    let mut ix = base.clone();
    ix.accounts[2].is_writable = false;
    env.rejected(ix, &[&maker], InstructionError::Immutable);
    // A forged system program.
    let mut ix = base.clone();
    ix.accounts[3].pubkey = Keypair::new().pubkey();
    env.rejected(ix, &[&maker], InstructionError::IncorrectProgramId);
    // The token program in its place.
    let mut ix = base.clone();
    ix.accounts[3].pubkey = token_program();
    env.rejected(ix, &[&maker], InstructionError::IncorrectProgramId);
    // A non-PDA address for the profile.
    let mut ix = base.clone();
    ix.accounts[2].pubkey = Keypair::new().pubkey();
    env.rejected(ix, &[&maker], qb(QuotebookError::InvalidPda));
    assert!(env.svm.get_account(&p).is_none());

    // Update path: a stored bump that does not derive the profile's address.
    set(&mut env, &maker, &p, b"wss://a.io", b"").unwrap();
    let good = env.data(&p);
    let mut bad_bump = good.clone();
    bad_bump[2] = bad_bump[2].wrapping_sub(1); // Hdr.bump
    env.set(p, PROGRAM_ID, bad_bump);
    let ix = set_ix(&maker.pubkey(), &env.payer.pubkey(), &p, b"wss://b.io", b"");
    env.rejected(ix, &[&maker], qb(QuotebookError::InvalidPda));
    env.set(p, PROGRAM_ID, good.clone());
    // Update path, read-only profile.
    let mut ix = set_ix(&maker.pubkey(), &env.payer.pubkey(), &p, b"wss://b.io", b"");
    ix.accounts[2].is_writable = false;
    env.rejected(ix, &[&maker], InstructionError::Immutable);
    assert_eq!(env.data(&p), good);
}

#[test]
fn test_close_profile_meta_and_account_matrix() {
    let mut env = Env::new();
    let maker = funded(&mut env);
    let p = profile_address(&maker.pubkey());
    set(&mut env, &maker, &p, b"wss://a.io", b"").unwrap();
    let dest = Keypair::new().pubkey();
    let base = close_ix(&maker.pubkey(), &p, &dest);

    let mut ix = base.clone();
    ix.accounts[0].is_signer = false;
    env.rejected(ix, &[], qb(QuotebookError::MissingSignature));
    let mut ix = base.clone();
    ix.accounts[1].is_writable = false;
    env.rejected(ix, &[&maker], InstructionError::Immutable);
    let mut ix = base.clone();
    ix.accounts[2].is_writable = false;
    env.rejected(ix, &[&maker], InstructionError::Immutable);
    // The profile as its own destination.
    env.rejected(
        close_ix(&maker.pubkey(), &p, &p),
        &[&maker],
        InstructionError::InvalidArgument,
    );
    // A stored bump that does not derive the address.
    let good = env.data(&p);
    let mut bad_bump = good.clone();
    bad_bump[2] = bad_bump[2].wrapping_sub(1); // Hdr.bump
    env.set(p, PROGRAM_ID, bad_bump);
    env.rejected(base.clone(), &[&maker], qb(QuotebookError::InvalidPda));
    env.set(p, PROGRAM_ID, good);
    // Too few accounts.
    let mut ix = base.clone();
    ix.accounts.pop();
    // `ProgramError::NotEnoughAccountKeys`, under the runtime's deprecated name.
    #[allow(deprecated)]
    let not_enough = InstructionError::NotEnoughAccountKeys;
    env.rejected(ix, &[&maker], not_enough);
    // The valid close still works afterwards.
    env.send(&[base], &[&maker]).unwrap();
    assert!(env.svm.get_account(&p).is_none_or(|a| a.lamports == 0));
}
