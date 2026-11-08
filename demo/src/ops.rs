//! Halt, drain and revoke (`docs/PLAN-phase2.md`, D1 and D3), as checks on finalized state.
//!
//! - **Drain:** the halt file stops new quotes, and signed ones stay fillable until they expire.
//!   The drain is over when a finalized Clock reaches the service's `newest_expires_at`, which
//!   after a restart covers the previous run's quotes through the end of the cold start. With no
//!   bound at all (a fresh service that signed nothing) it is over at once.
//! - **Revoke**, in an emergency: `cancel_all` up to the service's next nonce, then a token
//!   `Revoke` on every maker account, then both confirmed at `finalized`: the window's base at or
//!   above the new minimum and no allowance left to the maker's delegate PDA.

use quotebook_cli::rpc::Rpc;
use quotebook_cli::CliError;
use quotebook_core::{Nonce, UnixTimestamp};
use quotebook_maker::protocol::StatusMessage;
use quotebook_maker::risk::ChainSnapshot;

use crate::chain::{parse_clock, CLOCK_SYSVAR};

/// The Clock at `finalized`, and the slot it was read at.
///
/// # Errors
///
/// The RPC error, or a Clock that fails its checks.
pub fn finalized_clock(rpc: &mut dyn Rpc) -> Result<(u64, UnixTimestamp), CliError> {
    let (slot, accounts) = rpc.finalized_accounts(&[CLOCK_SYSVAR])?;
    let account = accounts.into_iter().next().flatten();
    let clock = parse_clock(account.as_ref(), slot).map_err(|e| CliError::Account(e.0))?;
    Ok((slot, clock))
}

/// Where a drain stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drain {
    /// No quote the service signed can fill any more.
    Done,
    /// The finalized Clock has not reached the newest expiry yet.
    Waiting {
        /// The newest expiry.
        until: i64,
    },
}

/// Judges a drain from the service's status and a finalized Clock.
///
/// # Errors
///
/// `Maker` if the service is not halted: draining a service that still quotes ends nowhere.
pub fn drain(status: &StatusMessage, finalized_clock: UnixTimestamp) -> Result<Drain, CliError> {
    if !status.halted {
        return Err(CliError::Maker(
            "the service is not halted: create its halt file first".into(),
        ));
    }
    Ok(match status.newest_expires_at {
        Some(until) if finalized_clock.get() < until => Drain::Waiting { until },
        _ => Drain::Done,
    })
}

/// Whether a finalized snapshot confirms a revoke: the window's base at or above `new_min`
/// (when a `cancel_all` was sent) and no allowance left to the delegate PDA on any account.
#[must_use]
pub fn revoke_confirmed(snapshot: &ChainSnapshot, new_min: Option<Nonce>) -> bool {
    let cancelled = new_min.is_none_or(|n| snapshot.window.window_base() >= n);
    cancelled
        && snapshot
            .accounts
            .iter()
            .all(|(_, s)| s.delegated_amount == 0)
}

/// The `cancel_all` minimum for a revoke: the service's next nonce, which is above every nonce
/// it signed. `None` when the window already starts there, since `cancel_all` must raise it.
#[must_use]
pub fn cancel_minimum(next_nonce: u64, snapshot: &ChainSnapshot) -> Option<Nonce> {
    let new_min = Nonce::new(next_nonce);
    (new_min > snapshot.window.window_base()).then_some(new_min)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_macros, clippy::unwrap_used)]

    use quotebook_core::NonceWindow;
    use quotebook_maker::risk::TokenAccountState;
    use solana_address::Address;

    use super::*;

    fn status(halted: bool, newest: Option<i64>) -> StatusMessage {
        StatusMessage {
            request_id: 1,
            halted,
            open_quotes: 0,
            next_nonce: 5,
            newest_expires_at: newest,
            snapshot_age_secs: Some(12),
            spot_ages: Vec::new(),
        }
    }

    #[test]
    fn a_drain_needs_the_halt_and_waits_for_the_finalized_clock() {
        let t = UnixTimestamp::new;
        assert!(drain(&status(false, Some(100)), t(200)).is_err());
        assert_eq!(drain(&status(true, None), t(0)).unwrap(), Drain::Done);
        assert_eq!(
            drain(&status(true, Some(100)), t(99)).unwrap(),
            Drain::Waiting { until: 100 }
        );
        // A quote fills only while `now < expires_at`: at the expiry it is dead.
        assert_eq!(
            drain(&status(true, Some(100)), t(100)).unwrap(),
            Drain::Done
        );
    }

    fn snapshot(window: NonceWindow, delegated: u64) -> ChainSnapshot {
        ChainSnapshot {
            slot: 1,
            clock: UnixTimestamp::new(1),
            window,
            accounts: vec![(
                Address::new_from_array([1; 32]),
                TokenAccountState {
                    amount: 10,
                    delegated_amount: delegated,
                },
            )],
        }
    }

    #[test]
    fn a_revoke_is_confirmed_only_by_both_effects() {
        let fresh = NonceWindow::new();
        let mut cancelled = NonceWindow::new();
        cancelled.cancel_all(Nonce::new(5)).unwrap();
        let five = Some(Nonce::new(5));
        assert!(
            !revoke_confirmed(&snapshot(fresh, 0), five),
            "not cancelled"
        );
        assert!(
            !revoke_confirmed(&snapshot(cancelled, 3), five),
            "allowance left"
        );
        assert!(revoke_confirmed(&snapshot(cancelled, 0), five));
        assert!(
            revoke_confirmed(&snapshot(fresh, 0), None),
            "no cancel needed"
        );
    }

    #[test]
    fn the_cancel_minimum_must_raise_the_window() {
        let fresh = NonceWindow::new();
        assert_eq!(cancel_minimum(0, &snapshot(fresh, 0)), None);
        assert_eq!(cancel_minimum(5, &snapshot(fresh, 0)), Some(Nonce::new(5)));
        let mut cancelled = NonceWindow::new();
        cancelled.cancel_all(Nonce::new(5)).unwrap();
        assert_eq!(cancel_minimum(5, &snapshot(cancelled, 0)), None);
    }
}
