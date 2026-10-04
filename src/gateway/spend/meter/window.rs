//! UTC calendar windows for the spend meter.
//!
//! Everything derives from one `now` in Unix seconds, so the daily, weekly and
//! monthly windows of a single request can never disagree about which instant
//! they were computed at.

use super::super::store::Period;

const DAY: u64 = 86_400;

/// One metering window: `[start, end)` in Unix seconds, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: u64,
    /// The instant the window resets, i.e. the next window's start.
    pub end: u64,
}

/// The window of `period` containing `now_secs`: daily starts 00:00 UTC,
/// weekly starts Monday 00:00 UTC, monthly starts the 1st 00:00 UTC.
pub fn window(period: Period, now_secs: u64) -> Window {
    let days = now_secs / DAY;
    match period {
        Period::Daily => Window {
            start: days * DAY,
            end: (days + 1) * DAY,
        },
        Period::Weekly => {
            // Day 0 (1970-01-01) was a Thursday, so Monday sits 3 days later.
            let since_monday = (days + 3) % 7;
            let monday = days.saturating_sub(since_monday);
            Window {
                start: monday * DAY,
                end: (monday + 7) * DAY,
            }
        }
        Period::Monthly => {
            let (year, month, _) = civil_from_days(days as i64);
            let (next_year, next_month) = if month == 12 {
                (year + 1, 1)
            } else {
                (year, month + 1)
            };
            Window {
                start: days_from_civil(year, month, 1) as u64 * DAY,
                end: days_from_civil(next_year, next_month, 1) as u64 * DAY,
            }
        }
    }
}

/// The first instant of the calendar month `months` before the one containing
/// `now_secs`; the retention horizon for monthly-granular pruning.
pub fn months_back_start(now_secs: u64, months: u64) -> u64 {
    // A thousand years is far past any retention; the clamp keeps the calendar
    // arithmetic below inside `i64`.
    let months = months.min(12_000);
    let (year, month, _) = civil_from_days((now_secs / DAY) as i64);
    let index = year
        .saturating_mul(12)
        .saturating_add(month - 1)
        .saturating_sub(i64::try_from(months).unwrap_or(i64::MAX));
    let (year, month) = (index.div_euclid(12), index.rem_euclid(12) + 1);
    // Before the Unix epoch there is nothing to prune.
    u64::try_from(days_from_civil(year, month, 1)).map_or(0, |days| days * DAY)
}

/// `YYYY-MM-DD 00:00 UTC` for the day containing `secs`. Every window ends at
/// a UTC midnight, so this is how a reset instant is shown to a client.
pub fn reset_label(secs: u64) -> String {
    let (year, month, day) = civil_from_days((secs / DAY) as i64);
    format!("{year:04}-{month:02}-{day:02} 00:00 UTC")
}

/// Howard Hinnant's `civil_from_days`: `(year, month, day)` of a Unix day.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Howard Hinnant's `days_from_civil`: the Unix day of a civil date.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
