//! Instruction handlers and the checks they share.

pub mod admin;
pub mod cancel;
pub mod create_series;
pub mod init_config;
pub mod init_maker;
pub mod settle;

use pinocchio::error::ProgramError;
use pinocchio::{AccountView, ProgramResult};
use quotebook_core::QuotebookError;

use crate::error::custom;

/// `account` signed the transaction.
#[inline]
fn signer(account: &AccountView) -> ProgramResult {
    if account.is_signer() {
        Ok(())
    } else {
        Err(custom(QuotebookError::MissingSignature))
    }
}

/// `account` is writable.
#[inline]
fn writable(account: &AccountView) -> ProgramResult {
    if account.is_writable() {
        Ok(())
    } else {
        Err(ProgramError::Immutable)
    }
}

/// `account` is the system program.
#[inline]
fn system_program(account: &AccountView) -> ProgramResult {
    if *account.address() == pinocchio_system::ID {
        Ok(())
    } else {
        Err(ProgramError::IncorrectProgramId)
    }
}
