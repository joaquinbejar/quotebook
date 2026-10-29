//! `Rpc` over LiteSVM, so the CLI logic runs against the real program with no network.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    dead_code,
    missing_docs
)]

use std::path::PathBuf;

use litesvm::LiteSVM;
use quotebook_cli::rpc::Rpc;
use quotebook_cli::CliError;
use solana_account::Account;
use solana_address::Address;
use solana_hash::Hash;
use solana_keypair::Keypair;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::Transaction;

pub const PROGRAM_ID: Address = Address::new_from_array([0x51; 32]);
pub const NOW: i64 = 1_700_000_000;
/// LiteSVM has no genesis; a fixed one stands in.
pub const GENESIS: [u8; 32] = [9; 32];

pub fn token_program() -> Address {
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        .parse()
        .unwrap()
}

pub struct LiteRpc {
    pub svm: LiteSVM,
}

impl Rpc for LiteRpc {
    fn account(&mut self, address: &Address) -> Result<Option<Account>, CliError> {
        if *address == solana_sdk_ids::sysvar::clock::ID {
            // The Clock sysvar as the cluster serves it (bincode, unix_timestamp at 32).
            let c: solana_clock::Clock = self.svm.get_sysvar();
            let mut data = Vec::with_capacity(40);
            data.extend_from_slice(&c.slot.to_le_bytes());
            data.extend_from_slice(&c.epoch_start_timestamp.to_le_bytes());
            data.extend_from_slice(&c.epoch.to_le_bytes());
            data.extend_from_slice(&c.leader_schedule_epoch.to_le_bytes());
            data.extend_from_slice(&c.unix_timestamp.to_le_bytes());
            return Ok(Some(Account {
                lamports: 1,
                data,
                owner: solana_sdk_ids::sysvar::ID,
                executable: false,
                rent_epoch: 0,
            }));
        }
        Ok(self.svm.get_account(address))
    }

    fn latest_blockhash(&mut self) -> Result<Hash, CliError> {
        self.svm.expire_blockhash();
        Ok(self.svm.latest_blockhash())
    }

    fn send(&mut self, transaction: &Transaction) -> Result<Signature, CliError> {
        self.svm
            .send_transaction(transaction.clone())
            .map(|m| m.signature)
            .map_err(|f| CliError::Rpc(format!("{:?}", f.err)))
    }

    fn genesis_hash(&mut self) -> Result<Hash, CliError> {
        Ok(Hash::new_from_array(GENESIS))
    }
}

impl LiteRpc {
    /// The program deployed through the upgradeable loader with `authority` as its upgrade
    /// authority, the clock at NOW, and `payer` funded.
    pub fn new(authority: &Address, payer: &Address) -> Self {
        let mut svm = LiteSVM::new();
        let so =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/deploy/quotebook_program.so");
        svm.add_program_from_file(PROGRAM_ID, &so)
            .unwrap_or_else(|e| panic!("load {}: {e}; run `make program` first", so.display()));
        let pd = quotebook_sdk::pda::program_data(&PROGRAM_ID).0;
        let mut account = svm.get_account(&pd).unwrap();
        account.data[12] = 1;
        account.data[13..45].copy_from_slice(authority.as_ref());
        svm.set_account(pd, account).unwrap();
        svm.airdrop(payer, 100_000_000_000).unwrap();
        let mut rpc = Self { svm };
        rpc.set_clock(NOW);
        rpc
    }

    pub fn set_clock(&mut self, t: i64) {
        let mut c: solana_clock::Clock = self.svm.get_sysvar();
        c.unix_timestamp = t;
        self.svm.set_sysvar(&c);
    }

    fn set_token(&mut self, address: Address, data: Vec<u8>) {
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm
            .set_account(
                address,
                Account {
                    lamports,
                    data,
                    owner: token_program(),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
    }

    pub fn mint(&mut self) -> Address {
        let a = Keypair::new().pubkey();
        let mut d = vec![0u8; 82];
        d[44] = 6;
        d[45] = 1;
        self.set_token(a, d);
        a
    }

    pub fn token_account(&mut self, mint: &Address, owner: &Address, amount: u64) -> Address {
        let a = Keypair::new().pubkey();
        let mut d = vec![0u8; 165];
        d[0..32].copy_from_slice(mint.as_ref());
        d[32..64].copy_from_slice(owner.as_ref());
        d[64..72].copy_from_slice(&amount.to_le_bytes());
        d[108] = 1;
        self.set_token(a, d);
        a
    }

    pub fn amount(&self, account: &Address) -> u64 {
        u64::from_le_bytes(
            self.svm.get_account(account).unwrap().data[64..72]
                .try_into()
                .unwrap(),
        )
    }
}
