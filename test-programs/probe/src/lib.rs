//! Test-only program for quotebook's fill-event tests (docs/PLAN-improvements.md, I7). Never
//! deployed.
//!
//! Instructions (first byte):
//! - `0`: log the rest of the data with `sol_log_data`, as a spoofing program would.
//! - `1`: call the program at account 0 by CPI with the rest of the data and accounts 1.., with
//!   their signer and writable flags, so quotebook's `settle` runs at depth 2.
//! - `2`: as `1`, then log the rest of the data again in the probe's own frame.

#![no_std]

use pinocchio::cpi::invoke_with_bounds;
use pinocchio::error::ProgramError;
use pinocchio::instruction::{InstructionAccount, InstructionView};
use pinocchio::{AccountView, Address, ProgramResult};

/// The most accounts a forwarded call may take; `settle` takes 13 (14 with a fee account).
const MAX_FORWARDED: usize = 16;

/// The placeholder address of unused meta slots.
static UNUSED: Address = Address::new_from_array([0; 32]);

// The macros expand to the `extern "C"` entrypoint, the allocator and panic hooks: generated
// `unsafe` and asserts, test program only.
#[allow(unsafe_code, clippy::disallowed_macros)]
mod entry {
    pinocchio::program_entrypoint!(super::process_instruction);
    pinocchio::no_allocator!();
    pinocchio::nostd_panic_handler!();
}

/// One byte slice as `sol_log_data` reads it.
#[cfg(target_os = "solana")]
#[repr(C)]
struct SolBytes {
    addr: *const u8,
    len: u64,
}

#[cfg(target_os = "solana")]
#[allow(unsafe_code)]
fn log_data(data: &[u8]) {
    let parts = [SolBytes {
        addr: data.as_ptr(),
        len: u64::try_from(data.len()).unwrap_or(0),
    }];
    // SAFETY: as in quotebook-program's `log::data`: one live `SolBytes` describing `data`,
    // read only during the call.
    unsafe {
        pinocchio::syscalls::sol_log_data(parts.as_ptr().cast::<u8>(), 1);
    }
}

#[cfg(not(target_os = "solana"))]
fn log_data(_data: &[u8]) {}

/// Dispatches one instruction (module docs).
///
/// # Errors
///
/// `InvalidInstructionData` for an unknown tag; `NotEnoughAccountKeys`; the callee's errors.
pub fn process_instruction(
    _program_id: &Address,
    accounts: &mut [AccountView],
    data: &[u8],
) -> ProgramResult {
    let (tag, rest) = data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;
    match tag {
        0 => {
            log_data(rest);
            Ok(())
        }
        1 | 2 => {
            let (callee, forwarded) = accounts
                .split_first()
                .ok_or(ProgramError::NotEnoughAccountKeys)?;
            if forwarded.len() > MAX_FORWARDED {
                return Err(ProgramError::InvalidArgument);
            }
            let mut metas: [InstructionAccount; MAX_FORWARDED] =
                core::array::from_fn(|_| InstructionAccount::readonly(&UNUSED));
            for (meta, a) in metas.iter_mut().zip(forwarded.iter()) {
                *meta = InstructionAccount::new(a.address(), a.is_writable(), a.is_signer());
            }
            let ix = InstructionView {
                program_id: callee.address(),
                data: rest,
                accounts: metas
                    .get(..forwarded.len())
                    .ok_or(ProgramError::InvalidArgument)?,
            };
            invoke_with_bounds::<MAX_FORWARDED, _>(&ix, forwarded)?;
            if *tag == 2 {
                log_data(rest);
            }
            Ok(())
        }
        _ => Err(ProgramError::InvalidInstructionData),
    }
}
