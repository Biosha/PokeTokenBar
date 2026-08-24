//! Hermes Agent — `~/.hermes/state.db` (or `$HERMES_HOME`). One row per session in the
//! `sessions` table. `reasoning_tokens` is a subset of `output_tokens`, so it is folded into
//! `output`. Cost: persisted `actual_cost_usd` when > 0, else `estimated_cost_usd` when > 0,
//! else fall back to the model pricing table.

use crate::entry::{dedup_keep_max, Entry};
use crate::provider::{ProviderCtx, UsageProvider};
use crate::{fsutil, sqld, usage_cache, util, windows};
use chrono::{DateTime, Utc};
use rusqlite::Row;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct HermesProvider;

const TABLE: &str = "sessions";
const SQL: &str = "SELECT id, model, billing_provider, started_at, message_count, \
     input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
     reasoning_tokens, estimated_cost_usd, actual_cost_usd \
     FROM sessions WHERE model IS NOT NULL AND TRIM(model) != ''";
const SQL_INC: &str = "SELECT id, model, billing_provider, started_at, message_count, \
     input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
     reasoning_tokens, estimated_cost_usd, actual_cost_usd \
     FROM sessions WHERE model IS NOT NULL AND TRIM(model) != '' AND rowid > ?";

impl UsageProvider for HermesProvider {
    fn id(&self) -> &'static str {
        "hermes"
    }
    fn display_name(&self) -> &'static str {
        "Hermes Agent"
    }
    fn reports_cost(&self) -> bool {
        true
    }
    fn available(&self, ctx: &ProviderCtx) -> bool {
        hermes_dbs(ctx).iter().any(|d| d.is_file())
    }
    fn read_entries(&self, ctx: &ProviderCtx, since: DateTime<Utc>) -> anyhow::Result<Vec<Entry>> {
        let cache = usage_cache::UsageCache::resolve();
        Ok(hermes_entries(ctx, since, cache.as_ref()))
    }
}

/// The read path, with the cache injected so tests can pin it to a temp file. `sessions` rows
/// accumulate in place as a session runs, so a changed file signature forces a full re-read
/// (a rowid watermark cannot see an edited row); on a quiescent file the rowid query is the
/// append safety net.
fn hermes_entries(
    ctx: &ProviderCtx,
    since: DateTime<Utc>,
    cache: Option<&usage_cache::UsageCache>,
) -> Vec<Entry> {
    let full = cache
        .map(|c| c.full_rescan_due("hermes", &ctx.tz))
        .unwrap_or(true);
    let mut entries: Vec<Entry> = Vec::new();
    let mut keep: Vec<String> = Vec::new();
    for db in hermes_dbs(ctx) {
        let conn = match sqld::open_ro(&db) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let sig = match sqld::db_signature(&db) {
            Some(s) => s,
            None => continue,
        };
        let key = usage_cache::source_key(&db);
        let tz = ctx.tz;
        let parse = move |row: &Row<'_>| {
            let id = sqld::col_text(row, 0)
                .and_then(|s| util::non_empty_str(Some(s.as_str())))
                .ok_or_else(|| anyhow::anyhow!("missing id"))?;
            let model = sqld::col_text(row, 1)
                .and_then(|s| util::non_empty_str(Some(s.as_str())))
                .ok_or_else(|| anyhow::anyhow!("missing model"))?;
            let date = hermes_date(row.get::<_, f64>(3).unwrap_or(0.0))
                .ok_or_else(|| anyhow::anyhow!("bad started_at"))?;
            if date < since {
                return Ok(None);
            }
            let input = sqld::col_i64(row, 5);
            let output = sqld::col_i64(row, 6) + sqld::col_i64(row, 9);
            let cache_write = sqld::col_i64(row, 8);
            let cache_read = sqld::col_i64(row, 7);
            if input + output + cache_write + cache_read == 0 {
                return Ok(None);
            }
            let estimated = row.get::<_, f64>(10).unwrap_or(0.0);
            let actual = row.get::<_, f64>(11).unwrap_or(0.0);
            let explicit_cost = if actual > 0.0 {
                Some(actual)
            } else if estimated > 0.0 {
                Some(estimated)
            } else {
                None
            };
            Ok(Some(Entry {
                id: format!("hermes|{id}"),
                date,
                local_day: windows::local_day(date, &tz),
                model,
                input,
                output,
                cache_write,
                cache_read,
                explicit_cost,
            }))
        };
        let plan = cache
            .map(|c| c.db_plan("hermes", &key, sig, full, true))
            .unwrap_or_else(usage_cache::DbPlan::full);
        let (here, markers) = match (cache, plan.incremental) {
            (Some(c), true) => match c.read_db_incremental(
                "hermes",
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
            c.db_commit("hermes", &key, sig, &markers, &here);
        }
        keep.push(key);
        entries.extend(here);
    }
    if let Some(c) = cache {
        c.prune_sources("hermes", &keep);
        c.prune_entries_before("hermes", since);
        if full {
            c.mark_full_scanned("hermes", &ctx.tz);
        }
    }
    dedup_keep_max(entries)
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

/// `$HERMES_HOME`/`~/.hermes`(s), mapping each to its `state.db` (or the file itself when it
/// already ends in `.db`).
pub fn hermes_dbs(ctx: &ProviderCtx) -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    match ctx.var("HERMES_HOME") {
        Some(v) => {
            for part in v.split(',') {
                let p = part.trim();
                if !p.is_empty() {
                    bases.push(fsutil::expand_tilde(p, &ctx.home));
                }
            }
        }
        None => bases.push(ctx.home.join(".hermes")),
    }
    bases
        .into_iter()
        .map(|base| {
            if base.extension().and_then(|s| s.to_str()) == Some("db") {
                base
            } else {
                base.join("state.db")
            }
        })
        .collect()
}

/// `started_at` is a numeric epoch (seconds, or milliseconds when ≥ 1e11) → UTC instant.
fn hermes_date(raw: f64) -> Option<DateTime<Utc>> {
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    let secs = if raw >= 1e11 { raw / 1000.0 } else { raw };
    DateTime::<Utc>::from_timestamp_secs(secs as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderCtx;
    use chrono::FixedOffset;
    use rusqlite::Connection;

    fn home(suffix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("hermes-test-{}-{}", std::process::id(), suffix))
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn write_db(root: &std::path::Path, sql: &str) {
        let db = root.join(".hermes").join("state.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&db);
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(sql).unwrap();
        drop(conn);
    }

    #[test]
    fn reads_session_tokens_reasoning_and_actual_cost() {
        let home = home("read");
        write_db(
            &home,
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT, billing_provider TEXT, \
             started_at REAL NOT NULL, message_count INTEGER DEFAULT 0, \
             input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0, \
             cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0, \
             reasoning_tokens INTEGER DEFAULT 0, estimated_cost_usd REAL, actual_cost_usd REAL); \
             INSERT INTO sessions VALUES \
             ('session-1','claude-sonnet-4-20250514','anthropic',1767312000,42,100,50,10,20,5,0.12,0.34);",
        );
        let ctx = ProviderCtx::for_test(home.clone(), FixedOffset::east_opt(0).unwrap());
        let entries = hermes_entries(&ctx, at("2026-01-01T00:00:00Z"), None);
        std::fs::remove_dir_all(&home).ok();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.input, 100);
        assert_eq!(e.output, 55);
        assert_eq!(e.cache_write, 20);
        assert_eq!(e.cache_read, 10);
        assert_eq!(e.total(), 185);
        assert_eq!(e.explicit_cost, Some(0.34));
    }

    #[test]
    fn accepts_millisecond_started_at() {
        let home = home("ms");
        write_db(
            &home,
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT, billing_provider TEXT, \
             started_at REAL NOT NULL, message_count INTEGER, input_tokens INTEGER, \
             output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER, \
             reasoning_tokens INTEGER, estimated_cost_usd REAL, actual_cost_usd REAL); \
             INSERT INTO sessions VALUES \
             ('session-ms','gpt-5','openai',1767312000000,1,10,5,0,0,0,0,0);",
        );
        let ctx = ProviderCtx::for_test(home.clone(), FixedOffset::east_opt(0).unwrap());
        let entries = hermes_entries(&ctx, at("2026-01-01T00:00:00Z"), None);
        std::fs::remove_dir_all(&home).ok();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].local_day, "2026-01-02");
        assert_eq!(entries[0].total(), 15);
    }

    #[test]
    fn in_place_session_edits_are_seen_via_the_signature_change() {
        use crate::usage_cache::UsageCache;
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let home = std::env::temp_dir().join(format!("ptb-hermes-{}-{n}", std::process::id()));
        let since = at("2026-01-01T00:00:00Z");
        write_db(
            &home,
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT, billing_provider TEXT, \
             started_at REAL NOT NULL, message_count INTEGER, input_tokens INTEGER, \
             output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER, \
             reasoning_tokens INTEGER, estimated_cost_usd REAL, actual_cost_usd REAL); \
             INSERT INTO sessions VALUES \
             ('s1','gpt-5','openai',1767312000,1,10,5,0,0,0,0,0);",
        );
        let ctx = ProviderCtx::for_test(home.clone(), FixedOffset::east_opt(0).unwrap());
        let cache = UsageCache::open(&home.join("cache/usage-cache.sqlite")).unwrap();

        assert_eq!(hermes_entries(&ctx, since, Some(&cache)).len(), 1);
        // Unchanged: served from the cache.
        assert_eq!(hermes_entries(&ctx, since, Some(&cache)).len(), 1);

        // A session accumulates in place (UPDATE, no new rowid): the changed signature must
        // force a full re-read — a rowid watermark cannot see an edited row.
        let conn = rusqlite::Connection::open(home.join(".hermes").join("state.db")).unwrap();
        conn.execute(
            "UPDATE sessions SET input_tokens = 90, output_tokens = 11 WHERE id = 's1'",
            [],
        )
        .unwrap();
        drop(conn);

        let next = hermes_entries(&ctx, since, Some(&cache));
        let plain = hermes_entries(&ctx, since, None);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].input, 90);
        assert_eq!(
            next, plain,
            "re-read after an in-place edit must equal a full read"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
