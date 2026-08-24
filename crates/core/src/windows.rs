//! Date-window math for local-day / week / month / rolling 5-hour buckets.
//!
//! All functions take the timezone explicitly so tests can pin it; production callers
//! pass `chrono::Local`.

use chrono::{DateTime, Datelike, TimeZone, Utc, Weekday};

/// Length of the rolling 5-hour window, in seconds (shared by the active block and the
/// enrichment floor).
pub const BLOCK_WINDOW_SECS: i64 = 5 * 3600;

pub fn block_window() -> chrono::Duration {
    chrono::Duration::try_seconds(BLOCK_WINDOW_SECS).expect("valid window")
}

/// Canonical `Z` RFC-3333 (chronos `to_rfc3339` emits `+00:00` for Utc in 0.4.45).
pub fn iso_utc(d: DateTime<Utc>) -> String {
    d.format("%Y-%m-%dT%H:%M:%S%.fZ").to_string()
}

/// Local calendar day key `yyyy-MM-dd`.
pub fn local_day(d: DateTime<Utc>, tz: &impl TimeZone) -> String {
    d.with_timezone(tz)
        .naive_local()
        .format("%Y-%m-%d")
        .to_string()
}

/// Local month key `yyyy-MM`.
pub fn month_key(d: DateTime<Utc>, tz: &impl TimeZone) -> String {
    d.with_timezone(tz)
        .naive_local()
        .format("%Y-%m")
        .to_string()
}

/// First instant of the given local day, as UTC.
fn midnight_local(d: DateTime<Utc>, day: chrono::NaiveDate, tz: &impl TimeZone) -> DateTime<Utc> {
    let naive = day.and_hms_opt(0, 0, 0).expect("valid midnight");
    tz.from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.to_utc())
        .unwrap_or(d)
}

/// Start of the local month (first day, 00:00 local) in UTC.
pub fn start_of_month(d: DateTime<Utc>, tz: &impl TimeZone) -> DateTime<Utc> {
    let nd = d.with_timezone(tz).date_naive();
    let first = chrono::NaiveDate::from_ymd_opt(nd.year(), nd.month(), 1).expect("valid date");
    midnight_local(d, first, tz)
}

/// Start of the local week for a configurable first weekday, in UTC.
pub fn start_of_week(d: DateTime<Utc>, tz: &impl TimeZone, first_day: Weekday) -> DateTime<Utc> {
    let nd = d.with_timezone(tz).date_naive();
    let fwd = first_day.num_days_from_sunday() as i32;
    let wd = nd.weekday().num_days_from_sunday() as i32;
    let delta = if wd < fwd { wd + 7 } else { wd } - fwd;
    let start = nd - chrono::Duration::days(delta as i64);
    midnight_local(d, start, tz)
}

/// Earliest start among month / week / rolling-5h — the floor a single scan must begin at
/// so all three enrichment windows are covered (append-only log assumption).
pub fn enrichment_scan_start(
    d: DateTime<Utc>,
    tz: &impl TimeZone,
    first_day: Weekday,
) -> DateTime<Utc> {
    let m = start_of_month(d, tz);
    let w = start_of_week(d, tz, first_day);
    let rolling = d - block_window();
    m.min(w).min(rolling)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, FixedOffset, Utc, Weekday};

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }
    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn local_day_fmt() {
        assert_eq!(local_day(at("2026-01-02T03:04:05Z"), &utc()), "2026-01-02");
    }

    #[test]
    fn month_key_fmt() {
        assert_eq!(month_key(at("2026-08-19T00:00:00Z"), &utc()), "2026-08");
    }

    #[test]
    fn start_of_month_ok() {
        assert_eq!(
            iso_utc(start_of_month(at("2026-02-01T01:00:00+00:00"), &utc())),
            "2026-02-01T00:00:00Z"
        );
    }

    #[test]
    fn start_of_week_monday() {
        // 2026-02-01 is a Sunday; Monday week starts 2026-01-26.
        assert_eq!(
            iso_utc(start_of_week(
                at("2026-02-01T01:00:00+00:00"),
                &utc(),
                Weekday::Mon
            )),
            "2026-01-26T00:00:00Z"
        );
    }

    #[test]
    fn enrichment_scan_start_is_earliest() {
        // Early in the month the week start can fall in the previous month → earliest wins.
        let s = enrichment_scan_start(at("2026-02-01T01:00:00+00:00"), &utc(), Weekday::Mon);
        assert_eq!(iso_utc(s), "2026-01-26T00:00:00Z");
    }
}
