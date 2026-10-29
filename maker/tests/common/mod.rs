//! A LiteSVM chain with quotebook deployed and one series, shared by the demo and the maker
//! tests. Quotebook instructions go through the SDK; the program must be built first
//! (`make program`).

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
use quotebook_core::{BaseLots, NonceWindow, Seconds};
use quotebook_maker::client::SignedQuote;
use quotebook_maker::market::MarketInfo;
use quotebook_sdk::core::instruction::CreateSeriesArgs;
use quotebook_sdk::{decode, ix, pda, SettleAccounts};
use solana_account::Account;
use solana_address::Address;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

pub const PROGRAM_ID: Address = Address::new_from_array([0x51; 32]);
pub const NOW: i64 = 1_700_000_000;
pub const FUNDS: u64 = 1_000_000_000_000;

pub fn token_program() -> Address {
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        .parse()
        .unwrap()
}

pub struct Maker {
    pub key: Keypair,
    pub base: Address,
    pub quote: Address,
}

pub struct Taker {
    pub key: Keypair,
    pub base: Address,
    pub quote: Address,
}

pub struct Chain {
    pub svm: LiteSVM,
    pub payer: Keypair,
    pub admin: Keypair,
    pub base_mint: Address,
    pub quote_mint: Address,
    pub market: MarketInfo,
}

impl Chain {
    /// The program, a config, and one series of two 6-decimal SPL mints: 1 base lot = 0.01
    /// base token, 1 quote lot = 0.0001 quote token, quotes live at most 60 s. Clock at NOW.
    pub fn new() -> Self {
        let mut svm = LiteSVM::new();
        let so =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/deploy/quotebook_program.so");
        svm.add_program_from_file(PROGRAM_ID, &so)
            .unwrap_or_else(|e| panic!("load {}: {e}; run `make program` first", so.display()));
        let payer = Keypair::new();
        let admin = Keypair::new();
        svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
        svm.airdrop(&admin.pubkey(), 1_000_000_000).unwrap();
        let program_data = pda::program_data(&PROGRAM_ID).0;
        let mut pd = svm.get_account(&program_data).unwrap();
        pd.data[12] = 1;
        pd.data[13..45].copy_from_slice(admin.pubkey().as_ref());
        svm.set_account(program_data, pd).unwrap();
        let placeholder = MarketInfo {
            program_id: PROGRAM_ID,
            deployment: [0; 16],
            series: Address::new_from_array([0; 32]),
            base_decimals: 6,
            quote_decimals: 6,
            base_lot_size: 0,
            quote_lot_size: 0,
            max_quote_ttl: Seconds::new(0),
            min_size: BaseLots::new(0),
        };
        let mut chain = Self {
            svm,
            payer,
            admin,
            base_mint: Address::new_from_array([0; 32]),
            quote_mint: Address::new_from_array([0; 32]),
            market: placeholder,
        };
        chain.set_clock(NOW);
        let payer = chain.payer.pubkey();
        let admin = chain.admin.insecure_clone();
        let deployment = quotebook_sdk::deployment_id_from_genesis_hash(&[42; 32]);
        chain
            .send(
                &[ix::init_config(
                    &PROGRAM_ID,
                    &admin.pubkey(),
                    &payer,
                    deployment,
                )],
                &[&admin],
            )
            .unwrap();
        chain.base_mint = chain.mint();
        chain.quote_mint = chain.mint();
        let args = CreateSeriesArgs {
            base_lot_size: 10_000,
            quote_lot_size: 100,
            max_quote_ttl: 60,
            min_size: 1,
        };
        let (base, quote) = (chain.base_mint, chain.quote_mint);
        chain
            .send(
                &[ix::create_series(
                    &PROGRAM_ID,
                    &admin.pubkey(),
                    &payer,
                    &base,
                    &quote,
                    0,
                    &args,
                )],
                &[&admin],
            )
            .unwrap();
        let series = pda::series(&PROGRAM_ID, &base, &quote, 0).0;
        let s = chain.svm.get_account(&series).unwrap();
        let s = decode::series(&PROGRAM_ID, &s.owner, &s.data).unwrap();
        chain.market = MarketInfo {
            program_id: PROGRAM_ID,
            deployment: s.deployment_id,
            series,
            base_decimals: 6,
            quote_decimals: 6,
            base_lot_size: s.base_lot_size.get(),
            quote_lot_size: s.quote_lot_size.get(),
            max_quote_ttl: s.max_quote_ttl,
            min_size: s.min_size,
        };
        chain
    }

    pub fn set_clock(&mut self, unix_timestamp: i64) {
        let mut clock: solana_clock::Clock = self.svm.get_sysvar();
        clock.unix_timestamp = unix_timestamp;
        self.svm.set_sysvar(&clock);
    }

    pub fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), String> {
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
            .map(|_| ())
            .map_err(|f| format!("{:?}", f.err))
    }

    fn set_token_account(&mut self, address: Address, data: Vec<u8>) {
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        let account = Account {
            lamports,
            data,
            owner: token_program(),
            executable: false,
            rent_epoch: 0,
        };
        self.svm.set_account(address, account).unwrap();
    }

    fn mint(&mut self) -> Address {
        let address = Keypair::new().pubkey();
        let mut d = vec![0u8; 82];
        d[44] = 6;
        d[45] = 1;
        self.set_token_account(address, d);
        address
    }

    fn token_account(&mut self, mint: &Address, owner: &Address) -> Address {
        let address = Keypair::new().pubkey();
        let mut d = vec![0u8; 165];
        d[0..32].copy_from_slice(mint.as_ref());
        d[32..64].copy_from_slice(owner.as_ref());
        d[64..72].copy_from_slice(&FUNDS.to_le_bytes());
        d[108] = 1;
        self.set_token_account(address, d);
        address
    }

    pub fn amount(&self, account: &Address) -> u64 {
        let data = self.svm.get_account(account).unwrap().data;
        u64::from_le_bytes(data[64..72].try_into().unwrap())
    }

    /// A maker with its state and token accounts, its delegate approved on both.
    pub fn maker(&mut self) -> Maker {
        let key = Keypair::new();
        self.svm.airdrop(&key.pubkey(), 1_000_000_000).unwrap();
        let payer = self.payer.pubkey();
        self.send(
            &[ix::init_maker(&PROGRAM_ID, &key.pubkey(), &payer)],
            &[&key],
        )
        .unwrap();
        let (base_mint, quote_mint) = (self.base_mint, self.quote_mint);
        let base = self.token_account(&base_mint, &key.pubkey());
        let quote = self.token_account(&quote_mint, &key.pubkey());
        for account in [base, quote] {
            self.send(
                &[ix::approve_delegate(
                    &PROGRAM_ID,
                    &token_program(),
                    &account,
                    &key.pubkey(),
                    FUNDS,
                )],
                &[&key],
            )
            .unwrap();
        }
        Maker { key, base, quote }
    }

    pub fn taker(&mut self) -> Taker {
        let key = Keypair::new();
        self.svm.airdrop(&key.pubkey(), 1_000_000_000).unwrap();
        let (base_mint, quote_mint) = (self.base_mint, self.quote_mint);
        let base = self.token_account(&base_mint, &key.pubkey());
        let quote = self.token_account(&quote_mint, &key.pubkey());
        Taker { key, base, quote }
    }

    /// The maker's on-chain nonce window.
    pub fn window(&self, maker: &Address) -> NonceWindow {
        let state = pda::maker_state(&PROGRAM_ID, maker).0;
        let state = self.svm.get_account(&state).unwrap();
        decode::maker_state(&PROGRAM_ID, &state.owner, &state.data)
            .unwrap()
            .nonces
    }

    /// The precompile + settle pair for `signed`, built by the SDK.
    pub fn settle_pair(
        &self,
        signed: &SignedQuote,
        maker: &Maker,
        taker: &Taker,
        size: BaseLots,
    ) -> [Instruction; 2] {
        let accounts = SettleAccounts {
            taker: taker.key.pubkey(),
            base_mint: self.base_mint,
            quote_mint: self.quote_mint,
            base_token_program: token_program(),
            quote_token_program: token_program(),
            maker_base: maker.base,
            maker_quote: maker.quote,
            taker_base: taker.base,
            taker_quote: taker.quote,
        };
        quotebook_sdk::settle_instructions(
            &PROGRAM_ID,
            &signed.quote,
            &signed.signature,
            &accounts,
            size,
            0,
        )
    }

    /// Settles `signed` for `size` lots through the SDK.
    pub fn settle(
        &mut self,
        signed: &SignedQuote,
        maker: &Maker,
        taker: &Taker,
        size: BaseLots,
    ) -> Result<(), String> {
        let accounts = SettleAccounts {
            taker: taker.key.pubkey(),
            base_mint: self.base_mint,
            quote_mint: self.quote_mint,
            base_token_program: token_program(),
            quote_token_program: token_program(),
            maker_base: maker.base,
            maker_quote: maker.quote,
            taker_base: taker.base,
            taker_quote: taker.quote,
        };
        let pair = quotebook_sdk::settle_instructions(
            &PROGRAM_ID,
            &signed.quote,
            &signed.signature,
            &accounts,
            size,
            0,
        );
        let key = taker.key.insecure_clone();
        self.send(&pair, &[&key])
    }
}
