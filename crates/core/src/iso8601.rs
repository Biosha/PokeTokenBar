//! Timestamp parsing for local logs. Mirrors the macOS `ISO8601Parser`: RFC-3339 with
//! fractional seconds of arbitrary precision, `Z` or explicit offset, plus a
//! truncate-to-millis fallback.

use chrono::{DateTime, Utc};

/// Parse an ISO-8601 / RFC-3339 timestamp to UTC, or `None`.
pub fn parse(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }

    // Some sources emit microsecond (6-digit) or longer fractions. Truncate the
    // fractional part to 3 digits and retry — matches the Swift fallback.
    if let Some(dot) = s.find('.') {
        if let Some(tz_off) = s[dot..]
            .bytes()
            .position(|b| b == b'+' || b == b'-' || b == b'Z')
        {
            let tz_pos = dot + tz_off;
            let frac = &s[dot + 1..tz_pos];
            let frac3 = format!("{:0<3}", frac.chars().take(3).collect::<String>());
            let rebuilt = format!("{}.{}{}", &s[..dot], frac3, &s[tz_pos..]);
            if let Ok(dt) = DateTime::parse_from_rfc3339(&rebuilt) {
                return Some(dt.with_timezone(&Utc));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_whole_second_z() {
        let d = parse("2026-01-02T03:04:05Z").unwrap();
        assert_eq!(
            d.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-01-02 03:04:05"
        );
    }

    #[test]
    fn parses_milli_fraction() {
        let d = parse("2026-01-02T03:04:05.123Z").unwrap();
        assert_eq!(d.timestamp_subsec_millis(), 123);
    }

    #[test]
    fn parses_micro_fraction() {
        let d = parse("2026-01-02T03:04:05.123456+00:00").unwrap();
        assert_eq!(d.timestamp_subsec_micros(), 123456);
    }

    #[test]
    fn applies_offset_to_utc() {
        let d = parse("2026-01-02T05:04:05+02:00").unwrap();
        assert_eq!(d.format("%H:%M:%S").to_string(), "03:04:05");
    }

    #[test]
    fn invalid_returns_none() {
        assert!(parse("").is_none());
        assert!(parse("not a date").is_none());
    }
}
