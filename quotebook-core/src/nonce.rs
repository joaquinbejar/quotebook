//! The per-maker replay window: a 16,384-bit ring of used nonces above a sliding watermark.
//!
//! A nonce `n` is fillable iff `n >= window_base` and it has not been used. Nonces in
//! `[window_base, window_base + 16,384)` map to bit `n mod 16,384` of the ring. Using a nonce
//! at or beyond the window's top first slides `window_base` up to `n - 16,383`; sliding from
//! `B` to `B'` clears exactly the slots of `[B, B')`, so they are free for the nonces entering
//! at the top. `cancel_all` is the same slide, driven by the maker, and also records the
//! watermark in `last_cancel_all` so a rejection can say which of the two moved the base.
//!
//! # When a slide can evict a live quote
//!
//! The ring itself is exact; the safety of sliding rests on a rule the maker follows when it
//! issues nonces (docs/PLAN.md, section 4.2):
//!
//! > for every nonce `M`, `issued_at(M) >= expires_at(N) + MAX_CLOCK_SKEW` for every
//! > `N <= M - 16,384`.
//!
//! [`min_issued_at_beyond_window`] computes that floor. Proof sketch: by
//! [`Quote::check_time`](crate::Quote::check_time), quote `Q` can be filled only at on-chain
//! times `t` with `issued_at(Q) - MAX_CLOCK_SKEW <= t < expires_at(Q)`. The window drops
//! nonce `N` only when a nonce `M >= N + 16,384` is consumed (a `settle` of quote `M`, or the
//! maker's own `cancel_nonce` or `cancel_all`). A `settle` of `M` happens at some `t >=
//! issued_at(M) - MAX_CLOCK_SKEW >= expires_at(N)`, where quote `N` is already expired. So no
//! fill evicts a quote that is still fillable. With `issued_at` non-decreasing in the nonce and
//! every lifetime at most `max_quote_ttl`, the rule holds if `issued_at(N + 16,384) -
//! issued_at(N) >= max_quote_ttl + MAX_CLOCK_SKEW` for every `N`: at most 16,384 nonces in any
//! half-open `issued_at` interval of that length, about 234 quotes per second at 60 s + 10 s.
//!
//! A horizon of `max_quote_ttl` alone is not enough: a quote accepted at `now = 0` may carry
//! `issued_at = 10, expires_at = 70`, and a nonce 16,384 issued 61 s later is fillable at
//! `t = 61`, while that quote still is.

use crate::{Nonce, QuotebookError, UnixTimestamp, MAX_CLOCK_SKEW};

/// Number of nonces the ring tracks above `window_base`.
pub const WINDOW_BITS: u64 = 16_384;

/// `WINDOW_BITS - 1`: masks a nonce to its ring slot.
const SLOT_MASK: u64 = 16_383;

/// Ring storage, in 64-bit words.
const WINDOW_WORDS: usize = 256;

/// Bits per ring word.
const WORD_BITS: u64 = 64;

/// `WORD_BITS - 1`: masks a slot to its bit within the word.
const WORD_MASK: u64 = 63;

/// Pod layout, kept apart for the same reason as `quote::layout`: `derive(Pod)` emits a
/// compile-time `const _: () = assert!(..)`, so the macro ban is lifted in this module only.
/// No logic here.
#[allow(clippy::disallowed_macros)]
mod layout {
    use bytemuck::{Pod, Zeroable};

    use super::WINDOW_WORDS;
    use crate::Nonce;

    /// The replay state of one maker, stored inside its `MakerState` account.
    ///
    /// Any bit pattern is a valid state, so the window can be loaded zero-copy from account
    /// data. `last_cancel_all <= window_base` always holds for states built through this API; if
    /// stored data breaks it, only the choice between `NonceCancelled` and `NonceOutOfWindow`
    /// changes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
    #[repr(C)]
    pub struct NonceWindow {
        pub(super) window_base: Nonce,
        pub(super) last_cancel_all: Nonce,
        pub(super) used: [u64; WINDOW_WORDS],
    }
}

pub use layout::NonceWindow;

impl Default for NonceWindow {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl NonceWindow {
    /// Size in bytes: two `u64` watermarks and the 2,048-byte ring.
    pub const LEN: usize = 2_064;

    /// A fresh window: every nonce from 0 up is fillable.
    #[must_use]
    #[inline]
    pub const fn new() -> Self {
        Self {
            window_base: Nonce::new(0),
            last_cancel_all: Nonce::new(0),
            used: [0; WINDOW_WORDS],
        }
    }

    /// The lowest nonce that can still be filled.
    #[must_use]
    #[inline]
    pub const fn window_base(&self) -> Nonce {
        self.window_base
    }

    /// The watermark set by the last `cancel_all`, or 0 if there was none.
    #[must_use]
    #[inline]
    pub const fn last_cancel_all(&self) -> Nonce {
        self.last_cancel_all
    }

    /// How many nonces inside the window are marked used.
    #[must_use]
    pub fn used_count(&self) -> u32 {
        // At most 16,384 bits in total, so the sum never reaches u32::MAX and the saturation
        // never engages; this is a count, not an amount.
        self.used
            .iter()
            .fold(0u32, |acc, w| acc.saturating_add(w.count_ones()))
    }

    /// Checks that `nonce` can be filled now, without changing the window.
    ///
    /// # Errors
    ///
    /// - `NonceCancelled` if `nonce` is below the last `cancel_all` watermark.
    /// - `NonceOutOfWindow` if a slide moved the window past `nonce`.
    /// - `NonceUsed` if `nonce` was already filled or cancelled.
    #[must_use = "the result carries a possible error"]
    pub fn check(&self, nonce: Nonce) -> Result<(), QuotebookError> {
        let offset = self.offset(nonce)?;
        if offset >= WINDOW_BITS {
            return Ok(());
        }
        if self.is_set(nonce)? {
            return Err(QuotebookError::NonceUsed);
        }
        Ok(())
    }

    /// Marks `nonce` used, sliding the window first if `nonce` is at or beyond its top. This
    /// is both a fill and `cancel_nonce`.
    ///
    /// On `NonceCancelled`, `NonceOutOfWindow` or `NonceUsed` the window is unchanged.
    ///
    /// # Errors
    ///
    /// The errors of [`NonceWindow::check`]; `Overflow` only if an internal index computation
    /// fails, which no state reachable through this API produces.
    #[must_use = "the result carries a possible error"]
    pub fn consume(&mut self, nonce: Nonce) -> Result<(), QuotebookError> {
        self.check(nonce)?;
        if self.offset(nonce)? >= WINDOW_BITS {
            let new_base = nonce
                .get()
                .checked_sub(SLOT_MASK)
                .ok_or(QuotebookError::Overflow)?;
            self.slide_to(new_base)?;
        }
        self.set(nonce)
    }

    /// Cancels every nonce below `new_min` by sliding the window up to it.
    ///
    /// # Errors
    ///
    /// `WatermarkNotIncreasing` if `new_min <= window_base`; `Overflow` as in
    /// [`NonceWindow::consume`].
    #[must_use = "the result carries a possible error"]
    pub fn cancel_all(&mut self, new_min: Nonce) -> Result<(), QuotebookError> {
        if new_min <= self.window_base {
            return Err(QuotebookError::WatermarkNotIncreasing);
        }
        self.slide_to(new_min.get())?;
        self.last_cancel_all = new_min;
        Ok(())
    }

    /// `nonce - window_base`, or the rejection for a nonce below the base.
    fn offset(&self, nonce: Nonce) -> Result<u64, QuotebookError> {
        nonce
            .get()
            .checked_sub(self.window_base.get())
            .ok_or(if nonce < self.last_cancel_all {
                QuotebookError::NonceCancelled
            } else {
                QuotebookError::NonceOutOfWindow
            })
    }

    /// The ring word index and the bit mask of `nonce`'s slot.
    fn slot(nonce: Nonce) -> Result<(usize, u64), QuotebookError> {
        let slot = nonce.get() & SLOT_MASK;
        let word = slot
            .checked_div(WORD_BITS)
            .and_then(|w| usize::try_from(w).ok())
            .ok_or(QuotebookError::Overflow)?;
        let bit = u32::try_from(slot & WORD_MASK).map_err(|_| QuotebookError::Overflow)?;
        let mask = 1u64.checked_shl(bit).ok_or(QuotebookError::Overflow)?;
        Ok((word, mask))
    }

    fn is_set(&self, nonce: Nonce) -> Result<bool, QuotebookError> {
        let (word, mask) = Self::slot(nonce)?;
        let w = self.used.get(word).ok_or(QuotebookError::Overflow)?;
        Ok(w & mask != 0)
    }

    fn set(&mut self, nonce: Nonce) -> Result<(), QuotebookError> {
        let (word, mask) = Self::slot(nonce)?;
        let w = self.used.get_mut(word).ok_or(QuotebookError::Overflow)?;
        *w |= mask;
        Ok(())
    }

    /// Moves the base up to `new_base` (> `window_base`), clearing the slots of the nonces
    /// that leave the window.
    fn slide_to(&mut self, new_base: u64) -> Result<(), QuotebookError> {
        let base = self.window_base.get();
        let distance = new_base.checked_sub(base).ok_or(QuotebookError::Overflow)?;
        if distance >= WINDOW_BITS {
            self.used = [0; WINDOW_WORDS];
        } else {
            // The leaving slots run from `base`'s slot for `distance` bits, wrapping once.
            let start = base & SLOT_MASK;
            let to_end = WINDOW_BITS
                .checked_sub(start)
                .ok_or(QuotebookError::Overflow)?;
            let first = distance.min(to_end);
            let end = start.checked_add(first).ok_or(QuotebookError::Overflow)?;
            self.clear_linear(start, end)?;
            let rest = distance
                .checked_sub(first)
                .ok_or(QuotebookError::Overflow)?;
            self.clear_linear(0, rest)?;
        }
        self.window_base = Nonce::new(new_base);
        Ok(())
    }

    /// Clears ring bits `[lo, hi)`, `hi <= WINDOW_BITS`; masks the partial words at both ends.
    fn clear_linear(&mut self, lo: u64, hi: u64) -> Result<(), QuotebookError> {
        if lo >= hi {
            return Ok(());
        }
        let ovf = || QuotebookError::Overflow;
        let first_word = lo.checked_div(WORD_BITS).ok_or_else(ovf)?;
        let last_word = hi
            .checked_sub(1)
            .and_then(|h| h.checked_div(WORD_BITS))
            .ok_or_else(ovf)?;
        let words = last_word
            .checked_sub(first_word)
            .and_then(|d| d.checked_add(1))
            .ok_or_else(ovf)?;
        let skip = usize::try_from(first_word).map_err(|_| ovf())?;
        let take = usize::try_from(words).map_err(|_| ovf())?;
        for (index, word) in self.used.iter_mut().enumerate().skip(skip).take(take) {
            let word_lo = u64::try_from(index)
                .ok()
                .and_then(|i| i.checked_mul(WORD_BITS))
                .ok_or_else(ovf)?;
            let word_hi = word_lo.checked_add(WORD_BITS).ok_or_else(ovf)?;
            let from = lo.max(word_lo).checked_sub(word_lo).ok_or_else(ovf)?;
            let to = hi.min(word_hi).checked_sub(word_lo).ok_or_else(ovf)?;
            *word &= !(low_bits(to)? & !low_bits(from)?);
        }
        Ok(())
    }
}

/// A mask of the `k` lowest bits, `k <= 64`.
fn low_bits(k: u64) -> Result<u64, QuotebookError> {
    if k >= WORD_BITS {
        return Ok(u64::MAX);
    }
    let k = u32::try_from(k).map_err(|_| QuotebookError::Overflow)?;
    1u64.checked_shl(k)
        .and_then(|b| b.checked_sub(1))
        .ok_or(QuotebookError::Overflow)
}

/// The earliest `issued_at` a maker may stamp on any nonce `M >= N + WINDOW_BITS`, where
/// `latest_expiry` is the latest `expires_at` among its nonces up to `N`:
/// `latest_expiry + MAX_CLOCK_SKEW`. Issuing at or after it means a fill of `M` can only land
/// once every quote it slides out of the window has expired (see the module documentation).
///
/// # Errors
///
/// `Overflow` if the sum does not fit `i64`.
#[must_use = "the result carries a possible error"]
pub fn min_issued_at_beyond_window(
    latest_expiry: UnixTimestamp,
) -> Result<UnixTimestamp, QuotebookError> {
    latest_expiry
        .get()
        .checked_add(MAX_CLOCK_SKEW.get())
        .map(UnixTimestamp::new)
        .ok_or(QuotebookError::Overflow)
}

// Compile-time check that the stored size is `NonceWindow::LEN`.
const _: [(); NonceWindow::LEN] = [(); core::mem::size_of::<NonceWindow>()];

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]
mod tests {
    use super::{low_bits, NonceWindow, WINDOW_BITS};
    use crate::{Nonce, QuotebookError};

    fn n(v: u64) -> Nonce {
        Nonce::new(v)
    }

    #[test]
    fn test_low_bits_edges() {
        assert_eq!(low_bits(0).unwrap(), 0);
        assert_eq!(low_bits(1).unwrap(), 1);
        assert_eq!(low_bits(63).unwrap(), u64::MAX >> 1);
        assert_eq!(low_bits(64).unwrap(), u64::MAX);
    }

    #[test]
    fn test_nonce_window_consume_twice_is_rejected() {
        let mut w = NonceWindow::new();
        w.consume(n(5)).unwrap();
        assert_eq!(w.consume(n(5)), Err(QuotebookError::NonceUsed));
        assert_eq!(w.check(n(5)), Err(QuotebookError::NonceUsed));
        assert_eq!(w.check(n(6)), Ok(()));
        assert_eq!(w.used_count(), 1);
    }

    #[test]
    fn test_nonce_window_slide_drops_old_nonces() {
        let mut w = NonceWindow::new();
        w.consume(n(0)).unwrap();
        w.consume(n(1)).unwrap();
        // Top of the window is 16,383; 16,384 slides the base to 1.
        w.consume(n(WINDOW_BITS)).unwrap();
        assert_eq!(w.window_base(), n(1));
        assert_eq!(w.check(n(0)), Err(QuotebookError::NonceOutOfWindow));
        assert_eq!(w.check(n(1)), Err(QuotebookError::NonceUsed));
        assert_eq!(w.used_count(), 2);
    }

    #[test]
    fn test_nonce_window_far_jump_clears_everything() {
        let mut w = NonceWindow::new();
        for v in 0..100 {
            w.consume(n(v)).unwrap();
        }
        w.consume(n(10 * WINDOW_BITS + 7)).unwrap();
        assert_eq!(w.used_count(), 1);
        assert_eq!(w.window_base(), n(9 * WINDOW_BITS + 8));
    }

    #[test]
    fn test_nonce_window_cancel_all_classifies_rejections() {
        let mut w = NonceWindow::new();
        w.cancel_all(n(100)).unwrap();
        assert_eq!(w.check(n(99)), Err(QuotebookError::NonceCancelled));
        w.consume(n(100 + WINDOW_BITS + 50)).unwrap();
        assert_eq!(w.check(n(99)), Err(QuotebookError::NonceCancelled));
        assert_eq!(w.check(n(120)), Err(QuotebookError::NonceOutOfWindow));
        assert_eq!(w.check(n(151)), Ok(()));
    }

    #[test]
    fn test_nonce_window_cancel_all_must_increase() {
        let mut w = NonceWindow::new();
        assert_eq!(
            w.cancel_all(n(0)),
            Err(QuotebookError::WatermarkNotIncreasing)
        );
        w.cancel_all(n(10)).unwrap();
        assert_eq!(
            w.cancel_all(n(10)),
            Err(QuotebookError::WatermarkNotIncreasing)
        );
        assert_eq!(
            w.cancel_all(n(9)),
            Err(QuotebookError::WatermarkNotIncreasing)
        );
    }

    #[test]
    fn test_nonce_window_unaligned_cancel_all_keeps_live_bits() {
        let mut w = NonceWindow::new();
        for v in 60..70 {
            w.consume(n(v)).unwrap();
        }
        // 65 is inside word 1 (bits 64..128) and not word aligned.
        w.cancel_all(n(65)).unwrap();
        assert_eq!(w.used_count(), 5);
        for v in 65..70 {
            assert_eq!(w.check(n(v)), Err(QuotebookError::NonceUsed));
        }
        // The cleared slots 60..65 are reused by 16,444..16,449 without a false NonceUsed.
        for v in 60..65 {
            assert_eq!(w.check(n(v + WINDOW_BITS)), Ok(()));
        }
    }

    #[test]
    fn test_nonce_window_near_u64_max() {
        let mut w = NonceWindow::new();
        w.consume(n(u64::MAX)).unwrap();
        assert_eq!(w.window_base(), n(u64::MAX - (WINDOW_BITS - 1)));
        assert_eq!(w.consume(n(u64::MAX)), Err(QuotebookError::NonceUsed));
        w.cancel_all(n(u64::MAX)).unwrap();
        assert_eq!(w.check(n(u64::MAX)), Err(QuotebookError::NonceUsed));
    }

    #[test]
    fn test_nonce_window_layout() {
        assert_eq!(core::mem::size_of::<NonceWindow>(), NonceWindow::LEN);
        assert_eq!(core::mem::align_of::<NonceWindow>(), 8);
    }
}
