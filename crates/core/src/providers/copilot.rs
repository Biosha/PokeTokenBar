//! Copilot CLI — `~/.copilot/session-store.db` (or `$COPILOT_HOME`), table
//! `assistant_usage_events`. One row per API call. `input_tokens` already includes the cached
//! prompt, so cache reads/writes are subtracted to avoid triple-counting. Premium-request
//! billing ⇒ tokens only, no cost estimate.

use crate::entry::Entry;
use crate::provider::{ProviderCtx, UsageProvider};
use crate::{fsutil, iso8601, sqld, usage_cache, windows};
use chrono::{DateTime, Utc};
use rusqlite::Row;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct CopilotProvider;

const TABLE: &str = "assistant_usage_events";
const SQL: &str = "SELECT id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, created_at \
     FROM assistant_usage_events";
const SQL_INC: &str = "SELECT id, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, created_at \
     FROM assistant_usage_events WHERE rowid > ?";

impl UsageProvider for CopilotProvider {
    fn id(&self) -> &'static str {
        "copilot"
    }
    fn display_name(&self) -> &'static str {
        "Copilot CLI"
    }
    fn reports_cost(&self) -> bool {
        false
    }
    fn available(&self, ctx: &ProviderCtx) -> bool {
        copilot_dbs(ctx).iter().any(|d| d.is_file())
    }
    fn read_entries(&self, ctx: &ProviderCtx, since: DateTime<Utc>) -> anyhow::Result<Vec<Entry>> {
        let cache = usage_cache::UsageCache::resolve();
        Ok(copilot_entries(ctx, since, cache.as_ref()))
    }
}

/// The read path, with the cache injected so tests can pin it to a temp file. One row per API
/// call (append-only events): the `rowid` watermark skips the history, and the file signature
/// (incl. `-wal`) is the staleness trigger.
fn copilot_entries(
    ctx: &ProviderCtx,
    since: DateTime<Utc>,
    cache: Option<&usage_cache::UsageCache>,
) -> Vec<Entry> {
    let full = cache
        .map(|c| c.full_rescan_due("copilot", &ctx.tz))
        .unwrap_or(true);
    let mut entries: Vec<Entry> = Vec::new();
    let mut keep: Vec<String> = Vec::new();
    for db in copilot_dbs(ctx) {
        let conn = match sqld::open_ro(&db) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let sig = match sqld::db_signature(&db) {
            Some(s) => s,
            None => continue,
        };
        let key = usage_cache::source_key(&db);
        let db_path = db.clone();
        let parse = move |row: &Row<'_>| {
            let id = sqld::col_i64(row, 0);
            let created =
                sqld::col_text(row, 6).ok_or_else(|| anyhow::anyhow!("no created_at"))?;
            let date = copilot_date(&created).ok_or_else(|| anyhow::anyhow!("bad date"))?;
            let cache_read = sqld::col_i64(row, 4);
            let cache_write = sqld::col_i64(row, 5);
            let input = (sqld::col_i64(row, 2) - cache_read - cache_write).max(0);
            Ok(Some(Entry {
                id: format!("copilot|{}|{}", db_path.display(), id),
                date,
                local_day: windows::local_day(date, &ctx.tz),
                model: sqld::col_text(row, 1).unwrap_or_else(|| "unknown".into()),
                input,
                output: sqld::col_i64(row, 3),
                cache_write,
                cache_read,
                explicit_cost: None,
            }))
        };
        let plan = cache
            .map(|c| c.db_plan("copilot", &key, sig, full, false))
            .unwrap_or_else(usage_cache::DbPlan::full);
        let (here, markers) = match (cache, plan.incremental) {
            (Some(c), true) => match c.read_db_incremental(
                "copilot",
                &key,
                &conn,
                &[(TABLE, SQL_INC)],
                0,
                |_table, row| parse(row).map(|e| e.into_iter().collect()),
            ) {
                Ok(x) => x,
                Err(_) => full_read(&conn, &parse),
            },
            _ => full_read(&conn, &parse),
        };
        if let Some(c) = cache {
            c.db_commit("copilot", &key, sig, &markers, &here);
        }
        keep.push(key);
        entries.extend(here);
    }
    if let Some(c) = cache {
        c.prune_sources("copilot", &keep);
        c.prune_entries_before("copilot", since);
        if full {
            c.mark_full_scanned("copilot", &ctx.tz);
        }
    }
    entries
}

fn full_read(
    conn: &rusqlite::Connection,
    parse: &impl Fn(&Row<'_>) -> anyhow::Result<Option<Entry>>,
) -> (Vec<Entry>, HashMap<String, i64>) {
    let here = sqld::rows(conn, SQL, |row| parse(row)).unwrap_or_default();
    let mut markers = HashMap::new();
    markers.insert(TABLE.to_string(), sqld::max_rowid(conn, TABLE));
    (here, markers)
}

/// `$COPILOT_HOME`/`~/.copilot`(s), mapping each to its `session-store.db` (or the file itself
/// when it already ends in `.db`).
pub fn copilot_dbs(ctx: &ProviderCtx) -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    match ctx.var("COPILOT_HOME") {
        Some(v) => {
            for part in v.split(',') {
                let p = part.trim();
                if !p.is_empty() {
                    bases.push(fsutil::expand_tilde(p, &ctx.home));
                }
            }
        }
        None => bases.push(ctx.home.join(".copilot")),
    }
    bases
        .into_iter()
        .map(|base| {
            if base.extension().and_then(|s| s.to_str()) == Some("db") {
                base
            } else {
                base.join("session-store.db")
            }
        })
        .collect()
}

/// `created_at` is either ISO-8601 with `Z` or SQLite's `datetime('now')` shape
/// `"YYYY-MM-DD HH:MM:SS"` (UTC). Normalize both.
pub fn copilot_date(raw: &str) -> Option<DateTime<Utc>> {
    let mut text = raw.trim().to_string();
    if text.len() < 19 {
        return None;
    }
    if let Some(pos) = text.find(' ') {
        text.replace_range(pos..pos + 1, "T");
    }
    let suffix = &text[11..];
    if !suffix.contains('Z') && !suffix.contains('+') && !suffix.contains('-') {
        text.push('Z');
    }
    iso8601::parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderCtx;
    use chrono::FixedOffset;
    use rusqlite::Connection;

    fn make_db(dir: &std::path::Path) -> PathBuf {
        let db = dir.join(".copilot").join("session-store.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE assistant_usage_events(
               id INTEGER PRIMARY KEY, model TEXT, input_tokens INTEGER, output_tokens INTEGER,
               cache_read_tokens INTEGER, cache_write_tokens INTEGER, created_at TEXT);
             INSERT INTO assistant_usage_events VALUES
               (1,'gpt-4o',1000,500,300,20,'2026-01-02T09:00:00Z'),
               (2,'gpt-4o', 200,100,  0, 0,'2026-01-02 10:00:00');",
        )
        .unwrap();
        drop(conn);
        db
    }

    #[test]
    fn reads_and_normalizes() {
        let dir = std::env::temp_dir().join(format!("copilot-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        make_db(&dir);
        let ctx = ProviderCtx::for_test(dir.clone(), FixedOffset::east_opt(0).unwrap());
        let entries = copilot_entries(&ctx, chrono::Utc::now(), None);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(entries.len(), 2);
        let e0 = &entries[0];
        assert_eq!(e0.input, 1000 - 300 - 20); // cache subset subtracted
        assert_eq!(e0.output, 500);
        let e1 = &entries[1];
        assert_eq!(e1.input, 200);
        assert_eq!(
            e1.date,
            e0.date
                .date_naive()
                .and_hms_opt(10, 0, 0)
                .unwrap()
                .and_utc()
        );
    }

    #[test]
    fn rowid_watermark_picks_up_appended_events() {
        use crate::usage_cache::UsageCache;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let home = std::env::temp_dir().join(format!("ptb-copilot-{}-{n}", std::process::id()));
        let db = make_db(&home);
        let ctx = ProviderCtx::for_test(home.clone(), FixedOffset::east_opt(0).unwrap());
        let cache = UsageCache::open(&home.join("cache/usage-cache.sqlite")).unwrap();
        let since = DateTime::<Utc>::from_timestamp(0, 0).unwrap();

        assert_eq!(copilot_entries(&ctx, since, Some(&cache)).len(), 2);

        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO assistant_usage_events VALUES (3,'gpt-4o',100,50,0,0,'2026-08-20T10:00:00Z')",
            [],
        )
        .unwrap();
        drop(conn);

        let next = copilot_entries(&ctx, since, Some(&cache));
        let plain = copilot_entries(&ctx, since, None);
        assert_eq!(next.len(), 3);
        assert_eq!(next, plain, "incremental must equal a full read");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn copilot_date_shapes() {
        assert_eq!(
            copilot_date("2026-01-02T09:30:00Z")
                .unwrap()
                .format("%H:%M:%S")
                .to_string(),
            "09:30:00"
        );
        assert_eq!(
            copilot_date("2026-01-02 09:30:00")
                .unwrap()
                .format("%H:%M:%S")
                .to_string(),
            "09:30:00"
        );
    }
}
