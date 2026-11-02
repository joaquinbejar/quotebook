//! I8 done-when: a `[precompile, settle]` transaction built by the TypeScript SDK alone (its
//! encoders, PDAs, account metas and its own Ed25519 signature) settles against the real
//! program in LiteSVM. The TypeScript side (`ts/scripts/emit-settle.ts`) writes the
//! instructions to `ts/vectors/ts-settle.txt`; `make ts-check` regenerates that file and fails
//! on any difference, so this test always replays what the current TypeScript SDK builds.
//!
//! The world is the vectors' fixed scenario (`sdk/examples/vectors.rs`): the same key seeds,
//! mints and token accounts.

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

use std::path::PathBuf;

use litesvm::LiteSVM;
use quotebook_sdk::core::instruction::CreateSeriesArgs;
use quotebook_sdk::{decode, ix, pda};
use solana_account::Account;
use solana_address::Address;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

const PROGRAM_ID: Address = Address::new_from_array([0x51; 32]);
const NOW: i64 = 1_700_000_000;
const FUNDS: u64 = 1_000_000;

fn key(seed: u8) -> Keypair {
    Keypair::new_from_array([seed; 32])
}

fn token_program() -> Address {
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        .parse()
        .unwrap()
}

fn set(svm: &mut LiteSVM, address: Address, owner: Address, data: Vec<u8>) {
    let lamports = svm.minimum_balance_for_rent_exemption(data.len());
    svm.set_account(
        address,
        Account {
            lamports,
            data,
            owner,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

fn token_account(mint: &Address, owner: &Address, amount: u64) -> Vec<u8> {
    let mut d = vec![0u8; 165];
    d[0..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = 1;
    d
}

fn amount(svm: &LiteSVM, a: &Address) -> u64 {
    u64::from_le_bytes(svm.get_account(a).unwrap().data[64..72].try_into().unwrap())
}

/// The instructions `ts/scripts/emit-settle.ts` wrote.
fn ts_instructions() -> Vec<Instruction> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ts/vectors/ts-settle.txt");
    let text = std::fs::read_to_string(&path).unwrap();
    let mut out: Vec<Instruction> = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        match f[0] {
            "ix" => {
                let data = (0..f[2].len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&f[2][i..i + 2], 16).unwrap())
                    .collect();
                out.push(Instruction {
                    program_id: f[1].parse().unwrap(),
                    accounts: vec![],
                    data,
                });
            }
            "acct" => out.last_mut().unwrap().accounts.push(AccountMeta {
                pubkey: f[1].parse().unwrap(),
                is_signer: f[2] == "1",
                is_writable: f[3] == "1",
            }),
            other => panic!("unexpected line kind {other:?}"),
        }
    }
    out
}

#[test]
fn test_typescript_built_settle_executes() {
    let mut svm = LiteSVM::new();
    let so =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/deploy/quotebook_program.so");
    svm.add_program_from_file(PROGRAM_ID, &so)
        .unwrap_or_else(|e| panic!("load {}: {e}; run `make program` first", so.display()));
    let (payer, admin, maker, taker) = (Keypair::new(), Keypair::new(), key(11), key(12));
    for k in [&payer, &admin, &maker, &taker] {
        svm.airdrop(&k.pubkey(), 10_000_000_000).unwrap();
    }
    let mut clock: solana_clock::Clock = svm.get_sysvar();
    clock.unix_timestamp = NOW;
    svm.set_sysvar(&clock);
    let program_data = pda::program_data(&PROGRAM_ID).0;
    let mut pd = svm.get_account(&program_data).unwrap();
    pd.data[12] = 1;
    pd.data[13..45].copy_from_slice(admin.pubkey().as_ref());
    svm.set_account(program_data, pd).unwrap();
    let (base_mint, quote_mint) = (key(21).pubkey(), key(22).pubkey());
    for m in [base_mint, quote_mint] {
        let mut d = vec![0u8; 82];
        d[44] = 6;
        d[45] = 1;
        set(&mut svm, m, token_program(), d);
    }
    let (maker_base, maker_quote) = (key(31).pubkey(), key(32).pubkey());
    let (taker_base, taker_quote) = (key(33).pubkey(), key(34).pubkey());
    for (a, mint, owner) in [
        (maker_base, base_mint, maker.pubkey()),
        (maker_quote, quote_mint, maker.pubkey()),
        (taker_base, base_mint, taker.pubkey()),
        (taker_quote, quote_mint, taker.pubkey()),
    ] {
        set(
            &mut svm,
            a,
            token_program(),
            token_account(&mint, &owner, FUNDS),
        );
    }
    let send = |svm: &mut LiteSVM, ixs: &[Instruction], signers: &[&Keypair]| {
        svm.expire_blockhash();
        let mut all = vec![&payer];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(
            ixs,
            Some(&payer.pubkey()),
            &all,
            svm.latest_blockhash(),
        );
        svm.send_transaction(tx)
    };
    // The scenario's market, through the Rust SDK.
    send(
        &mut svm,
        &[ix::init_config(
            &PROGRAM_ID,
            &admin.pubkey(),
            &payer.pubkey(),
            [7; 16],
        )],
        &[&admin],
    )
    .unwrap();
    let args = CreateSeriesArgs {
        base_lot_size: 100,
        quote_lot_size: 1,
        max_quote_ttl: 60,
        min_size: 1,
    };
    send(
        &mut svm,
        &[ix::create_series(
            &PROGRAM_ID,
            &admin.pubkey(),
            &payer.pubkey(),
            &base_mint,
            &quote_mint,
            0,
            &args,
        )],
        &[&admin],
    )
    .unwrap();
    send(
        &mut svm,
        &[ix::init_maker(
            &PROGRAM_ID,
            &maker.pubkey(),
            &payer.pubkey(),
        )],
        &[&maker],
    )
    .unwrap();
    send(
        &mut svm,
        &[ix::approve_delegate(
            &PROGRAM_ID,
            &token_program(),
            &maker_quote,
            &maker.pubkey(),
            FUNDS,
        )],
        &[&maker],
    )
    .unwrap();

    // The transaction the TypeScript SDK built, signed only by the taker (and the payer).
    let ixs = ts_instructions();
    assert_eq!(ixs.len(), 2);
    let meta = send(&mut svm, &ixs, &[&taker])
        .unwrap_or_else(|f| panic!("{:?}\n{}", f.err, f.meta.pretty_logs()));
    // A Bid of 3 lots at 250 ticks: base 3 × 100, quote 250 × 3 × 1.
    assert_eq!(amount(&svm, &maker_base), FUNDS + 300);
    assert_eq!(amount(&svm, &taker_base), FUNDS - 300);
    assert_eq!(amount(&svm, &maker_quote), FUNDS - 750);
    assert_eq!(amount(&svm, &taker_quote), FUNDS + 750);
    let state = svm
        .get_account(&pda::maker_state(&PROGRAM_ID, &maker.pubkey()).0)
        .unwrap();
    let state = decode::maker_state(&PROGRAM_ID, &state.owner, &state.data).unwrap();
    assert_eq!(state.fills, 1);
    assert!(meta.logs.iter().any(|l| l.starts_with("Program data: ")));
}
