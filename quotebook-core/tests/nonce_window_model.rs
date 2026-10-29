//! Model test for `NonceWindow`: random fills, `cancel_nonce`s and `cancel_all`s (aligned and
//! unaligned, short and long slides) against a `BTreeSet` reference that implements the rules
//! of docs/PLAN.md section 4.2 directly.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::integer_division,
    clippy::panic_in_result_fn,
    clippy::disallowed_macros,
    missing_docs
)]

use std::collections::BTreeSet;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};
use quotebook_core::{Nonce, NonceWindow, QuotebookError, WINDOW_BITS};

/// Cases per property. `QUOTEBOOK_PROPTEST_CASES` overrides the default.
fn config() -> Config {
    let cases = std::env::var("QUOTEBOOK_PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x0B0C_0002_5EED_0002),
        failure_persistence: None,
        ..Config::default()
    }
}

/// The reference: the set of used nonces in the window, the base and the last cancel_all.
#[derive(Default)]
struct Model {
    base: u64,
    last_cancel_all: u64,
    used: BTreeSet<u64>,
}

impl Model {
    fn reject_below_base(&self, n: u64) -> QuotebookError {
        if n < self.last_cancel_all {
            QuotebookError::NonceCancelled
        } else {
            QuotebookError::NonceOutOfWindow
        }
    }

    fn check(&self, n: u64) -> Result<(), QuotebookError> {
        if n < self.base {
            return Err(self.reject_below_base(n));
        }
        if self.used.contains(&n) {
            return Err(QuotebookError::NonceUsed);
        }
        Ok(())
    }

    fn slide(&mut self, new_base: u64) {
        self.base = new_base;
        self.used = self.used.split_off(&new_base);
    }

    fn consume(&mut self, n: u64) -> Result<(), QuotebookError> {
        self.check(n)?;
        if n - self.base >= WINDOW_BITS {
            self.slide(n - (WINDOW_BITS - 1));
        }
        self.used.insert(n);
        Ok(())
    }

    fn cancel_all(&mut self, m: u64) -> Result<(), QuotebookError> {
        if m <= self.base {
            return Err(QuotebookError::WatermarkNotIncreasing);
        }
        self.slide(m);
        self.last_cancel_all = m;
        Ok(())
    }
}

/// A nonce chosen relative to the window at the time the operation runs, so random sequences
/// keep hitting the interesting regions.
#[derive(Debug, Clone, Copy)]
enum Pick {
    /// `base - 1 - k`: below the window.
    Below(u16),
    /// `base + k mod WINDOW_BITS`: inside the window.
    Inside(u16),
    /// Near the top edge: `base + WINDOW_BITS - 4 + k`, `k < 8`.
    Edge(u8),
    /// `base + WINDOW_BITS + k`: forces a slide of up to ~3 windows.
    Beyond(u16),
    /// `base + k * WINDOW_BITS`: whole-window jumps, including the full clear.
    Jump(u8),
    /// A nonce already used in the model, if any (the k-th, wrapping).
    Reuse(u8),
}

#[derive(Debug, Clone, Copy)]
enum Op {
    Fill(Pick),
    CancelNonce(Pick),
    CancelAll(Pick),
}

fn pick() -> impl Strategy<Value = Pick> {
    prop_oneof![
        1 => any::<u16>().prop_map(Pick::Below),
        6 => any::<u16>().prop_map(Pick::Inside),
        2 => (0u8..8).prop_map(Pick::Edge),
        2 => any::<u16>().prop_map(Pick::Beyond),
        1 => (0u8..4).prop_map(Pick::Jump),
        3 => any::<u8>().prop_map(Pick::Reuse),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => pick().prop_map(Op::Fill),
        2 => pick().prop_map(Op::CancelNonce),
        1 => pick().prop_map(Op::CancelAll),
    ]
}

fn resolve(model: &Model, pick: Pick) -> u64 {
    let b = model.base;
    match pick {
        Pick::Below(k) => b.saturating_sub(1 + u64::from(k)),
        Pick::Inside(k) => b + u64::from(k) % WINDOW_BITS,
        Pick::Edge(k) => b + WINDOW_BITS - 4 + u64::from(k),
        Pick::Beyond(k) => b + WINDOW_BITS + u64::from(k),
        Pick::Jump(k) => b + u64::from(k) * WINDOW_BITS,
        Pick::Reuse(k) => {
            if model.used.is_empty() {
                b
            } else {
                *model
                    .used
                    .iter()
                    .nth(usize::from(k) % model.used.len())
                    .unwrap()
            }
        }
    }
}

/// The window must agree with the model on every used nonce, on the count (so no stray bit
/// is set anywhere in the ring), on the watermarks, and on a few probes around the edges.
fn assert_agrees(window: &NonceWindow, model: &Model) -> Result<(), TestCaseError> {
    prop_assert_eq!(window.window_base().get(), model.base);
    prop_assert_eq!(window.last_cancel_all().get(), model.last_cancel_all);
    prop_assert_eq!(window.used_count() as usize, model.used.len());
    for &n in &model.used {
        prop_assert_eq!(window.check(Nonce::new(n)), Err(QuotebookError::NonceUsed));
    }
    let b = model.base;
    for n in [
        b.saturating_sub(1),
        b,
        b + 1,
        b + 63,
        b + 64,
        b + WINDOW_BITS - 1,
        b + WINDOW_BITS,
        model.last_cancel_all.saturating_sub(1),
    ] {
        prop_assert_eq!(window.check(Nonce::new(n)), model.check(n), "probe {}", n);
    }
    Ok(())
}

fn run(start: u64, ops: &[Op]) -> Result<(), TestCaseError> {
    let mut window = NonceWindow::new();
    let mut model = Model::default();
    if start > 0 {
        prop_assert_eq!(window.cancel_all(Nonce::new(start)), Ok(()));
        prop_assert_eq!(model.cancel_all(start), Ok(()));
    }
    for &op in ops {
        let (got, want) = match op {
            Op::Fill(p) | Op::CancelNonce(p) => {
                let n = resolve(&model, p);
                (window.consume(Nonce::new(n)), model.consume(n))
            }
            Op::CancelAll(p) => {
                let n = resolve(&model, p);
                (window.cancel_all(Nonce::new(n)), model.cancel_all(n))
            }
        };
        prop_assert_eq!(got, want, "op {:?}", op);
        assert_agrees(&window, &model)?;
    }
    Ok(())
}

proptest! {
    #![proptest_config(config())]

    /// Random operation sequences from a random starting watermark, including unaligned ones.
    #[test]
    fn test_nonce_window_matches_model(
        start in prop_oneof![Just(0u64), 1u64..1_000_000, Just(u64::MAX / 2 + 13)],
        ops in proptest::collection::vec(op(), 1..96),
    ) {
        run(start, &ops)?;
    }
}

/// Dense sequential use, as a maker allocating from a counter would produce: two full windows
/// of fills, every one sliding by one after the first window.
#[test]
fn test_nonce_window_sequential_counter_matches_model() {
    let mut window = NonceWindow::new();
    let mut model = Model::default();
    for n in 0..(3 * WINDOW_BITS) {
        assert_eq!(window.consume(Nonce::new(n)), model.consume(n));
    }
    assert_eq!(window.window_base().get(), model.base);
    assert_eq!(window.used_count() as usize, model.used.len());
    assert_eq!(window.used_count() as u64, WINDOW_BITS);
}
