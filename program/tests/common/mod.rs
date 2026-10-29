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
    pub const SET_SERIES_PAUSED: u8 = 2;
    pub const INIT_MAKER: u8 = 3;
    pub const CANCEL_ALL: u8 = 4;
    pub const CANCEL_NONCE: u8 = 5;
    pub const SETTLE: u8 = 6;
    pub const SET_CONFIG: u8 = 7;
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

// ---------------------------------------------------------------------------------------
// settle: quotes, the ed25519 precompile, token accounts.

use quotebook_core::{
    BaseLots, Nonce, Quote, QuoteLotsPerBaseLot, QuoteTerms, SeriesId, Side, UnixTimestamp,
};

pub const ED25519: Address = solana_sdk_ids::ed25519_program::ID;
pub const INSTRUCTIONS: Address = solana_sdk_ids::sysvar::instructions::ID;
/// The on-chain clock every settle test starts from.
pub const NOW: i64 = 1_700_000_000;

impl Env {
    pub fn set_clock(&mut self, unix_timestamp: i64) {
        let mut clock: solana_clock::Clock = self.svm.get_sysvar();
        clock.unix_timestamp = unix_timestamp;
        self.svm.set_sysvar(&clock);
    }

    /// An initialized token account owned by `token_program`, with `extensions` TLV bytes
    /// for Token-2022 (empty: a plain 165-byte account).
    #[allow(clippy::too_many_arguments)]
    pub fn token_account(
        &mut self,
        token_program: Address,
        mint: &Address,
        owner: &Address,
        amount: u64,
        delegate: Option<(&Address, u64)>,
        extensions: &[u8],
    ) -> Address {
        let address = Keypair::new().pubkey();
        let data = token_account_data(mint, owner, amount, delegate, 1, extensions);
        self.set(address, token_program, data);
        address
    }

    pub fn token_amount(&self, account: &Address) -> u64 {
        u64::from_le_bytes(self.data(account)[64..72].try_into().unwrap())
    }
}

/// SPL `Account` layout (165 bytes), extended Token-2022 style when `extensions` is not empty.
pub fn token_account_data(
    mint: &Address,
    owner: &Address,
    amount: u64,
    delegate: Option<(&Address, u64)>,
    state: u8,
    extensions: &[u8],
) -> Vec<u8> {
    let mut d = vec![0u8; 165];
    d[0..32].copy_from_slice(mint.as_ref());
    d[32..64].copy_from_slice(owner.as_ref());
    d[64..72].copy_from_slice(&amount.to_le_bytes());
    if let Some((delegate, allowance)) = delegate {
        d[72..76].copy_from_slice(&1u32.to_le_bytes());
        d[76..108].copy_from_slice(delegate.as_ref());
        d[121..129].copy_from_slice(&allowance.to_le_bytes());
    }
    d[108] = state;
    if !extensions.is_empty() {
        d.push(2); // AccountType::Account
        d.extend_from_slice(extensions);
    }
    d
}

pub fn delegate_address(maker: &Address) -> Address {
    Address::find_program_address(&[b"delegate", maker.as_ref()], &PROGRAM_ID).0
}

/// A ready market: config, one series over two mints, a funded maker with token accounts
/// approved to its delegate PDA, and a funded taker.
pub struct Market {
    pub env: Env,
    pub series: Address,
    pub base_mint: Address,
    pub quote_mint: Address,
    pub base_program: Address,
    pub quote_program: Address,
    pub maker: Keypair,
    pub taker: Keypair,
    pub maker_base: Address,
    pub maker_quote: Address,
    pub taker_base: Address,
    pub taker_quote: Address,
}

/// Starting balances and allowances, in atoms.
pub const FUNDS: u64 = 1_000_000_000_000;

/// Which token program each mint uses, and the TLV extensions of each token account.
#[derive(Clone, Default)]
pub struct MarketSpec {
    pub base_2022: bool,
    pub quote_2022: bool,
    pub maker_base_ext: Vec<u8>,
    pub maker_quote_ext: Vec<u8>,
    pub taker_base_ext: Vec<u8>,
    pub taker_quote_ext: Vec<u8>,
}

impl Market {
    pub fn new() -> Self {
        Self::with(MarketSpec::default())
    }

    pub fn with(spec: MarketSpec) -> Self {
        let mut env = Env::new();
        env.set_clock(NOW);
        env.init_config();
        let program = |is_2022: bool| {
            if is_2022 {
                token_2022_program()
            } else {
                token_program()
            }
        };
        let (base_program, quote_program) = (program(spec.base_2022), program(spec.quote_2022));
        let base_mint = Keypair::new().pubkey();
        env.set(base_mint, base_program, mint_data(&[]));
        let quote_mint = Keypair::new().pubkey();
        env.set(quote_mint, quote_program, mint_data(&[]));
        let (series, _) = env.create_series_with(&base_mint, &quote_mint);
        let maker = env.maker();
        let taker = Keypair::new();
        env.svm.airdrop(&taker.pubkey(), 1_000_000_000).unwrap();
        let delegate = delegate_address(&maker.pubkey());
        let maker_base = env.token_account(
            base_program,
            &base_mint,
            &maker.pubkey(),
            FUNDS,
            Some((&delegate, FUNDS)),
            &spec.maker_base_ext,
        );
        let maker_quote = env.token_account(
            quote_program,
            &quote_mint,
            &maker.pubkey(),
            FUNDS,
            Some((&delegate, FUNDS)),
            &spec.maker_quote_ext,
        );
        let taker_base = env.token_account(
            base_program,
            &base_mint,
            &taker.pubkey(),
            FUNDS,
            None,
            &spec.taker_base_ext,
        );
        let taker_quote = env.token_account(
            quote_program,
            &quote_mint,
            &taker.pubkey(),
            FUNDS,
            None,
            &spec.taker_quote_ext,
        );
        Self {
            env,
            series,
            base_mint,
            quote_mint,
            base_program,
            quote_program,
            maker,
            taker,
            maker_base,
            maker_quote,
            taker_base,
            taker_quote,
        }
    }

    /// Default terms: open quote, Bid, price 3, size 10, live from NOW for 30 s, nonce 1.
    pub fn terms(&self) -> QuoteTerms {
        QuoteTerms {
            program_id: PROGRAM_ID,
            deployment: DEPLOYMENT,
            maker: self.maker.pubkey(),
            series: SeriesId::new(self.series),
            taker: Quote::OPEN_TAKER,
            side: Side::Bid,
            price: QuoteLotsPerBaseLot::new(3),
            size: BaseLots::new(10),
            issued_at: UnixTimestamp::new(NOW),
            expires_at: UnixTimestamp::new(NOW + 30),
            nonce: Nonce::new(1),
        }
    }

    /// The precompile instruction for `terms`, signed by `signer`.
    pub fn precompile(&self, terms: QuoteTerms, signer: &Keypair) -> Instruction {
        let quote = Quote::new(terms).unwrap();
        let sig = signer.sign_message(&quote.encode());
        let sig: [u8; 64] = sig.as_ref().try_into().unwrap();
        let mut data = quotebook_core::sigverify::instruction_data(&quote, &sig).to_vec();
        // `instruction_data` writes `quote.maker` as the key; use the actual signer's.
        data[16..48].copy_from_slice(signer.pubkey().as_ref());
        Instruction::new_with_bytes(ED25519, &data, vec![])
    }

    /// The settle instruction, every account in the documented order.
    pub fn settle_ix(&self, sig_ix_index: u8, fill_size: u64) -> Instruction {
        let maker = self.maker.pubkey();
        let mut data = vec![tag::SETTLE, sig_ix_index];
        data.extend_from_slice(&fill_size.to_le_bytes());
        Instruction::new_with_bytes(
            PROGRAM_ID,
            &data,
            vec![
                AccountMeta::new_readonly(self.taker.pubkey(), true),
                AccountMeta::new_readonly(self.series, false),
                AccountMeta::new(maker_state_address(&maker), false),
                AccountMeta::new_readonly(delegate_address(&maker), false),
                AccountMeta::new(self.maker_base, false),
                AccountMeta::new(self.maker_quote, false),
                AccountMeta::new(self.taker_base, false),
                AccountMeta::new(self.taker_quote, false),
                AccountMeta::new_readonly(self.base_mint, false),
                AccountMeta::new_readonly(self.quote_mint, false),
                AccountMeta::new_readonly(self.base_program, false),
                AccountMeta::new_readonly(self.quote_program, false),
                AccountMeta::new_readonly(INSTRUCTIONS, false),
            ],
        )
    }

    /// Precompile at index 0, settle at index 1, signed by the taker.
    pub fn settle(
        &mut self,
        terms: QuoteTerms,
        fill: u64,
    ) -> Result<TransactionMetadata, FailedTransactionMetadata> {
        let maker = self.maker.insecure_clone();
        let pre = self.precompile(terms, &maker);
        let ix = self.settle_ix(0, fill);
        self.send_as_taker(&[pre, ix])
    }

    pub fn send_as_taker(
        &mut self,
        ixs: &[Instruction],
    ) -> Result<TransactionMetadata, FailedTransactionMetadata> {
        let taker = self.taker.insecure_clone();
        self.env.send(ixs, &[&taker])
    }

    pub fn balances(&self) -> [u64; 4] {
        [
            self.env.token_amount(&self.maker_base),
            self.env.token_amount(&self.maker_quote),
            self.env.token_amount(&self.taker_base),
            self.env.token_amount(&self.taker_quote),
        ]
    }
}

/// Asserts that instruction `index` failed with `expected`.
#[track_caller]
pub fn assert_ix_err_at(
    result: Result<TransactionMetadata, FailedTransactionMetadata>,
    index: u8,
    expected: InstructionError,
) {
    match result {
        Ok(meta) => panic!(
            "expected {expected:?}, got success:\n{}",
            meta.pretty_logs()
        ),
        Err(failed) => {
            let got = format!("{:?}", failed.err);
            let want = format!("InstructionError({index}, {expected:?})");
            assert_eq!(got, want, "logs:\n{}", failed.meta.pretty_logs());
        }
    }
}

pub fn set_series_paused_ix(admin: &Address, series: &Address, paused: u8) -> Instruction {
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &[tag::SET_SERIES_PAUSED, paused],
        vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new_readonly(config_address(), false),
            AccountMeta::new(*series, false),
        ],
    )
}

pub fn set_config_ix(admin: &Address, new_admin: &Address) -> Instruction {
    let mut data = vec![tag::SET_CONFIG];
    data.extend_from_slice(new_admin.as_ref());
    Instruction::new_with_bytes(
        PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new_readonly(*admin, true),
            AccountMeta::new(config_address(), false),
        ],
    )
}
