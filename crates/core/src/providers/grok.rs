//! Grok CLI — `~/.grok/sessions/<id>/updates.jsonl`.
//!
//! Reads `sessionUpdate:"turn_completed"` lines and only the per-turn `usage`
//! (the `_meta.totalTokens` / auto-compact fields are context-window size, **not** usage).
//! Subagent sessions are skipped at file-selection time (their tokens are already folded into
//! the parent turn). Replays are dropped; global dedup on `prompt_id`.

use crate::entry::{dedup_keep_max, Entry};
use crate::provider::{ProviderCtx, UsageProvider};
use crate::{fsutil, iso8601, usage_cache, util, windows};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct GrokProvider;

impl UsageProvider for GrokProvider {
    fn id(&self) -> &'static str {
        "grok_cli"
    }
    fn display_name(&self) -> &'static str {
        "Grok CLI"
    }
    fn available(&self, ctx: &ProviderCtx) -> bool {
        grok_sessions_dir(ctx).is_dir()
    }
    fn read_entries(&self, ctx: &ProviderCtx, since: DateTime<Utc>) -> anyhow::Result<Vec<Entry>> {
        let cache = usage_cache::UsageCache::resolve();
        Ok(grok_entries(ctx, since, cache.as_ref()))
    }
}

/// The read path, with the cache injected so tests can pin it to a temp file. Files are
/// byte-offset-marked sources; only the appended complete lines of a grown file are parsed.
fn grok_entries(ctx: &ProviderCtx, since: DateTime<Utc>, cache: Option<&usage_cache::UsageCache>) -> Vec<Entry> {
    let full = cache
        .map(|c| c.full_rescan_due("grok_cli", &ctx.tz))
        .unwrap_or(true);
    let root = grok_sessions_dir(ctx);
    let files = fsutil::walk_modified(&root, &["jsonl"], since, false, 8);
    let mut entries: Vec<Entry> = Vec::new();
    let mut keep: Vec<String> = Vec::new();
    for f in files {
        if !is_grok_usage_file(&f.path) {
            continue;
        }
        let path = f.path;
        let key = usage_cache::source_key(&path);
        let file_entries = match cache {
            Some(c) => c.read_file_source(
                "grok_cli",
                &path,
                &key,
                full,
                || parse_grok_file(&path, ctx.tz),
                |bytes| parse_grok_lines(bytes, ctx.tz),
                |mut a, b| {
                    a.extend(b);
                    dedup_keep_max(a)
                },
            ),
            None => parse_grok_file(&path, ctx.tz),
        };
        keep.push(key);
        entries.extend(file_entries);
    }
    if let Some(c) = cache {
        c.prune_sources("grok_cli", &keep);
        c.prune_entries_before("grok_cli", since);
        if full {
            c.mark_full_scanned("grok_cli", &ctx.tz);
        }
    }
    dedup_keep_max(entries)
}

pub fn grok_sessions_dir(ctx: &ProviderCtx) -> PathBuf {
    if let Some(h) = ctx.var("GROK_HOME") {
        return fsutil::expand_tilde(&h, &ctx.home).join("sessions");
    }
    ctx.home.join(".grok/sessions")
}

fn is_grok_usage_file(path: &Path) -> bool {
    path.file_name().and_then(|s| s.to_str()) == Some("updates.jsonl")
        && !grok_session_is_subagent(path.parent().unwrap_or(path))
}

fn grok_session_is_subagent(session_dir: &Path) -> bool {
    let data = match std::fs::read(session_dir.join("summary.json")) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let obj: Value = match serde_json::from_slice(&data) {
        Ok(v) => v,
        Err(_) => return false,
    };
    util::get_str(&obj, "session_kind")
        .map(|k| k.starts_with("subagent"))
        .unwrap_or(false)
}

pub fn parse_grok_file(path: &Path, tz: chrono::FixedOffset) -> Vec<Entry> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    // The lenient path never returns `Err` (only the strict tail path does).
    let out = parse_grok_text(&text, tz, false).unwrap_or_default();
    dedup_keep_max(out)
}

/// Parse complete lines. `strict` (the incremental tail path) turns a `turn_completed` line
/// that is not valid JSON into `Err` so the caller re-reads the whole file.
fn parse_grok_text(text: &str, tz: chrono::FixedOffset, strict: bool) -> Result<Vec<Entry>, ()> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.contains("turn_completed") {
            if serde_json::from_str::<Value>(line).is_err() && strict {
                return Err(());
            }
            if let Some(e) = parse_grok_line(line, tz) {
                out.push(e);
            }
        }
    }
    Ok(out)
}

fn parse_grok_lines(bytes: &[u8], tz: chrono::FixedOffset) -> Result<Vec<Entry>, ()> {
    let text = std::str::from_utf8(bytes).map_err(|_| ())?;
    parse_grok_text(text, tz, true)
}

fn parse_grok_line(line: &str, tz: chrono::FixedOffset) -> Option<Entry> {
    let envelope: Value = serde_json::from_str(line).ok()?;
    let notification = envelope.get("params").unwrap_or(&envelope);
    let update = notification.get("update")?;
    if util::get_str(update, "sessionUpdate") != Some("turn_completed") {
        return None;
    }
    let usage = update.get("usage")?;
    let meta = notification.get("_meta");
    if is_truly_replay(meta) {
        return None;
    }
    let turn_id = util::non_empty_str(util::get_str(update, "prompt_id"))?;
    let date = grok_date(&envelope, meta)?;
    let output = util::get_int_opt(usage, "outputTokens")
        .or(util::get_int_opt(usage, "output_tokens"))
        .unwrap_or(0);
    let reported_cache = util::get_int_opt(usage, "cachedReadTokens")
        .or(util::get_int_opt(usage, "cached_read_tokens"))
        .unwrap_or(0);
    let (input, cache_read) = if let Some(full) = util::get_int_opt(usage, "inputTokens") {
        let clamped = reported_cache.min(full);
        (full - clamped, clamped)
    } else {
        // Headless projection: `input_tokens` is already cache-excluded.
        (
            util::get_int_opt(usage, "input_tokens").unwrap_or(0),
            reported_cache,
        )
    };
    // Keep `total == source total`: attribute any reported remainder to output.
    let parts = input + output + cache_read;
    let output = match util::get_int_opt(usage, "totalTokens")
        .or(util::get_int_opt(usage, "total_tokens"))
    {
        Some(reported) if reported > parts => output + (reported - parts),
        _ => output,
    };
    if input + output + cache_read == 0 {
        return None;
    }
    let model = grok_model(usage).unwrap_or_else(|| "grok".to_string());
    Some(Entry {
        id: format!("grok|{turn_id}"),
        date,
        local_day: windows::local_day(date, &tz),
        model,
        input,
        output,
        cache_write: 0,
        cache_read,
        explicit_cost: grok_cost(usage),
    })
}

fn is_truly_replay(meta: Option<&Value>) -> bool {
    meta.and_then(|m| m.get("isReplay"))
        .map(util::bool_value)
        .unwrap_or(false)
}

fn grok_date(envelope: &Value, meta: Option<&Value>) -> Option<DateTime<Utc>> {
    let ms = meta
        .and_then(|m| m.get("agentTimestampMs"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if ms > 0.0 {
        return DateTime::<Utc>::from_timestamp_millis(ms as i64);
    }
    let raw = envelope
        .get("timestamp")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    if raw > 0.0 {
        let secs = if raw >= 1e11 { raw / 1000.0 } else { raw };
        return DateTime::<Utc>::from_timestamp_secs(secs as i64);
    }
    util::get_str(envelope, "timestamp").and_then(iso8601::parse)
}

/// Server-computed cost only (1e10 ticks = $1); dropped when partial/incomplete flags are set.
fn grok_cost(usage: &Value) -> Option<f64> {
    let bv = |k: &str| usage.get(k).map(util::bool_value).unwrap_or(false);
    if bv("usageIsIncomplete") || bv("usage_is_incomplete") {
        return None;
    }
    if bv("costIsPartial") || bv("cost_is_partial") {
        return None;
    }
    let ticks = usage
        .get("costUsdTicks")
        .or_else(|| usage.get("cost_usd_ticks"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    (ticks > 0.0).then_some(ticks / 1e10)
}

/// Representative display model: the per-model row with the largest total.
fn grok_model(usage: &Value) -> Option<String> {
    let by = usage
        .get("modelUsage")
        .or_else(|| usage.get("model_usage"))?
        .as_object()?;
    let mut pairs: Vec<(&String, &Value)> = by.iter().collect();
    pairs.sort_by_key(|(k, _)| *k);
    let mut best: Option<(String, i64)> = None;
    for (name, fields) in pairs {
        let total = fields
            .get("totalTokens")
            .or_else(|| fields.get("total_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if best.as_ref().is_none_or(|(_, t)| total > *t) {
            best = Some((name.clone(), total));
        }
    }
    best.and_then(|(n, _)| util::non_empty(Some(&Value::String(n))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    /// `total == input + output + cacheRead` must hold even when the source total is higher.
    #[test]
    fn total_identity_holds() {
        let line = serde_json::json!({
            "timestamp": 1700000000i64,
            "params": {
                "sessionId": "s",
                "update": {
                    "sessionUpdate": "turn_completed",
                    "prompt_id": "p1",
                    "usage": {
                        "inputTokens": 100, "outputTokens": 10,
                        "cachedReadTokens": 40, "totalTokens": 120,
                        "modelUsage": { "grok-beta": { "totalTokens": 120 } }
                    }
                },
                "_meta": { "agentTimestampMs": 1700000000000i64 }
            }
        })
        .to_string();
        let e = parse_grok_line(&line, FixedOffset::east_opt(0).unwrap()).unwrap();
        assert_eq!(e.input, 60); // 100 - 40
        assert_eq!(e.cache_read, 40);
        assert_eq!(e.total(), 120);
        assert_eq!(e.model, "grok-beta");
    }

    #[test]
    fn incremental_tail_matches_full_read() {
        use crate::usage_cache::UsageCache;
        use chrono::Utc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let home = std::env::temp_dir().join(format!("ptb-grok-{}-{n}", std::process::id()));
        let session = home.join(".grok/sessions/s1");
        std::fs::create_dir_all(&session).unwrap();
        let file = session.join("updates.jsonl");
        let turn = |id: &str, out: i64| {
            serde_json::json!({
                "timestamp": 1_787_000_000_i64,
                "params": {
                    "sessionId": "s1",
                    "update": {
                        "sessionUpdate": "turn_completed",
                        "prompt_id": id,
                        "usage": { "inputTokens": 100, "outputTokens": out, "totalTokens": 100 + out }
                    }
                }
            })
            .to_string()
        };
        std::fs::write(&file, format!("{}\n", turn("p1", 10))).unwrap();
        let cache = UsageCache::open(&home.join("cache/usage-cache.sqlite")).unwrap();
        let ctx = ProviderCtx::for_test(home.clone(), FixedOffset::east_opt(0).unwrap());
        let since = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        assert_eq!(grok_entries(&ctx, since, Some(&cache)).len(), 1);

        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
        f.write_all(format!("{}\n", turn("p2", 20)).as_bytes()).unwrap();
        let next = grok_entries(&ctx, since, Some(&cache));
        let full = dedup_keep_max(parse_grok_file(&file, ctx.tz));
        assert_eq!(next.len(), 2);
        assert_eq!(full.len(), 2, "incremental must equal a full parse");
        assert_eq!(
            next.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            full.iter().map(|e| e.id.clone()).collect::<Vec<_>>()
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
