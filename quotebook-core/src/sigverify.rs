//! Reading a verified quote out of an ed25519 precompile instruction (docs/PLAN.md, 4.1).
//!
//! The runtime verifies every ed25519 precompile instruction of a transaction before it runs,
//! and fails the transaction if any signature is invalid. `settle` then needs to know *what*
//! was verified. This module parses the precompile instruction's data and accepts it only when
//! it holds exactly one signature whose signature, public key and message all live inside that
//! same instruction (every instruction index `0xFFFF`). Otherwise an attacker could have the
//! precompile verify a genuine signature over bytes in another instruction while the bytes
//! read here say something else. The quote is then decoded from exactly the bytes the
//! precompile verified, and the signing key must be the quote's maker.
//!
//! The program checks the instruction's program id and its position in the transaction.

use crate::{Quote, QuotebookError};

/// Offset of the first `Ed25519SignatureOffsets` record: `num_signatures: u8`, one padding
/// byte.
pub const OFFSETS_START: usize = 2;

/// Size of one `Ed25519SignatureOffsets` record: seven little-endian `u16`.
pub const OFFSETS_LEN: usize = 14;

/// An instruction index meaning "this instruction".
pub const SELF_INSTRUCTION: u16 = u16::MAX;

/// Size of an ed25519 signature.
pub const SIGNATURE_LEN: usize = 64;

/// Size of an ed25519 public key.
pub const PUBKEY_LEN: usize = 32;

/// The seven fields of an `Ed25519SignatureOffsets` record, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureOffsets {
    /// Offset of the 64-byte signature.
    pub signature_offset: u16,
    /// Instruction holding the signature.
    pub signature_instruction_index: u16,
    /// Offset of the 32-byte public key.
    pub public_key_offset: u16,
    /// Instruction holding the public key.
    pub public_key_instruction_index: u16,
    /// Offset of the message.
    pub message_data_offset: u16,
    /// Length of the message.
    pub message_data_size: u16,
    /// Instruction holding the message.
    pub message_instruction_index: u16,
}

impl SignatureOffsets {
    /// Decodes a 14-byte record.
    #[must_use]
    pub fn decode(bytes: &[u8; OFFSETS_LEN]) -> Self {
        let (pairs, _) = bytes.as_chunks::<2>();
        let mut f = pairs.iter().map(|p| u16::from_le_bytes(*p));
        // Exactly seven pairs, so `next` never falls back.
        let mut next = || f.next().unwrap_or(0);
        Self {
            signature_offset: next(),
            signature_instruction_index: next(),
            public_key_offset: next(),
            public_key_instruction_index: next(),
            message_data_offset: next(),
            message_data_size: next(),
            message_instruction_index: next(),
        }
    }

    /// The 14-byte encoding.
    #[must_use]
    pub fn encode(&self) -> [u8; OFFSETS_LEN] {
        let mut out = [0u8; OFFSETS_LEN];
        let fields = [
            self.signature_offset,
            self.signature_instruction_index,
            self.public_key_offset,
            self.public_key_instruction_index,
            self.message_data_offset,
            self.message_data_size,
            self.message_instruction_index,
        ];
        let (pairs, _) = out.as_chunks_mut::<2>();
        for (pair, field) in pairs.iter_mut().zip(fields) {
            *pair = field.to_le_bytes();
        }
        out
    }
}

/// The bytes at `offset .. offset + len` of `data`.
fn range(data: &[u8], offset: u16, len: usize) -> Result<&[u8], QuotebookError> {
    let start = usize::from(offset);
    let end = start
        .checked_add(len)
        .ok_or(QuotebookError::MalformedSigVerify)?;
    data.get(start..end)
        .ok_or(QuotebookError::MalformedSigVerify)
}

/// Parses the data of an ed25519 precompile instruction and returns the quote it verified.
///
/// # Errors
///
/// - `MalformedSigVerify` unless the data holds exactly one signature record and every range
///   it names lies inside the data.
/// - `SigVerifyNotSelfContained` if any of the three instruction indexes is not `0xFFFF`.
/// - `QuoteLengthMismatch` if the signed message is not [`Quote::LEN`] bytes.
/// - The errors of [`Quote::decode`] for the message bytes.
/// - `SignerMismatch` if the verified public key is not the quote's maker.
#[must_use = "the result carries a possible error"]
pub fn verified_quote(data: &[u8]) -> Result<Quote, QuotebookError> {
    let (count, rest) = data
        .split_first()
        .ok_or(QuotebookError::MalformedSigVerify)?;
    if *count != 1 {
        return Err(QuotebookError::MalformedSigVerify);
    }
    let (_padding, rest) = rest
        .split_first()
        .ok_or(QuotebookError::MalformedSigVerify)?;
    let (record, _) = rest
        .split_first_chunk::<OFFSETS_LEN>()
        .ok_or(QuotebookError::MalformedSigVerify)?;
    let o = SignatureOffsets::decode(record);
    if o.signature_instruction_index != SELF_INSTRUCTION
        || o.public_key_instruction_index != SELF_INSTRUCTION
        || o.message_instruction_index != SELF_INSTRUCTION
    {
        return Err(QuotebookError::SigVerifyNotSelfContained);
    }
    if usize::from(o.message_data_size) != Quote::LEN {
        return Err(QuotebookError::QuoteLengthMismatch);
    }
    range(data, o.signature_offset, SIGNATURE_LEN)?;
    let pubkey = range(data, o.public_key_offset, PUBKEY_LEN)?;
    let message = range(data, o.message_data_offset, Quote::LEN)?;
    let quote = Quote::decode(message)?;
    if pubkey != quote.terms().maker.as_ref() {
        return Err(QuotebookError::SignerMismatch);
    }
    Ok(quote)
}

/// Builds the data of a self-contained, single-signature ed25519 precompile instruction over
/// `quote`, in the layout the Solana SDK uses: offsets record, public key, signature,
/// message. Clients use it to build the instruction that precedes `settle`.
#[must_use]
pub fn instruction_data(
    quote: &Quote,
    signature: &[u8; SIGNATURE_LEN],
) -> [u8; INSTRUCTION_DATA_LEN] {
    let mut out = [0u8; INSTRUCTION_DATA_LEN];
    let offsets = SignatureOffsets {
        signature_offset: SIGNATURE_AT,
        signature_instruction_index: SELF_INSTRUCTION,
        public_key_offset: PUBKEY_AT,
        public_key_instruction_index: SELF_INSTRUCTION,
        message_data_offset: MESSAGE_AT,
        message_data_size: QUOTE_LEN_U16,
        message_instruction_index: SELF_INSTRUCTION,
    };
    let parts: [&[u8]; 5] = [
        &[1, 0],
        &offsets.encode(),
        quote.terms().maker.as_ref(),
        signature,
        &quote.encode(),
    ];
    let mut at = 0usize;
    for part in parts {
        let end = at.saturating_add(part.len());
        if let Some(dst) = out.get_mut(at..end) {
            dst.copy_from_slice(part);
        }
        at = end;
    }
    out
}

/// `Quote::LEN` as the record's `u16`.
const QUOTE_LEN_U16: u16 = 208;
/// Where [`instruction_data`] puts the public key.
const PUBKEY_AT: u16 = 16;
/// Where [`instruction_data`] puts the signature.
const SIGNATURE_AT: u16 = 48;
/// Where [`instruction_data`] puts the message.
const MESSAGE_AT: u16 = 112;

/// Length of [`instruction_data`]'s output: 16 + 32 + 64 + 208.
pub const INSTRUCTION_DATA_LEN: usize = 320;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]
mod tests {
    extern crate std;

    use super::{instruction_data, verified_quote, SignatureOffsets, INSTRUCTION_DATA_LEN};
    use crate::{
        Address, BaseLots, Nonce, Quote, QuoteLotsPerBaseLot, QuoteTerms, QuotebookError, SeriesId,
        Side, UnixTimestamp,
    };
    use std::vec::Vec;

    fn quote(maker: [u8; 32]) -> Quote {
        Quote::new(QuoteTerms {
            program_id: Address::new_from_array([1; 32]),
            deployment: [2; 16],
            maker: Address::new_from_array(maker),
            series: SeriesId::new(Address::new_from_array([4; 32])),
            taker: Quote::OPEN_TAKER,
            side: Side::Bid,
            price: QuoteLotsPerBaseLot::new(5),
            size: BaseLots::new(6),
            issued_at: UnixTimestamp::new(0),
            expires_at: UnixTimestamp::new(60),
            nonce: Nonce::new(7),
        })
        .unwrap()
    }

    fn data() -> Vec<u8> {
        instruction_data(&quote([3; 32]), &[9; 64]).to_vec()
    }

    fn with_offsets(edit: impl FnOnce(&mut SignatureOffsets)) -> Vec<u8> {
        let mut d = data();
        let mut o = SignatureOffsets::decode(d[2..16].try_into().unwrap());
        edit(&mut o);
        d[2..16].copy_from_slice(&o.encode());
        d
    }

    #[test]
    fn test_verified_quote_round_trips_sdk_layout() {
        let d = data();
        assert_eq!(d.len(), INSTRUCTION_DATA_LEN);
        assert_eq!(verified_quote(&d), Ok(quote([3; 32])));
        assert_eq!(&d[16..48], &[3; 32]);
        assert_eq!(&d[48..112], &[9; 64]);
    }

    #[test]
    fn test_verified_quote_signature_count_must_be_one() {
        for count in [0u8, 2, 255] {
            let mut d = data();
            d[0] = count;
            assert_eq!(verified_quote(&d), Err(QuotebookError::MalformedSigVerify));
        }
    }

    #[test]
    fn test_verified_quote_each_index_must_be_self() {
        for which in 0..3 {
            for index in [0u16, 1, 7, u16::MAX - 1] {
                let d = with_offsets(|o| match which {
                    0 => o.signature_instruction_index = index,
                    1 => o.public_key_instruction_index = index,
                    _ => o.message_instruction_index = index,
                });
                assert_eq!(
                    verified_quote(&d),
                    Err(QuotebookError::SigVerifyNotSelfContained)
                );
            }
        }
    }

    #[test]
    fn test_verified_quote_message_size_must_be_quote_len() {
        for size in [0u16, 207, 209, u16::MAX] {
            let d = with_offsets(|o| o.message_data_size = size);
            assert_eq!(verified_quote(&d), Err(QuotebookError::QuoteLengthMismatch));
        }
    }

    #[test]
    fn test_verified_quote_ranges_must_fit() {
        let len = INSTRUCTION_DATA_LEN as u16;
        for d in [
            with_offsets(|o| o.signature_offset = len - 63),
            with_offsets(|o| o.public_key_offset = len - 31),
            with_offsets(|o| o.message_data_offset = len - 207),
            with_offsets(|o| o.signature_offset = u16::MAX),
            with_offsets(|o| o.message_data_offset = u16::MAX),
        ] {
            assert_eq!(verified_quote(&d), Err(QuotebookError::MalformedSigVerify));
        }
        let mut d = data();
        d.truncate(INSTRUCTION_DATA_LEN - 1);
        assert_eq!(verified_quote(&d), Err(QuotebookError::MalformedSigVerify));
        for short in [0usize, 1, 2, 15] {
            assert_eq!(
                verified_quote(&data()[..short]),
                Err(QuotebookError::MalformedSigVerify)
            );
        }
    }

    /// The public key named by the offsets, not some other key in the data, must be the maker.
    #[test]
    fn test_verified_quote_pubkey_must_be_maker() {
        let mut d = data();
        d[16] ^= 1;
        assert_eq!(verified_quote(&d), Err(QuotebookError::SignerMismatch));
        // Point the key at the signature bytes instead.
        let d = with_offsets(|o| o.public_key_offset = 48);
        assert_eq!(verified_quote(&d), Err(QuotebookError::SignerMismatch));
    }

    /// The quote is read at the message offset: moving it to a second copy of a different
    /// quote returns that quote (which the precompile then must have verified), never the
    /// first one.
    #[test]
    fn test_verified_quote_reads_exactly_the_signed_bytes() {
        let mut d = data();
        let other = quote([3; 32]);
        let mut t = *other.terms();
        t.nonce = Nonce::new(99);
        let other = Quote::new(t).unwrap();
        d.extend_from_slice(&other.encode());
        let d2 = {
            let mut o = SignatureOffsets::decode(d[2..16].try_into().unwrap());
            o.message_data_offset = INSTRUCTION_DATA_LEN as u16;
            let mut d2 = d.clone();
            d2[2..16].copy_from_slice(&o.encode());
            d2
        };
        assert_eq!(verified_quote(&d), Ok(quote([3; 32])));
        assert_eq!(verified_quote(&d2), Ok(other));
    }

    #[test]
    fn test_verified_quote_bad_message_bytes_are_rejected() {
        let mut d = data();
        d[112] ^= 1; // domain tag
        assert_eq!(verified_quote(&d), Err(QuotebookError::WrongDomain));
        let mut d = data();
        d[112 + 160] = 2; // side
        assert_eq!(verified_quote(&d), Err(QuotebookError::MalformedQuote));
    }

    #[test]
    fn test_signature_offsets_round_trip() {
        let o = SignatureOffsets {
            signature_offset: 1,
            signature_instruction_index: 2,
            public_key_offset: 3,
            public_key_instruction_index: 4,
            message_data_offset: 5,
            message_data_size: 6,
            message_instruction_index: 0xFFFF,
        };
        assert_eq!(SignatureOffsets::decode(&o.encode()), o);
    }
}
