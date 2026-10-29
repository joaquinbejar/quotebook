//! Mint validation for `create_series` (docs/PLAN.md, section 3.3).
//!
//! A mint must be an initialized SPL Token or Token-2022 mint. Token-2022 extensions are
//! checked against an allowlist: extensions that only change metadata or display pass, and
//! everything that changes what a transfer does or who can move funds is rejected, as is any
//! extension type this program does not know.

use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address};
use quotebook_core::QuotebookError;

use crate::error::custom;

/// SPL Token.
pub const TOKEN_PROGRAM: Address =
    solana_address::address!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Token-2022.
pub const TOKEN_2022_PROGRAM: Address =
    solana_address::address!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// Size of the base `Mint` state.
const MINT_LEN: usize = 82;
/// Offset of `Mint::is_initialized`.
const MINT_INITIALIZED: usize = 45;
/// Token-2022 pads every extended account to the base `Account` size, then the account type.
const ACCOUNT_TYPE_OFFSET: usize = 165;
/// `AccountType::Mint`.
const ACCOUNT_TYPE_MINT: u8 = 1;
/// A Token-2022 `Multisig` has this length and is never a mint.
const MULTISIG_LEN: usize = 355;
/// `AccountState::Frozen`, the value of a `DefaultAccountState` extension that is rejected.
const ACCOUNT_STATE_FROZEN: u8 = 2;

/// Token-2022 `ExtensionType` values (spl-token-2022-interface 3.1.2, `extension/mod.rs`).
mod ext {
    pub const UNINITIALIZED: u16 = 0;
    pub const MINT_CLOSE_AUTHORITY: u16 = 3;
    pub const DEFAULT_ACCOUNT_STATE: u16 = 6;
    pub const INTEREST_BEARING_CONFIG: u16 = 10;
    pub const METADATA_POINTER: u16 = 18;
    pub const TOKEN_METADATA: u16 = 19;
    pub const GROUP_POINTER: u16 = 20;
    pub const TOKEN_GROUP: u16 = 21;
    pub const GROUP_MEMBER_POINTER: u16 = 22;
    pub const TOKEN_GROUP_MEMBER: u16 = 23;
    pub const SCALED_UI_AMOUNT: u16 = 25;
}

/// Validates `mint` and returns whether it is a Token-2022 mint.
///
/// # Errors
///
/// - `InvalidMint` if the owner is neither token program, the data is not an initialized mint
///   or the Token-2022 extension data is malformed.
/// - `MintExtensionUnsupported` for a frozen `DefaultAccountState` or any extension outside
///   the allowlist: transfer fee, transfer hook, confidential transfer, permanent delegate,
///   non-transferable, pausable, permissioned burn, account-only or unknown types.
#[must_use = "the result carries a possible error"]
pub fn validate(mint: &AccountView) -> Result<bool, ProgramError> {
    let is_2022 = if mint.owned_by(&TOKEN_PROGRAM) {
        false
    } else if mint.owned_by(&TOKEN_2022_PROGRAM) {
        true
    } else {
        return Err(custom(QuotebookError::InvalidMint));
    };
    let data = mint.try_borrow()?;
    if data.get(MINT_INITIALIZED) != Some(&1) {
        return Err(custom(QuotebookError::InvalidMint));
    }
    if data.len() == MINT_LEN {
        return Ok(is_2022);
    }
    if !is_2022 || data.len() == MULTISIG_LEN {
        return Err(custom(QuotebookError::InvalidMint));
    }
    check_extensions(&data)?;
    Ok(true)
}

/// Checks the padding, account type and TLV extensions of an extended Token-2022 mint.
fn check_extensions(data: &[u8]) -> Result<(), ProgramError> {
    let malformed = || custom(QuotebookError::InvalidMint);
    let padding = data
        .get(MINT_LEN..ACCOUNT_TYPE_OFFSET)
        .ok_or_else(malformed)?;
    if padding.iter().any(|&b| b != 0) {
        return Err(malformed());
    }
    let (account_type, mut tlv) = data
        .get(ACCOUNT_TYPE_OFFSET..)
        .and_then(<[u8]>::split_first)
        .ok_or_else(malformed)?;
    if *account_type != ACCOUNT_TYPE_MINT {
        return Err(malformed());
    }
    // Same walk as Token-2022: fewer than two bytes left or an `Uninitialized` type ends the
    // list; a header or value that runs past the data is malformed.
    while let Some((kind, rest)) = tlv.split_first_chunk::<2>() {
        let kind = u16::from_le_bytes(*kind);
        if kind == ext::UNINITIALIZED {
            break;
        }
        let (len, rest) = rest.split_first_chunk::<2>().ok_or_else(malformed)?;
        let len = usize::from(u16::from_le_bytes(*len));
        let (value, rest) = rest.split_at_checked(len).ok_or_else(malformed)?;
        check_extension(kind, value)?;
        tlv = rest;
    }
    Ok(())
}

fn check_extension(kind: u16, value: &[u8]) -> Result<(), ProgramError> {
    match kind {
        ext::DEFAULT_ACCOUNT_STATE => match value {
            [state] if *state != ACCOUNT_STATE_FROZEN => Ok(()),
            [_] => Err(custom(QuotebookError::MintExtensionUnsupported)),
            _ => Err(custom(QuotebookError::InvalidMint)),
        },
        ext::MINT_CLOSE_AUTHORITY
        | ext::INTEREST_BEARING_CONFIG
        | ext::METADATA_POINTER
        | ext::TOKEN_METADATA
        | ext::GROUP_POINTER
        | ext::TOKEN_GROUP
        | ext::GROUP_MEMBER_POINTER
        | ext::TOKEN_GROUP_MEMBER
        | ext::SCALED_UI_AMOUNT => Ok(()),
        _ => Err(custom(QuotebookError::MintExtensionUnsupported)),
    }
}
