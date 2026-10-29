//! Nonce allocation under the issuance bound of docs/PLAN.md, section 4.2.
//!
//! The on-chain window holds 16,384 nonces. A fill of nonce `M` slides out every nonce up to
//! `M - 16,384`, so the maker must never stamp `M` before every one of those quotes has
//! expired: `issued_at(M) >= expires_at(N) + MAX_CLOCK_SKEW` for all `N <= M - 16,384`
//! ([`quotebook_core::min_issued_at_beyond_window`]). The issuer keeps, for each of the last
//! 16,384 nonces, the latest expiry issued up to and including it (a running maximum, since
//! lifetimes may differ), and refuses a nonce whose `issued_at` is below that floor.
//!
//! # Restarts
//!
//! That history lives in memory. A restarted service has lost it, and nothing on chain
//! protects a still-valid quote it signed before the restart: `check_time` bounds each
//! quote's own lifetime, not whether a later nonce evicts it. So a restart is fail-closed
//! ([`NonceIssuer::resume`]): it refuses to issue anything until
//! `started_at + max_quote_ttl + MAX_CLOCK_SKEW`, and then until it is
//! [refreshed](NonceIssuer::refresh) with the maker's on-chain window read at or after that
//! time, so it starts above every nonce used,
//! including quotes from before the restart filled during the wait. Every quote signed
//! before the restart was issued before `started_at` and lives at most `max_quote_ttl`, so by
//! then all of them have expired and none can be evicted while live. The cost is a cold start
//! of `max_quote_ttl + MAX_CLOCK_SKEW` (70 s at 60 s + 10 s) with no quotes. Persisting the
//! issuer would remove the delay; not in v1.

use quotebook_core::{
    min_issued_at_beyond_window, Nonce, NonceWindow, Seconds, UnixTimestamp, MAX_CLOCK_SKEW,
    WINDOW_BITS,
};

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
    /// The issuer resumed after a restart and is waiting out the lifetime of every quote it
    /// may have signed before, or has not yet been refreshed with a window read after that.
    #[error("cold start: no quotes before {until} and a window read from then")]
    ColdStart {
        /// The first `issued_at` allowed.
        until: i64,
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
    /// After a restart, the first `issued_at` allowed.
    not_before: Option<i64>,
    /// After a restart, whether a window read from `not_before` on has been applied.
    refreshed: bool,
}

impl NonceIssuer {
    /// For a maker key that has **never signed a quote**: starts at `first`, at or above its
    /// on-chain `window_base`. Every restart uses [`NonceIssuer::resume`] instead.
    #[must_use]
    pub fn fresh(first: Nonce) -> Self {
        Self {
            next: first.get(),
            max_expiry: vec![i64::MIN; RING],
            running_max: i64::MIN,
            issued: 0,
            not_before: None,
            refreshed: true,
        }
    }

    /// For a maker key that may have signed quotes before (any restart). It refuses to issue
    /// before `started_at + max_quote_ttl + MAX_CLOCK_SKEW`, and after that until
    /// [`NonceIssuer::refresh`] applies the maker's on-chain window read from then on (see the
    /// module documentation). `window` (the maker's `MakerState.nonces`, read at start) sets a
    /// provisional first nonce.
    ///
    /// `max_quote_ttl` must be the largest lifetime this key ever signed, across every series
    /// it quotes; `started_at` must come from a clock no earlier than the one that stamped the
    /// old quotes.
    ///
    /// # Errors
    ///
    /// `Invalid` if the arithmetic overflows.
    pub fn resume(
        window: &NonceWindow,
        started_at: UnixTimestamp,
        max_quote_ttl: Seconds,
    ) -> Result<Self, IssueError> {
        let first = Self::first_free(window)?;
        let not_before = started_at
            .get()
            .checked_add(max_quote_ttl.get())
            .and_then(|t| t.checked_add(MAX_CLOCK_SKEW.get()))
            .ok_or(IssueError::Invalid)?;
        let mut issuer = Self::fresh(Nonce::new(first));
        issuer.not_before = Some(not_before);
        issuer.refreshed = false;
        Ok(issuer)
    }

    /// The first nonce above everything `window` has seen used, and not below its base.
    fn first_free(window: &NonceWindow) -> Result<u64, IssueError> {
        let above_used = match window.highest_used() {
            Some(n) => n.get().checked_add(1).ok_or(IssueError::Invalid)?,
            None => 0,
        };
        Ok(window.window_base().get().max(above_used))
    }

    /// Applies the maker's on-chain window, read at `read_at`: the next nonce moves above
    /// every nonce used (it never moves down). After a [`resume`](NonceIssuer::resume), the
    /// first refresh with `read_at` at or after the cold start's end lets the issuer quote.
    ///
    /// # Errors
    ///
    /// `ColdStart` if the issuer is resuming and `read_at` is before the end of the cold start
    /// (a window read then may miss fills of pre-restart quotes); `Invalid` on overflow.
    pub fn refresh(
        &mut self,
        window: &NonceWindow,
        read_at: UnixTimestamp,
    ) -> Result<(), IssueError> {
        if let Some(until) = self.not_before {
            if read_at.get() < until {
                return Err(IssueError::ColdStart { until });
            }
        }
        self.next = self.next.max(Self::first_free(window)?);
        self.refreshed = true;
        Ok(())
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
        if let Some(until) = self.not_before {
            if issued_at.get() < until || !self.refreshed {
                return Err(IssueError::ColdStart { until });
            }
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
    use quotebook_core::{Nonce, NonceWindow, Seconds, UnixTimestamp, MAX_CLOCK_SKEW, WINDOW_BITS};

    fn t(v: i64) -> UnixTimestamp {
        UnixTimestamp::new(v)
    }

    /// A full window of quotes at t = 0 living 60 s: the next nonce must wait for 60 + 10.
    #[test]
    fn test_issuer_refuses_below_the_floor() {
        let mut issuer = NonceIssuer::fresh(Nonce::new(100));
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
        let mut issuer = NonceIssuer::fresh(Nonce::new(0));
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
        let mut issuer = NonceIssuer::fresh(Nonce::new(0));
        // 234 quotes per second, 60 s lifetimes: 16,384 nonces span 70 s.
        for i in 0..(3 * WINDOW_BITS) as i64 {
            let at = i / 234;
            issuer.issue(t(at), t(at + 60)).unwrap();
        }
    }

    #[test]
    fn test_issuer_resume_starts_above_used_and_waits_out_old_quotes() {
        let mut window = NonceWindow::new();
        window.cancel_all(Nonce::new(50)).unwrap();
        window.consume(Nonce::new(60)).unwrap();
        window.consume(Nonce::new(55)).unwrap();
        let mut issuer = NonceIssuer::resume(&window, t(1_000), Seconds::new(60)).unwrap();
        assert_eq!(issuer.peek(), Nonce::new(61));
        let until = 1_000 + 60 + MAX_CLOCK_SKEW.get();
        assert_eq!(
            issuer.issue(t(until - 1), t(until + 29)),
            Err(IssueError::ColdStart { until })
        );
        // Past the cold start but not refreshed: still refused.
        assert_eq!(
            issuer.issue(t(until), t(until + 30)),
            Err(IssueError::ColdStart { until })
        );
        // A window read too early is refused; one read from `until` on is applied, here with
        // nonce 70 filled during the wait.
        window.consume(Nonce::new(70)).unwrap();
        assert_eq!(
            issuer.refresh(&window, t(until - 1)),
            Err(IssueError::ColdStart { until })
        );
        issuer.refresh(&window, t(until)).unwrap();
        assert_eq!(
            issuer.issue(t(until), t(until + 30)).unwrap(),
            Nonce::new(71)
        );

        // Nothing used yet: start at the watermark.
        let mut fresh_window = NonceWindow::new();
        fresh_window.cancel_all(Nonce::new(7)).unwrap();
        let issuer = NonceIssuer::resume(&fresh_window, t(0), Seconds::new(60)).unwrap();
        assert_eq!(issuer.peek(), Nonce::new(7));
    }

    #[test]
    fn test_issuer_rejects_inverted_lifetime() {
        let mut issuer = NonceIssuer::fresh(Nonce::new(0));
        assert_eq!(issuer.issue(t(10), t(9)), Err(IssueError::Invalid));
    }
}
