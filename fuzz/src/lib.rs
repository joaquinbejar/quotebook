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

// ---------------------------------------------------------------------------------------
// quote_decode: structured inputs reach the accepting path and every later quote rule.

/// A `quote_decode` input: raw bytes, or a valid encoding from arbitrary terms plus edits.
#[derive(Debug, Arbitrary)]
pub enum QuoteInput {
    Raw(Vec<u8>),
    Structured { terms: ArbTerms, edits: Vec<Edit> },
}

impl QuoteInput {
    /// The bytes this input describes. Edits apply as on a precompile record, with the whole
    /// buffer as the "message" (`MessageByte` and `Byte` both hit the quote).
    pub fn bytes(&self) -> Vec<u8> {
        match self {
            QuoteInput::Raw(b) => b.clone(),
            QuoteInput::Structured { terms, edits } => {
                let mut d = terms.quote().encode().to_vec();
                for e in edits {
                    match e {
                        // Count and Field edits (precompile-specific) become byte edits.
                        Edit::Count(v) => apply(&mut d, 0, &Edit::Byte(160, *v)),
                        Edit::Field(i, v) => {
                            apply(&mut d, 0, &Edit::Byte(u16::from(*i), (*v & 0xff) as u8))
                        }
                        Edit::PubKey(_) => {}
                        other => apply(&mut d, 0, other),
                    }
                }
                d
            }
        }
    }
}

/// The `quote_decode` oracle; panics on disagreement. Returns the outcome name for coverage.
pub fn check_quote_decode(data: &[u8]) -> String {
    let got = Quote::decode(data);
    match (&got, reference_quote_error(data)) {
        (Ok(q), None) => assert_eq!(&q.encode()[..], data, "decode must be canonical"),
        (Err(g), Some(w)) => assert_eq!(*g, w, "error disagrees with the reference"),
        (g, w) => panic!("core {g:?} but reference {w:?}"),
    }
    match got {
        Ok(_) => "Ok".to_string(),
        Err(e) => format!("{e:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// accounts_decode: structured inputs build valid layouts of all four account types.

/// An `accounts_decode` input: raw bytes, or a layout with the right length and header for
/// one of the four account types plus edits. Config, Series and MakerState bodies are
/// arbitrary; a MakerProfile body is built canonical (see [`canonical_profile_body`]) so the
/// edits reach every `profile::check_canonical` rule from an accepting start.
#[derive(Debug, Arbitrary)]
pub enum AccountInput {
    Raw(Vec<u8>),
    Structured {
        kind: u8,
        bump: u8,
        body: Vec<u8>,
        edits: Vec<Edit>,
    },
}

impl AccountInput {
    pub fn bytes(&self) -> Vec<u8> {
        use quotebook_core::accounts::{
            AccountTag, Config, MakerProfile, MakerState, ProgramAccount, Series, VERSION,
        };
        match self {
            AccountInput::Raw(b) => b.clone(),
            AccountInput::Structured {
                kind,
                bump,
                body,
                edits,
            } => {
                let (tag, len) = match kind % 4 {
                    0 => (AccountTag::Config as u8, Config::LEN),
                    1 => (AccountTag::Series as u8, Series::LEN),
                    2 => (AccountTag::MakerState as u8, MakerState::LEN),
                    _ => (AccountTag::MakerProfile as u8, MakerProfile::LEN),
                };
                let mut d = vec![0u8; len];
                d[0] = tag;
                d[1] = VERSION;
                d[2] = *bump;
                if tag == AccountTag::MakerProfile as u8 {
                    canonical_profile_body(&mut d, body);
                } else {
                    for (i, b) in d.iter_mut().skip(8).enumerate() {
                        if !body.is_empty() {
                            *b = body[i % body.len()];
                        }
                    }
                }
                for e in edits {
                    apply(&mut d, 8, e);
                }
                d
            }
        }
    }
}

/// The `accounts_decode` oracle for one type; panics on disagreement. Returns whether it
/// accepted.
fn check_account<T: quotebook_core::accounts::ProgramAccount + bytemuck::Pod>(data: &[u8]) -> bool {
    use quotebook_core::accounts::{self, VERSION};
    let ok = data.len() == T::LEN && data[0] == T::TAG as u8 && data[1] == VERSION;
    let got = accounts::decode::<T>(data);
    assert_eq!(got.is_some(), ok, "accepts exactly length, tag and version");
    if let Some(v) = got {
        assert_eq!(bytemuck::bytes_of(&v), data, "re-encodes to the same bytes");
    }
    got.is_some()
}

// MakerProfile layout (ADR 0002): hdr 0..8 (tag, version, bump, 5 zero bytes), maker 8..40,
// endpoint_len 40, name_len 41, zero padding 42..48, endpoint 48..176, name 176..208.
const P_EL: usize = 40;
const P_NL: usize = 41;
const P_PAD: core::ops::Range<usize> = 42..48;
const P_ENDPOINT: usize = 48;
const P_NAME: usize = 176;

/// Fills a profile body canonically from arbitrary bytes: a maker, an endpoint `wss://` plus
/// 1 to 122 bytes of 0x21..=0x7E, a name of 0 to 32 bytes of 0x20..=0x7E, zero elsewhere.
fn canonical_profile_body(d: &mut [u8], body: &[u8]) {
    let at = |i: usize| body.get(i).copied().unwrap_or(0);
    for i in 0..32 {
        d[8 + i] = at(i);
    }
    let el = 7 + usize::from(at(32)) % 122;
    let nl = usize::from(at(33)) % 33;
    d[P_EL] = el as u8;
    d[P_NL] = nl as u8;
    d[P_ENDPOINT..P_ENDPOINT + 6].copy_from_slice(b"wss://");
    for i in 6..el {
        d[P_ENDPOINT + i] = 0x21 + at(34 + i) % 94;
    }
    for i in 0..nl {
        d[P_NAME + i] = 0x20 + at(200 + i) % 95;
    }
}

/// Reference text rules (ADR 0002), written from the ADR rather than `profile::check_text`.
pub fn reference_profile_text(endpoint: &[u8], name: &[u8]) -> bool {
    (7..=128).contains(&endpoint.len())
        && endpoint.starts_with(b"wss://")
        && endpoint.iter().all(|b| (0x21..=0x7e).contains(b))
        && name.len() <= 32
        && name.iter().all(|b| (0x20..=0x7e).contains(b))
}

/// Reference canonical rule for a decoded 208-byte profile.
pub fn reference_profile_canonical(d: &[u8]) -> bool {
    let el = usize::from(d[P_EL]);
    let nl = usize::from(d[P_NL]);
    if el > 128 || nl > 32 {
        return false;
    }
    let endpoint = &d[P_ENDPOINT..P_ENDPOINT + 128];
    let name = &d[P_NAME..P_NAME + 32];
    d[3..8].iter().all(|b| *b == 0)
        && d[P_PAD].iter().all(|b| *b == 0)
        && reference_profile_text(&endpoint[..el], &name[..nl])
        && endpoint[el..].iter().all(|b| *b == 0)
        && name[nl..].iter().all(|b| *b == 0)
}

/// Runs the oracle for all four types, and for an accepted profile the canonical rule.
/// Returns the accepted type's name ("MakerProfileNonCanonical" for a profile that decodes
/// but is not canonical), or "None".
pub fn check_accounts(data: &[u8]) -> &'static str {
    use quotebook_core::accounts::{Config, MakerProfile, MakerState, Series};
    use quotebook_core::profile::check_canonical;
    let c = check_account::<Config>(data);
    let s = check_account::<Series>(data);
    let m = check_account::<MakerState>(data);
    let p = check_account::<MakerProfile>(data);
    match (c, s, m, p) {
        (true, false, false, false) => "Config",
        (false, true, false, false) => "Series",
        (false, false, true, false) => "MakerState",
        (false, false, false, true) => {
            let profile: MakerProfile = bytemuck::pod_read_unaligned(data);
            let got = check_canonical(&profile);
            let want = reference_profile_canonical(data);
            assert_eq!(got.is_ok(), want, "check_canonical disagrees with the reference");
            if let Err(e) = got {
                assert_eq!(e, QuotebookError::InvalidProfile);
                "MakerProfileNonCanonical"
            } else {
                "MakerProfile"
            }
        }
        (false, false, false, false) => "None",
        _ => panic!("one layout decoded as two account types"),
    }
}

// ---------------------------------------------------------------------------------------
// instruction_args: set_profile arguments.

/// Predicts `ProfileArgs::decode(data)`: `endpoint_len`, the endpoint, `name_len`, the name,
/// exactly; then the text rules.
pub fn reference_profile_args(data: &[u8]) -> Result<(), QuotebookError> {
    let bad_len = Err(QuotebookError::InvalidInstructionData);
    let Some(&el) = data.first() else {
        return bad_len;
    };
    let el = usize::from(el);
    let Some(&nl) = data.get(1 + el) else {
        return bad_len;
    };
    let nl = usize::from(nl);
    if data.len() != 2 + el + nl {
        return bad_len;
    }
    if reference_profile_text(&data[1..1 + el], &data[2 + el..]) {
        Ok(())
    } else {
        Err(QuotebookError::InvalidProfile)
    }
}
