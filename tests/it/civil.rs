//! Civil-date arithmetic for tests that write or read RFC 3339 instants
//! without a date dependency. The conversions are Howard Hinnant's
//! `days_from_civil` and `civil_from_days`, and hold for any proleptic
//! Gregorian date.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The wall clock as time since the Unix epoch.
pub fn since_epoch() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
}

/// Days since 1970-01-01 of a civil date; `month` and `day` count from 1.
pub fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date `(year, month, day)` that is `days` after 1970-01-01.
pub fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The RFC 3339 UTC instant `secs` seconds after the epoch, to the second.
pub fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Epoch seconds of an RFC 3339 instant whose offset is UTC (`Z` or
/// `+00:00`). The offset is not read, and a fraction of a second is
/// dropped.
pub fn utc_epoch_seconds(ts: &str) -> Option<i64> {
    let (date, rest) = ts.split_once('T')?;
    let time = rest.split(['+', 'Z']).next()?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let mut t = time.split(':');
    let (hh, mm, ss): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next().unwrap_or("0").split('.').next()?.parse().ok()?,
    );
    Some(days_from_civil(y, m, day) * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// Every day from 1600-01-01 to 2400-12-31 converts to a civil date and
/// back, and consecutive days give consecutive dates. The range holds the
/// century years 1700, 1800 and 1900 (not leap) and 2000 (leap).
#[test]
fn civil_dates_round_trip() {
    let first = days_from_civil(1600, 1, 1);
    let last = days_from_civil(2400, 12, 31);
    let mut prev = civil_from_days(first - 1);
    for days in first..=last {
        let (y, m, d) = civil_from_days(days);
        assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        let next_day = (prev.0, prev.1, prev.2 + 1);
        let next_month = (prev.0, prev.1 + 1, 1);
        let next_year = (prev.0 + 1, 1, 1);
        assert!(
            [next_day, next_month, next_year].contains(&(y, m, d)),
            "{prev:?} is followed by {y}-{m}-{d}"
        );
        prev = (y, m, d);
    }
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(
        civil_from_days(days_from_civil(2000, 2, 29) + 1),
        (2000, 3, 1)
    );
    assert_eq!(
        civil_from_days(days_from_civil(1900, 2, 28) + 1),
        (1900, 3, 1)
    );
    assert_eq!(
        civil_from_days(days_from_civil(2024, 2, 28) + 1),
        (2024, 2, 29)
    );
    assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    assert_eq!(
        utc_epoch_seconds("2000-02-29T00:00:00+00:00"),
        Some(951_782_400)
    );
}
