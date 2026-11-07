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
    /// Accounts owned by `program_id` with exactly `data_size` bytes whose first byte is `tag`
    /// (RPC `getProgramAccounts` with a size and a memcmp filter).
    ///
    /// # Errors
    ///
    /// `Rpc` if the call fails, or if this RPC cannot list accounts (the default).
    fn program_accounts(
        &mut self,
        program_id: &Address,
        data_size: u64,
        tag: u8,
    ) -> Result<Vec<(Address, Account)>, CliError> {
        let _ = (program_id, data_size, tag);
        Err(CliError::Rpc(
            "this RPC cannot list program accounts".into(),
        ))
    }
    /// Every account of `addresses` at `finalized`, in one `getMultipleAccounts`, so all of them
    /// are from the one slot returned with them.
    ///
    /// # Errors
    ///
    /// `Rpc` if the call fails, or if this RPC cannot read at `finalized` (the default).
    fn finalized_accounts(
        &mut self,
        addresses: &[Address],
    ) -> Result<(u64, Vec<Option<Account>>), CliError> {
        let _ = addresses;
        Err(CliError::Rpc("this RPC cannot read at finalized".into()))
    }
    /// One page of `getSignaturesForAddress` at `finalized`, newest first, starting below
    /// `before` when given.
    ///
    /// # Errors
    ///
    /// `Rpc` if the call fails, or if this RPC cannot list signatures (the default).
    fn finalized_signatures(
        &mut self,
        address: &Address,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SignatureInfo>, CliError> {
        let _ = (address, before, limit);
        Err(CliError::Rpc("this RPC cannot list signatures".into()))
    }
    /// `getTransaction` at `finalized`, JSON encoding, versions up to 0; `None` if the node does
    /// not have it.
    ///
    /// # Errors
    ///
    /// `Rpc` if the call fails, or if this RPC cannot read transactions (the default).
    fn finalized_transaction(
        &mut self,
        signature: &str,
    ) -> Result<Option<serde_json::Value>, CliError> {
        let _ = signature;
        Err(CliError::Rpc("this RPC cannot read transactions".into()))
    }
}

/// One entry of `getSignaturesForAddress`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureInfo {
    /// The transaction's first signature, base58.
    pub signature: String,
    /// Its slot.
    pub slot: u64,
    /// Its block time, if the node has it.
    pub block_time: Option<i64>,
    /// Whether the transaction failed.
    pub failed: bool,
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

    fn program_accounts(
        &mut self,
        program_id: &Address,
        data_size: u64,
        tag: u8,
    ) -> Result<Vec<(Address, Account)>, CliError> {
        use solana_rpc_client::api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
        use solana_rpc_client::api::filter::{Memcmp, RpcFilterType};
        let config = RpcProgramAccountsConfig {
            filters: Some(vec![
                RpcFilterType::DataSize(data_size),
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, vec![tag])),
            ]),
            account_config: RpcAccountInfoConfig {
                encoding: Some(solana_account_decoder_client_types::UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                ..RpcAccountInfoConfig::default()
            },
            with_context: None,
            sort_results: None,
        };
        let accounts = self
            .client
            .get_program_ui_accounts_with_config(program_id, config)
            .map_err(|e| CliError::Rpc(self.describe(&e)))?;
        accounts
            .into_iter()
            .map(|(address, ui)| {
                ui.to_account()
                    .map(|account| (address, account))
                    .ok_or_else(|| {
                        CliError::Rpc("undecodable account in getProgramAccounts".into())
                    })
            })
            .collect()
    }

    fn finalized_accounts(
        &mut self,
        addresses: &[Address],
    ) -> Result<(u64, Vec<Option<Account>>), CliError> {
        self.client
            .get_multiple_accounts_with_commitment(addresses, CommitmentConfig::finalized())
            .map(|r| (r.context.slot, r.value))
            .map_err(|e| CliError::Rpc(self.describe(&e)))
    }

    fn finalized_signatures(
        &mut self,
        address: &Address,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SignatureInfo>, CliError> {
        use solana_rpc_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
        let before = before
            .map(str::parse::<Signature>)
            .transpose()
            .map_err(|_| CliError::Rpc("not a signature".into()))?;
        let config = GetConfirmedSignaturesForAddress2Config {
            before,
            until: None,
            limit: Some(limit),
            commitment: Some(CommitmentConfig::finalized()),
        };
        let page = self
            .client
            .get_signatures_for_address_with_config(address, config)
            .map_err(|e| CliError::Rpc(self.describe(&e)))?;
        Ok(page
            .into_iter()
            .map(|s| SignatureInfo {
                signature: s.signature,
                slot: s.slot,
                block_time: s.block_time,
                failed: s.err.is_some(),
            })
            .collect())
    }

    fn finalized_transaction(
        &mut self,
        signature: &str,
    ) -> Result<Option<serde_json::Value>, CliError> {
        use solana_rpc_client::api::request::RpcRequest;
        let params = serde_json::json!([
            signature,
            {
                "encoding": "json",
                "commitment": "finalized",
                "maxSupportedTransactionVersion": 0
            }
        ]);
        let value: serde_json::Value = self
            .client
            .send(RpcRequest::GetTransaction, params)
            .map_err(|e| CliError::Rpc(self.describe(&e)))?;
        Ok((!value.is_null()).then_some(value))
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
