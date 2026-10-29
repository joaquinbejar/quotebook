//! LiteSVM harness shared by the program tests.
//!
//! Instruction encoders are restated here from docs/PLAN.md rather than imported from the
//! program, so the tests check the wire format independently. Account layouts are read back
//! through the program's `state` types, which define them.

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
    dead_code,
    missing_docs
)]

use std::path::PathBuf;

use litesvm::types::{FailedTransactionMetadata, TransactionMetadata};
use litesvm::LiteSVM;
use quotebook_program::state::{Config, MakerState, Series};
use solana_account::Account;
use solana_address::Address;
use solana_instruction::error::InstructionError;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

pub const PROGRAM_ID: Address = Address::new_from_array([0x51; 32]);
pub const SYSTEM: Address = Address::new_from_array([0; 32]);
pub const LOADER: Address = solana_sdk_ids::bpf_loader_upgradeable::ID;
pub const DEPLOYMENT: [u8; 16] = *b"test-deployment!";

/// SPL Token and Token-2022 program ids, decoded from base58 here so the test does not reuse
/// the program's constants.
pub fn token_program() -> Address {
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        .parse()
        .unwrap()
}

pub fn token_2022_program() -> Address {
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
        .parse()
        .unwrap()
}

/// Instruction tags (docs/PLAN.md, 3.2).
pub mod tag {
    pub const INIT_CONFIG: u8 = 0;
    pub const CREATE_SERIES: u8 = 1;
    pub const INIT_MAKER: u8 = 3;
    pub const CANCEL_ALL: u8 = 4;
    pub const CANCEL_NONCE: u8 = 5;
}

fn program_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/deploy/quotebook_program.so")
}

/// A VM with quotebook deployed through the upgradeable loader, `admin` as its upgrade
/// authority, and a funded `payer`.
pub struct Env {
    pub svm: LiteSVM,
    pub admin: Keypair,
    pub payer: Keypair,
}

impl Env {
    pub fn new() -> Self {
        let mut svm = LiteSVM::new();
        let path = program_path();
        svm.add_program_from_file(PROGRAM_ID, &path)
            .unwrap_or_else(|e| {
                panic!(
                    "load {}: {e}; run `make program` (cargo build-sbf) first",
                    path.display()
                )
            });
        let admin = Keypair::new();
        let payer = Keypair::new();
        svm.airdrop(&admin.pubkey(), 10_000_000_000).unwrap();
        svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
        let mut env = Self { svm, admin, payer };
        let authority = env.admin.pubkey();
        env.set_upgrade_authority(Some(authority));
        env
    }

    /// Rewrites the `ProgramData` metadata's upgrade authority.
    pub fn set_upgrade_authority(&mut self, authority: Option<Address>) {
        let address = program_data_address();
        let mut account = self.svm.get_account(&address).unwrap();
        assert_eq!(
            u32::from_le_bytes(account.data[0..4].try_into().unwrap()),
            3
        );
        match authority {
            Some(a) => {
                account.data[12] = 1;
                account.data[13..45].copy_from_slice(a.as_ref());
            }
            None => {
                account.data[12] = 0;
                account.data[13..45].fill(0);
            }
        }
        self.svm.set_account(address, account).unwrap();
    }

    /// Sends `ixs` signed by the payer (fee payer) and `signers`.
    pub fn send(
        &mut self,
        ixs: &[Instruction],
        signers: &[&Keypair],
    ) -> Result<TransactionMetadata, FailedTransactionMetadata> {
        self.svm.expire_blockhash();
        let mut all: Vec<&Keypair> = vec![&self.payer];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(
            ixs,
            Some(&self.payer.pubkey()),
            &all,
            self.svm.latest_blockhash(),
        );
        self.svm.send_transaction(tx)
    }

    pub fn set(&mut self, address: Address, owner: Address, data: Vec<u8>) {
        let lamports = self
            .svm
            .minimum_balance_for_rent_exemption(data.len())
            .max(1);
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

    pub fn data(&self, address: &Address) -> Vec<u8> {
        self.svm.get_account(address).unwrap().data
    }

    pub fn config(&self) -> Config {
        bytemuck::pod_read_unaligned(&self.data(&config_address()))
    }

    pub fn series(&self, address: &Address) -> Series {
        bytemuck::pod_read_unaligned(&self.data(address))
    }

    pub fn maker_state(&self, maker: &Address) -> MakerState {
        bytemuck::pod_read_unaligned(&self.data(&maker_state_address(maker)))
    }

    /// `init_config` with the default deployment id; panics on failure.
    pub fn init_config(&mut self) -> TransactionMetadata {
        let ix = init_config_ix(&self.admin.pubkey(), &self.payer.pubkey(), DEPLOYMENT);
        let admin = self.admin.insecure_clone();
        self.send(&[ix], &[&admin]).unwrap()
    }

    /// Creates an initialized SPL Token mint at a fresh address.
    pub fn spl_mint(&mut self) -> Address {
        let address = Keypair::new().pubkey();
        self.set(address, token_program(), mint_data(&[]));
        address
    }

    /// Creates a Token-2022 mint with the given TLV extension bytes (empty: base mint only).
    pub fn mint_2022(&mut self, tlv: &[u8]) -> Address {
        let address = Keypair::new().pubkey();
        self.set(address, token_2022_program(), mint_data(tlv));
        address
    }

    /// `create_series` for two fresh SPL mints with default arguments; panics on failure.
    pub fn create_series(&mut self) -> (Address, TransactionMetadata) {
        let base = self.spl_mint();
        let quote = self.spl_mint();
        self.create_series_with(&base, &quote)
    }

    pub fn create_series_with(
        &mut self,
        base: &Address,
        quote: &Address,
    ) -> (Address, TransactionMetadata) {
        let index = self.config().series_count;
        let series = series_address(base, quote, index);
        let ix = create_series_ix(
            &self.admin.pubkey(),
            &self.payer.pubkey(),
            &series,
            base,
            quote,
            SeriesArgs::default().encode(),
        );
        let admin = self.admin.insecure_clone();
        (series, self.send(&[ix], &[&admin]).unwrap())
    }

    /// A funded maker with an initialized `MakerState`.
    pub fn maker(&mut self) -> Keypair {
        let maker = Keypair::new();
        self.svm.airdrop(&maker.pubkey(), 1_000_000_000).unwrap();
        let ix = init_maker_ix(&maker.pubkey(), &self.payer.pubkey());
        self.send(&[ix], &[&maker]).unwrap();
        maker
    }
}

pub fn program_data_address() -> Address {
    Address::find_program_address(&[PROGRAM_ID.as_ref()], &LOADER).0
}

pub fn config_address() -> Address {
    Address::find_program_address(&[b"config"], &PROGRAM_ID).0
}

pub fn series_address(base: &Address, quote: &Address, index: u64) -> Address {
    Address::find_program_address(
        &[
            b"series",
            base.as_ref(),
            quote.as_ref(),
            &index.to_le_bytes(),
        ],
        &PROGRAM_ID,
    )
    .0
}

pub fn maker_state_address(maker: &Address) -> Address {
    Address::find_program_address(&[b"maker", maker.as_ref()], &PROGRAM_ID).0
}

pub fn delegate_bump(maker: &Address) -> u8 {
    Address::find_program_address(&[b"delegate", maker.as_ref()], &PROGRAM_ID).1
}

pub fn init_config_ix(admin: &Address, payer: &Address, deployment: [u8; 16]) -> Instruction {
    let mut data = vec![tag::INIT_CONFIG];
    data.extend_from_slice(&deployment);
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new(config_address(), false),
            AccountMeta::new_readonly(program_data_address(), false),
            AccountMeta::new_readonly(SYSTEM, false),
        ],
    )
}

#[derive(Debug, Clone, Copy)]
pub struct SeriesArgs {
    pub base_lot_size: u64,
    pub quote_lot_size: u64,
    pub max_quote_ttl: i64,
    pub min_size: u64,
}

impl Default for SeriesArgs {
    fn default() -> Self {
        Self {
            base_lot_size: 1_000,
            quote_lot_size: 10,
            max_quote_ttl: 60,
            min_size: 1,
        }
    }
}

impl SeriesArgs {
    pub fn encode(self) -> Vec<u8> {
        let mut v = Vec::with_capacity(32);
        v.extend_from_slice(&self.base_lot_size.to_le_bytes());
        v.extend_from_slice(&self.quote_lot_size.to_le_bytes());
        v.extend_from_slice(&self.max_quote_ttl.to_le_bytes());
        v.extend_from_slice(&self.min_size.to_le_bytes());
        v
    }
}

pub fn create_series_ix(
    admin: &Address,
    payer: &Address,
    series: &Address,
    base: &Address,
    quote: &Address,
    args: Vec<u8>,
) -> Instruction {
    let mut data = vec![tag::CREATE_SERIES];
    data.extend_from_slice(&args);
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new(config_address(), false),
            AccountMeta::new(*series, false),
            AccountMeta::new_readonly(*base, false),
            AccountMeta::new_readonly(*quote, false),
            AccountMeta::new_readonly(SYSTEM, false),
        ],
    )
}

pub fn init_maker_ix(maker: &Address, payer: &Address) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &[tag::INIT_MAKER],
        vec![
            AccountMeta::new_readonly(*maker, true),
            AccountMeta::new(*payer, true),
            AccountMeta::new(maker_state_address(maker), false),
            AccountMeta::new_readonly(SYSTEM, false),
        ],
    )
}

pub fn cancel_ix(tag: u8, maker: &Address, state: &Address, value: u64) -> Instruction {
    let mut data = vec![tag];
    data.extend_from_slice(&value.to_le_bytes());
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new_readonly(*maker, true),
            AccountMeta::new(*state, false),
        ],
    )
}

/// Base SPL `Mint` (82 bytes, initialized, 6 decimals), extended Token-2022 style when `tlv`
/// is not empty: zero padding to 165, account type `Mint`, then the TLV bytes.
pub fn mint_data(tlv: &[u8]) -> Vec<u8> {
    let mut data = vec![0u8; 82];
    data[36..44].copy_from_slice(&1_000_000u64.to_le_bytes()); // supply
    data[44] = 6; // decimals
    data[45] = 1; // is_initialized
    if !tlv.is_empty() {
        data.resize(165, 0);
        data.push(1); // AccountType::Mint
        data.extend_from_slice(tlv);
    }
    data
}

/// One TLV entry: `type: u16`, `length: u16`, value.
pub fn tlv(kind: u16, value: &[u8]) -> Vec<u8> {
    let mut v = kind.to_le_bytes().to_vec();
    v.extend_from_slice(&(value.len() as u16).to_le_bytes());
    v.extend_from_slice(value);
    v
}

/// Asserts that the first instruction failed with `expected`.
#[track_caller]
pub fn assert_ix_err(
    result: Result<TransactionMetadata, FailedTransactionMetadata>,
    expected: InstructionError,
) {
    match result {
        Ok(meta) => panic!(
            "expected {expected:?}, got success:\n{}",
            meta.pretty_logs()
        ),
        Err(failed) => {
            let got = format!("{:?}", failed.err);
            let want = format!("InstructionError(0, {expected:?})");
            assert_eq!(got, want, "logs:\n{}", failed.meta.pretty_logs());
        }
    }
}

/// `InstructionError::Custom` with a quotebook error code.
pub fn qb(e: quotebook_core::QuotebookError) -> InstructionError {
    InstructionError::Custom(e.code())
}
