//! Keypair files. The key never leaves this module except as a `Keypair`; errors name only
//! the path.

use solana_keypair::{read_keypair_file, Keypair};

use crate::CliError;

/// Reads a Solana CLI keypair file (a JSON array of 64 bytes).
///
/// # Errors
///
/// `Keypair` with the path if it cannot be read or parsed.
pub fn load(path: &str) -> Result<Keypair, CliError> {
    read_keypair_file(path).map_err(|_| CliError::Keypair(path.to_owned()))
}
