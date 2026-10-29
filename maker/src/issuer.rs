//! Nonce allocation under the issuance bound of docs/PLAN.md, section 4.2.
//!
//! The on-chain window holds 16,384 nonces. A fill of nonce `M` slides out every nonce up to
//! `M - 16,384`, so the maker must never stamp `M` before every one of those quotes has
//! expired: `issued_at(M) >= expires_at(N) + MAX_CLOCK_SKEW` for all `N <= M - 16,384`
//! ([`quotebook_core::min_issued_at_beyond_window`]). The issuer keeps, for each of the last
//! 16,384 nonces, the latest expiry issued up to and including it (a running maximum, since
//! lifetimes may differ), and refuses a nonce whose `issued_at` is below that floor.

use quotebook_core::{min_issued_at_beyond_window, Nonce, UnixTimestamp, WINDOW_BITS};

/// The window size as a ring length.
const RING: usize = 16_384;
/// `WINDOW_BITS - 1`.
const SLOT_MASK: u64 = 16_383;

/// Why a nonce was not issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IssueError {
    /// Issuing now could let a fill evict a quote that is still live.
    #[error("rate limit: nonce {nonce} may not be issued before {earliest}")]
    TooSoon {
        /// The nonce that would be next.
        nonce: u64,
        /// The earliest `issued_at` the bound allows for it.
        earliest: i64,
    },
    /// The quote expires before it is issued, or a counter overflowed.
    #[error("invalid lifetime or counter overflow")]
    Invalid,
}

/// Allocates nonces from a counter, enforcing the issuance bound.
#[derive(Debug, Clone)]
pub struct NonceIssuer {
    next: u64,
    /// Running maximum of `expires_at` up to each nonce, indexed by `nonce mod 16,384`.
    max_expiry: Vec<i64>,
    running_max: i64,
    /// Nonces issued since start; the floor applies once a full window is behind.
    issued: u64,
}

impl NonceIssuer {
    /// Starts at `first`, which must be at or above the maker's on-chain `window_base` and
    /// above every nonce issued before (a restart must resume from persisted state or after a
    /// `cancel_all`).
    #[must_use]
    pub fn new(first: Nonce) -> Self {
        Self {
            next: first.get(),
            max_expiry: vec![i64::MIN; RING],
            running_max: i64::MIN,
            issued: 0,
        }
    }

    /// The nonce the next successful [`NonceIssuer::issue`] returns.
    #[must_use]
    pub fn peek(&self) -> Nonce {
        Nonce::new(self.next)
    }

    fn slot(&self) -> usize {
        // `RING` is a power of two equal to `WINDOW_BITS`, so this is `next mod 16,384`.
        usize::try_from(self.next & SLOT_MASK).unwrap_or(0)
    }

    /// Issues the next nonce for a quote live from `issued_at` to `expires_at`.
    ///
    /// # Errors
    ///
    /// `TooSoon` if the quote 16,384 nonces back (or any before it) may still be live at
    /// `issued_at - MAX_CLOCK_SKEW`; `Invalid` if `expires_at < issued_at` or a counter
    /// overflows.
    pub fn issue(
        &mut self,
        issued_at: UnixTimestamp,
        expires_at: UnixTimestamp,
    ) -> Result<Nonce, IssueError> {
        if expires_at < issued_at {
            return Err(IssueError::Invalid);
        }
        let slot = self.slot();
        if self.issued >= WINDOW_BITS {
            // The slot still holds the running maximum as of nonce `next - 16,384`.
            let back = self
                .max_expiry
                .get(slot)
                .copied()
                .ok_or(IssueError::Invalid)?;
            let floor = min_issued_at_beyond_window(UnixTimestamp::new(back))
                .map_err(|_| IssueError::Invalid)?;
            if issued_at < floor {
                return Err(IssueError::TooSoon {
                    nonce: self.next,
                    earliest: floor.get(),
                });
            }
        }
        let nonce = self.next;
        self.running_max = self.running_max.max(expires_at.get());
        let entry = self.max_expiry.get_mut(slot).ok_or(IssueError::Invalid)?;
        *entry = self.running_max;
        self.next = nonce.checked_add(1).ok_or(IssueError::Invalid)?;
        self.issued = self.issued.checked_add(1).ok_or(IssueError::Invalid)?;
        Ok(Nonce::new(nonce))
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    clippy::disallowed_macros
)]
mod tests {
    use super::{IssueError, NonceIssuer};
    use quotebook_core::{Nonce, UnixTimestamp, MAX_CLOCK_SKEW, WINDOW_BITS};

    fn t(v: i64) -> UnixTimestamp {
        UnixTimestamp::new(v)
    }

    /// A full window of quotes at t = 0 living 60 s: the next nonce must wait for 60 + 10.
    #[test]
    fn test_issuer_refuses_below_the_floor() {
        let mut issuer = NonceIssuer::new(Nonce::new(100));
        for i in 0..WINDOW_BITS {
            assert_eq!(issuer.issue(t(0), t(60)).unwrap(), Nonce::new(100 + i));
        }
        let floor = 60 + MAX_CLOCK_SKEW.get();
        assert_eq!(
            issuer.issue(t(floor - 1), t(floor + 59)),
            Err(IssueError::TooSoon {
                nonce: 100 + WINDOW_BITS,
                earliest: floor
            })
        );
        // Refusal does not consume a nonce.
        assert_eq!(issuer.peek(), Nonce::new(100 + WINDOW_BITS));
        assert_eq!(
            issuer.issue(t(floor), t(floor + 60)).unwrap(),
            Nonce::new(100 + WINDOW_BITS)
        );
    }

    /// A long-lived quote early in the window holds the floor up for every later nonce,
    /// because the floor uses the running maximum of expiries.
    #[test]
    fn test_issuer_floor_uses_the_latest_expiry_so_far() {
        let mut issuer = NonceIssuer::new(Nonce::new(0));
        issuer.issue(t(0), t(500)).unwrap();
        for _ in 1..WINDOW_BITS {
            issuer.issue(t(0), t(10)).unwrap();
        }
        // Nonce 16,384 evicts nonce 0, which lives until 500.
        assert!(issuer.issue(t(509), t(520)).is_err());
        issuer.issue(t(510), t(520)).unwrap();
        // Nonce 16,385 evicts nonce 1 (expiry 10), but the running maximum is still 500.
        assert!(issuer.issue(t(509), t(520)).is_err());
    }

    #[test]
    fn test_issuer_spread_out_issuance_is_never_refused() {
        let mut issuer = NonceIssuer::new(Nonce::new(0));
        // 234 quotes per second, 60 s lifetimes: 16,384 nonces span 70 s.
        for i in 0..(3 * WINDOW_BITS) as i64 {
            let at = i / 234;
            issuer.issue(t(at), t(at + 60)).unwrap();
        }
    }

    #[test]
    fn test_issuer_rejects_inverted_lifetime() {
        let mut issuer = NonceIssuer::new(Nonce::new(0));
        assert_eq!(issuer.issue(t(10), t(9)), Err(IssueError::Invalid));
    }
}
