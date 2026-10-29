//! The maker's signing key. Its `Debug` shows only the public key, so it can never reach a log.

use std::fmt;

use quotebook_core::Quote;
use solana_address::Address;
use solana_keypair::Keypair;
use solana_signer::Signer;

/// The maker's ed25519 key.
pub struct MakerKey(Keypair);

impl MakerKey {
    /// Wraps `keypair`.
    #[must_use]
    pub fn new(keypair: Keypair) -> Self {
        Self(keypair)
    }

    /// The maker's address, the quotes' `maker`.
    #[must_use]
    pub fn address(&self) -> Address {
        self.0.pubkey()
    }

    /// Signs the canonical bytes of `quote`. The precompile verifies exactly these bytes.
    #[must_use]
    pub fn sign(&self, quote: &Quote) -> [u8; 64] {
        let signature = self.0.sign_message(&quote.encode());
        let mut out = [0u8; 64];
        out.copy_from_slice(signature.as_ref());
        out
    }
}

impl fmt::Debug for MakerKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MakerKey").field(&self.address()).finish()
    }
}
