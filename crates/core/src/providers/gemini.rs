//! Gemini CLI — `~/.gemini/tmp/<hash>/chats/session-*.jsonl` and legacy `.json`
//! `ConversationRecord { messages: [...] }`.
//!
//! Token mapping preserves `total == totalTokenCount`:
//! `input = (input − cached) + tool`, `output = output + thoughts`, `cache_read = cached`.

use crate::entry::Entry;
use crate::provider::{ProviderCtx, UsageProvider};
use crate::{fsutil, iso8601, usage_cache, util, windows};
use chrono::{DateTime, FixedOffset, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

pub struct GeminiProvider;

impl UsageProvider for GeminiProvider {
    fn id(&self) -> &'static str {
        "gemini_cli"
    }
    fn display_name(&self) -> &'static str {
        "Gemini CLI"
    }
    fn available(&self, ctx: &ProviderCtx) -> bool {
        ctx.home.join(".gemini/tmp").is_dir()
    }
    fn read_entries(&self, ctx: &ProviderCtx, since: DateTime<Utc>) -> anyhow::Result<Vec<Entry>> {
        let cache = usage_cache::UsageCache::resolve();
        Ok(gemini_entries(ctx, since, cache.as_ref()))
    }
}

/// The read path, with the cache injected so tests can pin it to a temp file. A session file's
/// parse is not split-friendly (its fallback timestamps and positional ids span the whole
/// file), so each source is size-marked and re-read whole when its size changes.
fn gemini_entries(ctx: &ProviderCtx, since: DateTime<Utc>, cache: Option<&usage_cache::UsageCache>) -> Vec<Entry> {
    let full = cache
        .map(|c| c.full_rescan_due("gemini_cli", &ctx.tz))
        .unwrap_or(true);
    let root = ctx.home.join(".gemini/tmp");
    let files = fsutil::walk_modified(&root, &["jsonl", "json"], since, true, 8);
    let mut entries: Vec<Entry> = Vec::new();
    let mut keep: Vec<String> = Vec::new();
    for f in files {
        let path = f.path;
        let key = usage_cache::source_key(&path);
        let file_entries = match cache {
            Some(c) => c.read_file_source_whole("gemini_cli", &path, &key, full, || {
                parse_gemini_file(&path, ctx.tz)
            }),
            None => parse_gemini_file(&path, ctx.tz),
        };
        keep.push(key);
        entries.extend(file_entries);
    }
    if let Some(c) = cache {
        c.prune_sources("gemini_cli", &keep);
        c.prune_entries_before("gemini_cli", since);
        if full {
            c.mark_full_scanned("gemini_cli", &ctx.tz);
        }
    }
    entries
}

pub fn parse_gemini_file(path: &Path, tz: FixedOffset) -> Vec<Entry> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let file = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let mut by_id: HashMap<String, Entry> = HashMap::new();
    let mut order: Vec<String> = Vec::new();

    fn absorb(
        obj: &Value,
        fallback: Option<DateTime<Utc>>,
        file: &str,
        tz: FixedOffset,
        by_id: &mut HashMap<String, Entry>,
        order: &mut Vec<String>,
        seq: &mut i64,
    ) {
        let tokens = match obj.get("tokens") {
            Some(t) if t.is_object() => t,
            _ => return,
        };
        let id = util::get_str(obj, "id")
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                *seq += 1;
                format!("n{seq}")
            });
        let key = format!("gemini|{file}|{id}");
        let date = match util::get_str(obj, "timestamp")
            .and_then(iso8601::parse)
            .or(fallback)
        {
            Some(d) => d,
            None => return,
        };
        let input = tokens.get("input").map(util::int_value).unwrap_or(0);
        let cached = tokens.get("cached").map(util::int_value).unwrap_or(0);
        let tool = tokens.get("tool").map(util::int_value).unwrap_or(0);
        let output = tokens.get("output").map(util::int_value).unwrap_or(0)
            + tokens.get("thoughts").map(util::int_value).unwrap_or(0);
        if !by_id.contains_key(&key) {
            order.push(key.clone());
        }
        by_id.insert(
            key.clone(),
            Entry {
                id: key,
                date,
                local_day: windows::local_day(date, &tz),
                model: util::get_str(obj, "model").unwrap_or("gemini").to_string(),
                input: (input - cached).max(0) + tool,
                output,
                cache_write: 0,
                cache_read: cached,
                explicit_cost: None,
            },
        );
    }

    let mut seq = 0i64;
    if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
        let mut last_ts: Option<DateTime<Utc>> = None;
        for line in text.lines() {
            if !line.contains("\"tokens\"") && !line.contains("\"timestamp\"") {
                continue;
            }
            let obj: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(ts) = util::get_str(&obj, "timestamp").and_then(iso8601::parse) {
                last_ts = Some(ts);
            }
            absorb(&obj, last_ts, file, tz, &mut by_id, &mut order, &mut seq);
        }
    } else {
        let obj: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let session_start = util::get_str(&obj, "startTime").and_then(iso8601::parse);
        if let Some(messages) = obj.get("messages").and_then(Value::as_array) {
            for m in messages {
                absorb(m, session_start, file, tz, &mut by_id, &mut order, &mut seq);
            }
        }
    }
    order.into_iter().filter_map(|k| by_id.remove(&k)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;

    #[test]
    fn jsonl_mapping() {
        let rec = serde_json::json!({
            "id": "m1",
            "timestamp": "2026-01-02T03:00:00Z",
            "model": "gemini-2.5-flash",
            "tokens": { "input": 100, "cached": 40, "tool": 5, "output": 30, "thoughts": 7 }
        })
        .to_string();
        // write to a temp file
        let dir = std::env::temp_dir().join(format!("gemini-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        std::fs::write(&path, &rec).unwrap();

        let entries = parse_gemini_file(&path, FixedOffset::east_opt(0).unwrap());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.input, 60 + 5); // (100 - 40) + 5
        assert_eq!(e.output, 30 + 7);
        assert_eq!(e.cache_read, 40);
        assert_eq!(e.total(), 142); // input 65 + output 37 + cache_read 40
    }

    #[test]
    fn size_marked_source_is_cached_and_reread_on_change() {
        use crate::usage_cache::UsageCache;
        use chrono::Utc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let home = std::env::temp_dir().join(format!("ptb-gemini-{}-{n}", std::process::id()));
        let chats = home.join(".gemini/tmp/h1/chats");
        std::fs::create_dir_all(&chats).unwrap();
        let path = chats.join("session.jsonl");
        let rec = |id: &str| {
            serde_json::json!({
                "id": id, "timestamp": "2026-08-20T10:00:00Z", "model": "gemini-2.5-flash",
                "tokens": { "input": 100, "cached": 40, "tool": 0, "output": 30 }
            })
            .to_string()
        };
        std::fs::write(&path, format!("{}\n", rec("m1"))).unwrap();
        let cache = UsageCache::open(&home.join("cache/usage-cache.sqlite")).unwrap();
        let ctx = ProviderCtx::for_test(home.clone(), FixedOffset::east_opt(0).unwrap());
        let since = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        assert_eq!(gemini_entries(&ctx, since, Some(&cache)).len(), 1);

        // Unchanged: served from the cache.
        assert_eq!(gemini_entries(&ctx, since, Some(&cache)).len(), 1);

        // Appended: re-read whole, still identical to a fresh parse.
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(format!("{}\n", rec("m2")).as_bytes()).unwrap();
        let next = gemini_entries(&ctx, since, Some(&cache));
        assert_eq!(next.len(), 2);
        assert_eq!(next, parse_gemini_file(&path, ctx.tz));
        std::fs::remove_dir_all(&home).ok();
    }
}
