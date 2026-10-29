//! Rust client SDK for quotebook, request-for-quote trading on Solana.
//!
//! - [`pda`]: every program-derived address.
//! - [`ix`]: a builder for every instruction, in the account order the program expects.
//! - [`settle_instructions`]: the ed25519 precompile instruction and the `settle` instruction
//!   for a maker-signed [`Quote`].
//! - [`decode`]: program accounts from RPC account data.
//!
//! The quote encoding, account layouts and error codes come from [`quotebook_core`],
//! re-exported as [`core`].

pub use quotebook_core as core;

pub mod decode;
pub mod ix;
pub mod pda;

use quotebook_core::sigverify;
use quotebook_core::{BaseLots, Quote};
use solana_address::Address;
use solana_instruction::Instruction;

pub use decode::DecodeError;
pub use ix::SettleAccounts;

/// The ed25519 signature-verification precompile.
pub const ED25519_PROGRAM: Address = solana_sdk_ids::ed25519_program::ID;

/// The bytes a maker signs for `quote`: its canonical 208-byte encoding.
#[must_use]
pub fn quote_message(quote: &Quote) -> [u8; Quote::LEN] {
    quote.encode()
}

/// A `Config` deployment id from the cluster's genesis hash (RPC `getGenesisHash`, decoded
/// from base58): its first 16 bytes (docs/PLAN.md, section 2).
#[must_use]
pub fn deployment_id_from_genesis_hash(genesis_hash: &[u8; 32]) -> [u8; 16] {
    let mut id = [0u8; 16];
    let (head, _) = genesis_hash.split_at(16);
    id.copy_from_slice(head);
    id
}

/// The self-contained ed25519 precompile instruction that verifies `signature` over `quote`
/// by `quote.maker`.
#[must_use]
pub fn precompile_instruction(quote: &Quote, signature: &[u8; 64]) -> Instruction {
    Instruction::new_with_bytes(
        ED25519_PROGRAM,
        &sigverify::instruction_data(quote, signature),
        vec![],
    )
}

/// The two instructions that fill `quote`: the precompile instruction, to be placed at
/// `precompile_index` in the transaction, and the `settle` instruction that reads it, to be
/// placed anywhere after it.
///
/// For the common transaction `[precompile, settle]`, `precompile_index` is 0; put any
/// compute-budget instructions after the pair or count them in.
#[must_use]
pub fn settle_instructions(
    program_id: &Address,
    quote: &Quote,
    signature: &[u8; 64],
    accounts: &SettleAccounts,
    fill_size: BaseLots,
    precompile_index: u8,
) -> [Instruction; 2] {
    [
        precompile_instruction(quote, signature),
        ix::settle(program_id, quote, accounts, precompile_index, fill_size),
    ]
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use quotebook_core::instruction::{SettleArgs, Tag};
    use quotebook_core::{Nonce, QuoteLotsPerBaseLot, QuoteTerms, SeriesId, Side, UnixTimestamp};

    fn quote() -> Quote {
        Quote::new(QuoteTerms {
            program_id: Address::new_from_array([1; 32]),
            deployment: [2; 16],
            maker: Address::new_from_array([3; 32]),
            series: SeriesId::new(Address::new_from_array([4; 32])),
            taker: Quote::OPEN_TAKER,
            side: Side::Ask,
            price: QuoteLotsPerBaseLot::new(5),
            size: BaseLots::new(6),
            issued_at: UnixTimestamp::new(0),
            expires_at: UnixTimestamp::new(60),
            nonce: Nonce::new(7),
        })
        .unwrap()
    }

    fn accounts() -> SettleAccounts {
        let a = |b| Address::new_from_array([b; 32]);
        SettleAccounts {
            taker: a(10),
            base_mint: a(11),
            quote_mint: a(12),
            base_token_program: a(13),
            quote_token_program: a(14),
            maker_base: a(15),
            maker_quote: a(16),
            taker_base: a(17),
            taker_quote: a(18),
        }
    }

    #[test]
    fn test_precompile_instruction_is_what_settle_reads() {
        let q = quote();
        let ix = precompile_instruction(&q, &[9; 64]);
        assert_eq!(ix.program_id, ED25519_PROGRAM);
        assert!(ix.accounts.is_empty());
        assert_eq!(sigverify::verified_quote(&ix.data), Ok(q));
    }

    #[test]
    fn test_settle_instruction_layout() {
        let program = Address::new_from_array([0x51; 32]);
        let q = quote();
        let [pre, settle] =
            settle_instructions(&program, &q, &[9; 64], &accounts(), BaseLots::new(4), 2);
        assert_eq!(pre.program_id, ED25519_PROGRAM);
        assert_eq!(settle.program_id, program);
        let (tag, args) = Tag::split(&settle.data).unwrap();
        assert_eq!(tag, Tag::Settle);
        assert_eq!(
            SettleArgs::decode(args).unwrap(),
            SettleArgs {
                sig_ix_index: 2,
                fill_size: BaseLots::new(4)
            }
        );
        let keys: Vec<Address> = settle.accounts.iter().map(|m| m.pubkey).collect();
        let maker = q.terms().maker;
        let a = accounts();
        assert_eq!(
            keys,
            vec![
                a.taker,
                q.terms().series.address(),
                pda::maker_state(&program, &maker).0,
                pda::delegate(&program, &maker).0,
                a.maker_base,
                a.maker_quote,
                a.taker_base,
                a.taker_quote,
                a.base_mint,
                a.quote_mint,
                a.base_token_program,
                a.quote_token_program,
                solana_sdk_ids::sysvar::instructions::ID,
            ]
        );
        let writable: Vec<bool> = settle.accounts.iter().map(|m| m.is_writable).collect();
        assert_eq!(
            writable,
            [
                false, false, true, false, true, true, true, true, false, false, false, false,
                false
            ]
        );
        assert!(settle.accounts[0].is_signer);
        assert!(settle.accounts[1..].iter().all(|m| !m.is_signer));
    }

    #[test]
    fn test_deployment_id_is_the_genesis_hash_prefix() {
        let mut hash = [0u8; 32];
        for (i, b) in hash.iter_mut().enumerate() {
            *b = u8::try_from(i).unwrap();
        }
        let id = deployment_id_from_genesis_hash(&hash);
        assert_eq!(&id[..], &hash[..16]);
    }
}
