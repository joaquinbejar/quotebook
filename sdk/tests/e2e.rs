//! End-to-end: every quotebook instruction in this test is built by the SDK, run against the
//! program, SPL Token and the ed25519 precompile in LiteSVM, and read back with the SDK's
//! decoders. Only the setup the SDK does not cover is done by hand: deploying the program,
//! setting its upgrade authority, the clock, and raw mint and token accounts.

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

use std::path::PathBuf;

use litesvm::LiteSVM;
use quotebook_sdk::core::instruction::CreateSeriesArgs;
use quotebook_sdk::core::{
    BaseLots, Nonce, Quote, QuoteLotsPerBaseLot, QuoteTerms, QuotebookError, SeriesId, Side,
    UnixTimestamp,
};
use quotebook_sdk::{decode, deployment_id_from_genesis_hash, ix, pda, SettleAccounts};
use solana_account::Account;
use solana_address::Address;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

const PROGRAM_ID: Address = Address::new_from_array([0x51; 32]);
const NOW: i64 = 1_700_000_000;

fn token_program() -> Address {
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        .parse()
        .unwrap()
}

struct Vm {
    svm: LiteSVM,
    payer: Keypair,
}

impl Vm {
    fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<u64, String> {
        self.svm.expire_blockhash();
        let mut all = vec![&self.payer];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(
            ixs,
            Some(&self.payer.pubkey()),
            &all,
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|m| m.compute_units_consumed)
            .map_err(|f| format!("{:?}", f.err))
    }

    fn set(&mut self, address: Address, owner: Address, data: Vec<u8>) {
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm
            .set_account(
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

    fn account(&self, address: &Address) -> Account {
        self.svm.get_account(address).unwrap()
    }

    fn amount(&self, token_account: &Address) -> u64 {
        u64::from_le_bytes(self.account(token_account).data[64..72].try_into().unwrap())
    }
}

fn mint(vm: &mut Vm) -> Address {
    let address = Keypair::new().pubkey();
    let mut d = vec![0u8; 82];
    d[44] = 6;
    d[45] = 1;
    vm.set(address, token_program(), d);
    address
}

fn token_account(vm: &mut Vm, mint: &Address, owner: &Address, amount: u64) -> Address {
    let address = Keypair::new().pubkey();
    let mut d = vec![0u8; 165];
    d[0..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    d[108] = 1;
    vm.set(address, token_program(), d);
    address
}

#[test]
fn test_sdk_end_to_end() {
    // Setup the SDK does not cover.
    let mut svm = LiteSVM::new();
    let so =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/deploy/quotebook_program.so");
    svm.add_program_from_file(PROGRAM_ID, &so)
        .unwrap_or_else(|e| panic!("load {}: {e}; run `make program` first", so.display()));
    let payer = Keypair::new();
    let admin = Keypair::new();
    let maker = Keypair::new();
    let taker = Keypair::new();
    for k in [&payer, &admin, &maker, &taker] {
        svm.airdrop(&k.pubkey(), 10_000_000_000).unwrap();
    }
    let mut clock: solana_clock::Clock = svm.get_sysvar();
    clock.unix_timestamp = NOW;
    svm.set_sysvar(&clock);
    let mut vm = Vm { svm, payer };
    let program_data = pda::program_data(&PROGRAM_ID).0;
    let mut pd = vm.account(&program_data);
    pd.data[12] = 1;
    pd.data[13..45].copy_from_slice(admin.pubkey().as_ref());
    vm.svm.set_account(program_data, pd).unwrap();
    let base_mint = mint(&mut vm);
    let quote_mint = mint(&mut vm);
    let maker_base = token_account(&mut vm, &base_mint, &maker.pubkey(), 1_000_000);
    let maker_quote = token_account(&mut vm, &quote_mint, &maker.pubkey(), 1_000_000);
    let taker_base = token_account(&mut vm, &base_mint, &taker.pubkey(), 1_000_000);
    let taker_quote = token_account(&mut vm, &quote_mint, &taker.pubkey(), 1_000_000);

    // Everything below goes through the SDK.
    let payer = vm.payer.pubkey();
    let deployment = deployment_id_from_genesis_hash(&[7; 32]);
    vm.send(
        &[ix::init_config(
            &PROGRAM_ID,
            &admin.pubkey(),
            &payer,
            deployment,
        )],
        &[&admin],
    )
    .unwrap();
    let (config_address, _) = pda::config(&PROGRAM_ID);
    let config = vm.account(&config_address);
    let config = decode::config(&PROGRAM_ID, &config.owner, &config.data).unwrap();
    assert_eq!(config.deployment_id, deployment);

    let args = CreateSeriesArgs {
        base_lot_size: 100,
        quote_lot_size: 1,
        max_quote_ttl: 60,
        min_size: 1,
    };
    vm.send(
        &[ix::create_series(
            &PROGRAM_ID,
            &admin.pubkey(),
            &payer,
            &base_mint,
            &quote_mint,
            config.series_count,
            &args,
        )],
        &[&admin],
    )
    .unwrap();
    let (series, _) = pda::series(&PROGRAM_ID, &base_mint, &quote_mint, 0);
    let s = vm.account(&series);
    let s = decode::series(&PROGRAM_ID, &s.owner, &s.data).unwrap();
    assert_eq!((s.base_mint, s.quote_mint), (base_mint, quote_mint));

    // The maker: state, then the delegate allowance on the quote account it pays from.
    vm.send(
        &[ix::init_maker(&PROGRAM_ID, &maker.pubkey(), &payer)],
        &[&maker],
    )
    .unwrap();
    vm.send(
        &[ix::approve_delegate(
            &PROGRAM_ID,
            &token_program(),
            &maker_quote,
            &maker.pubkey(),
            50_000,
        )],
        &[&maker],
    )
    .unwrap();

    // The maker signs a firm bid for the taker; the taker settles 3 of 5 lots.
    let quote = Quote::new(QuoteTerms {
        program_id: PROGRAM_ID,
        deployment,
        maker: maker.pubkey(),
        series: SeriesId::new(series),
        taker: taker.pubkey(),
        side: Side::Bid,
        price: QuoteLotsPerBaseLot::new(250),
        size: BaseLots::new(5),
        issued_at: UnixTimestamp::new(NOW),
        expires_at: UnixTimestamp::new(NOW + 30),
        nonce: Nonce::new(1),
    })
    .unwrap();
    let signature: [u8; 64] = maker
        .sign_message(&quotebook_sdk::quote_message(&quote))
        .as_ref()
        .try_into()
        .unwrap();
    let accounts = SettleAccounts {
        taker: taker.pubkey(),
        base_mint,
        quote_mint,
        base_token_program: token_program(),
        quote_token_program: token_program(),
        maker_base,
        maker_quote,
        taker_base,
        taker_quote,
    };
    let pair = quotebook_sdk::settle_instructions(
        &PROGRAM_ID,
        &quote,
        &signature,
        &accounts,
        BaseLots::new(3),
        0,
    );
    vm.send(&pair, &[&taker]).unwrap();
    // base = 3 × 100; quote = 250 × 3 × 1.
    assert_eq!(vm.amount(&maker_base), 1_000_300);
    assert_eq!(vm.amount(&taker_base), 999_700);
    assert_eq!(vm.amount(&maker_quote), 999_250);
    assert_eq!(vm.amount(&taker_quote), 1_000_750);
    let (state_address, _) = pda::maker_state(&PROGRAM_ID, &maker.pubkey());
    let state = vm.account(&state_address);
    let state = decode::maker_state(&PROGRAM_ID, &state.owner, &state.data).unwrap();
    assert_eq!(state.fills, 1);
    assert_eq!(
        state.nonces.check(Nonce::new(1)),
        Err(QuotebookError::NonceUsed)
    );
    // The allowance was drawn down by the maker's leg.
    let mq = vm.account(&maker_quote);
    assert_eq!(
        u64::from_le_bytes(mq.data[121..129].try_into().unwrap()),
        49_250
    );

    // The same signed pair cannot be replayed.
    let err = vm.send(&pair, &[&taker]).unwrap_err();
    assert!(
        err.contains(&format!("Custom({})", QuotebookError::NonceUsed.code())),
        "{err}"
    );

    // The maker cancels nonce 2 before anyone fills it; a quote with it then fails.
    vm.send(
        &[ix::cancel_nonce(
            &PROGRAM_ID,
            &maker.pubkey(),
            Nonce::new(2),
        )],
        &[&maker],
    )
    .unwrap();
    vm.send(
        &[ix::cancel_all(&PROGRAM_ID, &maker.pubkey(), Nonce::new(10))],
        &[&maker],
    )
    .unwrap();
    let state = vm.account(&state_address);
    let state = decode::maker_state(&PROGRAM_ID, &state.owner, &state.data).unwrap();
    assert_eq!(state.nonces.window_base(), Nonce::new(10));

    // Admin: pause the series, hand over the admin role.
    vm.send(
        &[ix::set_series_paused(
            &PROGRAM_ID,
            &admin.pubkey(),
            &series,
            true,
        )],
        &[&admin],
    )
    .unwrap();
    let s = vm.account(&series);
    assert_eq!(
        decode::series(&PROGRAM_ID, &s.owner, &s.data)
            .unwrap()
            .paused,
        1
    );
    let new_admin = Keypair::new();
    vm.send(
        &[ix::set_config(
            &PROGRAM_ID,
            &admin.pubkey(),
            &new_admin.pubkey(),
        )],
        &[&admin],
    )
    .unwrap();
    let c = vm.account(&config_address);
    assert_eq!(
        decode::config(&PROGRAM_ID, &c.owner, &c.data)
            .unwrap()
            .admin,
        new_admin.pubkey()
    );

    // Decoders reject foreign owners and the wrong account type.
    assert_eq!(
        decode::config(&PROGRAM_ID, &token_program(), &c.data).unwrap_err(),
        quotebook_sdk::DecodeError::WrongOwner
    );
    assert!(decode::series(&PROGRAM_ID, &PROGRAM_ID, &c.data).is_err());
}
