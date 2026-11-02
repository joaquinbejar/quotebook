//! Maker profiles (ADR 0002, docs/adr/0002-maker-registry.md): the `set_profile` arguments and
//! the canonical-encoding rules the program applies on every write and clients apply on every
//! read.
//!
//! - `endpoint`: 7 to 128 bytes, starting with `wss://`, every byte printable ASCII without a
//!   space (0x21 to 0x7E).
//! - `name`: 0 to 32 bytes, printable ASCII, space allowed (0x20 to 0x7E).
//! - Every byte without data is zero: the header's padding, the profile's padding, and both
//!   text tails.

use crate::accounts::{MakerProfile, VERSION};
use crate::QuotebookError;

/// The longest endpoint, in bytes.
pub const ENDPOINT_MAX: usize = 128;
/// The longest name, in bytes.
pub const NAME_MAX: usize = 32;
/// The required endpoint scheme.
pub const ENDPOINT_SCHEME: &[u8] = b"wss://";

const BAD: QuotebookError = QuotebookError::InvalidProfile;

/// Checks an endpoint and a name against the rules above.
///
/// # Errors
///
/// `InvalidProfile`.
pub fn check_text(endpoint: &[u8], name: &[u8]) -> Result<(), QuotebookError> {
    let endpoint_ok = (ENDPOINT_SCHEME.len().saturating_add(1)..=ENDPOINT_MAX)
        .contains(&endpoint.len())
        && endpoint.starts_with(ENDPOINT_SCHEME)
        && endpoint.iter().all(|b| (0x21..=0x7e).contains(b));
    let name_ok = name.len() <= NAME_MAX && name.iter().all(|b| (0x20..=0x7e).contains(b));
    if endpoint_ok && name_ok {
        Ok(())
    } else {
        Err(BAD)
    }
}

/// Arguments of `set_profile`: `endpoint_len: u8`, the endpoint, `name_len: u8`, the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileArgs {
    endpoint: [u8; ENDPOINT_MAX],
    endpoint_len: u8,
    name: [u8; NAME_MAX],
    name_len: u8,
}

/// An encoded [`ProfileArgs`]: at most `2 + 128 + 32` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodedProfileArgs {
    bytes: [u8; ProfileArgs::MAX_LEN],
    len: usize,
}

impl EncodedProfileArgs {
    /// The encoded bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&self.bytes)
    }
}

impl ProfileArgs {
    /// The longest encoding.
    pub const MAX_LEN: usize = 2 + ENDPOINT_MAX + NAME_MAX;

    /// Validated arguments.
    ///
    /// # Errors
    ///
    /// `InvalidProfile` (see [`check_text`]).
    pub fn new(endpoint: &[u8], name: &[u8]) -> Result<Self, QuotebookError> {
        check_text(endpoint, name)?;
        let mut e = [0u8; ENDPOINT_MAX];
        e.get_mut(..endpoint.len())
            .ok_or(BAD)?
            .copy_from_slice(endpoint);
        let mut n = [0u8; NAME_MAX];
        n.get_mut(..name.len()).ok_or(BAD)?.copy_from_slice(name);
        Ok(Self {
            endpoint: e,
            endpoint_len: u8::try_from(endpoint.len()).map_err(|_| BAD)?,
            name: n,
            name_len: u8::try_from(name.len()).map_err(|_| BAD)?,
        })
    }

    /// The endpoint bytes.
    #[must_use]
    pub fn endpoint(&self) -> &[u8] {
        self.endpoint
            .get(..usize::from(self.endpoint_len))
            .unwrap_or(&[])
    }

    /// The name bytes.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        self.name.get(..usize::from(self.name_len)).unwrap_or(&[])
    }

    /// The encoding.
    #[must_use]
    pub fn encode(&self) -> EncodedProfileArgs {
        let mut bytes = [0u8; Self::MAX_LEN];
        let (e, n) = (self.endpoint(), self.name());
        let mut at = 0usize;
        for part in [&[self.endpoint_len][..], e, &[self.name_len][..], n] {
            let end = at.saturating_add(part.len());
            if let Some(dst) = bytes.get_mut(at..end) {
                dst.copy_from_slice(part);
            }
            at = end;
        }
        EncodedProfileArgs { bytes, len: at }
    }

    /// Decodes and validates `set_profile` arguments: exactly `1 + endpoint_len + 1 +
    /// name_len` bytes.
    ///
    /// # Errors
    ///
    /// `InvalidInstructionData` for a wrong length; `InvalidProfile` for the text rules.
    pub fn decode(args: &[u8]) -> Result<Self, QuotebookError> {
        let data = QuotebookError::InvalidInstructionData;
        let (el, rest) = args.split_first().ok_or(data)?;
        let (endpoint, rest) = rest.split_at_checked(usize::from(*el)).ok_or(data)?;
        let (nl, name) = rest.split_first().ok_or(data)?;
        if name.len() != usize::from(*nl) {
            return Err(data);
        }
        Self::new(endpoint, name)
    }

    /// Writes the arguments into `profile`'s text fields, zeroing the tails.
    pub fn write_to(&self, profile: &mut MakerProfile) {
        profile.endpoint = self.endpoint;
        profile.endpoint_len = self.endpoint_len;
        profile.name = self.name;
        profile.name_len = self.name_len;
        profile._pad = [0; 6];
    }
}

/// Checks that a stored profile is canonical: header padding zero, version current, text rules,
/// zero padding and zero tails.
///
/// # Errors
///
/// `InvalidProfile`.
pub fn check_canonical(p: &MakerProfile) -> Result<(), QuotebookError> {
    let el = usize::from(p.endpoint_len);
    let nl = usize::from(p.name_len);
    let endpoint = p.endpoint.get(..el).ok_or(BAD)?;
    let name = p.name.get(..nl).ok_or(BAD)?;
    check_text(endpoint, name)?;
    let tails_zero = p
        .endpoint
        .get(el..)
        .is_some_and(|t| t.iter().all(|b| *b == 0))
        && p.name.get(nl..).is_some_and(|t| t.iter().all(|b| *b == 0));
    if p.hdr._pad != [0; 5] || p.hdr.version != VERSION || p._pad != [0; 6] || !tails_zero {
        return Err(BAD);
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::disallowed_macros
)]
mod tests {
    use super::{check_canonical, check_text, ProfileArgs, ENDPOINT_MAX, NAME_MAX};
    use crate::accounts::{new_hdr, MakerProfile};
    use crate::QuotebookError;
    use solana_address::Address;

    const BAD: Result<(), QuotebookError> = Err(QuotebookError::InvalidProfile);

    #[test]
    fn test_text_rules_at_their_boundaries() {
        assert_eq!(check_text(b"wss://", b""), BAD); // 6 bytes
        assert_eq!(check_text(b"wss://a", b""), Ok(())); // 7
        let long: Vec<u8> = b"wss://"
            .iter()
            .copied()
            .chain(core::iter::repeat_n(b'a', 122))
            .collect();
        assert_eq!(long.len(), ENDPOINT_MAX);
        assert_eq!(check_text(&long, b""), Ok(()));
        let longer = [long.clone(), b"a".to_vec()].concat();
        assert_eq!(check_text(&longer, b""), BAD); // 129
        assert_eq!(check_text(b"ws://abc", b""), BAD);
        assert_eq!(check_text(b"wss://a b", b""), BAD);
        assert_eq!(check_text(b"wss://a\x01", b""), BAD);
        assert_eq!(check_text(b"wss://a", &[b'n'; NAME_MAX]), Ok(()));
        assert_eq!(check_text(b"wss://a", &[b'n'; NAME_MAX + 1]), BAD);
        assert_eq!(check_text(b"wss://a", b"Desk 7"), Ok(()));
        assert_eq!(check_text(b"wss://a", b"tab\t"), BAD);
    }

    #[test]
    fn test_args_round_trip_and_reject_bad_lengths() {
        let a = ProfileArgs::new(b"wss://maker.example:9000/rfq", b"Example desk").unwrap();
        let enc = a.encode();
        assert_eq!(ProfileArgs::decode(enc.as_slice()), Ok(a));
        let mut short = enc.as_slice().to_vec();
        short.pop();
        assert_eq!(
            ProfileArgs::decode(&short),
            Err(QuotebookError::InvalidInstructionData)
        );
        let mut long = enc.as_slice().to_vec();
        long.push(b'x');
        assert_eq!(
            ProfileArgs::decode(&long),
            Err(QuotebookError::InvalidInstructionData)
        );
        assert_eq!(
            ProfileArgs::decode(&[]),
            Err(QuotebookError::InvalidInstructionData)
        );
    }

    #[test]
    fn test_canonical_profile_checks() {
        let a = ProfileArgs::new(b"wss://m.example", b"desk").unwrap();
        let mut p: MakerProfile = bytemuck::Zeroable::zeroed();
        p.hdr = new_hdr::<MakerProfile>(254);
        p.maker = Address::new_from_array([3; 32]);
        a.write_to(&mut p);
        assert_eq!(check_canonical(&p), Ok(()));
        let mutate = |f: fn(&mut MakerProfile)| {
            let mut q = p;
            f(&mut q);
            check_canonical(&q)
        };
        assert_eq!(mutate(|q| q._pad[0] = 1), BAD);
        assert_eq!(mutate(|q| q.hdr._pad[4] = 1), BAD);
        assert_eq!(mutate(|q| q.endpoint[100] = b'x'), BAD);
        assert_eq!(mutate(|q| q.name[31] = b'x'), BAD);
        assert_eq!(mutate(|q| q.endpoint_len = 200), BAD);
        assert_eq!(mutate(|q| q.name_len = 33), BAD);
    }
}
