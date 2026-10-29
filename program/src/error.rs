//! Mapping from [`QuotebookError`] to the runtime's `ProgramError`.

use pinocchio::error::ProgramError;
use quotebook_core::QuotebookError;

/// `ProgramError::Custom` with the error's stable code (100 and up).
#[cold]
#[inline(never)]
#[must_use]
pub fn custom(e: QuotebookError) -> ProgramError {
    ProgramError::Custom(e.code())
}
