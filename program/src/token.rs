//! Token-account checks for `settle` (docs/PLAN.md, section 3.3).
//!
//! Each of the four token accounts must belong to the series' token program, hold the series'
//! mint, be initialized (not frozen) and be owned by the right party. Two Token-2022
//! account-level extensions make a valid-looking `settle` fail inside the token program, so
//! they are rejected up front with a clear error: CPI Guard on the taker's paying account (the
//! taker signs that transfer as owner through a CPI, which CPI Guard forbids) and required
//! incoming memos on a receiving account.

use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};
use quotebook_core::QuotebookError;

use crate::error::custom;
use crate::mint::TOKEN_2022_PROGRAM;
use crate::tlv;

/// Size of the base token `Account` state.
const ACCOUNT_LEN: usize = 165;
/// A Token-2022 `Multisig` has this length.
const MULTISIG_LEN: usize = 355;
/// Offset of `Account::state`.
const STATE: usize = 108;
/// `AccountState::Initialized`.
const INITIALIZED: u8 = 1;
/// `AccountType::Account`.
const ACCOUNT_TYPE_ACCOUNT: u8 = 2;
/// `ExtensionType::MemoTransfer`.
const MEMO_TRANSFER: u16 = 8;
/// `ExtensionType::CpiGuard`.
const CPI_GUARD: u16 = 11;

/// What a token account does in this `settle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Source of a transfer its owner (the taker) signs through the CPI.
    PaysAsOwner,
    /// Source of a transfer the maker's delegate PDA signs.
    PaysAsDelegate,
    /// Destination of a transfer.
    Receives,
}

/// Checks a token account for `settle`.
///
/// # Errors
///
/// - `Immutable` if it is not writable.
/// - `InvalidVaultOrTokenAccount` if another program owns it, its data is not a token account,
///   it holds another mint, another party owns it, or it is not initialized (or frozen).
/// - `AccountExtensionUnsupported` for CPI Guard on a [`Role::PaysAsOwner`] account or
///   required memos on a [`Role::Receives`] account.
#[must_use = "the result carries a possible error"]
pub fn check_account(
    account: &AccountView,
    token_program: &Address,
    mint: &Address,
    owner: &Address,
    role: Role,
) -> ProgramResult {
    if !account.is_writable() {
        return Err(ProgramError::Immutable);
    }
    let bad = || custom(QuotebookError::InvalidVaultOrTokenAccount);
    if !account.owned_by(token_program) {
        return Err(bad());
    }
    let data = account.try_borrow()?;
    if data.len() != ACCOUNT_LEN {
        if *token_program != TOKEN_2022_PROGRAM || data.len() == MULTISIG_LEN {
            return Err(bad());
        }
        let ext = tlv::extensions(&data, ACCOUNT_LEN, ACCOUNT_TYPE_ACCOUNT, bad())?;
        tlv::walk(ext, bad(), |kind, value| {
            let enabled = value.first() == Some(&1);
            match (kind, role) {
                (CPI_GUARD, Role::PaysAsOwner) | (MEMO_TRANSFER, Role::Receives) if enabled => {
                    Err(custom(QuotebookError::AccountExtensionUnsupported))
                }
                _ => Ok(()),
            }
        })?;
    }
    let (account_mint, rest) = data.split_first_chunk::<32>().ok_or_else(bad)?;
    let (account_owner, _) = rest.split_first_chunk::<32>().ok_or_else(bad)?;
    if account_mint != mint.as_array() || account_owner != owner.as_array() {
        return Err(bad());
    }
    if data.get(STATE) != Some(&INITIALIZED) {
        return Err(bad());
    }
    Ok(())
}
