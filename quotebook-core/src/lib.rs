//! Shared core of quotebook, request-for-quote trading on Solana.
//!
//! `no_std`, no allocator, no floating point and no `unsafe`. Used by the on-chain program and
//! by the off-chain SDK and maker service, so both sides agree on:
//!
//! - the quantity newtypes ([`BaseLots`], [`QuoteLotsPerBaseLot`], [`Nonce`], ...) and
//!   [`Side`];
//! - the program's error codes, [`QuotebookError`].
//!
//! This is milestone 1 of `docs/PLAN.md`.

#![cfg_attr(not(test), no_std)]

mod error;
mod quantity;

pub use error::QuotebookError;
pub use quantity::{
    BaseAtoms, BaseLots, Nonce, QuoteAtoms, QuoteLotsPerBaseLot, SeriesId, Side, UnixTimestamp,
};
pub use solana_address::Address;
