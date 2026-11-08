//! The board: the spot of each metal and the marks priced on it, and the `PriceFeed` of a series.
//!
//! A read of a feed account and the repricing it causes happen under one lock, so a mid is never
//! a new spot with an old mark. Every mid checks the market hours, the settlement window and the
//! spot's age against the clock again, so nothing is trusted because it was fine when priced.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use metalbook_core::Underlying;
use quotebook_maker::server::{PriceFeed, TimeSource};
use rust_decimal::Decimal;
use solana_address::Address;

use crate::config::{DemoConfig, UnderlyingConfig};
use crate::marks::{price_expiry, MarkInputs};
use crate::schedule::market_open;
use crate::series::ListedSeries;
use crate::spot::{SpotCell, SpotError, SpotPrice};

#[derive(Debug, Clone, Copy)]
struct Metal {
    config: UnderlyingConfig,
    cell: SpotCell,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    series: ListedSeries,
    mark: Option<StoredMark>,
}

#[derive(Debug, Clone, Copy)]
struct StoredMark {
    value: Decimal,
    /// The `publish_time` of the spot it was priced on.
    spot_publish_time: i64,
}

#[derive(Debug)]
struct State {
    metals: Vec<Metal>,
    entries: Vec<Entry>,
}

/// Spot and marks of every series the demo quotes.
pub struct Board {
    state: Mutex<State>,
    clock: Box<dyn TimeSource>,
}

impl std::fmt::Debug for Board {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Board").finish_non_exhaustive()
    }
}

impl Board {
    /// An empty board: no series and no spot, so no mid until both have been read.
    #[must_use]
    pub fn new(config: &DemoConfig, clock: Box<dyn TimeSource>) -> Self {
        let metals = config
            .underlyings
            .iter()
            .map(|u| Metal {
                config: *u,
                cell: SpotCell::new(u.feed_id),
            })
            .collect();
        Self {
            state: Mutex::new(State {
                metals,
                entries: Vec::new(),
            }),
            clock,
        }
    }

    /// Replaces the series quoted and prices them.
    pub fn set_series(&self, series: &[ListedSeries]) {
        let now = self.now();
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        state.entries = series
            .iter()
            .map(|s| Entry {
                series: *s,
                mark: None,
            })
            .collect();
        let underlyings: Vec<Underlying> =
            state.metals.iter().map(|m| m.config.underlying).collect();
        for underlying in underlyings {
            state.reprice(underlying, now);
        }
    }

    /// Checks one read of `underlying`'s feed account and reprices that metal's series.
    ///
    /// # Errors
    ///
    /// [`SpotError`], which [`SpotCell::ingest`] explains. `None` if the metal is not configured
    /// or the lock is poisoned: the board then gives no mid for it anyway.
    pub fn ingest(
        &self,
        underlying: Underlying,
        owner: &[u8; 32],
        data: &[u8],
    ) -> Option<Result<SpotPrice, SpotError>> {
        let now = self.now();
        let mut state = self.state.lock().ok()?;
        let outcome = state
            .metals
            .iter_mut()
            .find(|m| m.config.underlying == underlying)
            .map(|m| m.cell.ingest(owner, data, now));
        state.reprice(underlying, now);
        outcome
    }

    /// The mark of the series, USDC per contract, or `None`: no quote.
    #[must_use]
    pub fn mid(&self, series: &Address) -> Option<Decimal> {
        let now = self.now();
        if !market_open(now) {
            return None;
        }
        let state = self.state.lock().ok()?;
        let entry = state
            .entries
            .iter()
            .find(|e| e.series.metalbook_series == *series)?;
        if in_settlement_window(&entry.series, now)? {
            return None;
        }
        let spot = state
            .metals
            .iter()
            .find(|m| m.config.underlying == entry.series.underlying())?
            .cell
            .current(now)?;
        let mark = entry.mark?;
        if mark.spot_publish_time != spot.publish_time || mark.value <= Decimal::ZERO {
            return None;
        }
        Some(mark.value)
    }

    /// Seconds since the spot of the series' metal was published, for the status answer: the
    /// last accepted value however old, so a stale feed shows its age. `None` if the series is
    /// not listed or no spot was ever accepted.
    #[must_use]
    pub fn spot_age(&self, series: &Address) -> Option<i64> {
        let now = self.now();
        let state = self.state.lock().ok()?;
        let entry = state
            .entries
            .iter()
            .find(|e| e.series.metalbook_series == *series)?;
        let spot = state
            .metals
            .iter()
            .find(|m| m.config.underlying == entry.series.underlying())?
            .cell
            .last()?;
        now.checked_sub(spot.publish_time)
    }

    /// The `PriceFeed` of one metalbook series.
    #[must_use]
    pub fn feed(self: &Arc<Self>, series: Address) -> SeriesFeed {
        SeriesFeed {
            board: Arc::clone(self),
            series,
        }
    }

    fn now(&self) -> i64 {
        self.clock.now().get()
    }
}

impl State {
    /// One `price_strip` per expiry of `underlying`. Marks that cannot be priced are cleared, so
    /// an old mark never outlives a failure.
    fn reprice(&mut self, underlying: Underlying, now: i64) {
        for entry in self
            .entries
            .iter_mut()
            .filter(|e| e.series.underlying() == underlying)
        {
            entry.mark = None;
        }
        let Some(metal) = self
            .metals
            .iter()
            .find(|m| m.config.underlying == underlying)
        else {
            return;
        };
        let Some(spot) = metal.cell.current(now) else {
            return;
        };
        let inputs = MarkInputs {
            spot: spot.price,
            vol: metal.config.vol,
            rate: metal.config.rate,
            carry: metal.config.carry,
            now,
        };
        let quotable = |e: &Entry| {
            e.series.underlying() == underlying
                && in_settlement_window(&e.series, now) == Some(false)
        };
        let expiries: BTreeSet<i64> = self
            .entries
            .iter()
            .filter(|e| quotable(e))
            .map(|e| e.series.expiry())
            .collect();
        for expiry in expiries {
            let in_strip = |e: &&mut Entry| quotable(e) && e.series.expiry() == expiry;
            let terms: Vec<_> = self
                .entries
                .iter_mut()
                .filter(in_strip)
                .map(|e| e.series.terms)
                .collect();
            let marks = match price_expiry(&terms, &inputs) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(expiry, error = %e, "strip not priced");
                    continue;
                }
            };
            for (entry, mark) in self.entries.iter_mut().filter(in_strip).zip(marks) {
                entry.mark = Some(StoredMark {
                    value: mark.value,
                    spot_publish_time: spot.publish_time,
                });
            }
        }
    }
}

/// Whether `now` is at or past `expiry − window_s`; `None` if that cannot be computed, which the
/// callers treat as no quote.
fn in_settlement_window(series: &ListedSeries, now: i64) -> Option<bool> {
    let start = series.expiry().checked_sub(i64::from(series.window_s))?;
    Some(now >= start)
}

/// The mid of one series, for the maker service.
#[derive(Debug, Clone)]
pub struct SeriesFeed {
    board: Arc<Board>,
    series: Address,
}

impl PriceFeed for SeriesFeed {
    fn mid(&self) -> Option<Decimal> {
        self.board.mid(&self.series)
    }

    fn age_secs(&self) -> Option<i64> {
        self.board.spot_age(&self.series)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros,
        clippy::indexing_slicing,
        clippy::unwrap_used
    )]

    use std::str::FromStr;
    use std::sync::atomic::{AtomicI64, Ordering};

    use pyth_lite::RECEIVER_PROGRAM_ID;
    use quotebook_core::{BaseLots, Seconds};
    use quotebook_maker::market::{MarketInfo, Rounding};
    use quotebook_maker::pricing::{self, PricingParams};

    use super::*;
    use crate::config::{SeriesPair, UnderlyingConfig};
    use crate::marks::{price_expiry, MarkInputs};
    use crate::test_support::{capped_call, update, Update, FEED, WINDOW_S};

    /// Thursday 2026-11-05 18:13:20Z: the market is open.
    const NOW: i64 = 1_793_900_000;
    const EXPIRY: i64 = 1_794_603_600;

    #[derive(Clone)]
    struct TestClock(Arc<AtomicI64>);

    impl TimeSource for TestClock {
        fn now(&self) -> quotebook_core::UnixTimestamp {
            quotebook_core::UnixTimestamp::new(self.0.load(Ordering::SeqCst))
        }
    }

    fn address(n: u8) -> Address {
        Address::new_from_array([n; 32])
    }

    fn config() -> DemoConfig {
        DemoConfig {
            metalbook_program: address(90),
            quotebook_program: address(91),
            usdc_mint: address(92),
            underlyings: vec![UnderlyingConfig {
                underlying: Underlying::Xag,
                feed_account: address(93),
                feed_id: FEED,
                vol: Decimal::new(30, 2),
                rate: Decimal::new(4, 2),
                carry: Decimal::ZERO,
            }],
            pairs: vec![SeriesPair {
                metalbook_series: address(1),
                quotebook_series: address(2),
            }],
        }
    }

    fn board() -> (Arc<Board>, Arc<AtomicI64>) {
        let time = Arc::new(AtomicI64::new(NOW));
        let board = Arc::new(Board::new(
            &config(),
            Box::new(TestClock(Arc::clone(&time))),
        ));
        board.set_series(&[capped_call(1, 52, EXPIRY)]);
        (board, time)
    }

    fn read(board: &Board, publish_time: i64) -> Result<SpotPrice, SpotError> {
        board
            .ingest(
                Underlying::Xag,
                &RECEIVER_PROGRAM_ID,
                &update(Update::at(publish_time)),
            )
            .unwrap()
    }

    fn expected(series: &[ListedSeries], now: i64) -> Vec<Decimal> {
        let terms: Vec<_> = series.iter().map(|s| s.terms).collect();
        let inputs = MarkInputs {
            spot: Decimal::new(5_284_681, 5),
            vol: Decimal::new(30, 2),
            rate: Decimal::new(4, 2),
            carry: Decimal::ZERO,
            now,
        };
        price_expiry(&terms, &inputs)
            .unwrap()
            .into_iter()
            .map(|m| m.value)
            .collect()
    }

    #[test]
    fn a_fresh_spot_gives_the_mark() {
        let (board, _) = board();
        let series = capped_call(1, 52, EXPIRY);
        assert_eq!(
            board.feed(series.metalbook_series).mid(),
            None,
            "no spot yet"
        );
        read(&board, NOW).unwrap();
        let mid = board.feed(series.metalbook_series).mid().unwrap();
        assert_eq!(mid, expected(&[series], NOW)[0]);
        assert!(mid > Decimal::ZERO && mid < Decimal::new(52, 1));
    }

    #[test]
    fn the_status_reports_the_spot_age_even_when_stale() {
        let (board, time) = board();
        let series = capped_call(1, 52, EXPIRY).metalbook_series;
        let feed = board.feed(series);
        assert_eq!(feed.age_secs(), None, "no spot yet");
        read(&board, NOW - 40).unwrap();
        assert_eq!(feed.age_secs(), Some(40));
        // Past the age limit there is no mid, but the age is still reported.
        time.store(NOW + 900, Ordering::SeqCst);
        assert_eq!(feed.mid(), None);
        assert_eq!(feed.age_secs(), Some(940));
        assert_eq!(board.feed(address(77)).age_secs(), None, "not listed");
    }

    #[test]
    fn an_unknown_series_has_no_mid() {
        let (board, _) = board();
        read(&board, NOW).unwrap();
        assert_eq!(board.mid(&address(77)), None);
    }

    #[test]
    fn the_reader_stopping_with_a_value_in_hand_stops_quotes() {
        let (board, time) = board();
        let series = capped_call(1, 52, EXPIRY).metalbook_series;
        read(&board, NOW).unwrap();
        // No further read: the mark in hand holds only until the spot's age limit.
        time.store(NOW + 600, Ordering::SeqCst);
        assert!(board.mid(&series).is_some());
        time.store(NOW + 601, Ordering::SeqCst);
        assert_eq!(board.mid(&series), None);
    }

    #[test]
    fn a_stale_update_leaves_the_stored_value_to_its_own_limit() {
        let (board, time) = board();
        let series = capped_call(1, 52, EXPIRY).metalbook_series;
        read(&board, NOW).unwrap();
        time.store(NOW + 300, Ordering::SeqCst);
        assert_eq!(
            read(&board, NOW - 400),
            Err(SpotError::Pyth(pyth_lite::PythError::Stale))
        );
        assert!(board.mid(&series).is_some());
    }

    #[test]
    fn a_stale_feed_gives_no_mid_and_a_new_update_restores_it() {
        let (board, time) = board();
        let series = capped_call(1, 52, EXPIRY).metalbook_series;
        read(&board, NOW).unwrap();
        let later = NOW + 700;
        time.store(later, Ordering::SeqCst);
        assert!(read(&board, NOW).is_err());
        assert_eq!(board.mid(&series), None);
        read(&board, later - 5).unwrap();
        assert!(board.mid(&series).is_some());
    }

    #[test]
    fn a_broken_update_stops_quotes_at_once() {
        let (board, _) = board();
        let series = capped_call(1, 52, EXPIRY).metalbook_series;
        read(&board, NOW).unwrap();
        let future = NOW + 31;
        assert!(read(&board, future).is_err());
        assert_eq!(board.mid(&series), None);

        read(&board, NOW).unwrap();
        assert!(board.mid(&series).is_some());
        let bad = update(Update {
            exponent: 1,
            ..Update::at(NOW)
        });
        assert!(board
            .ingest(Underlying::Xag, &RECEIVER_PROGRAM_ID, &bad)
            .unwrap()
            .is_err());
        assert_eq!(board.mid(&series), None);
    }

    #[test]
    fn a_publish_time_going_backwards_does_not_move_the_mid() {
        let (board, _) = board();
        let series = capped_call(1, 52, EXPIRY).metalbook_series;
        read(&board, NOW).unwrap();
        let before = board.mid(&series).unwrap();
        let older = update(Update {
            price: 4_000_000,
            ..Update::at(NOW - 10)
        });
        assert!(board
            .ingest(Underlying::Xag, &RECEIVER_PROGRAM_ID, &older)
            .unwrap()
            .is_err());
        assert_eq!(board.mid(&series), Some(before));
    }

    #[test]
    fn the_closed_market_gives_no_mid_with_a_fresh_spot() {
        let (board, time) = board();
        let series = capped_call(1, 52, EXPIRY).metalbook_series;
        // Thursday 2026-11-05: 22:00Z is the daily break.
        let open = 1_793_915_999;
        let closed = 1_793_916_000;
        time.store(open, Ordering::SeqCst);
        read(&board, open).unwrap();
        assert!(board.mid(&series).is_some());
        time.store(closed, Ordering::SeqCst);
        read(&board, closed).unwrap();
        assert_eq!(board.mid(&series), None);
    }

    #[test]
    fn the_settlement_window_gives_no_mid() {
        let (board, time) = board();
        let series = capped_call(1, 52, EXPIRY).metalbook_series;
        let start = EXPIRY - i64::from(WINDOW_S);
        time.store(start - 1, Ordering::SeqCst);
        read(&board, start - 1).unwrap();
        assert!(board.mid(&series).is_some());
        time.store(start, Ordering::SeqCst);
        assert_eq!(board.mid(&series), None, "with the mark in hand");
        read(&board, start).unwrap();
        assert_eq!(board.mid(&series), None, "and after repricing");
    }

    #[test]
    fn each_expiry_gets_its_own_strip_and_marks_land_on_their_series() {
        let later = EXPIRY + 604_800;
        let series = [
            capped_call(1, 52, EXPIRY),
            capped_call(2, 50, later),
            capped_call(3, 48, EXPIRY),
            capped_call(4, 52, later),
        ];
        let (board, _) = board();
        board.set_series(&series);
        read(&board, NOW).unwrap();
        let first = expected(&[series[0], series[2]], NOW);
        let second = expected(&[series[1], series[3]], NOW);
        assert_eq!(board.mid(&series[0].metalbook_series), Some(first[0]));
        assert_eq!(board.mid(&series[2].metalbook_series), Some(first[1]));
        assert_eq!(board.mid(&series[1].metalbook_series), Some(second[0]));
        assert_eq!(board.mid(&series[3].metalbook_series), Some(second[1]));
    }

    #[test]
    fn a_zero_mark_gives_no_mid() {
        // Spot 52.85 against a strike of 800: the spread rounds to nothing, 18 decimals down.
        let deep = capped_call(9, 800, EXPIRY);
        let (board, _) = board();
        board.set_series(&[deep]);
        read(&board, NOW).unwrap();
        assert_eq!(expected(&[deep], NOW)[0], Decimal::ZERO);
        assert_eq!(board.mid(&deep.metalbook_series), None);
    }

    #[test]
    fn a_leg_outside_optfix_domain_clears_its_whole_expiry_only() {
        // S/K of 52.85 / 1,100 is below optfix's 0.05: its cap cannot be priced.
        let bad = capped_call(9, 1_000, EXPIRY);
        let same_expiry = capped_call(1, 52, EXPIRY);
        let other_expiry = capped_call(2, 52, EXPIRY + 604_800);
        let (board, _) = board();
        board.set_series(&[bad, same_expiry, other_expiry]);
        read(&board, NOW).unwrap();
        assert_eq!(board.mid(&bad.metalbook_series), None);
        assert_eq!(board.mid(&same_expiry.metalbook_series), None);
        assert!(board.mid(&other_expiry.metalbook_series).is_some());
    }

    #[test]
    fn ticks_round_toward_the_maker() {
        let (board, _) = board();
        let series = capped_call(1, 52, EXPIRY).metalbook_series;
        read(&board, NOW).unwrap();
        let mid = board.mid(&series).unwrap();
        // The quotebook series of the I9 demo: 0-decimal base, USDC quote, 0.01 USDC per tick.
        let market = MarketInfo {
            program_id: address(91),
            deployment: [0u8; 16],
            series: address(2),
            base_decimals: 0,
            quote_decimals: 6,
            base_lot_size: 1,
            quote_lot_size: 10_000,
            max_quote_ttl: Seconds::new(60),
            min_size: BaseLots::new(1),
        };
        let down = market.price_to_ticks(mid, Rounding::Down).unwrap().get();
        let up = market.price_to_ticks(mid, Rounding::Up).unwrap().get();
        let tick = Decimal::from_str("0.01").unwrap();
        assert_eq!(up, down + 1);
        assert!(Decimal::from(down) * tick <= mid);
        assert!(Decimal::from(up) * tick >= mid);

        let quoted = pricing::quote(
            mid,
            Decimal::ZERO,
            &PricingParams::relative_defaults(),
            &market,
        )
        .unwrap();
        assert!(Decimal::from(quoted.bid_ticks.get()) * tick <= mid);
        assert!(Decimal::from(quoted.ask_ticks.get()) * tick >= mid);
    }
}
