//! The CLI error type.

use quotebook_maker::client::ClientError;

/// Why a command failed.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// A command-line argument is missing or invalid.
    #[error("argument: {0}")]
    Args(String),
    /// A keypair file could not be read. Carries the path, never the contents.
    #[error("cannot read keypair file {0}")]
    Keypair(String),
    /// The RPC call failed or the transaction was rejected.
    #[error("rpc: {0}")]
    Rpc(String),
    /// An account is missing or is not what it should be.
    #[error("account: {0}")]
    Account(String),
    /// The maker refused, or its quote fails a check.
    #[error("quote: {0}")]
    Quote(#[from] ClientError),
    /// The maker service could not be configured.
    #[error("maker: {0}")]
    Maker(String),
}
