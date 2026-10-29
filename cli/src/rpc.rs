//! The chain, behind a trait: a real cluster over `solana-rpc-client`, or LiteSVM in tests.

use quotebook_core::UnixTimestamp;
use solana_account::Account;
use solana_address::Address;
use solana_commitment_config::CommitmentConfig;
use solana_hash::Hash;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signature::Signature;
use solana_signer::Signer;
use solana_transaction::Transaction;

use crate::error::Redactor;
use crate::CliError;

/// What the tools need from a cluster.
pub trait Rpc {
    /// The account at `address`, or `None` if it does not exist.
    ///
    /// # Errors
    ///
    /// `Rpc` if the call fails.
    fn account(&mut self, address: &Address) -> Result<Option<Account>, CliError>;
    /// A recent blockhash.
    ///
    /// # Errors
    ///
    /// `Rpc` if the call fails.
    fn latest_blockhash(&mut self) -> Result<Hash, CliError>;
    /// Sends a signed transaction and waits for confirmation.
    ///
    /// # Errors
    ///
    /// `Rpc` if it fails or is rejected.
    fn send(&mut self, transaction: &Transaction) -> Result<Signature, CliError>;
    /// The cluster's genesis hash.
    ///
    /// # Errors
    ///
    /// `Rpc` if the call fails.
    fn genesis_hash(&mut self) -> Result<Hash, CliError>;
}

/// A real cluster, at `confirmed` commitment. The URL (which may embed an API key) is never
/// shown: not in `Debug`, not in errors.
pub struct Cluster {
    client: RpcClient,
    redactor: Redactor,
}

impl Cluster {
    /// An RPC error as text: the client's whole message, including the program logs it
    /// appends for a failed preflight, and only then redacted, so neither the message nor the
    /// logs can carry the URL, the bare key or a bare path token.
    #[must_use]
    pub fn describe(&self, e: &solana_rpc_client::api::client_error::Error) -> String {
        self.redactor.redact(&e.to_string())
    }
}

impl Cluster {
    /// Connects to `url` (for example `https://api.devnet.solana.com`).
    #[must_use]
    pub fn new(url: &str) -> Self {
        Self {
            client: RpcClient::new_with_commitment(url.to_owned(), CommitmentConfig::confirmed()),
            redactor: Redactor::new(url),
        }
    }
}

impl std::fmt::Debug for Cluster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Cluster").field(&"<redacted-url>").finish()
    }
}

impl Rpc for Cluster {
    fn account(&mut self, address: &Address) -> Result<Option<Account>, CliError> {
        self.client
            .get_account_with_commitment(address, CommitmentConfig::confirmed())
            .map(|r| r.value)
            .map_err(|e| CliError::Rpc(self.describe(&e)))
    }

    fn latest_blockhash(&mut self) -> Result<Hash, CliError> {
        self.client
            .get_latest_blockhash()
            .map_err(|e| CliError::Rpc(self.describe(&e)))
    }

    fn send(&mut self, transaction: &Transaction) -> Result<Signature, CliError> {
        self.client
            .send_and_confirm_transaction(transaction)
            .map_err(|e| CliError::Rpc(self.describe(&e)))
    }

    fn genesis_hash(&mut self) -> Result<Hash, CliError> {
        self.client
            .get_genesis_hash()
            .map_err(|e| CliError::Rpc(self.describe(&e)))
    }
}

/// The account at `address`, which must exist.
///
/// # Errors
///
/// `Account` if it does not exist; the RPC error.
pub fn existing(rpc: &mut dyn Rpc, address: &Address) -> Result<Account, CliError> {
    rpc.account(address)?
        .ok_or_else(|| CliError::Account(format!("{address} does not exist")))
}

/// The cluster clock, from the Clock sysvar (`unix_timestamp` at bytes 32..40).
///
/// # Errors
///
/// `Account` if the sysvar is missing or short; the RPC error.
pub fn clock(rpc: &mut dyn Rpc) -> Result<UnixTimestamp, CliError> {
    let account = existing(rpc, &solana_sdk_ids::sysvar::clock::ID)?;
    let bytes: [u8; 8] = account
        .data
        .get(32..40)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| CliError::Account("clock sysvar too short".into()))?;
    Ok(UnixTimestamp::new(i64::from_le_bytes(bytes)))
}

/// Signs `instructions` with `payer` (fee payer) and `signers` (deduplicated) and sends them.
///
/// # Errors
///
/// `Rpc` if signing or sending fails.
pub fn send(
    rpc: &mut dyn Rpc,
    instructions: &[Instruction],
    payer: &Keypair,
    signers: &[&Keypair],
) -> Result<Signature, CliError> {
    let mut all: Vec<&Keypair> = vec![payer];
    for s in signers {
        if all.iter().all(|k| k.pubkey() != s.pubkey()) {
            all.push(s);
        }
    }
    let blockhash = rpc.latest_blockhash()?;
    let mut tx = Transaction::new_with_payer(instructions, Some(&payer.pubkey()));
    tx.try_sign(&all, blockhash)
        .map_err(|e| CliError::Rpc(e.to_string()))?;
    rpc.send(&tx)
}
