//! Shared pieces of the quotebook fuzz targets (docs/PLAN-improvements.md, I6).
//!
//! - [`reference_verified_quote`]: an independent reference parser for the ed25519 precompile
//!   data, written from the rules in docs/PLAN.md 4.1 rather than from
//!   `quotebook-core/src/sigverify.rs`. It predicts accept or the exact error, and the
//!   `sigverify` target asserts that the code under test agrees.
//! - [`SigInput`]: a structured input that starts from a valid precompile record and mutates
//!   chosen fields, so every rejection branch is reached, not only the first length check.
//!
//! Fuzz code may panic: a panic is how a target reports a finding.

use arbitrary::Arbitrary;
use quotebook_core::{
    sigverify, Address, BaseLots, Nonce, Quote, QuoteLotsPerBaseLot, QuoteTerms, QuotebookError,
    SeriesId, Side, UnixTimestamp, QUOTE_DOMAIN,
};

/// The outcome the reference parser predicts. `Ok` carries the 208 message bytes and the
/// 32-byte public key it read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Predicted {
    Ok {
        message: [u8; 208],
        pubkey: [u8; 32],
    },
    Err(QuotebookError),
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    let lo = *data.get(at)?;
    let hi = *data.get(at + 1)?;
    Some(u16::from(lo) | (u16::from(hi) << 8))
}

fn slice(data: &[u8], offset: u16, len: usize) -> Option<&[u8]> {
    let start = offset as usize;
    data.get(start..start + len)
}

/// Reference decode of a 208-byte quote message: the predicted error, or `None` if valid.
pub fn reference_quote_error(m: &[u8]) -> Option<QuotebookError> {
    if m.len() != 208 {
        return Some(QuotebookError::QuoteLengthMismatch);
    }
    if m[0..16] != QUOTE_DOMAIN {
        return Some(QuotebookError::WrongDomain);
    }
    // side at 160, padding 161..168, price 168..176, size 176..184.
    if m[161..168].iter().any(|b| *b != 0) {
        return Some(QuotebookError::MalformedQuote);
    }
    if m[160] > 1 {
        return Some(QuotebookError::MalformedQuote);
    }
    let price = u64::from_le_bytes(m[168..176].try_into().unwrap());
    let size = u64::from_le_bytes(m[176..184].try_into().unwrap());
    if price == 0 || size == 0 {
        return Some(QuotebookError::MalformedQuote);
    }
    None
}

/// Predicts `sigverify::verified_quote(data)`, in the documented check order:
/// count byte, padding byte, offsets record, the three instruction indexes, the message size,
/// the signature, public key and message ranges, the quote decode, and the maker binding.
pub fn reference_verified_quote(data: &[u8]) -> Predicted {
    use QuotebookError as E;
    let Some(&count) = data.first() else {
        return Predicted::Err(E::MalformedSigVerify);
    };
    if count != 1 {
        return Predicted::Err(E::MalformedSigVerify);
    }
    if data.len() < 2 + 14 {
        return Predicted::Err(E::MalformedSigVerify);
    }
    let f = |i: usize| u16_at(data, 2 + 2 * i).unwrap();
    let (sig_off, sig_ix, pk_off, pk_ix, msg_off, msg_size, msg_ix) =
        (f(0), f(1), f(2), f(3), f(4), f(5), f(6));
    if sig_ix != u16::MAX || pk_ix != u16::MAX || msg_ix != u16::MAX {
        return Predicted::Err(E::SigVerifyNotSelfContained);
    }
    if msg_size != 208 {
        return Predicted::Err(E::QuoteLengthMismatch);
    }
    if slice(data, sig_off, 64).is_none() {
        return Predicted::Err(E::MalformedSigVerify);
    }
    let Some(pk) = slice(data, pk_off, 32) else {
        return Predicted::Err(E::MalformedSigVerify);
    };
    let Some(m) = slice(data, msg_off, 208) else {
        return Predicted::Err(E::MalformedSigVerify);
    };
    if let Some(e) = reference_quote_error(m) {
        return Predicted::Err(e);
    }
    // maker at 64..96 of the message.
    if pk != &m[64..96] {
        return Predicted::Err(E::SignerMismatch);
    }
    Predicted::Ok {
        message: m.try_into().unwrap(),
        pubkey: pk.try_into().unwrap(),
    }
}

/// Arbitrary quote terms (price and size forced non-zero so `Quote::new` accepts them).
#[derive(Debug, Arbitrary)]
pub struct ArbTerms {
    pub program_id: [u8; 32],
    pub deployment: [u8; 16],
    pub maker: [u8; 32],
    pub series: [u8; 32],
    pub taker: [u8; 32],
    pub ask: bool,
    pub price: u64,
    pub size: u64,
    pub issued_at: i64,
    pub expires_at: i64,
    pub nonce: u64,
}

impl ArbTerms {
    pub fn quote(&self) -> Quote {
        Quote::new(QuoteTerms {
            program_id: Address::new_from_array(self.program_id),
            deployment: self.deployment,
            maker: Address::new_from_array(self.maker),
            series: SeriesId::new(Address::new_from_array(self.series)),
            taker: Address::new_from_array(self.taker),
            side: if self.ask { Side::Ask } else { Side::Bid },
            price: QuoteLotsPerBaseLot::new(self.price.max(1)),
            size: BaseLots::new(self.size.max(1)),
            issued_at: UnixTimestamp::new(self.issued_at),
            expires_at: UnixTimestamp::new(self.expires_at),
            nonce: Nonce::new(self.nonce),
        })
        .unwrap()
    }
}

/// One edit applied to a valid precompile record.
#[derive(Debug, Arbitrary)]
pub enum Edit {
    /// Overwrite the signature count.
    Count(u8),
    /// Overwrite field `i % 7` of the offsets record.
    Field(u8, u16),
    /// Overwrite one byte at `at % len`.
    Byte(u16, u8),
    /// Overwrite a byte inside the message (`at % 208`), to reach the quote decode checks.
    MessageByte(u8, u8),
    /// Replace the public key with other bytes (breaks the maker binding).
    PubKey([u8; 32]),
    /// Truncate to `len % (len + 1)` bytes.
    Truncate(u16),
    /// Append bytes.
    Extend(Vec<u8>),
}

/// A structured `sigverify` input: either raw bytes, or a valid record plus edits.
#[derive(Debug, Arbitrary)]
pub enum SigInput {
    Raw(Vec<u8>),
    Structured {
        terms: ArbTerms,
        signature: [u8; 64],
        edits: Vec<Edit>,
    },
}

impl SigInput {
    /// The precompile data this input describes.
    pub fn bytes(&self) -> Vec<u8> {
        match self {
            SigInput::Raw(b) => b.clone(),
            SigInput::Structured {
                terms,
                signature,
                edits,
            } => {
                let quote = terms.quote();
                let mut d = sigverify::instruction_data(&quote, signature).to_vec();
                let message_at = message_offset(&d);
                for e in edits {
                    apply(&mut d, message_at, e);
                }
                d
            }
        }
    }
}

/// The message offset recorded in a freshly built record.
fn message_offset(d: &[u8]) -> usize {
    u16_at(d, 2 + 2 * 4).unwrap_or(0) as usize
}

fn apply(d: &mut Vec<u8>, message_at: usize, e: &Edit) {
    match e {
        Edit::Count(c) => {
            if let Some(b) = d.first_mut() {
                *b = *c;
            }
        }
        Edit::Field(i, v) => {
            let at = 2 + 2 * (usize::from(*i) % 7);
            if d.len() >= at + 2 {
                d[at..at + 2].copy_from_slice(&v.to_le_bytes());
            }
        }
        Edit::Byte(at, v) => {
            if !d.is_empty() {
                let i = usize::from(*at) % d.len();
                d[i] = *v;
            }
        }
        Edit::MessageByte(at, v) => {
            let i = message_at + usize::from(*at) % 208;
            if let Some(b) = d.get_mut(i) {
                *b = *v;
            }
        }
        Edit::PubKey(k) => {
            let at = u16_at(d, 2 + 2 * 2).unwrap_or(0) as usize;
            if let Some(dst) = d.get_mut(at..at + 32) {
                dst.copy_from_slice(k);
            }
        }
        Edit::Truncate(n) => {
            let len = d.len();
            d.truncate(usize::from(*n) % (len + 1));
        }
        Edit::Extend(extra) => d.extend_from_slice(extra),
    }
}

/// Runs the `sigverify` oracle on `data`; panics on any disagreement. Returns the outcome so
/// callers (the corpus coverage test) can count variants.
pub fn check_sigverify(data: &[u8]) -> Result<(), QuotebookError> {
    let got = sigverify::verified_quote(data);
    let want = reference_verified_quote(data);
    match (&got, &want) {
        (Ok(q), Predicted::Ok { message, pubkey }) => {
            assert_eq!(
                &q.encode(),
                message,
                "accepted quote must re-encode to the message"
            );
            assert_eq!(
                q.terms().maker.as_ref(),
                pubkey,
                "maker must be the verified key"
            );
        }
        (Err(g), Predicted::Err(w)) => assert_eq!(g, w, "error disagrees with the reference"),
        _ => panic!("core {got:?} but reference {want:?}"),
    }
    got.map(|_| ())
}
