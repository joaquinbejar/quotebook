//! Program accounts: `Config`, `Series` and `MakerState` (docs/PLAN.md, section 3.1), shared
//! by the program and its clients.
//!
//! Every account starts with an 8-byte [`Hdr`] and is `Pod`. The program loads them
//! zero-copy after its owner, length, tag and version checks; clients decode a copy with
//! [`decode`], which makes the same data checks.

use bytemuck::Pod;

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
    use crate::{BaseAtoms, BaseLots, NonceWindow, QuoteAtoms, Seconds};
    use bytemuck::{Pod, Zeroable};
    use solana_address::Address;

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

/// Decodes a copy of a program account of type `T` from its data, at any alignment. The caller
/// checks that quotebook owns the account.
///
/// Returns `None` for a wrong length, tag or version.
#[must_use]
pub fn decode<T: ProgramAccount>(data: &[u8]) -> Option<T> {
    if data.len() != T::LEN {
        return None;
    }
    let value: T = bytemuck::try_pod_read_unaligned(data).ok()?;
    let hdr = value.hdr();
    (hdr.tag == T::TAG as u8 && hdr.version == VERSION).then_some(value)
}
