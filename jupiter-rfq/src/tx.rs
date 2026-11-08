//! A hand parser and encoder for Solana transactions, legacy and v0
//! (docs/design/jupiter-rfq.md, "Decoding": neither `axum` nor `base64` decodes one, and the
//! toolkit's decoder pulls in Anchor and `solana-sdk`).
//!
//! The wire format: a compact-u16 count of 64-byte signatures, then the message. A v0 message
//! starts with `0x80`; a legacy one starts with its header. The message is the header
//! (`num_required_signatures`, `num_readonly_signed`, `num_readonly_unsigned`), the static
//! account keys, the recent blockhash, the compiled instructions and, for v0 only, the address
//! table lookups.
//!
//! Parsing is strict: canonical compact lengths, no trailing bytes, at most one packet
//! (1,232 bytes), and the sanitize rules a validator applies (signature count, header bounds,
//! unique keys, program and account indexes in range, the fee payer never a program).
//! Lookups are parsed so the length checks hold, and the validator then refuses them.

use solana_address::Address;

/// The most bytes a transaction may have: one packet.
pub const PACKET_DATA_SIZE: usize = 1232;
/// The length of an ed25519 signature.
pub const SIGNATURE_LEN: usize = 64;
/// The version prefix bit of a versioned message.
const VERSION_PREFIX: u8 = 0x80;
/// At most this many accounts in a message, static and loaded.
const MAX_ACCOUNTS: usize = 256;

/// A message's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// No prefix.
    Legacy,
    /// Prefix `0x80`.
    V0,
}

/// The message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Signatures the transaction needs; the first keys are the signers.
    pub num_required_signatures: u8,
    /// The last signers that are read-only.
    pub num_readonly_signed: u8,
    /// The last static keys that are read-only.
    pub num_readonly_unsigned: u8,
}

/// One instruction, with indexes into the account keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledInstruction {
    /// Index of the program's key.
    pub program_index: u8,
    /// Indexes of the instruction's accounts.
    pub accounts: Vec<u8>,
    /// The instruction data.
    pub data: Vec<u8>,
}

/// A v0 address table lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupTable {
    /// The table account.
    pub table: Address,
    /// Indexes loaded as writable.
    pub writable: Vec<u8>,
    /// Indexes loaded as read-only.
    pub readonly: Vec<u8>,
}

/// A decoded message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Legacy or v0.
    pub version: Version,
    /// The header.
    pub header: Header,
    /// The static account keys; the first is the fee payer.
    pub account_keys: Vec<Address>,
    /// The recent blockhash.
    pub recent_blockhash: [u8; 32],
    /// The instructions, in order.
    pub instructions: Vec<CompiledInstruction>,
    /// v0 lookups; always empty for a legacy message.
    pub lookups: Vec<LookupTable>,
}

/// A decoded transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    /// The signatures, one per required signer, in key order; all zeros where unsigned.
    pub signatures: Vec<[u8; SIGNATURE_LEN]>,
    /// The message.
    pub message: Message,
    /// The message's bytes as received: what each signer signs.
    pub message_bytes: Vec<u8>,
}

/// Why bytes are not a transaction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// Not base64.
    #[error("transaction is not valid base64")]
    Base64,
    /// Above one packet.
    #[error("transaction is {0} bytes, above the 1232-byte packet")]
    TooLarge(usize),
    /// The bytes end inside a field.
    #[error("transaction ends early")]
    Truncated,
    /// A compact length is not canonical or exceeds `u16`.
    #[error("compact length is not canonical or exceeds u16")]
    BadLength,
    /// A version other than legacy and v0.
    #[error("unsupported message version {0}")]
    Version(u8),
    /// Bytes after the message.
    #[error("{0} trailing bytes after the message")]
    Trailing(usize),
    /// The signature count differs from the header.
    #[error("{signatures} signatures for {required} required")]
    SignatureCount {
        /// Signatures present.
        signatures: usize,
        /// Signatures the header requires.
        required: u8,
    },
    /// The header contradicts the keys.
    #[error("message header is inconsistent: {0}")]
    Header(&'static str),
    /// A static key appears twice.
    #[error("account key {0} appears twice")]
    DuplicateKey(Address),
    /// An instruction's program index is the fee payer or out of range.
    #[error("instruction {0}: program index is the fee payer or out of range")]
    ProgramIndex(usize),
    /// An instruction's account index is out of range.
    #[error("instruction {0}: account index out of range")]
    AccountIndex(usize),
    /// A lookup loads no address.
    #[error("lookup {0} loads no address")]
    EmptyLookup(usize),
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ParseError> {
        let end = self.pos.checked_add(n).ok_or(ParseError::Truncated)?;
        let slice = self.bytes.get(self.pos..end).ok_or(ParseError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ParseError> {
        self.take(1)?.first().copied().ok_or(ParseError::Truncated)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ParseError> {
        self.take(N)?.try_into().map_err(|_| ParseError::Truncated)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// A compact-u16: 7 bits per byte, low first, at most 3 bytes, canonical.
    fn short_len(&mut self) -> Result<usize, ParseError> {
        let mut value: u32 = 0;
        for (nth, shift) in [0u32, 7, 14].into_iter().enumerate() {
            let byte = self.u8()?;
            // A zero continuation byte encodes nothing: the shorter form is the canonical one.
            if nth > 0 && byte == 0 {
                return Err(ParseError::BadLength);
            }
            let part = u32::from(byte & 0x7f)
                .checked_shl(shift)
                .ok_or(ParseError::BadLength)?;
            value |= part;
            if byte & 0x80 == 0 {
                if value > u32::from(u16::MAX) {
                    return Err(ParseError::BadLength);
                }
                return usize::try_from(value).map_err(|_| ParseError::BadLength);
            }
        }
        Err(ParseError::BadLength)
    }

    fn bytes_vec(&mut self) -> Result<Vec<u8>, ParseError> {
        let n = self.short_len()?;
        Ok(self.take(n)?.to_vec())
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }
}

fn write_short_len(out: &mut Vec<u8>, len: usize) {
    let mut rest = len;
    loop {
        let low = u8::try_from(rest & 0x7f).unwrap_or(0);
        rest = rest.checked_shr(7).unwrap_or(0);
        if rest == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

impl Transaction {
    /// Decodes standard base64.
    ///
    /// # Errors
    ///
    /// `Base64`, `TooLarge`, or any error of [`Transaction::parse`].
    pub fn from_base64(text: &str) -> Result<Self, ParseError> {
        use base64::Engine as _;
        // Four base64 characters per three bytes: anything longer cannot fit a packet.
        let max_chars = PACKET_DATA_SIZE
            .checked_add(2)
            .and_then(|n| n.checked_div(3))
            .and_then(|n| n.checked_mul(4))
            .unwrap_or(0);
        if text.len() > max_chars {
            return Err(ParseError::TooLarge(text.len()));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(text)
            .map_err(|_| ParseError::Base64)?;
        Self::parse(&bytes)
    }

    /// Decodes and sanitizes a serialized transaction.
    ///
    /// # Errors
    ///
    /// See [`ParseError`].
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        if bytes.len() > PACKET_DATA_SIZE {
            return Err(ParseError::TooLarge(bytes.len()));
        }
        let mut r = Reader { bytes, pos: 0 };
        let n = r.short_len()?;
        let mut signatures = Vec::with_capacity(n.min(16));
        for _ in 0..n {
            signatures.push(r.array::<SIGNATURE_LEN>()?);
        }
        let start = r.pos;
        let message = Message::read(&mut r)?;
        let message_bytes = bytes
            .get(start..r.pos)
            .ok_or(ParseError::Truncated)?
            .to_vec();
        if r.remaining() != 0 {
            return Err(ParseError::Trailing(r.remaining()));
        }
        if signatures.len() != usize::from(message.header.num_required_signatures) {
            return Err(ParseError::SignatureCount {
                signatures: signatures.len(),
                required: message.header.num_required_signatures,
            });
        }
        message.sanitize()?;
        Ok(Self {
            signatures,
            message,
            message_bytes,
        })
    }

    /// The wire bytes: the signatures, then the message.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        write_short_len(&mut out, self.signatures.len());
        for s in &self.signatures {
            out.extend_from_slice(s);
        }
        out.extend_from_slice(&self.message.serialize());
        out
    }

    /// The wire bytes in standard base64.
    #[must_use]
    pub fn to_base64(&self) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(self.serialize())
    }
}

impl Message {
    fn read(r: &mut Reader<'_>) -> Result<Self, ParseError> {
        let version = match r.peek() {
            Some(b) if b & VERSION_PREFIX != 0 => {
                r.u8()?;
                let v = b & !VERSION_PREFIX;
                if v != 0 {
                    return Err(ParseError::Version(v));
                }
                Version::V0
            }
            Some(_) => Version::Legacy,
            None => return Err(ParseError::Truncated),
        };
        let header = Header {
            num_required_signatures: r.u8()?,
            num_readonly_signed: r.u8()?,
            num_readonly_unsigned: r.u8()?,
        };
        let n_keys = r.short_len()?;
        let mut account_keys = Vec::with_capacity(n_keys.min(MAX_ACCOUNTS));
        for _ in 0..n_keys {
            account_keys.push(Address::new_from_array(r.array::<32>()?));
        }
        let recent_blockhash = r.array::<32>()?;
        let n_ix = r.short_len()?;
        let mut instructions = Vec::with_capacity(n_ix.min(64));
        for _ in 0..n_ix {
            let program_index = r.u8()?;
            let accounts = r.bytes_vec()?;
            let data = r.bytes_vec()?;
            instructions.push(CompiledInstruction {
                program_index,
                accounts,
                data,
            });
        }
        let mut lookups = Vec::new();
        if version == Version::V0 {
            let n = r.short_len()?;
            for _ in 0..n {
                let table = Address::new_from_array(r.array::<32>()?);
                let writable = r.bytes_vec()?;
                let readonly = r.bytes_vec()?;
                lookups.push(LookupTable {
                    table,
                    writable,
                    readonly,
                });
            }
        }
        Ok(Self {
            version,
            header,
            account_keys,
            recent_blockhash,
            instructions,
            lookups,
        })
    }

    /// The addresses lookups would load.
    #[must_use]
    pub fn loaded_count(&self) -> usize {
        self.lookups.iter().fold(0usize, |n, l| {
            n.saturating_add(l.writable.len())
                .saturating_add(l.readonly.len())
        })
    }

    fn sanitize(&self) -> Result<(), ParseError> {
        let h = self.header;
        let keys = self.account_keys.len();
        if h.num_required_signatures == 0 {
            return Err(ParseError::Header("no signer"));
        }
        if usize::from(h.num_required_signatures) > keys {
            return Err(ParseError::Header("more signers than keys"));
        }
        if h.num_readonly_signed >= h.num_required_signatures {
            return Err(ParseError::Header("the fee payer must be writable"));
        }
        let unsigned = keys.saturating_sub(usize::from(h.num_required_signatures));
        if usize::from(h.num_readonly_unsigned) > unsigned {
            return Err(ParseError::Header(
                "more read-only unsigned keys than unsigned keys",
            ));
        }
        let total = keys.saturating_add(self.loaded_count());
        if total > MAX_ACCOUNTS {
            return Err(ParseError::Header("more than 256 accounts"));
        }
        for (i, key) in self.account_keys.iter().enumerate() {
            if self
                .account_keys
                .iter()
                .skip(i.saturating_add(1))
                .any(|k| k == key)
            {
                return Err(ParseError::DuplicateKey(*key));
            }
        }
        for (i, lookup) in self.lookups.iter().enumerate() {
            if lookup.writable.is_empty() && lookup.readonly.is_empty() {
                return Err(ParseError::EmptyLookup(i));
            }
        }
        for (i, ix) in self.instructions.iter().enumerate() {
            let p = usize::from(ix.program_index);
            if p == 0 || p >= keys {
                return Err(ParseError::ProgramIndex(i));
            }
            if ix.accounts.iter().any(|a| usize::from(*a) >= total) {
                return Err(ParseError::AccountIndex(i));
            }
        }
        Ok(())
    }

    /// The static key at `index`, if any. Indexes into lookups give `None`.
    #[must_use]
    pub fn key(&self, index: u8) -> Option<&Address> {
        self.account_keys.get(usize::from(index))
    }

    /// Whether the key at `index` signs.
    #[must_use]
    pub fn is_signer(&self, index: usize) -> bool {
        index < usize::from(self.header.num_required_signatures)
    }

    /// The fee payer, the first key.
    #[must_use]
    pub fn fee_payer(&self) -> Option<&Address> {
        self.account_keys.first()
    }

    /// The message's wire bytes.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.version == Version::V0 {
            out.push(VERSION_PREFIX);
        }
        out.push(self.header.num_required_signatures);
        out.push(self.header.num_readonly_signed);
        out.push(self.header.num_readonly_unsigned);
        write_short_len(&mut out, self.account_keys.len());
        for k in &self.account_keys {
            out.extend_from_slice(k.as_ref());
        }
        out.extend_from_slice(&self.recent_blockhash);
        write_short_len(&mut out, self.instructions.len());
        for ix in &self.instructions {
            out.push(ix.program_index);
            write_short_len(&mut out, ix.accounts.len());
            out.extend_from_slice(&ix.accounts);
            write_short_len(&mut out, ix.data.len());
            out.extend_from_slice(&ix.data);
        }
        if self.version == Version::V0 {
            write_short_len(&mut out, self.lookups.len());
            for l in &self.lookups {
                out.extend_from_slice(l.table.as_ref());
                write_short_len(&mut out, l.writable.len());
                out.extend_from_slice(&l.writable);
                write_short_len(&mut out, l.readonly.len());
                out.extend_from_slice(&l.readonly);
            }
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::disallowed_macros)]
mod tests {
    use super::*;

    fn short(bytes: &[u8]) -> Result<usize, ParseError> {
        Reader { bytes, pos: 0 }.short_len()
    }

    #[test]
    fn compact_lengths_are_strict() {
        assert_eq!(short(&[0x00]), Ok(0));
        assert_eq!(short(&[0x7f]), Ok(127));
        assert_eq!(short(&[0x80, 0x01]), Ok(128));
        assert_eq!(short(&[0xff, 0xff, 0x03]), Ok(65_535));
        // Above u16, an alias with a zero continuation byte, and a fourth byte.
        assert_eq!(short(&[0x80, 0x80, 0x04]), Err(ParseError::BadLength));
        assert_eq!(short(&[0x80, 0x00]), Err(ParseError::BadLength));
        assert_eq!(short(&[0x81, 0x80, 0x80]), Err(ParseError::BadLength));
        assert_eq!(short(&[0x80]), Err(ParseError::Truncated));
        for n in [0usize, 1, 127, 128, 300, 16_383, 16_384, 65_535] {
            let mut out = Vec::new();
            write_short_len(&mut out, n);
            assert_eq!(short(&out), Ok(n));
        }
    }
}
