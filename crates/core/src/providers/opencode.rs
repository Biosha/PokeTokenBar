//! OpenCode — `$OPENCODE_DATA_DIR`/`~/.local/share/opencode`. Reads `opencode.db`
//! (`message` table, JSON `data` per message) and legacy `storage/message/*.json`.
//! `time.created` is epoch (ms or s) or ISO; `cost` is persisted per message.

use crate::entry::{dedup_keep_max, Entry};
use crate::provider::{ProviderCtx, UsageProvider};
use crate::{fsutil, iso8601, sqld, usage_cache, util, windows};
use chrono::{DateTime, FixedOffset, Utc};
use rusqlite::Row;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct OpenCodeProvider;

const TABLE: &str = "message";
const MSG_SELECT: &str = "SELECT id, session_id, data FROM message";
const MSG_SELECT_INC: &str = "SELECT id, session_id, data FROM message WHERE rowid > ?";
/// How many rows at/below the watermark get re-parsed on every incremental pass: OpenCode
/// finalizes a message by updating its `data` blob in place once the response completes, and
/// that row is always one of the most recent ones. A thousand rows is hours of activity even
/// on a busy box, and recent rows are small (the tail read is well under a millisecond).
const TAIL_OVERLAP: i64 = 1000;

impl UsageProvider for OpenCodeProvider {
    fn id(&self) -> &'static str {
        "opencode"
    }
    fn display_name(&self) -> &'static str {
        "OpenCode"
    }
    fn reports_cost(&self) -> bool {
        true
    }
    fn available(&self, ctx: &ProviderCtx) -> bool {
        opencode_roots(ctx).iter().any(|r| {
            opencode_db(r).map(|d| d.is_file()).unwrap_or(false)
                || r.join("storage/message").is_dir()
        })
    }
    fn read_entries(&self, ctx: &ProviderCtx, since: DateTime<Utc>) -> anyhow::Result<Vec<Entry>> {
        let cache = usage_cache::UsageCache::resolve();
        Ok(opencode_entries(ctx, since, cache.as_ref()))
    }
}

/// The read path, with the cache injected so tests can pin it to a temp file. The `message`
/// table is append-only except for finalization: OpenCode updates a message's `data` blob in
/// place once the response completes. That is covered without a full re-read by re-parsing
/// the last [`TAIL_OVERLAP`] rows at/below the rowid watermark on every incremental pass
/// (recent rows are small; the tail read is well under a millisecond), with the keep-max
/// dedup letting the finalized row win over its cached partial self. A table that shrank
/// below its watermark (a rotated file) fails the incremental read and falls back to a full
/// one, as does any daily full rescan. Legacy `storage/message/*.json` files are whole
/// documents, size-marked and re-read when their size changes. The window filter and the
/// global (db-vs-legacy) dedup stay after the merge, exactly as before the cache.
fn opencode_entries(
    ctx: &ProviderCtx,
    since: DateTime<Utc>,
    cache: Option<&usage_cache::UsageCache>,
) -> Vec<Entry> {
    let full = cache
        .map(|c| c.full_rescan_due("opencode", &ctx.tz))
        .unwrap_or(true);
    let tz = ctx.tz;
    let mut entries: Vec<Entry> = Vec::new();
    let mut keep: Vec<String> = Vec::new();
    for root in opencode_roots(ctx) {
        if let Some(db) = opencode_db(&root).filter(|d| d.is_file()) {
            let conn = match sqld::open_ro(&db) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let sig = match sqld::db_signature(&db) {
                Some(s) => s,
                None => continue,
            };
            let key = usage_cache::source_key(&db);
            let parse = |row: &Row<'_>| {
                let (id, data) = match (sqld::col_text(row, 0), sqld::col_text(row, 2)) {
                    (Some(a), Some(b)) => (a, b),
                    _ => return Ok(Vec::new()),
                };
                Ok(serde_json::from_str::<Value>(&data)
                    .ok()
                    .and_then(|o| parse_message(&o, &id, tz))
                    .into_iter()
                    .collect::<Vec<Entry>>())
            };
            let plan = cache
                .map(|c| c.db_plan("opencode", &key, sig, full, false))
                .unwrap_or_else(usage_cache::DbPlan::full);
            let (here, markers) = match (cache, plan.incremental) {
                (Some(c), true) => match c.read_db_incremental(
                    "opencode",
                    &key,
                    &conn,
                    &[(TABLE, MSG_SELECT_INC)],
                    TAIL_OVERLAP,
                    |_table, row| parse(row),
                ) {
                    Ok(x) => x,
                    Err(_) => full_read_db(&conn, tz),
                },
                _ => full_read_db(&conn, tz),
            };
            if let Some(c) = cache {
                c.db_commit("opencode", &key, sig, &markers, &here);
            }
            keep.push(key);
            entries.extend(here);
        }
        let legacy = root.join("storage/message");
        for f in fsutil::walk_modified(&legacy, &["json"], since, false, 8) {
            let path = f.path;
            let key = usage_cache::source_key(&path);
            let file_entries = match cache {
                Some(c) => {
                    c.read_file_source_whole("opencode", &path, &key, full, || {
                        parse_legacy_file(&path, tz)
                    })
                }
                None => parse_legacy_file(&path, tz),
            };
            keep.push(key);
            entries.extend(file_entries);
        }
    }
    let entries = entries.into_iter().filter(|e| e.date >= since).collect::<Vec<_>>();
    if let Some(c) = cache {
        c.prune_sources("opencode", &keep);
        c.prune_entries_before("opencode", since);
        if full {
            c.mark_full_scanned("opencode", &ctx.tz);
        }
    }
    dedup_keep_max(entries)
}

fn full_read_db(
    conn: &rusqlite::Connection,
    tz: FixedOffset,
) -> (Vec<Entry>, HashMap<String, i64>) {
    let here = sqld::rows(conn, MSG_SELECT, |row| {
        let (id, data) = match (sqld::col_text(row, 0), sqld::col_text(row, 2)) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(None),
        };
        Ok(serde_json::from_str::<Value>(&data)
            .ok()
            .and_then(|o| parse_message(&o, &id, tz)))
    })
    .unwrap_or_default();
    let mut markers = HashMap::new();
    markers.insert(TABLE.to_string(), sqld::max_rowid(conn, TABLE));
    (here, markers)
}

/// One legacy message file: a single JSON document, or nothing.
fn parse_legacy_file(path: &Path, tz: FixedOffset) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let fallback = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("msg")
        .to_string();
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|o| parse_message(&o, &fallback, tz))
        .into_iter()
        .collect()
}

pub fn opencode_roots(ctx: &ProviderCtx) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    match ctx.var("OPENCODE_DATA_DIR") {
        Some(v) => {
            for part in v.split(',') {
                let p = part.trim();
                if !p.is_empty() {
                    roots.push(fsutil::expand_tilde(p, &ctx.home));
                }
            }
        }
        None => roots.push(ctx.home.join(".local/share/opencode")),
    }
    roots
}

/// The root itself (if a `.db`), else `root/opencode.db` if present, else the
/// alphabetically-first `opencode-<channel>.db`.
pub fn opencode_db(root: &Path) -> Option<PathBuf> {
    if root.extension().and_then(|s| s.to_str()) == Some("db") {
        return Some(root.to_path_buf());
    }
    let standard = root.join("opencode.db");
    if standard.is_file() {
        return Some(standard);
    }
    let rd = std::fs::read_dir(root).ok()?;
    let mut candidates: Vec<String> = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with("opencode-") && name.ends_with(".db") {
            let channel = &name["opencode-".len()..name.len() - 3];
            if !channel.is_empty()
                && channel
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                candidates.push(name);
            }
        }
    }
    candidates.sort();
    candidates.first().map(|n| root.join(n))
}

/// A message row / legacy file: `tokens`+`time.created`+`modelID`+`providerID` are required.
pub fn parse_message(object: &Value, fallback_id: &str, tz: FixedOffset) -> Option<Entry> {
    let tokens = object.get("tokens")?;
    let created = object.get("time").and_then(|t| t.get("created"))?;
    let date = date_value(created)?;
    let model = util::get_str(object, "modelID")?.to_string();
    util::get_str(object, "providerID")?;
    let cache = tokens.get("cache");
    let cache_of = |k: &str| {
        cache
            .and_then(|c| c.get(k))
            .map(util::int_value)
            .unwrap_or(0)
    };
    let cost = object
        .get("cost")
        .and_then(Value::as_f64)
        .filter(|c| *c > 0.0);
    Some(Entry {
        id: format!(
            "opencode|{}",
            util::get_str(object, "id").unwrap_or(fallback_id)
        ),
        date,
        local_day: windows::local_day(date, &tz),
        model,
        input: util::get_int(tokens, "input"),
        output: util::get_int(tokens, "output"),
        cache_write: cache_of("write"),
        cache_read: cache_of("read"),
        explicit_cost: cost,
    })
}

/// `time.created`: epoch ms / seconds (int or float) or an ISO-8601 string.
pub fn date_value(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(ms) = v.as_i64() {
        return if ms >= 100_000_000_000 {
            DateTime::<Utc>::from_timestamp_millis(ms)
        } else {
            DateTime::<Utc>::from_timestamp_secs(ms)
        };
    }
    if let Some(f) = v.as_f64() {
        let ds = if f >= 1e11 { f / 1000.0 } else { f };
        return DateTime::<Utc>::from_timestamp_micros((ds * 1_000_000.0) as i64);
    }
    v.as_str().and_then(iso8601::parse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use rusqlite::Connection;

    fn make_db(dir: &Path) -> PathBuf {
        let db = dir.join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE message(id TEXT, session_id TEXT, data TEXT, time_created INTEGER);
             INSERT INTO message VALUES
               ('m1','s1','{\"id\":\"m1\",\"modelID\":\"anthropic/claude-3-5-sonnet\",\"providerID\":\"anthropic\",\"time\":{\"created\":1767344400000},\"tokens\":{\"input\":100,\"output\":50,\"cache\":{\"read\":10,\"write\":5}},\"cost\":0.01}',1767344400000)",
        )
        .unwrap();
        drop(conn);
        db
    }

    #[test]
    fn reads_db_row() {
        let dir = std::env::temp_dir().join(format!("opencode-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        make_db(&dir);
        let ctx = ProviderCtx::for_test(dir.clone(), FixedOffset::east_opt(0).unwrap());
        let entries =
            opencode_entries(&ctx, DateTime::<Utc>::from_timestamp(0, 0).unwrap(), None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].input, 100);
        assert_eq!(entries[0].cache_read, 10);
        assert_eq!(entries[0].explicit_cost, Some(0.01));
    }

    fn fresh_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("opencode-cache-{}-{}-{n}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn db_changes_are_seen_via_the_signature_change() {
        use crate::usage_cache::UsageCache;
        let dir = fresh_dir("rowid");
        let db = make_db(&dir);
        let cache = UsageCache::open(&dir.join("cache/usage-cache.sqlite")).unwrap();
        let ctx = ProviderCtx::for_test(dir.clone(), FixedOffset::east_opt(0).unwrap());
        let floor = DateTime::<Utc>::from_timestamp(0, 0).unwrap();

        assert_eq!(opencode_entries(&ctx, floor, Some(&cache)).len(), 1);
        // Unchanged file: served from the cache, nothing re-parsed.
        assert_eq!(opencode_entries(&ctx, floor, Some(&cache)).len(), 1);

        // Append a message: the rowid watermark reads it.
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m2','s2','{\"id\":\"m2\",\"modelID\":\"anthropic/claude-3-5-sonnet\",\"providerID\":\"anthropic\",\"time\":{\"created\":1767430800000},\"tokens\":{\"input\":7,\"output\":3}}',1767430800000)",
            [],
        )
        .unwrap();
        drop(conn);
        let entries = opencode_entries(&ctx, floor, Some(&cache));
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            vec!["opencode|m1", "opencode|m2"]
        );

        // OpenCode finalizes a message by updating its `data` in place (same rowid): a plain
        // rowid watermark cannot see that, so the tail overlap must re-read it.
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE message SET data = '{\"id\":\"m1\",\"modelID\":\"anthropic/claude-3-5-sonnet\",\"providerID\":\"anthropic\",\"time\":{\"created\":1767344400000},\"tokens\":{\"input\":400,\"output\":90}}' WHERE id = 'm1'",
            [],
        )
        .unwrap();
        drop(conn);
        let entries = opencode_entries(&ctx, floor, Some(&cache));
        let m1 = entries.iter().find(|e| e.id == "opencode|m1").unwrap();
        assert_eq!(m1.input, 400, "an in-place update must not be hidden by the cache");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_message_files_are_size_marked() {
        use crate::usage_cache::UsageCache;
        let dir = fresh_dir("legacy");
        let legacy = dir.join(".local/share/opencode/storage/message");
        std::fs::create_dir_all(&legacy).unwrap();
        let file = legacy.join("m1.json");
        std::fs::write(
            &file,
            "{\"id\":\"m1\",\"modelID\":\"anthropic/claude-3-5-sonnet\",\"providerID\":\"anthropic\",\"time\":{\"created\":1767344400000},\"tokens\":{\"input\":100,\"output\":50}}",
        )
        .unwrap();
        let cache = UsageCache::open(&dir.join("cache/usage-cache.sqlite")).unwrap();
        let ctx = ProviderCtx::for_test(dir.clone(), FixedOffset::east_opt(0).unwrap());
        let floor = DateTime::<Utc>::from_timestamp(0, 0).unwrap();

        let entries = opencode_entries(&ctx, floor, Some(&cache));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].input, 100);
        // Unchanged file: the size marker serves the cache.
        assert_eq!(opencode_entries(&ctx, floor, Some(&cache)).len(), 1);

        // The file is rewritten (a different document): the size change forces a re-read.
        std::fs::write(
            &file,
            "{\"id\":\"m1b\",\"modelID\":\"anthropic/claude-3-5-sonnet\",\"providerID\":\"anthropic\",\"time\":{\"created\":1767430800000},\"tokens\":{\"input\":9999,\"output\":3}}",
        )
        .unwrap();
        let entries = opencode_entries(&ctx, floor, Some(&cache));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "opencode|m1b");
        assert_eq!(entries[0].input, 9999);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_requires_provider_and_model() {
        let tz = FixedOffset::east_opt(0).unwrap();
        let miss: Value = serde_json::json!({
            "tokens": {"input": 1}, "time": {"created": "2026-01-02T03:00:00Z"},
            "modelID": "m"
        });
        assert!(parse_message(&miss, "x", tz).is_none()); // no providerID
    }
}
