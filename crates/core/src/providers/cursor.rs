//! Cursor IDE — `cursorDiskKV` table in the VSCode `state.vscdb` under the Cursor user-data dir
//! (Linux `~/.config/Cursor/User/globalStorage`, macOS `.../Cursor/User/globalStorage`; plus
//! `Cursor Nightly` variants). One row per chat bubble keyed `bubbleId:*`; the JSON `value` holds
//! `tokenCount.{inputTokens,outputTokens}`, `createdAt` (ISO-8601 or epoch) and `modelType`.
//! Flat-rate => tokens only, no cost.

use crate::entry::{dedup_keep_max, Entry};
use crate::provider::{ProviderCtx, UsageProvider};
use crate::{fsutil, iso8601, sqld, usage_cache, util, windows};
use chrono::{DateTime, FixedOffset, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct CursorProvider;

const TABLE: &str = "cursorDiskKV";
/// Cold scan of the key index over `bubbleId:*` only. `GLOB` is case-sensitive, so a stray
/// lowercase `bubbleid:*` row never matches. The table has no time column, so the date floor is
/// applied per-row on the parsed bubble.
const SQL: &str = "SELECT key, value FROM cursorDiskKV WHERE key GLOB 'bubbleId:*'";
const SQL_INC: &str = "SELECT key, value FROM cursorDiskKV WHERE key GLOB 'bubbleId:*' AND rowid > ?";

impl UsageProvider for CursorProvider {
    fn id(&self) -> &'static str {
        "cursor"
    }
    fn display_name(&self) -> &'static str {
        "Cursor"
    }
    fn reports_cost(&self) -> bool {
        false
    }
    fn available(&self, ctx: &ProviderCtx) -> bool {
        cursor_roots(ctx).iter().any(|r| cursor_db(r).is_file())
    }
    fn read_entries(&self, ctx: &ProviderCtx, since: DateTime<Utc>) -> anyhow::Result<Vec<Entry>> {
        let cache = usage_cache::UsageCache::resolve();
        Ok(cursor_entries(ctx, since, cache.as_ref()))
    }
}

/// The read path, with the cache injected so tests can pin it to a temp file. `cursorDiskKV`
/// bubbles are edited in place as a chat is refined, so a changed file signature forces a full
/// re-read (a rowid watermark cannot see an edited row); on a quiescent file the rowid query is
/// the append safety net.
fn cursor_entries(
    ctx: &ProviderCtx,
    since: DateTime<Utc>,
    cache: Option<&usage_cache::UsageCache>,
) -> Vec<Entry> {
    let full = cache
        .map(|c| c.full_rescan_due("cursor", &ctx.tz))
        .unwrap_or(true);
    let mut entries: Vec<Entry> = Vec::new();
    let mut keep: Vec<String> = Vec::new();
    for root in cursor_roots(ctx) {
        let db = cursor_db(&root);
        if !db.is_file() {
            continue;
        }
        let conn = match sqld::open_ro(&db) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let sig = match sqld::db_signature(&db) {
            Some(s) => s,
            None => continue,
        };
        let key = usage_cache::source_key(&db);
        let plan = cache
            .map(|c| c.db_plan("cursor", &key, sig, full, true))
            .unwrap_or_else(usage_cache::DbPlan::full);
        let (here, markers) = match (cache, plan.incremental) {
            (Some(c), true) => match c.read_db_incremental(
                "cursor",
                &key,
                &conn,
                &[(TABLE, SQL_INC)],
                0,
                |_table, row| parse_row(row, since, &ctx.tz).map(|e| e.into_iter().collect()),
            ) {
                Ok(x) => x,
                Err(_) => full_read(&conn, since, &ctx.tz),
            },
            _ => full_read(&conn, since, &ctx.tz),
        };
        if let Some(c) = cache {
            c.db_commit("cursor", &key, sig, &markers, &here);
        }
        keep.push(key);
        entries.extend(here);
    }
    if let Some(c) = cache {
        c.prune_sources("cursor", &keep);
        c.prune_entries_before("cursor", since);
        if full {
            c.mark_full_scanned("cursor", &ctx.tz);
        }
    }
    dedup_keep_max(entries)
}

fn full_read(
    conn: &rusqlite::Connection,
    since: DateTime<Utc>,
    tz: &FixedOffset,
) -> (Vec<Entry>, HashMap<String, i64>) {
    let here = sqld::rows(conn, SQL, |row| parse_row(row, since, tz)).unwrap_or_default();
    let mut markers = HashMap::new();
    markers.insert(TABLE.to_string(), sqld::max_rowid(conn, TABLE));
    (here, markers)
}

/// `$CURSOR_DATA_DIR` (comma-separated, tilde-expanded) else the per-OS defaults — Linux
/// `~/.config/Cursor…` and macOS `~/Library/Application Support/Cursor…`, each stable + nightly.
/// Both OS paths are always listed so the same code is platform-agnostic; a root that does not
/// exist on the current OS is simply skipped.
pub fn cursor_roots(ctx: &ProviderCtx) -> Vec<PathBuf> {
    if let Some(v) = ctx.var("CURSOR_DATA_DIR") {
        return v
            .split(',')
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(|p| fsutil::expand_tilde(p, &ctx.home))
            .collect();
    }
    vec![
        ctx.home.join(".config/Cursor/User/globalStorage"),
        ctx.home.join(".config/Cursor Nightly/User/globalStorage"),
        ctx.home
            .join("Library/Application Support/Cursor/User/globalStorage"),
        ctx.home
            .join("Library/Application Support/Cursor Nightly/User/globalStorage"),
    ]
}

/// `state.vscdb` under the root (or the root itself when it is already a `.vscdb` file).
pub fn cursor_db(root: &Path) -> PathBuf {
    if root.extension().and_then(|s| s.to_str()) == Some("vscdb") {
        root.to_path_buf()
    } else {
        root.join("state.vscdb")
    }
}

/// One row -> entry. The `value` column is a BLOB in real Cursor (a text literal in the tests);
/// both storage classes are read, then parsed as a JSON bubble.
fn parse_row(
    row: &rusqlite::Row<'_>,
    since: DateTime<Utc>,
    tz: &FixedOffset,
) -> anyhow::Result<Option<Entry>> {
    let key = match sqld::col_text(row, 0) {
        Some(k) => k,
        None => return Ok(None),
    };
    let raw = match row.get_ref(1usize) {
        Ok(r) => r,
        Err(_) => return Ok(None),
    };
    let bytes: &[u8] = match &raw {
        rusqlite::types::ValueRef::Text(b) => b,
        rusqlite::types::ValueRef::Blob(b) => b,
        _ => return Ok(None),
    };
    let obj = match serde_json::from_slice::<Value>(bytes) {
        Ok(o) => o,
        Err(_) => return Ok(None),
    };
    Ok(parse_cursor_bubble(&obj, &key, since, tz))
}

/// Parse a single `cursorDiskKV` bubble blob into a usage entry, or `None` unless it is a
/// non-zero-token bubble at/after `since`. Mirrors the reference `parseCursorBubble`.
fn parse_cursor_bubble(
    obj: &Value,
    key: &str,
    since: DateTime<Utc>,
    tz: &FixedOffset,
) -> Option<Entry> {
    let tokens = obj.get("tokenCount")?;
    if !tokens.is_object() {
        return None;
    }
    let input = util::get_int(tokens, "inputTokens");
    let output = util::get_int(tokens, "outputTokens");
    if input + output <= 0 {
        return None;
    }
    let date = flexible_date(obj.get("createdAt"))?;
    if date < since {
        return None;
    }
    let model = util::get_str(obj, "modelType")
        .map(str::to_string)
        .unwrap_or_else(|| "unknown".into());
    Some(Entry {
        id: format!("cursor|{key}"),
        date,
        local_day: windows::local_day(date, tz),
        model,
        input,
        output,
        cache_write: 0,
        cache_read: 0,
        explicit_cost: None,
    })
}

/// `createdAt` is an ISO-8601 string or an epoch number (seconds or millis), matching OpenCode.
fn flexible_date(value: Option<&Value>) -> Option<DateTime<Utc>> {
    match value? {
        Value::String(s) => iso8601::parse(s),
        Value::Number(n) => {
            let raw = n.as_f64()?;
            if !raw.is_finite() || raw <= 0.0 {
                return None;
            }
            let secs = if raw >= 100_000_000_000.0 {
                raw / 1000.0
            } else {
                raw
            };
            let whole = secs as i64;
            let frac = (secs - whole as f64).clamp(0.0, 1.0);
            let nsecs = ((frac * 1e9) as u32).min(999_999_999);
            chrono::DateTime::from_timestamp(whole, nsecs)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderCtx;
    use chrono::FixedOffset;
    use rusqlite::{params, Connection};
    use serde_json::json;
    use std::collections::HashMap;

    fn base() -> PathBuf {
        std::env::temp_dir().join(format!("cursor-test-{}", std::process::id()))
    }

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = base().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tz0() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn floor() -> DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2000-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn since2026() -> DateTime<Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn mk_db(dir: &Path, rows: &[(&str, &str)]) -> PathBuf {
        let db = dir.join("state.vscdb");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
        )
        .unwrap();
        for (k, v) in rows {
            conn.execute(
                "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
                params![*k, v.as_bytes().to_vec()],
            )
            .unwrap();
        }
        drop(conn);
        db
    }

    /// Point the provider at `dir` (its `state.vscdb`) via the override var.
    fn ctx_dir(dir: &Path) -> ProviderCtx {
        let mut env = HashMap::new();
        env.insert(
            "CURSOR_DATA_DIR".to_string(),
            dir.to_string_lossy().to_string(),
        );
        ProviderCtx {
            home: dir.to_path_buf(),
            env,
            tz: tz0(),
        }
    }

    fn ctx_default(home: &Path) -> ProviderCtx {
        ProviderCtx {
            home: home.to_path_buf(),
            env: HashMap::new(),
            tz: tz0(),
        }
    }

    fn pb_key(key: &str, bubble: Value) -> Option<Entry> {
        parse_cursor_bubble(&bubble, key, floor(), &tz0())
    }

    fn pb(bubble: Value) -> Option<Entry> {
        pb_key("bubbleId:t:m", bubble)
    }

    // ---- end-to-end SQLite path ----

    #[test]
    fn e2e_reads_bubble_tokens_glob_case_sensitive() {
        let dir = fresh_dir("e2e-tokens");
        mk_db(
            &dir,
            &[
                (
                    "bubbleId:tab-1:msg-1",
                    r#"{"tokenCount":{"inputTokens":1500,"outputTokens":800},"createdAt":"2026-01-04T10:34:54.766Z","modelType":"claude-3.5-sonnet"}"#,
                ),
                ("composerData:other", r#"{"unrelated":true}"#),
                (
                    "bubbleId:tab-1:msg-zero",
                    r#"{"tokenCount":{"inputTokens":0,"outputTokens":0},"createdAt":"2026-01-04T11:00:00.000Z","modelType":"gpt-4o"}"#,
                ),
                (
                    "bubbleid:tab-1:wrong-case",
                    r#"{"tokenCount":{"inputTokens":999,"outputTokens":1},"createdAt":"2026-01-04T12:00:00.000Z","modelType":"gpt-4o"}"#,
                ),
            ],
        );
        let entries = cursor_entries(&ctx_dir(&dir), floor(), None);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            entries.len(),
            1,
            "GLOB must be case-sensitive and skip zero/other keys"
        );
        let e = &entries[0];
        assert_eq!(e.input, 1500);
        assert_eq!(e.output, 800);
        assert_eq!(e.model, "claude-3.5-sonnet");
        assert_eq!(e.id, "cursor|bubbleId:tab-1:msg-1");
        assert!(e.explicit_cost.is_none());
    }

    #[test]
    fn e2e_skips_bubbles_before_since() {
        let dir = fresh_dir("e2e-since");
        mk_db(
            &dir,
            &[
                (
                    "bubbleId:tab:old",
                    r#"{"tokenCount":{"inputTokens":10,"outputTokens":5},"createdAt":"2025-12-01T00:00:00.000Z","modelType":"gpt-4o"}"#,
                ),
                (
                    "bubbleId:tab:new",
                    r#"{"tokenCount":{"inputTokens":20,"outputTokens":10},"createdAt":"2026-01-10T00:00:00.000Z","modelType":"gpt-4o"}"#,
                ),
            ],
        );
        let entries = cursor_entries(&ctx_dir(&dir), since2026(), None);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "cursor|bubbleId:tab:new");
    }

    #[test]
    fn e2e_multi_root_union() {
        let a = fresh_dir("e2e-multi-a");
        let b = fresh_dir("e2e-multi-b");
        mk_db(
            &a,
            &[
                (
                    "bubbleId:a:0",
                    r#"{"tokenCount":{"inputTokens":11,"outputTokens":1},"createdAt":"2026-01-04T10:00:00.000Z","modelType":"gpt-4o"}"#,
                ),
                (
                    "bubbleId:a:1",
                    r#"{"tokenCount":{"inputTokens":12,"outputTokens":1},"createdAt":"2026-01-04T10:01:00.000Z","modelType":"gpt-4o"}"#,
                ),
            ],
        );
        mk_db(
            &b,
            &[
                (
                    "bubbleId:b:0",
                    r#"{"tokenCount":{"inputTokens":21,"outputTokens":1},"createdAt":"2026-01-04T10:02:00.000Z","modelType":"gpt-4o"}"#,
                ),
                (
                    "bubbleId:b:1",
                    r#"{"tokenCount":{"inputTokens":22,"outputTokens":1},"createdAt":"2026-01-04T10:03:00.000Z","modelType":"gpt-4o"}"#,
                ),
            ],
        );
        let mut env = HashMap::new();
        env.insert(
            "CURSOR_DATA_DIR".to_string(),
            format!("{},{}", a.display(), b.display()),
        );
        let ctx = ProviderCtx {
            home: a.clone(),
            env,
            tz: tz0(),
        };
        let p = CursorProvider;
        assert!(p.available(&ctx));
        let entries = cursor_entries(&ctx, floor(), None);
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();

        assert_eq!(
            entries
                .iter()
                .filter(|e| e.id.contains("bubbleId:a:"))
                .count(),
            2
        );
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.id.contains("bubbleId:b:"))
                .count(),
            2
        );
        assert_eq!(entries.len(), 4);
    }

    #[test]
    fn e2e_absent_not_available() {
        let dir = fresh_dir("e2e-none");
        let p = CursorProvider;
        assert!(!p.available(&ctx_dir(&dir)));
        assert!(cursor_entries(&ctx_dir(&dir), floor(), None).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn e2e_corrupt_db_no_crash() {
        let dir = fresh_dir("e2e-corrupt");
        std::fs::write(dir.join("state.vscdb"), b"not-a-sqlite-database").unwrap();
        let entries = cursor_entries(&ctx_dir(&dir), floor(), None);
        std::fs::remove_dir_all(&dir).ok();
        assert!(entries.is_empty());
    }

    #[test]
    fn in_place_bubble_edits_are_seen_via_the_signature_change() {
        use crate::usage_cache::UsageCache;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = fresh_dir(&format!("cache-{n}"));
        mk_db(
            &dir,
            &[(
                "bubbleId:tab:1",
                r#"{"tokenCount":{"inputTokens":10,"outputTokens":5},"createdAt":"2026-01-04T10:00:00.000Z","modelType":"gpt-4o"}"#,
            )],
        );
        let cache = UsageCache::open(&dir.join("cache/usage-cache.sqlite")).unwrap();
        let ctx = ctx_dir(&dir);

        assert_eq!(cursor_entries(&ctx, floor(), Some(&cache)).len(), 1);
        // Unchanged: served from the cache.
        assert_eq!(cursor_entries(&ctx, floor(), Some(&cache)).len(), 1);

        // Cursor rewrites a bubble in place (UPDATE, no new rowid): the changed signature must
        // force a full re-read — a rowid watermark cannot see an edited row.
        let conn = rusqlite::Connection::open(dir.join("state.vscdb")).unwrap();
        conn.execute(
            "UPDATE cursorDiskKV SET value = ?1 WHERE key = 'bubbleId:tab:1'",
            rusqlite::params![
                r#"{"tokenCount":{"inputTokens":40,"outputTokens":7},"createdAt":"2026-01-04T10:00:00.000Z","modelType":"gpt-4o"}"#
                    .as_bytes()
                    .to_vec()
            ],
        )
        .unwrap();
        // And appends a fresh bubble on top.
        conn.execute(
            "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
            rusqlite::params![
                "bubbleId:tab:2",
                r#"{"tokenCount":{"inputTokens":30,"outputTokens":3},"createdAt":"2026-01-05T10:00:00.000Z","modelType":"gpt-4o"}"#
                    .as_bytes()
                    .to_vec()
            ],
        )
        .unwrap();
        drop(conn);

        let next = cursor_entries(&ctx, floor(), Some(&cache));
        let plain = cursor_entries(&ctx, floor(), None);
        assert_eq!(next.len(), 2);
        assert!(next.iter().any(|e| e.input == 40), "the edited bubble must be re-read");
        assert_eq!(next, plain, "re-read after in-place edits must equal a full read");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn default_roots_cover_linux_and_macos() {
        let home = fresh_dir("roots");
        let roots: Vec<String> = cursor_roots(&ctx_default(&home))
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        std::fs::remove_dir_all(&home).ok();

        assert!(roots
            .iter()
            .any(|r| r.contains(".config/Cursor/User/globalStorage")));
        assert!(roots
            .iter()
            .any(|r| r.contains("Cursor Nightly/User/globalStorage")));
        assert!(roots
            .iter()
            .any(|r| r.contains("Library/Application Support/Cursor/User/globalStorage")));
        assert!(roots
            .iter()
            .any(|r| r.contains("Cursor Nightly/User/globalStorage")));
    }

    // ---- bubble parsing ----

    #[test]
    fn parse_with_tokens() {
        let e = pb(json!({
            "modelType": "claude-3.5-sonnet",
            "createdAt": "2026-01-04T10:34:54.766Z",
            "tokenCount": { "inputTokens": 1500, "outputTokens": 800 },
        }))
        .unwrap();
        assert_eq!(e.input, 1500);
        assert_eq!(e.output, 800);
        assert_eq!(e.model, "claude-3.5-sonnet");
        assert!(e.id.starts_with("cursor|"));
    }

    #[test]
    fn parse_fractional_seconds() {
        let e = pb(json!({
            "createdAt": "2026-01-04T10:34:54.766Z",
            "modelType": "gpt-4o",
            "tokenCount": { "inputTokens": 100, "outputTokens": 50 },
        }))
        .unwrap();
        assert_eq!(e.input, 100);
        assert_eq!(e.output, 50);
        assert_eq!(e.model, "gpt-4o");
    }

    #[test]
    fn parse_numeric_created_at_millis() {
        let e = pb(json!({
            "createdAt": 1_767_312_000_000_i64,
            "tokenCount": { "inputTokens": 10, "outputTokens": 5 },
            "modelType": "gpt-4o",
        }))
        .unwrap();
        assert_eq!(e.input, 10);
    }

    #[test]
    fn parse_numeric_created_at_seconds() {
        let e = pb(json!({
            "createdAt": 1_767_312_000_i64,
            "tokenCount": { "inputTokens": 7, "outputTokens": 3 },
            "modelType": "gpt-4o",
        }))
        .unwrap();
        assert_eq!(e.total(), 10);
    }

    #[test]
    fn parse_ignores_zero_tokens() {
        assert!(pb(json!({
            "createdAt": "2026-01-04T10:34:54.766Z",
            "tokenCount": { "inputTokens": 0, "outputTokens": 0 },
        }))
        .is_none());
    }

    #[test]
    fn parse_ignores_old_entries() {
        assert!(pb(json!({
            "createdAt": "1999-01-01T00:00:00Z",
            "tokenCount": { "inputTokens": 100, "outputTokens": 50 },
        }))
        .is_none());
    }

    #[test]
    fn parse_missing_token_count() {
        assert!(pb(json!({
            "modelType": "gpt-4",
            "createdAt": "2026-01-04T10:34:54.766Z",
        }))
        .is_none());
    }

    #[test]
    fn parse_missing_model_falls_back_to_unknown() {
        let e = pb(json!({
            "createdAt": "2026-01-04T10:34:54.766Z",
            "tokenCount": { "inputTokens": 100, "outputTokens": 50 },
        }))
        .unwrap();
        assert_eq!(e.model, "unknown");
    }

    #[test]
    fn parse_missing_created_at() {
        assert!(pb(json!({
            "tokenCount": { "inputTokens": 100, "outputTokens": 50 },
        }))
        .is_none());
    }

    #[test]
    fn parse_invalid_created_at() {
        assert!(pb(json!({
            "createdAt": "not-a-date",
            "tokenCount": { "inputTokens": 100, "outputTokens": 50 },
        }))
        .is_none());
    }

    #[test]
    fn parse_id_prefix() {
        let e = pb_key(
            "bubbleId:abc:def",
            json!({
                "createdAt": "2026-01-04T10:34:54.766Z",
                "tokenCount": { "inputTokens": 100, "outputTokens": 50 },
            }),
        )
        .unwrap();
        assert_eq!(e.id, "cursor|bubbleId:abc:def");
    }

    #[test]
    fn provider_identity() {
        let p = CursorProvider;
        assert_eq!(p.id(), "cursor");
        assert_eq!(p.display_name(), "Cursor");
        assert!(!p.reports_cost());
    }
}
