//! Assemble a [`UsageSnapshot`] from all providers — the port of `UsageStore`'s refresh path
//! (critical-day + best-effort enrichment derived from a single scan over the enrichment window
//! so the append-only `modified since` assumption holds).

use crate::aggregate;
use crate::entry::{Bucket, Entry};
use crate::provider::{all as providers, ProviderCtx};
use crate::types::{DailyUsage, PeriodUsage, ProviderSnapshot, UsageSnapshot};
use crate::windows;
use chrono::{DateTime, Utc, Weekday};

pub fn build_snapshot(
    ctx: &ProviderCtx,
    now: DateTime<Utc>,
    first_weekday: Weekday,
) -> UsageSnapshot {
    let tz = &ctx.tz;
    let since = windows::enrichment_scan_start(now, tz, first_weekday);
    let today_key = windows::local_day(now, tz);
    let week_from = windows::local_day(windows::start_of_week(now, tz, first_weekday), tz);
    let week_key = week_from.clone();
    let month_from = windows::local_day(windows::start_of_month(now, tz), tz);
    let month_key = windows::month_key(now, tz);
    let now_s = windows::iso_utc(now);

    let mut snapshots: Vec<ProviderSnapshot> = Vec::new();
    let mut combined = Bucket::default();

    for p in providers() {
        if !p.available(ctx) {
            continue;
        }
        let entries: Vec<Entry> = p.read_entries(ctx, since).unwrap_or_default();
        let today = aggregate::daily(&entries, &today_key);
        let block = aggregate::active_block(&entries, now);
        let week = nonempty(&entries, &week_key, &week_from, &today_key);
        let month = nonempty(&entries, &month_key, &month_from, &today_key);

        if let Some(t) = &today {
            combined.input += t.input_tokens;
            combined.output += t.output_tokens;
            combined.cache_write += t.cache_creation_tokens;
            combined.cache_read += t.cache_read_tokens;
            combined.cost += t.total_cost;
        }
        snapshots.push(ProviderSnapshot {
            provider_id: p.id().to_string(),
            display_name: p.display_name().to_string(),
            reports_cost: p.reports_cost(),
            today,
            active_block: block,
            week_total: week,
            month_total: month,
            fetched_at: now_s.clone(),
        });
    }

    let combined_today = (combined.total() > 0).then_some(DailyUsage {
        date: today_key.clone(),
        input_tokens: combined.input,
        output_tokens: combined.output,
        cache_creation_tokens: combined.cache_write,
        cache_read_tokens: combined.cache_read,
        total_tokens: combined.total(),
        total_cost: combined.cost,
    });

    UsageSnapshot {
        generated_at: now_s,
        combined_today,
        providers: snapshots,
    }
}

fn nonempty(entries: &[Entry], key: &str, from: &str, to: &str) -> Option<PeriodUsage> {
    let p = aggregate::period(entries, key, from, to);
    (p.total_tokens > 0).then_some(p)
}
