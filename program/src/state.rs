//! Program accounts: `Config`, `Series` and `MakerState` (docs/PLAN.md, section 3.1).
//!
//! Every account starts with an 8-byte [`Hdr`] and is `Pod`, loaded zero-copy. The only way
//! from account data to a typed account is [`load`] / [`load_mut`], which check the owner, the
//! exact length, the tag and the version before the cast.

use bytemuck::Pod;
use pinocchio::account::{Ref, RefMut};
use pinocchio::error::ProgramError;
use pinocchio::{AccountView, Address, ProgramResult};
use quotebook_core::QuotebookError;

use crate::error::custom;

pub use layout::{Config, Hdr, MakerState, Series};

/// Layout version written into every header.
pub const VERSION: u8 = 1;

/// PDA seed of [`Config`]: `["config"]`.
pub const CONFIG_SEED: &[u8] = b"config";
/// PDA seed prefix of [`Series`]: `["series", base_mint, quote_mint, series_index_le]`.
pub const SERIES_SEED: &[u8] = b"series";
/// PDA seed prefix of [`MakerState`]: `["maker", maker]`.
pub const MAKER_SEED: &[u8] = b"maker";
/// PDA seed prefix of a maker's delegate authority: `["delegate", maker]`.
pub const DELEGATE_SEED: &[u8] = b"delegate";

/// Account discriminators, the first header byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AccountTag {
    /// [`Config`].
    Config = 1,
    /// [`Series`].
    Series = 2,
    /// [`MakerState`].
    MakerState = 3,
}

/// Pod account layouts. `derive(Pod)` on a multi-field struct emits a compile-time
/// `const _: () = assert!(..)`, so the macro ban is lifted in this declaration-only module
/// (rules/global_rules.md). No logic here.
#[allow(clippy::disallowed_macros)]
mod layout {
    use bytemuck::{Pod, Zeroable};
    use pinocchio::Address;
    use quotebook_core::{BaseAtoms, BaseLots, NonceWindow, QuoteAtoms, Seconds};

    /// The 8-byte header of every program account.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
    #[repr(C)]
    pub struct Hdr {
        /// [`AccountTag`](super::AccountTag) discriminant.
        pub tag: u8,
        /// Layout version, [`VERSION`](super::VERSION).
        pub version: u8,
        /// The canonical bump of the account's PDA.
        pub bump: u8,
        /// Zero.
        pub _pad: [u8; 5],
    }

    /// Program-wide configuration, PDA `["config"]`. 64 bytes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
    #[repr(C)]
    pub struct Config {
        /// Header, tag `Config`.
        pub hdr: Hdr,
        /// The authority that creates series.
        pub admin: Address,
        /// Number of series created; the next series' index.
        pub series_count: u64,
        /// First 16 bytes of the cluster's genesis hash, set once by `init_config`.
        pub deployment_id: [u8; 16],
    }

    /// A tradable pair, PDA `["series", base_mint, quote_mint, series_index_le]`. 144 bytes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
    #[repr(C)]
    pub struct Series {
        /// Header, tag `Series`.
        pub hdr: Hdr,
        /// `Config.series_count` at creation; part of the PDA seeds.
        pub series_index: u64,
        /// Copy of `Config.deployment_id`; `settle` compares quotes against it.
        pub deployment_id: [u8; 16],
        /// The base mint.
        pub base_mint: Address,
        /// The quote mint.
        pub quote_mint: Address,
        /// Base atoms per base lot; non-zero.
        pub base_lot_size: BaseAtoms,
        /// Quote atoms per quote lot; non-zero.
        pub quote_lot_size: QuoteAtoms,
        /// Longest allowed `expires_at - issued_at`; positive.
        pub max_quote_ttl: Seconds,
        /// Smallest fill, in base lots; at least 1.
        pub min_size: BaseLots,
        /// 1 while paused.
        pub paused: u8,
        /// 1 if the base mint is a Token-2022 mint.
        pub base_is_2022: u8,
        /// 1 if the quote mint is a Token-2022 mint.
        pub quote_is_2022: u8,
        /// Zero.
        pub _pad: [u8; 5],
        /// Reserved for fees; zero.
        pub _reserved: [u8; 8],
    }

    /// A maker's replay state, PDA `["maker", maker]`. 2,120 bytes. Permanent: there is no
    /// close instruction in v1 (docs/PLAN.md, section 3.2).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
    #[repr(C)]
    pub struct MakerState {
        /// Header, tag `MakerState`.
        pub hdr: Hdr,
        /// The maker's wallet.
        pub maker: Address,
        /// Number of fills, for clients.
        pub fills: u64,
        /// Canonical bump of the maker's `["delegate", maker]` PDA.
        pub delegate_bump: u8,
        /// Zero.
        pub _pad: [u8; 7],
        /// The nonce replay window.
        pub nonces: NonceWindow,
    }
}

/// A program account type: its tag and exact size.
pub trait ProgramAccount: Pod {
    /// The header tag.
    const TAG: AccountTag;
    /// Size in bytes.
    const LEN: usize;
    /// The header.
    fn hdr(&self) -> &Hdr;
}

impl ProgramAccount for Config {
    const TAG: AccountTag = AccountTag::Config;
    const LEN: usize = 64;
    #[inline]
    fn hdr(&self) -> &Hdr {
        &self.hdr
    }
}

impl ProgramAccount for Series {
    const TAG: AccountTag = AccountTag::Series;
    const LEN: usize = 144;
    #[inline]
    fn hdr(&self) -> &Hdr {
        &self.hdr
    }
}

impl ProgramAccount for MakerState {
    const TAG: AccountTag = AccountTag::MakerState;
    const LEN: usize = 2_120;
    #[inline]
    fn hdr(&self) -> &Hdr {
        &self.hdr
    }
}

// Compile-time checks that each layout is exactly its documented size.
const _: [(); Config::LEN] = [(); core::mem::size_of::<Config>()];
const _: [(); Series::LEN] = [(); core::mem::size_of::<Series>()];
const _: [(); MakerState::LEN] = [(); core::mem::size_of::<MakerState>()];

/// A header for a freshly created account of type `T`.
#[must_use]
#[inline]
pub fn new_hdr<T: ProgramAccount>(bump: u8) -> Hdr {
    Hdr {
        tag: T::TAG as u8,
        version: VERSION,
        bump,
        _pad: [0; 5],
    }
}

/// Checks owner, length, tag and version of an existing account of type `T`.
fn check<T: ProgramAccount>(
    account: &AccountView,
    program_id: &Address,
) -> Result<(), ProgramError> {
    if !account.owned_by(program_id) {
        return Err(custom(QuotebookError::InvalidAccountOwner));
    }
    if account.data_len() != T::LEN {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(())
}

fn cast<T: ProgramAccount>(data: &[u8]) -> Result<&T, ProgramError> {
    let value: &T = bytemuck::try_from_bytes(data).map_err(|_| ProgramError::InvalidAccountData)?;
    let hdr = value.hdr();
    if hdr.tag != T::TAG as u8 || hdr.version != VERSION {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(value)
}

/// Borrows an existing program account as `T`.
///
/// # Errors
///
/// `InvalidAccountOwner` if quotebook does not own it; `InvalidAccountData` for a wrong length,
/// tag, version or alignment; a borrow error if it is already borrowed mutably.
#[must_use = "the result carries a possible error"]
pub fn load<'a, T: ProgramAccount>(
    account: &'a AccountView,
    program_id: &Address,
) -> Result<Ref<'a, T>, ProgramError> {
    check::<T>(account, program_id)?;
    let data = account.try_borrow()?;
    Ref::try_map(data, |d| cast::<T>(d)).map_err(|(_, e)| e)
}

/// Borrows an existing program account as `T`, mutably.
///
/// # Errors
///
/// As [`load`], plus `Immutable` if the account is not writable.
#[must_use = "the result carries a possible error"]
pub fn load_mut<'a, T: ProgramAccount>(
    account: &'a mut AccountView,
    program_id: &Address,
) -> Result<RefMut<'a, T>, ProgramError> {
    if !account.is_writable() {
        return Err(ProgramError::Immutable);
    }
    check::<T>(account, program_id)?;
    let data = account.try_borrow_mut()?;
    RefMut::try_map(data, |d| {
        cast::<T>(d)?;
        bytemuck::try_from_bytes_mut(d).map_err(|_| ProgramError::InvalidAccountData)
    })
    .map_err(|(_, e)| e)
}

/// Fills a freshly created, zeroed account of `T::LEN` bytes in place with `f` (in place, so a
/// 2 KiB `MakerState` never sits on the 4 KiB SBF stack).
///
/// # Errors
///
/// `InvalidAccountData` if the length or alignment is wrong; a borrow error.
#[must_use = "the result carries a possible error"]
pub fn init<T: ProgramAccount>(account: &mut AccountView, f: impl FnOnce(&mut T)) -> ProgramResult {
    let mut data = account.try_borrow_mut()?;
    let value: &mut T =
        bytemuck::try_from_bytes_mut(&mut data).map_err(|_| ProgramError::InvalidAccountData)?;
    f(value);
    Ok(())
}
