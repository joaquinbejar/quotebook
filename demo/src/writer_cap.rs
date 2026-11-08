//! The cap on contracts written (`docs/PLAN-phase2.md`, D5, msg 53489), read from the chain alone.
//!
//! The inventory band limits long tokens held, not contracts written: after selling them the band
//! would allow another mint. So the cap counts, per writer and across every metalbook series and
//! expiry, the contracts written and not claimed, and the collateral they lock:
//!
//! - **Source:** every metalbook `Writer` account of the owner at `finalized`
//!   (`getProgramAccounts` filtered by size, tag and owner), and each one's series at `finalized`
//!   for its `max_payout_per_contract`.
//! - **Open:** a position's `written` while `claimed` is 0. Committed is that times its series'
//!   maximum payout. Only a finalized `close` (fewer written) or `claim_writer` (claimed) releases
//!   capacity.
//! - **Fails closed:** a failed or partial read is an error, never an empty exposure.
//!
//! D4's watcher alerts on [`Exposure::over_cap`]; D5's cycle mints only within
//! [`Exposure::room_for`].

use metalbook_core::accounts::AccountTag;
use metalbook_sdk::decode;
use quotebook_cli::rpc::Rpc;
use quotebook_cli::CliError;
use solana_address::Address;

/// At most this many contracts written and not claimed, over every series.
pub const MAX_WRITTEN_CONTRACTS: u64 = 2;
/// At most this much collateral committed, USDC atoms (10.40 USDC).
pub const MAX_COMMITTED_COLLATERAL: u64 = 10_400_000;

/// The size of a metalbook `Writer` account.
pub const WRITER_LEN: u64 = 80;
/// Where a `Writer` account holds its owner.
pub const WRITER_OWNER_OFFSET: usize = 40;

/// What a writer has open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Exposure {
    /// Contracts written and not claimed.
    pub contracts: u64,
    /// Their collateral, USDC atoms.
    pub committed: u64,
    /// Positions read, open or not.
    pub positions: usize,
}

impl Exposure {
    /// Whether it is above the cap: a bug, or the key used elsewhere.
    #[must_use]
    pub const fn over_cap(&self) -> bool {
        self.contracts > MAX_WRITTEN_CONTRACTS || self.committed > MAX_COMMITTED_COLLATERAL
    }

    /// Whether one more contract locking `max_payout` atoms stays within the cap.
    #[must_use]
    pub fn room_for(&self, max_payout: u64) -> bool {
        let contracts = self.contracts.checked_add(1);
        let committed = self.committed.checked_add(max_payout);
        matches!(
            (contracts, committed),
            (Some(c), Some(m)) if c <= MAX_WRITTEN_CONTRACTS && m <= MAX_COMMITTED_COLLATERAL
        )
    }

    /// Whether any contract at all fits: the cap's count and collateral both have room.
    #[must_use]
    pub const fn has_room(&self) -> bool {
        self.contracts < MAX_WRITTEN_CONTRACTS && self.committed < MAX_COMMITTED_COLLATERAL
    }
}

fn rpc(what: impl Into<String>) -> CliError {
    CliError::Rpc(what.into())
}

/// Reads the exposure of `owner` on the metalbook program, at `finalized`.
///
/// # Errors
///
/// `Rpc` on a failed read, a position or series that does not decode, a position of another
/// owner, a missing series, or an overflow: the caller then mints nothing.
pub fn read_exposure(
    rpc_client: &mut dyn Rpc,
    metalbook_program: &Address,
    owner: &Address,
) -> Result<Exposure, CliError> {
    let found = rpc_client.finalized_program_accounts(
        metalbook_program,
        WRITER_LEN,
        &[
            (0, vec![AccountTag::Writer as u8]),
            (WRITER_OWNER_OFFSET, owner.as_ref().to_vec()),
        ],
    )?;
    let mut open: Vec<(Address, u64)> = Vec::new();
    for (address, account) in &found {
        let position = decode::position(metalbook_program, &account.owner, &account.data)
            .map_err(|e| rpc(format!("writer {address}: {e:?}")))?;
        if Address::new_from_array(position.owner) != *owner {
            return Err(rpc(format!("writer {address} of another owner")));
        }
        if position.claimed == 0 && position.written > 0 {
            open.push((Address::new_from_array(position.series), position.written));
        }
    }
    let mut series: Vec<Address> = open.iter().map(|(s, _)| *s).collect();
    series.sort_unstable();
    series.dedup();
    let mut payouts: Vec<(Address, u64)> = Vec::with_capacity(series.len());
    if !series.is_empty() {
        let (_, accounts) = rpc_client.finalized_accounts(&series)?;
        if accounts.len() != series.len() {
            return Err(rpc(
                "getMultipleAccounts returned the wrong number of accounts",
            ));
        }
        for (address, account) in series.iter().zip(accounts) {
            let account = account.ok_or_else(|| rpc(format!("series {address} missing")))?;
            let s = decode::series(metalbook_program, &account.owner, &account.data)
                .map_err(|e| rpc(format!("series {address}: {e:?}")))?;
            let payout = s
                .terms()
                .and_then(|t| t.max_payout_per_contract())
                .map_err(|e| rpc(format!("series {address}: {e:?}")))?;
            payouts.push((*address, payout.get()));
        }
    }
    let mut exposure = Exposure {
        positions: found.len(),
        ..Exposure::default()
    };
    for (s, written) in open {
        let payout = payouts
            .iter()
            .find(|(a, _)| *a == s)
            .map(|(_, p)| *p)
            .ok_or_else(|| rpc(format!("series {s} not read")))?;
        let committed = written
            .checked_mul(payout)
            .ok_or_else(|| rpc("committed collateral overflows"))?;
        exposure.contracts = exposure
            .contracts
            .checked_add(written)
            .ok_or_else(|| rpc("written contracts overflow"))?;
        exposure.committed = exposure
            .committed
            .checked_add(committed)
            .ok_or_else(|| rpc("committed collateral overflows"))?;
    }
    Ok(exposure)
}
