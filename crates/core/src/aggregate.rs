//! Derive day / period / active-block totals from normalized entries — the port of the
//! aggregation tail of `LocalUsageReader`.

use crate::entry::{Bucket, Entry};
use crate::types::{BlockUsage, DailyUsage, PeriodUsage};
use crate::windows::block_window;
use chrono::DateTime;
use chrono::Utc;

/// Totals for a specific local day, or `None` when that day has no usage.
pub fn daily(entries: &[Entry], local_day: &str) -> Option<DailyUsage> {
    let mut b = Bucket::default();
    for e in entries.iter().filter(|e| e.local_day == local_day) {
        b.add(e);
    }
    if b.total() == 0 {
        return None;
    }
    Some(DailyUsage {
        date: local_day.to_string(),
        input_tokens: b.input,
        output_tokens: b.output,
        cache_creation_tokens: b.cache_write,
        cache_read_tokens: b.cache_read,
        total_tokens: b.total(),
        total_cost: b.cost,
    })
}

/// Inclusive local-day range `[from_day, to_day]` → period totals.
pub fn period(entries: &[Entry], period_key: &str, from_day: &str, to_day: &str) -> PeriodUsage {
    let mut b = Bucket::default();
    for e in entries
        .iter()
        .filter(|e| e.local_day.as_str() >= from_day && e.local_day.as_str() <= to_day)
    {
        b.add(e);
    }
    PeriodUsage {
        period: period_key.to_string(),
        total_tokens: b.total(),
        total_cost: b.cost,
    }
}

/// Rolling 5-hour window of recent usage, with a tokens-per-minute burn estimate.
pub fn active_block(entries: &[Entry], now: DateTime<Utc>) -> Option<BlockUsage> {
    let window_start = now - block_window();
    let mut recent: Vec<&Entry> = entries.iter().filter(|e| e.date >= window_start).collect();
    recent.sort_by_key(|a| a.date);
    let first = *recent.first()?;
    let mut b = Bucket::default();
    for e in &recent {
        b.add(e);
    }
    let minutes = (now.signed_duration_since(first.date).num_seconds() as f64 / 60.0).max(1.0);
    Some(BlockUsage {
        id: format!("block-{}", first.date.timestamp()),
        start_time: crate::windows::iso_utc(first.date),
        end_time: crate::windows::iso_utc(first.date + block_window()),
        is_active: true,
        total_tokens: b.total(),
        cost_usd: b.cost,
        tokens_per_minute: Some(b.total() as f64 / minutes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, FixedOffset, Utc};

    fn tz() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }
    fn e(day: &str, at: i64, input: i64, out: i64) -> Entry {
        Entry {
            id: format!("{day}-{at}"),
            date: DateTime::<Utc>::from_timestamp(at, 0).unwrap(),
            local_day: day.to_string(),
            model: "claude-sonnet-4-6".into(),
            input,
            output: out,
            cache_write: 0,
            cache_read: 0,
            explicit_cost: None,
        }
    }

    #[test]
    fn daily_filters_by_day() {
        let entries = vec![
            e("2026-01-01", 1_700_000_000, 10, 5),
            e("2026-01-02", 1_700_100_000, 7, 3),
        ];
        let d = daily(&entries, "2026-01-02").unwrap();
        assert_eq!(d.total_tokens, 10);
    }

    #[test]
    fn daily_none_when_empty() {
        let entries = vec![e("2026-01-01", 1_700_000_000, 10, 5)];
        assert!(daily(&entries, "1999-01-01").is_none());
    }

    #[test]
    fn period_inclusive_range() {
        let entries = vec![e("2026-01-01", 100, 1, 1), e("2026-01-03", 300, 2, 2)];
        let p = period(&entries, "2026-01-01", "2026-01-01", "2026-01-02");
        assert_eq!(p.total_tokens, 2);
    }

    #[test]
    fn active_block_computes_burn() {
        let now = DateTime::parse_from_rfc3339("2026-01-02T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // 30 minutes before now.
        let entries = vec![e("2026-01-02", now.timestamp() - 1800, 100, 100)];
        let b = active_block(&entries, now).unwrap();
        assert_eq!(b.total_tokens, 200);
        assert!(b.tokens_per_minute.unwrap() > 2.9);
        let _ = tz();
    }

    #[test]
    fn active_block_none_when_old() {
        let now = DateTime::parse_from_rfc3339("2026-01-02T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let entries = vec![e("2026-01-01", now.timestamp() - 9 * 3600, 100, 100)];
        assert!(active_block(&entries, now).is_none());
    }
}
