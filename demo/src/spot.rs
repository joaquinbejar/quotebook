//! The spot value from a Pyth push-feed account, and every check on it.
//!
//! `pyth-lite` checks the owner, discriminator, length, level Full, feed id, positive price, age
//! and confidence. It treats a `publish_time` ahead of `now` as fresh and does not check the
//! exponent, so this module adds both, and refuses a `publish_time` lower than the last one
//! accepted.
//!
//! What each outcome does to the stored value:
//! - a read that failed (the RPC) changes nothing: the old value stands until the age limit;
//! - an update older than the last accepted, or itself too old, changes nothing either (an RPC
//!   node lagging, or the feed quiet), and the age limit applies to what is stored;
//! - any other refusal means the feed is broken or too uncertain, so there is no value until a
//!   good update arrives.
//!
//! Age is computed again on every [`SpotCell::current`], so if the reader stops, quotes stop too.

use pyth_lite::{FeedId, PriceRequirements, PriceUpdate, PythError, RECEIVER_PROGRAM_ID};
use rust_decimal::Decimal;

/// A spot older than this gives no quote, seconds. Twice the feeds' observed cadence.
pub const MAX_SPOT_AGE_SECS: i64 = 600;
/// A wider `conf / price` gives no quote, basis points.
pub const MAX_CONF_BPS: u16 = 50;
/// A `publish_time` further ahead of `now` than this is refused, seconds.
pub const MAX_FUTURE_SECS: i64 = 30;
/// The lowest exponent accepted.
pub const MIN_EXPONENT: i32 = -12;
/// The highest exponent accepted.
pub const MAX_EXPONENT: i32 = 0;

/// An accepted spot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpotPrice {
    /// USD per ounce, exactly the feed's `price × 10^exponent`.
    pub price: Decimal,
    /// The update's `publish_time`, Unix seconds.
    pub publish_time: i64,
}

/// Why an update was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SpotError {
    /// A `pyth-lite` check failed.
    #[error("pyth: {0}")]
    Pyth(PythError),
    /// `publish_time` is more than [`MAX_FUTURE_SECS`] ahead of `now`.
    #[error("publish_time {publish_time} is ahead of now {now}")]
    Future {
        /// The update's `publish_time`.
        publish_time: i64,
        /// The time of the check.
        now: i64,
    },
    /// The exponent is outside [`MIN_EXPONENT`]`..=`[`MAX_EXPONENT`].
    #[error("exponent {0} out of range")]
    Exponent(i32),
    /// `publish_time` is lower than the last one accepted.
    #[error("publish_time {publish_time} is below the last accepted {last}")]
    Backwards {
        /// The update's `publish_time`.
        publish_time: i64,
        /// The last accepted `publish_time`.
        last: i64,
    },
    /// The price does not fit a `Decimal`.
    #[error("price does not fit")]
    Overflow,
}

/// One feed's latest accepted value.
#[derive(Debug, Clone, Copy)]
pub struct SpotCell {
    feed_id: FeedId,
    last: Option<SpotPrice>,
    broken: bool,
}

impl SpotCell {
    /// An empty cell for `feed_id`.
    #[must_use]
    pub const fn new(feed_id: [u8; 32]) -> Self {
        Self {
            feed_id: FeedId::new(feed_id),
            last: None,
            broken: false,
        }
    }

    /// Checks one read of the feed account at `now`, and stores it if it passes.
    ///
    /// # Errors
    ///
    /// [`SpotError`]. See the module documentation for what each does to the stored value.
    pub fn ingest(
        &mut self,
        owner: &[u8; 32],
        data: &[u8],
        now: i64,
    ) -> Result<SpotPrice, SpotError> {
        let accepted = self.check(owner, data, now);
        match accepted {
            Ok(spot) => {
                self.last = Some(spot);
                self.broken = false;
            }
            Err(SpotError::Backwards { .. } | SpotError::Pyth(PythError::Stale)) => {}
            Err(_) => self.broken = true,
        }
        accepted
    }

    fn check(&self, owner: &[u8; 32], data: &[u8], now: i64) -> Result<SpotPrice, SpotError> {
        let update =
            PriceUpdate::load(owner, data, &RECEIVER_PROGRAM_ID).map_err(SpotError::Pyth)?;
        let max_age = MAX_SPOT_AGE_SECS.unsigned_abs();
        let requirements = PriceRequirements::new(self.feed_id, Some(max_age), MAX_CONF_BPS);
        let verified = update
            .verify(&requirements, pyth_lite::UnixTimestamp::new(now))
            .map_err(SpotError::Pyth)?;
        let publish_time = verified.publish_time().get();
        if ahead(publish_time, now) {
            return Err(SpotError::Future { publish_time, now });
        }
        let exponent = verified.exponent();
        if !(MIN_EXPONENT..=MAX_EXPONENT).contains(&exponent) {
            return Err(SpotError::Exponent(exponent));
        }
        if let Some(last) = self.last {
            if publish_time < last.publish_time {
                return Err(SpotError::Backwards {
                    publish_time,
                    last: last.publish_time,
                });
            }
        }
        let scale = exponent.unsigned_abs();
        let price = Decimal::try_from_i128_with_scale(i128::from(verified.price().get()), scale)
            .map_err(|_| SpotError::Overflow)?;
        Ok(SpotPrice {
            price,
            publish_time,
        })
    }

    /// The value to price on at `now`, or `None`: no good update yet, the feed broken, older than
    /// [`MAX_SPOT_AGE_SECS`], or too far ahead of `now`.
    #[must_use]
    pub fn current(&self, now: i64) -> Option<SpotPrice> {
        if self.broken {
            return None;
        }
        let spot = self.last?;
        let age = now.checked_sub(spot.publish_time)?;
        if age > MAX_SPOT_AGE_SECS || ahead(spot.publish_time, now) {
            return None;
        }
        Some(spot)
    }
}

/// An unrepresentable difference counts as ahead.
fn ahead(publish_time: i64, now: i64) -> bool {
    publish_time
        .checked_sub(now)
        .is_none_or(|lead| lead > MAX_FUTURE_SECS)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_macros, clippy::unwrap_used)]

    use super::*;
    use crate::test_support::{update, Update, FEED};

    const T: i64 = 1_793_900_000;

    fn cell_with(publish_time: i64) -> SpotCell {
        let mut cell = SpotCell::new(FEED);
        let good = update(Update::at(publish_time));
        cell.ingest(&RECEIVER_PROGRAM_ID, &good, publish_time)
            .unwrap();
        cell
    }

    #[test]
    fn a_good_update_is_exact() {
        let mut cell = SpotCell::new(FEED);
        let spot = cell
            .ingest(&RECEIVER_PROGRAM_ID, &update(Update::at(T)), T)
            .unwrap();
        // 5284681 × 10^-5.
        assert_eq!(spot.price, Decimal::new(5_284_681, 5));
        assert_eq!(cell.current(T), Some(spot));
    }

    #[test]
    fn age_is_checked_on_every_read_of_the_cell() {
        let cell = cell_with(T);
        assert!(cell.current(T + MAX_SPOT_AGE_SECS).is_some());
        assert!(cell.current(T + MAX_SPOT_AGE_SECS + 1).is_none());
    }

    #[test]
    fn a_stale_update_is_refused_and_the_old_value_ages_out() {
        let mut cell = cell_with(T);
        let now = T + 300;
        let old = update(Update::at(now - MAX_SPOT_AGE_SECS - 1));
        assert_eq!(
            cell.ingest(&RECEIVER_PROGRAM_ID, &old, now),
            Err(SpotError::Pyth(PythError::Stale))
        );
        assert_eq!(cell.current(now).unwrap().publish_time, T);
    }

    #[test]
    fn a_future_publish_time_is_refused_past_the_tolerance() {
        let mut cell = SpotCell::new(FEED);
        let within = update(Update::at(T + MAX_FUTURE_SECS));
        assert!(cell.ingest(&RECEIVER_PROGRAM_ID, &within, T).is_ok());

        let mut cell = cell_with(T);
        let beyond = update(Update::at(T + MAX_FUTURE_SECS + 1));
        assert_eq!(
            cell.ingest(&RECEIVER_PROGRAM_ID, &beyond, T),
            Err(SpotError::Future {
                publish_time: T + MAX_FUTURE_SECS + 1,
                now: T
            })
        );
        assert!(cell.current(T).is_none(), "a broken feed gives no value");
    }

    #[test]
    fn a_clock_moving_back_cannot_make_a_future_value_fresh() {
        let cell = cell_with(T);
        assert!(cell.current(T - MAX_FUTURE_SECS).is_some());
        assert!(cell.current(T - MAX_FUTURE_SECS - 1).is_none());
    }

    #[test]
    fn an_out_of_range_exponent_is_refused() {
        for exponent in [1, MIN_EXPONENT - 1] {
            let mut cell = cell_with(T);
            let bad = update(Update {
                exponent,
                ..Update::at(T)
            });
            assert_eq!(
                cell.ingest(&RECEIVER_PROGRAM_ID, &bad, T),
                Err(SpotError::Exponent(exponent))
            );
            assert!(cell.current(T).is_none());
        }
        for exponent in [MIN_EXPONENT, MAX_EXPONENT] {
            let mut cell = SpotCell::new(FEED);
            let good = update(Update {
                exponent,
                ..Update::at(T)
            });
            assert!(cell.ingest(&RECEIVER_PROGRAM_ID, &good, T).is_ok());
        }
    }

    #[test]
    fn a_publish_time_going_backwards_keeps_the_newer_value() {
        let mut cell = cell_with(T);
        let older = update(Update {
            price: 1_000_000,
            ..Update::at(T - 10)
        });
        assert_eq!(
            cell.ingest(&RECEIVER_PROGRAM_ID, &older, T),
            Err(SpotError::Backwards {
                publish_time: T - 10,
                last: T
            })
        );
        let spot = cell.current(T).unwrap();
        assert_eq!(spot.publish_time, T);
        assert_eq!(spot.price, Decimal::new(5_284_681, 5));
    }

    #[test]
    fn the_same_update_read_twice_is_fine() {
        let mut cell = cell_with(T);
        assert!(cell
            .ingest(&RECEIVER_PROGRAM_ID, &update(Update::at(T)), T + 60)
            .is_ok());
    }

    #[test]
    fn a_wide_confidence_gives_no_value_until_a_good_update() {
        let mut cell = cell_with(T);
        // 50 bps of 5,284,681 is 26,423.4.
        let at_limit = update(Update {
            conf: 26_423,
            ..Update::at(T)
        });
        assert!(cell.ingest(&RECEIVER_PROGRAM_ID, &at_limit, T).is_ok());
        let wide = update(Update {
            conf: 26_424,
            ..Update::at(T + 5)
        });
        assert_eq!(
            cell.ingest(&RECEIVER_PROGRAM_ID, &wide, T + 5),
            Err(SpotError::Pyth(PythError::ConfidenceTooWide))
        );
        assert!(
            cell.current(T + 5).is_none(),
            "the old value does not stand"
        );
        let good = update(Update::at(T + 10));
        assert!(cell.ingest(&RECEIVER_PROGRAM_ID, &good, T + 10).is_ok());
        assert!(cell.current(T + 10).is_some());
    }

    #[test]
    fn wrong_owner_feed_level_and_price_are_refused() {
        let owner = [9u8; 32];
        let mut cell = cell_with(T);
        assert_eq!(
            cell.ingest(&owner, &update(Update::at(T)), T),
            Err(SpotError::Pyth(PythError::WrongOwner))
        );
        assert!(cell.current(T).is_none());

        let mut cell = cell_with(T);
        let other = update(Update {
            feed: [7u8; 32],
            ..Update::at(T)
        });
        assert_eq!(
            cell.ingest(&RECEIVER_PROGRAM_ID, &other, T),
            Err(SpotError::Pyth(PythError::WrongFeed))
        );

        let mut cell = cell_with(T);
        let partial = update(Update {
            full: false,
            ..Update::at(T)
        });
        assert_eq!(
            cell.ingest(&RECEIVER_PROGRAM_ID, &partial, T),
            Err(SpotError::Pyth(PythError::NotFullyVerified))
        );

        let mut cell = cell_with(T);
        let negative = update(Update {
            price: -1,
            ..Update::at(T)
        });
        assert_eq!(
            cell.ingest(&RECEIVER_PROGRAM_ID, &negative, T),
            Err(SpotError::Pyth(PythError::NonPositivePrice))
        );

        let mut cell = cell_with(T);
        assert_eq!(
            cell.ingest(&RECEIVER_PROGRAM_ID, &[0u8; 134], T),
            Err(SpotError::Pyth(PythError::WrongDiscriminator))
        );
    }
}
