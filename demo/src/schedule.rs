//! The metals market's listed closures, in New York time.
//!
//! Only two windows exist: the daily break, 17:00 to 18:00, and the weekend, Friday 17:00 to
//! Sunday 18:00. They are computed in New York time so daylight saving moves them with the market
//! (22:00 to 23:00Z in winter, 21:00 to 22:00Z in summer). A holiday or any other stop is not
//! listed: there the spot's age stops quotes ([`crate::spot`]).

const SECONDS_PER_DAY: i64 = 86_400;
const SECONDS_PER_HOUR: i64 = 3_600;
const DAYS_PER_WEEK: i64 = 7;
/// Days between 0000-03-01 and 1970-01-01.
const EPOCH_SHIFT: i64 = 719_468;
const DAYS_PER_ERA: i64 = 146_097;
const YEARS_PER_ERA: i64 = 400;

const CLOSE_HOUR: i64 = 17;
const OPEN_HOUR: i64 = 18;
const SUNDAY: i64 = 0;
const FRIDAY: i64 = 5;
const SATURDAY: i64 = 6;

/// Whether the metals market is open at `now`, Unix seconds. An out-of-range `now` counts as
/// closed.
#[must_use]
pub fn market_open(now: i64) -> bool {
    open(now).unwrap_or(false)
}

/// One listed closure: closed from `start` (included) to `end` (excluded), Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closure {
    /// The first closed second.
    pub start: i64,
    /// The first open second after it: the scheduled reopening.
    pub end: i64,
}

/// The longest listed closure is the weekend, 49 hours; a search stops after this many.
const MAX_CLOSURE_HOURS: i64 = 72;
/// How far back [`last_closure_before`] looks: more than a week, so it always finds one.
const MAX_LOOKBACK_HOURS: i64 = 8 * 24;

/// The listed closure that contains `now`, or `None` when the market is open (or `now` is out of
/// range). Both windows start and end on a whole UTC hour, since New York's offset is a whole
/// number of hours and daylight saving changes on one, so the search steps by hours.
#[must_use]
pub fn closure_at(now: i64) -> Option<Closure> {
    if open(now)? {
        return None;
    }
    let hour = now.checked_sub(now.checked_rem_euclid(SECONDS_PER_HOUR)?)?;
    let mut start = hour;
    for _ in 0..MAX_CLOSURE_HOURS {
        let before = start.checked_sub(SECONDS_PER_HOUR)?;
        if open(before)? {
            break;
        }
        start = before;
    }
    let mut end = hour.checked_add(SECONDS_PER_HOUR)?;
    for _ in 0..MAX_CLOSURE_HOURS {
        if open(end)? {
            return Some(Closure { start, end });
        }
        end = end.checked_add(SECONDS_PER_HOUR)?;
    }
    None
}

/// The latest listed closure that ended at or before `now`.
#[must_use]
pub fn last_closure_before(now: i64) -> Option<Closure> {
    let mut hour = now.checked_sub(now.checked_rem_euclid(SECONDS_PER_HOUR)?)?;
    for _ in 0..MAX_LOOKBACK_HOURS {
        let before = hour.checked_sub(1)?;
        if open(hour)? && !open(before)? {
            return closure_at(before);
        }
        hour = hour.checked_sub(SECONDS_PER_HOUR)?;
    }
    None
}

fn open(now: i64) -> Option<bool> {
    let offset_hours: i64 = if in_daylight_time(now)? { -4 } else { -5 };
    let local = now.checked_add(offset_hours.checked_mul(SECONDS_PER_HOUR)?)?;
    let days = local.checked_div_euclid(SECONDS_PER_DAY)?;
    let hour = local
        .checked_rem_euclid(SECONDS_PER_DAY)?
        .checked_div(SECONDS_PER_HOUR)?;
    let closed = match weekday(days)? {
        SATURDAY => true,
        FRIDAY => hour >= CLOSE_HOUR,
        SUNDAY => hour < OPEN_HOUR,
        _ => hour == CLOSE_HOUR,
    };
    Some(!closed)
}

/// US daylight time: from the second Sunday of March, 02:00 local (07:00Z), until the first
/// Sunday of November, 02:00 local (06:00Z).
fn in_daylight_time(now: i64) -> Option<bool> {
    let year = year_of(now.checked_div_euclid(SECONDS_PER_DAY)?)?;
    let start = first_sunday(year, 3)?.checked_add(DAYS_PER_WEEK)?;
    let start = start
        .checked_mul(SECONDS_PER_DAY)?
        .checked_add(7_i64.checked_mul(SECONDS_PER_HOUR)?)?;
    let end = first_sunday(year, 11)?
        .checked_mul(SECONDS_PER_DAY)?
        .checked_add(6_i64.checked_mul(SECONDS_PER_HOUR)?)?;
    Some(now >= start && now < end)
}

/// The first Sunday of a month, in days since 1970-01-01.
fn first_sunday(year: i64, month: i64) -> Option<i64> {
    let first = days_from_civil(year, month, 1)?;
    let ahead = DAYS_PER_WEEK
        .checked_sub(weekday(first)?)?
        .checked_rem_euclid(DAYS_PER_WEEK)?;
    first.checked_add(ahead)
}

/// 0 is Sunday. 1970-01-01 was a Thursday.
fn weekday(days: i64) -> Option<i64> {
    days.checked_add(4)?.checked_rem_euclid(DAYS_PER_WEEK)
}

/// Days since 1970-01-01 of a civil date (March-based years; a year's era has 146,097 days).
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    let y = if month <= 2 {
        year.checked_sub(1)?
    } else {
        year
    };
    let era = y.checked_div_euclid(YEARS_PER_ERA)?;
    let yoe = y.checked_rem_euclid(YEARS_PER_ERA)?;
    let shifted_month = month.checked_add(9)?.checked_rem_euclid(12)?;
    let doy = shifted_month
        .checked_mul(153)?
        .checked_add(2)?
        .checked_div(5)?
        .checked_add(day)?
        .checked_sub(1)?;
    let doe = yoe
        .checked_mul(365)?
        .checked_add(yoe.checked_div(4)?)?
        .checked_sub(yoe.checked_div(100)?)?
        .checked_add(doy)?;
    era.checked_mul(DAYS_PER_ERA)?
        .checked_add(doe)?
        .checked_sub(EPOCH_SHIFT)
}

/// The civil year of a day number since 1970-01-01.
fn year_of(days: i64) -> Option<i64> {
    let z = days.checked_add(EPOCH_SHIFT)?;
    let era = z.checked_div_euclid(DAYS_PER_ERA)?;
    let doe = z.checked_rem_euclid(DAYS_PER_ERA)?;
    let yoe = doe
        .checked_sub(doe.checked_div(1_460)?)?
        .checked_add(doe.checked_div(36_524)?)?
        .checked_sub(doe.checked_div(146_096)?)?
        .checked_div(365)?;
    let doy = doe.checked_sub(
        yoe.checked_mul(365)?
            .checked_add(yoe.checked_div(4)?)?
            .checked_sub(yoe.checked_div(100)?)?,
    )?;
    let march_year = yoe.checked_add(era.checked_mul(YEARS_PER_ERA)?)?;
    // March-based day of year: 306 is January 1 of the next civil year.
    if doy >= 306 {
        march_year.checked_add(1)
    } else {
        Some(march_year)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros,
        clippy::unwrap_used
    )]

    use super::*;

    /// Unix seconds of a UTC date and time.
    fn at(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
        days_from_civil(y, mo, d).unwrap() * SECONDS_PER_DAY + h * 3_600 + mi * 60 + s
    }

    #[test]
    fn civil_dates_are_right() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(at(2026, 11, 13, 21, 0, 0), 1_794_603_600);
        assert_eq!(year_of(days_from_civil(2026, 1, 1).unwrap()), Some(2026));
        assert_eq!(year_of(days_from_civil(2026, 12, 31).unwrap()), Some(2026));
        assert_eq!(year_of(days_from_civil(2028, 2, 29).unwrap()), Some(2028));
        assert_eq!(weekday(days_from_civil(2026, 11, 8).unwrap()), Some(SUNDAY));
    }

    #[test]
    fn the_measured_winter_break_is_closed() {
        // The 24-hour sample's one gap: 21:59:59Z to 23:00:28Z on Wednesday 2026-11-04.
        assert!(market_open(at(2026, 11, 4, 21, 59, 59)));
        assert!(!market_open(at(2026, 11, 4, 22, 0, 0)));
        assert!(!market_open(at(2026, 11, 4, 22, 59, 59)));
        assert!(market_open(at(2026, 11, 4, 23, 0, 0)));
    }

    #[test]
    fn the_summer_break_moves_an_hour() {
        assert!(market_open(at(2026, 7, 8, 20, 59, 59)));
        assert!(!market_open(at(2026, 7, 8, 21, 0, 0)));
        assert!(!market_open(at(2026, 7, 8, 21, 59, 59)));
        assert!(market_open(at(2026, 7, 8, 22, 0, 0)));
    }

    #[test]
    fn the_weekend_is_closed() {
        assert!(market_open(at(2026, 11, 6, 21, 59, 59)));
        assert!(!market_open(at(2026, 11, 6, 22, 0, 0)));
        assert!(!market_open(at(2026, 11, 7, 12, 0, 0)));
        assert!(!market_open(at(2026, 11, 8, 22, 59, 59)));
        assert!(market_open(at(2026, 11, 8, 23, 0, 0)));
    }

    #[test]
    fn daylight_time_changes_on_the_right_instants() {
        // 2026: March 8 and November 1.
        assert!(!in_daylight_time(at(2026, 3, 8, 6, 59, 59)).unwrap());
        assert!(in_daylight_time(at(2026, 3, 8, 7, 0, 0)).unwrap());
        assert!(in_daylight_time(at(2026, 11, 1, 5, 59, 59)).unwrap());
        assert!(!in_daylight_time(at(2026, 11, 1, 6, 0, 0)).unwrap());
    }

    #[test]
    fn the_daily_break_is_one_closure() {
        let winter = Closure {
            start: at(2026, 11, 4, 22, 0, 0),
            end: at(2026, 11, 4, 23, 0, 0),
        };
        assert_eq!(closure_at(at(2026, 11, 4, 21, 59, 59)), None);
        assert_eq!(closure_at(winter.start), Some(winter));
        assert_eq!(closure_at(at(2026, 11, 4, 22, 30, 0)), Some(winter));
        assert_eq!(closure_at(winter.end), None);
        let summer = Closure {
            start: at(2026, 7, 8, 21, 0, 0),
            end: at(2026, 7, 8, 22, 0, 0),
        };
        assert_eq!(closure_at(at(2026, 7, 8, 21, 59, 59)), Some(summer));
    }

    #[test]
    fn the_weekend_is_one_closure_of_49_hours() {
        let weekend = Closure {
            start: at(2026, 11, 6, 22, 0, 0),
            end: at(2026, 11, 8, 23, 0, 0),
        };
        assert_eq!(closure_at(at(2026, 11, 7, 12, 0, 0)), Some(weekend));
        assert_eq!(closure_at(at(2026, 11, 8, 22, 59, 59)), Some(weekend));
        assert_eq!(weekend.end - weekend.start, 49 * 3_600);
    }

    #[test]
    fn a_weekend_across_the_daylight_change_is_one_closure() {
        // Friday 17:00 EDT to Sunday 18:00 EST: 50 hours.
        let weekend = Closure {
            start: at(2026, 10, 30, 21, 0, 0),
            end: at(2026, 11, 1, 23, 0, 0),
        };
        assert_eq!(closure_at(at(2026, 10, 31, 12, 0, 0)), Some(weekend));
        assert_eq!(weekend.end - weekend.start, 50 * 3_600);
    }

    #[test]
    fn the_last_closure_is_the_latest_that_ended() {
        let winter = Closure {
            start: at(2026, 11, 4, 22, 0, 0),
            end: at(2026, 11, 4, 23, 0, 0),
        };
        assert_eq!(last_closure_before(winter.end), Some(winter));
        assert_eq!(last_closure_before(at(2026, 11, 5, 12, 0, 0)), Some(winter));
        // Inside the next day's break, the previous one is still the last that ended.
        assert_eq!(
            last_closure_before(at(2026, 11, 5, 22, 30, 0)),
            Some(winter)
        );
        let weekend = Closure {
            start: at(2026, 11, 6, 22, 0, 0),
            end: at(2026, 11, 8, 23, 0, 0),
        };
        assert_eq!(
            last_closure_before(at(2026, 11, 8, 23, 5, 0)),
            Some(weekend)
        );
    }

    #[test]
    fn an_out_of_range_time_is_closed() {
        assert!(!market_open(i64::MAX));
        assert!(!market_open(i64::MIN));
    }
}
