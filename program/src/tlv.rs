//! Token-2022 extension TLV walking, shared by the mint and token-account checks.

use pinocchio::error::ProgramError;
use pinocchio::ProgramResult;

/// `ExtensionType::Uninitialized`: ends the list.
const UNINITIALIZED: u16 = 0;

/// Walks the TLV entries of Token-2022 extension data, calling `f(type, value)` for each, the
/// same way Token-2022 does: fewer than two bytes left or an `Uninitialized` type ends the
/// list, and a header or value running past the data is malformed.
///
/// # Errors
///
/// `malformed` for malformed data; whatever `f` returns.
#[must_use = "the result carries a possible error"]
pub fn walk(
    mut tlv: &[u8],
    malformed: ProgramError,
    mut f: impl FnMut(u16, &[u8]) -> ProgramResult,
) -> ProgramResult {
    while let Some((kind, rest)) = tlv.split_first_chunk::<2>() {
        let kind = u16::from_le_bytes(*kind);
        if kind == UNINITIALIZED {
            break;
        }
        let Some((len, rest)) = rest.split_first_chunk::<2>() else {
            return Err(malformed);
        };
        let len = usize::from(u16::from_le_bytes(*len));
        let Some((value, rest)) = rest.split_at_checked(len) else {
            return Err(malformed);
        };
        f(kind, value)?;
        tlv = rest;
    }
    Ok(())
}

/// Splits an extended Token-2022 account (mint or token account) into its TLV data: checks
/// the zero padding after the base state up to byte 165 and the account type byte.
///
/// # Errors
///
/// `malformed` if the data is shorter than 166 bytes, the padding is not zero or the account
/// type is not `account_type`.
#[must_use = "the result carries a possible error"]
pub fn extensions(
    data: &[u8],
    base_len: usize,
    account_type: u8,
    malformed: ProgramError,
) -> Result<&[u8], ProgramError> {
    /// Token-2022 pads every extended account to the base `Account` size.
    const ACCOUNT_TYPE_OFFSET: usize = 165;
    let Some(padding) = data.get(base_len..ACCOUNT_TYPE_OFFSET) else {
        return Err(malformed);
    };
    if padding.iter().any(|&b| b != 0) {
        return Err(malformed);
    }
    match data
        .get(ACCOUNT_TYPE_OFFSET..)
        .and_then(<[u8]>::split_first)
    {
        Some((t, tlv)) if *t == account_type => Ok(tlv),
        _ => Err(malformed),
    }
}
