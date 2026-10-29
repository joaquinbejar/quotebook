//! quotebook on-chain program: request-for-quote settlement of ed25519-signed quotes.
//!
//! Pinocchio, `no_std`, no allocator. This is milestone 2 of `docs/PLAN.md`: the accounts
//! (`Config`, `Series`, `MakerState`) and the instructions `init_config`, `create_series`,
//! `init_maker`, `cancel_all` and `cancel_nonce`. `settle` follows in milestone 3.

#![no_std]

pub mod error;
pub mod instruction;
pub mod loader;
pub mod mint;
pub mod pda;
pub mod processor;
pub mod state;

use pinocchio::{AccountView, Address, ProgramResult};

use crate::instruction::Tag;
use crate::processor::{cancel, create_series, init_config, init_maker};

// The macros expand to the `extern "C"` entrypoint and the target's allocator and panic hooks;
// that generated code is the only `unsafe` in this crate. `no_allocator!` also defines
// `allocate_unchecked`, whose offset helper asserts its bounds; nothing here calls it.
#[allow(unsafe_code, clippy::disallowed_macros)]
mod entry {
    pinocchio::program_entrypoint!(super::process_instruction);
    pinocchio::no_allocator!();
    pinocchio::nostd_panic_handler!();
}

/// Dispatches one instruction.
///
/// # Errors
///
/// `InvalidInstruction` for an unknown tag; otherwise the handler's errors.
pub fn process_instruction(
    program_id: &Address,
    accounts: &mut [AccountView],
    data: &[u8],
) -> ProgramResult {
    let (tag, args) = Tag::split(data)?;
    match tag {
        Tag::InitConfig => init_config::process(program_id, accounts, args),
        Tag::CreateSeries => create_series::process(program_id, accounts, args),
        Tag::InitMaker => init_maker::process(program_id, accounts, args),
        Tag::CancelAll => cancel::process_cancel_all(program_id, accounts, args),
        Tag::CancelNonce => cancel::process_cancel_nonce(program_id, accounts, args),
    }
}
