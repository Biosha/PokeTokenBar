//! Kiro CLI — `data.sqlite3` conversation-history DB (root: `$KIRO_CLI_HOME`, else
//! `~/.config/kiro-cli` on Linux / `~/Library/Application Support/kiro-cli` on macOS).
//!
//! Kiro's `RequestMetadata` (upstream `aws/amazon-q-developer-cli`) never persists a token
//! count, so tokens here are a bytes/4 estimate. `request_metadata.user_prompt_length` is only
//! the freshly typed user message (it excludes the resent history), so it can't be used; each
//! turn's prompt is instead the accumulated history (seeding `latest_summary`, which stands in
//! for turns compaction deleted from the DB) plus that turn's own `user` text, and output is
//! `response_size`.
//!
//! Two schema generations coexist and share a parser: `conversations_v2` (kiro-cli < 2.0.1,
//! dedicated `conversation_id` column) and `conversations` (2.0.1+, keyed by working directory;
//! the id lives in the JSON body).
//!
//! Kiro *deletes* turns from its DB on `/clear`/compaction. The macOS app merges each scan with
//! a process-lifetime cache so a cleared turn stays counted for the session. This headless port
//! does a straightforward per-call read with no in-process merge — each call re-derives entries
//! idempotently via a stable per-turn id, so a cleared turn is not double-counted but is also
//! not retained after it is removed from the DB.

use crate::entry::{dedup_keep_max, Entry};
use crate::provider::{ProviderCtx, UsageProvider};
use crate::{fsutil, sqld, usage_cache, util, windows};
use chrono::{DateTime, FixedOffset, Utc};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;

pub struct KiroProvider;

const DB_NAME: &str = "data.sqlite3";
const TABLE_V2: &str = "conversations_v2";
const TABLE_V1: &str = "conversations";
const SQL_V2: &str = "SELECT conversation_id, value FROM conversations_v2";
const SQL_V1: &str = "SELECT value FROM conversations";
const SQL_V2_INC: &str = "SELECT conversation_id, value FROM conversations_v2 WHERE rowid > ?";
const SQL_V1_INC: &str = "SELECT value FROM conversations WHERE rowid > ?";
/// Bytes-per-token: the only locally available precision for the byte→token estimate.
const BYTES_PER_TOKEN: i64 = 4;

impl UsageProvider for KiroProvider {
    fn id(&self) -> &'static str {
        "kiro"
    }
    fn display_name(&self) -> &'static str {
        "Kiro"
    }
    fn reports_cost(&self) -> bool {
        false
    }
    fn available(&self, ctx: &ProviderCtx) -> bool {
        kiro_dbs(ctx).iter().any(|d| d.is_file())
    }
    fn read_entries(&self, ctx: &ProviderCtx, since: DateTime<Utc>) -> anyhow::Result<Vec<Entry>> {
        let cache = usage_cache::UsageCache::resolve();
        Ok(kiro_entries(ctx, since, cache.as_ref()))
    }
}

/// The read path, with the cache injected so tests can pin it to a temp file. Kiro rewrites
/// conversation rows in place (turns are appended, edited and *deleted* on `/clear`/compaction),
/// so a changed file signature forces a full re-read of both schema generations; on a quiescent
/// file the rowid query is the append safety net.
fn kiro_entries(
    ctx: &ProviderCtx,
    since: DateTime<Utc>,
    cache: Option<&usage_cache::UsageCache>,
) -> Vec<Entry> {
    let full = cache
        .map(|c| c.full_rescan_due("kiro", &ctx.tz))
        .unwrap_or(true);
    let mut entries: Vec<Entry> = Vec::new();
    let mut keep: Vec<String> = Vec::new();
    for db in kiro_dbs(ctx) {
        let conn = match sqld::open_ro(&db) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let sig = match sqld::db_signature(&db) {
            Some(s) => s,
            None => continue,
        };
        let key = usage_cache::source_key(&db);
        let fallback = db
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(DB_NAME)
            .to_string();
        let plan = cache
            .map(|c| c.db_plan("kiro", &key, sig, full, true))
            .unwrap_or_else(usage_cache::DbPlan::full);
        let (here, markers) = match (cache, plan.incremental) {
            (Some(c), true) => match c.read_db_incremental(
                "kiro",
                &key,
                &conn,
                &[
                    (TABLE_V2, SQL_V2_INC),
                    (TABLE_V1, SQL_V1_INC),
                ],
                0,
                |table, row| match table {
                    TABLE_V2 => match sqld::col_text(row, 1) {
                        Some(value) => Ok(conv_entries(
                            sqld::col_text(row, 0),
                            &value,
                            Some(&fallback),
                            since,
                            &ctx.tz,
                        )),
                        None => Ok(Vec::new()),
                    },
                    TABLE_V1 => match sqld::col_text(row, 0) {
                        Some(value) => Ok(conv_entries(None, &value, None, since, &ctx.tz)),
                        None => Ok(Vec::new()),
                    },
                    other => Err(anyhow::anyhow!("unexpected table {other}")),
                },
            ) {
                Ok(x) => x,
                Err(_) => full_read_kiro(&conn, &fallback, since, &ctx.tz),
            },
            _ => full_read_kiro(&conn, &fallback, since, &ctx.tz),
        };
        if let Some(c) = cache {
            c.db_commit("kiro", &key, sig, &markers, &here);
        }
        keep.push(key);
        entries.extend(here);
    }
    if let Some(c) = cache {
        c.prune_sources("kiro", &keep);
        c.prune_entries_before("kiro", since);
        if full {
            c.mark_full_scanned("kiro", &ctx.tz);
        }
    }
    dedup_keep_max(entries)
}

fn full_read_kiro(
    conn: &rusqlite::Connection,
    fallback: &str,
    since: DateTime<Utc>,
    tz: &FixedOffset,
) -> (Vec<Entry>, HashMap<String, i64>) {
    let mut here = v2_entries(conn, fallback, since, tz);
    here.extend(v1_entries(conn, since, tz));
    let mut markers = HashMap::new();
    markers.insert(TABLE_V2.to_string(), sqld::max_rowid(conn, TABLE_V2));
    markers.insert(TABLE_V1.to_string(), sqld::max_rowid(conn, TABLE_V1));
    (here, markers)
}

/// One conversation row (both schema generations) -> its turn entries. `fallback` names a row
/// whose id is nowhere to be found: the v2 generation falls back to the file name, v1 drops
/// the row (it cannot be keyed). Malformed JSON is skipped, mirroring the pre-cache read.
fn conv_entries(
    col_id: Option<String>,
    value: &str,
    fallback: Option<&str>,
    since: DateTime<Utc>,
    tz: &FixedOffset,
) -> Vec<Entry> {
    let object = match serde_json::from_str::<Value>(value) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(conv) = col_id
        .filter(|c| !c.is_empty())
        .or_else(|| util::non_empty_str(object.get("conversation_id").and_then(Value::as_str)))
        .or_else(|| fallback.map(str::to_string))
    else {
        return Vec::new();
    };
    kiro_turn_entries(&conv, &object, since, tz)
}

/// `$KIRO_CLI_HOME`/per-OS default, mapping each to its `data.sqlite3` (or the file itself when
/// it already ends in `.sqlite3`).
pub fn kiro_dbs(ctx: &ProviderCtx) -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    match ctx.var("KIRO_CLI_HOME") {
        Some(v) => {
            for part in v.split(',') {
                let p = part.trim();
                if !p.is_empty() {
                    bases.push(fsutil::expand_tilde(p, &ctx.home));
                }
            }
        }
        None => {
            #[cfg(target_os = "macos")]
            bases.push(ctx.home.join("Library/Application Support/kiro-cli"));
            #[cfg(not(target_os = "macos"))]
            bases.push(ctx.home.join(".config/kiro-cli"));
        }
    }
    let dbs = bases
        .into_iter()
        .map(|base| {
            if base.extension().and_then(|s| s.to_str()) == Some("sqlite3") {
                base
            } else {
                base.join(DB_NAME)
            }
        })
        .collect();
    fsutil::normalized_roots(dbs)
}

/// `conversations_v2` (kiro-cli < 2.0.1): one row per conversation, id in a dedicated column.
fn v2_entries(
    conn: &rusqlite::Connection,
    fallback: &str,
    since: DateTime<Utc>,
    tz: &FixedOffset,
) -> Vec<Entry> {
    let rows: Vec<(Option<String>, String)> =
        match sqld::rows(conn, SQL_V2, |row| match sqld::col_text(row, 1) {
            Some(value) => Ok(Some((sqld::col_text(row, 0), value))),
            None => Ok(None),
        }) {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
    rows.into_iter()
        .flat_map(|(col_id, value)| conv_entries(col_id, &value, Some(fallback), since, tz))
        .collect()
}

/// `conversations` (kiro-cli 2.0.1+): one row per working directory; the id must be read from the
/// JSON body and a row without one can't be keyed, so it is dropped.
fn v1_entries(conn: &rusqlite::Connection, since: DateTime<Utc>, tz: &FixedOffset) -> Vec<Entry> {
    let rows: Vec<String> = match sqld::rows(conn, SQL_V1, |row| Ok(sqld::col_text(row, 0))) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    rows.into_iter()
        .flat_map(|value| conv_entries(None, &value, None, since, tz))
        .collect()
}

/// Walk one conversation's `history`, emitting one estimated entry per timestamped turn in-window.
fn kiro_turn_entries(
    conversation_id: &str,
    object: &Value,
    since: DateTime<Utc>,
    tz: &FixedOffset,
) -> Vec<Entry> {
    let turns = match object.get("history").and_then(Value::as_array) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    // `latest_summary` stands in for compacted-away turns — still resent on every later request.
    let mut cumulative: i64 =
        json_value_byte_len(object.get("latest_summary").unwrap_or(&Value::Null));
    for turn in turns {
        let turn = match turn.as_object() {
            Some(o) => o,
            None => continue,
        };
        let user_bytes = field_byte_len(turn.get("user"));
        let assistant_bytes = field_byte_len(turn.get("assistant"));
        if let Some(e) = turn_entry(conversation_id, turn, cumulative + user_bytes, since, tz) {
            out.push(e);
        }
        // Bytes must accumulate for every turn (even one skipped above) — later turns resend it.
        cumulative += user_bytes + assistant_bytes;
    }
    out
}

/// One turn's estimated entry from its `request_metadata` + accumulated prompt bytes.
fn turn_entry(
    conversation_id: &str,
    turn: &Map<String, Value>,
    prompt_bytes: i64,
    since: DateTime<Utc>,
    tz: &FixedOffset,
) -> Option<Entry> {
    let meta = turn.get("request_metadata").and_then(Value::as_object)?;
    let raw_ts = meta
        .get("request_start_timestamp_ms")
        .and_then(Value::as_f64)?;
    if raw_ts <= 0.0 {
        return None;
    }
    let date = kiro_date(raw_ts)?;
    if date < since {
        return None;
    }
    let input = prompt_bytes / BYTES_PER_TOKEN;
    let output =
        util::int_value(meta.get("response_size").unwrap_or(&Value::Null)) / BYTES_PER_TOKEN;
    if input + output <= 0 {
        return None;
    }
    let model = util::non_empty_str(meta.get("model_id").and_then(Value::as_str))
        .unwrap_or_else(|| "unknown".into());
    Some(Entry {
        id: format!("kiro|{}|{}", conversation_id, raw_ts as i64),
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

/// Epoch seconds/millis (`request_start_timestamp_ms` is ms; `>= 1e11` means milliseconds).
fn kiro_date(raw: f64) -> Option<DateTime<Utc>> {
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    if raw >= 1e11 {
        DateTime::<Utc>::from_timestamp_millis(raw as i64)
    } else {
        DateTime::<Utc>::from_timestamp_secs(raw as i64)
    }
}

/// Byte length of a JSON value's content (the reference `kiro-usage` `_text_len`), used to seed
/// the running history from `latest_summary`.
fn json_value_byte_len(v: &Value) -> i64 {
    match v {
        Value::String(s) => s.len() as i64,
        Value::Number(n) => n.to_string().len() as i64,
        Value::Array(a) => a.iter().map(json_value_byte_len).sum(),
        Value::Object(o) => o.values().map(json_value_byte_len).sum(),
        Value::Null | Value::Bool(_) => 0,
    }
}

/// Byte length of a turn's `user`/`assistant` field: the stringified value of every key except
/// `images` (a base64 blob that would dwarf the text and isn't separately token-modeled).
fn field_byte_len(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::String(s)) => s.len() as i64,
        Some(Value::Object(o)) => o
            .iter()
            .filter(|(k, _)| *k != "images")
            .map(|(_, val)| json_value_byte_len(val))
            .sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate;
    use rusqlite::Connection;
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CTR: AtomicUsize = AtomicUsize::new(0);

    fn temp_home() -> PathBuf {
        std::env::temp_dir()
            .join(format!("kiro-test-{}", std::process::id()))
            .join(format!("t{}", CTR.fetch_add(1, Ordering::SeqCst)))
    }

    fn cleanup(home: &Path) {
        std::fs::remove_dir_all(home).ok();
    }

    fn setup(home: &Path) -> (ProviderCtx, PathBuf) {
        let ctx = ProviderCtx::for_test(home.to_path_buf(), FixedOffset::east_opt(0).unwrap());
        let db = kiro_dbs(&ctx)[0].clone();
        (ctx, db)
    }

    fn since_default() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn conv(id: &str, history: Vec<Value>) -> Value {
        json!({ "conversation_id": id, "history": history, "latest_summary": null })
    }

    fn turn(ts: i64, model: Option<&str>, user: &str, assistant: &str, resp: i64) -> Value {
        let mut meta = json!({
            "request_start_timestamp_ms": ts,
            "response_size": resp,
            "time_between_chunks": [],
            "tool_use_ids_and_names": [],
        });
        if let Some(m) = model {
            meta["model_id"] = json!(m);
        }
        json!({
            "user": { "content": user },
            "assistant": { "content": assistant },
            "request_metadata": meta,
        })
    }

    fn turn_missing_ts(model: Option<&str>, user: &str, assistant: &str) -> Value {
        let mut meta = json!({ "response_size": 0 });
        if let Some(m) = model {
            meta["model_id"] = json!(m);
        }
        json!({
            "user": { "content": user },
            "assistant": { "content": assistant },
            "request_metadata": meta,
        })
    }

    fn seed_v2(db: &Path, rows: &[(String, String)]) {
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(db).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversations_v2 (
                conversation_id TEXT PRIMARY KEY, key TEXT, created_at INTEGER, updated_at INTEGER, value TEXT);",
        )
        .unwrap();
        for (id, value) in rows {
            conn.execute(
                "INSERT INTO conversations_v2 (conversation_id, key, created_at, updated_at, value) \
                 VALUES (?1, ?2, 0, 0, ?3)",
                rusqlite::params![id, "/Users/dev/project", value],
            )
            .unwrap();
        }
        drop(conn);
    }

    fn seed_v1(db: &Path, rows: &[(String, String)]) {
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(db).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS conversations (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
        for (cwd, value) in rows {
            conn.execute(
                "INSERT INTO conversations (key, value) VALUES (?1, ?2)",
                rusqlite::params![cwd, value],
            )
            .unwrap();
        }
        drop(conn);
    }

    // MARK: token accounting

    #[test]
    fn first_turn_input_is_user_message_estimate() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(
            &db,
            &[(
                "conv-1".into(),
                conv(
                    "conv-1",
                    vec![turn(
                        1_780_000_000_000,
                        Some("claude-sonnet-4.5"),
                        &"u".repeat(400),
                        "",
                        200,
                    )],
                )
                .to_string(),
            )],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].input, 100, "400 bytes / 4 = 100 estimated input");
        assert_eq!(entries[0].output, 50, "200 bytes / 4 = 50 estimated output");
        assert_eq!(entries[0].cache_read, 0);
        assert_eq!(entries[0].cache_write, 0);
        assert_eq!(entries[0].model, "claude-sonnet-4.5");
        assert_eq!(entries[0].id, "kiro|conv-1|1780000000000");
    }

    #[test]
    fn later_turn_includes_the_accumulated_conversation_history() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(
            &db,
            &[(
                "conv-1".into(),
                conv(
                    "conv-1",
                    vec![
                        turn(
                            1_780_000_000_000,
                            Some("claude-sonnet-4.5"),
                            &"u".repeat(400),
                            &"a".repeat(800),
                            800,
                        ),
                        turn(
                            1_780_000_100_000,
                            Some("claude-sonnet-4.5"),
                            &"u".repeat(40),
                            "",
                            40,
                        ),
                    ],
                )
                .to_string(),
            )],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        let second = entries
            .iter()
            .find(|e| e.id == "kiro|conv-1|1780000100000")
            .unwrap();
        assert_eq!(second.input, (400 + 800 + 40) / 4);
    }

    #[test]
    fn skipped_turns_still_contribute_to_later_history() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(
            &db,
            &[(
                "conv-1".into(),
                conv(
                    "conv-1",
                    vec![
                        turn_missing_ts(
                            Some("claude-sonnet-4.5"),
                            &"u".repeat(400),
                            &"a".repeat(400),
                        ),
                        turn(
                            1_780_000_000_000,
                            Some("claude-sonnet-4.5"),
                            &"u".repeat(40),
                            "",
                            40,
                        ),
                    ],
                )
                .to_string(),
            )],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert_eq!(entries[0].input, (400 + 400 + 40) / 4);
    }

    #[test]
    fn latest_summary_seeds_the_accumulated_history() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        let cv = json!({
            "conversation_id": "conv-1",
            "latest_summary": ["s".repeat(120)],
            "history": [turn(1_780_000_000_000, Some("claude-sonnet-4.5"), &"u".repeat(40), "", 40)],
        });
        seed_v2(&db, &[("conv-1".into(), cv.to_string())]);
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert_eq!(entries[0].input, (120 + 40) / 4);
    }

    // MARK: rescan stability

    #[test]
    fn rescanning_unchanged_db_produces_stable_ids() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(
            &db,
            &[(
                "conv-1".into(),
                conv(
                    "conv-1",
                    vec![
                        turn(
                            1_780_000_000_000,
                            Some("claude-sonnet-4.5"),
                            &"u".repeat(400),
                            &"a".repeat(200),
                            200,
                        ),
                        turn(
                            1_780_000_100_000,
                            Some("claude-sonnet-4.5"),
                            &"u".repeat(40),
                            "",
                            40,
                        ),
                    ],
                )
                .to_string(),
            )],
        );
        let since = since_default();
        let first = kiro_entries(&ctx, since, None);
        let second = kiro_entries(&ctx, since, None);
        cleanup(&home);
        let ids1: HashSet<_> = first.iter().map(|e| e.id.clone()).collect();
        let ids2: HashSet<_> = second.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids1, ids2);
        let mut both = first.clone();
        both.extend(second);
        assert_eq!(dedup_keep_max(both).len(), first.len());
    }

    #[test]
    fn missing_model_id_falls_back_to_unknown() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(
            &db,
            &[(
                "conv-1".into(),
                conv(
                    "conv-1",
                    vec![turn(1_780_000_000_000, None, &"u".repeat(40), "", 40)],
                )
                .to_string(),
            )],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert_eq!(entries[0].model, "unknown");
    }

    #[test]
    fn zero_byte_turns_are_skipped() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(
            &db,
            &[(
                "conv-1".into(),
                conv(
                    "conv-1",
                    vec![
                        turn(1_780_000_000_000, Some("claude-sonnet-4.5"), "", "", 0),
                        turn(
                            1_780_000_001_000,
                            Some("claude-sonnet-4.5"),
                            &"u".repeat(4),
                            "",
                            0,
                        ),
                    ],
                )
                .to_string(),
            )],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert_eq!(
            entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            ["kiro|conv-1|1780000001000"]
        );
    }

    #[test]
    fn images_field_is_excluded_from_the_byte_estimate() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        let t = json!({
            "user": { "content": "u".repeat(40), "images": ["x".repeat(1_000_000)] },
            "assistant": {},
            "request_metadata": {
                "request_start_timestamp_ms": 1_780_000_000_000i64,
                "model_id": "claude-sonnet-4.5",
                "response_size": 40,
            },
        });
        seed_v2(
            &db,
            &[("conv-1".into(), conv("conv-1", vec![t]).to_string())],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert_eq!(
            entries[0].input, 10,
            "only the 40-byte `content` counts, not the image blob"
        );
    }

    // MARK: timestamps / windowing

    #[test]
    fn turns_missing_timestamp_are_skipped() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(
            &db,
            &[(
                "conv-1".into(),
                conv(
                    "conv-1",
                    vec![
                        turn_missing_ts(Some("claude-sonnet-4.5"), &"u".repeat(400), ""),
                        turn(
                            1_780_000_000_000,
                            Some("claude-sonnet-4.5"),
                            &"u".repeat(400),
                            "",
                            200,
                        ),
                    ],
                )
                .to_string(),
            )],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn turns_before_the_window_are_excluded() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(
            &db,
            &[(
                "conv-1".into(),
                conv(
                    "conv-1",
                    vec![
                        turn(
                            1_766_000_000_000,
                            Some("claude-sonnet-4.5"),
                            &"u".repeat(400),
                            "",
                            200,
                        ),
                        turn(
                            1_767_500_000_000,
                            Some("claude-sonnet-4.5"),
                            &"u".repeat(400),
                            "",
                            200,
                        ),
                    ],
                )
                .to_string(),
            )],
        );
        let since = DateTime::<Utc>::from_timestamp_millis(1_767_000_000_000).unwrap();
        let entries = kiro_entries(&ctx, since, None);
        cleanup(&home);
        assert_eq!(
            entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            ["kiro|conv-1|1767500000000"]
        );
    }

    #[test]
    fn conversation_without_request_metadata_is_skipped() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        let cv =
            json!({ "conversation_id": "conv-1", "history": [{ "user": {}, "assistant": {} }] });
        seed_v2(&db, &[("conv-1".into(), cv.to_string())]);
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert!(entries.is_empty());
    }

    // MARK: schema generations

    #[test]
    fn reads_the_v2_conversations_table() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(
            &db,
            &[(
                "conv-1".into(),
                conv(
                    "conv-1",
                    vec![turn(
                        1_780_000_000_000,
                        Some("claude-sonnet-4.5"),
                        &"u".repeat(400),
                        "",
                        200,
                    )],
                )
                .to_string(),
            )],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert_eq!(
            entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            ["kiro|conv-1|1780000000000"]
        );
    }

    #[test]
    fn reads_the_v1_conversations_table() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v1(
            &db,
            &[(
                "/Users/dev/project".into(),
                conv(
                    "conv-2",
                    vec![turn(
                        1_780_000_000_000,
                        Some("claude-sonnet-4.5"),
                        &"u".repeat(800),
                        "",
                        400,
                    )],
                )
                .to_string(),
            )],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert_eq!(entries[0].id, "kiro|conv-2|1780000000000");
        assert_eq!(entries[0].input, 200);
        assert_eq!(entries[0].output, 100);
    }

    #[test]
    fn v1_row_without_conversation_id_is_skipped() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        let cv = json!({ "history": [{ "request_metadata": { "request_start_timestamp_ms": 1_780_000_000_000i64, "model_id": "claude-sonnet-4.5", "response_size": 200 } }] });
        seed_v1(&db, &[("/Users/dev/project".into(), cv.to_string())]);
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert!(entries.is_empty());
    }

    #[test]
    fn malformed_json_rows_are_skipped_in_both_schemas() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(&db, &[("conv-1".into(), "{not valid json".into())]);
        seed_v1(
            &db,
            &[("/Users/dev/project".into(), "{not valid json".into())],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert!(entries.is_empty());
    }

    #[test]
    fn conversation_without_history_array_is_skipped() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(
            &db,
            &[("conv-1".into(), r#"{"conversation_id":"conv-1"}"#.into())],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert!(entries.is_empty());
    }

    #[test]
    fn same_conversation_in_both_tables_is_not_double_counted() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        let t = turn(
            1_780_000_000_000,
            Some("claude-sonnet-4.5"),
            &"u".repeat(400),
            "",
            200,
        );
        seed_v2(
            &db,
            &[("conv-1".into(), conv("conv-1", vec![t.clone()]).to_string())],
        );
        seed_v1(
            &db,
            &[(
                "/Users/dev/project".into(),
                conv("conv-1", vec![t]).to_string(),
            )],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn conversations_across_both_tables_are_both_counted() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(
            &db,
            &[(
                "conv-1".into(),
                conv(
                    "conv-1",
                    vec![turn(
                        1_780_000_000_000,
                        Some("claude-sonnet-4.5"),
                        &"u".repeat(400),
                        "",
                        200,
                    )],
                )
                .to_string(),
            )],
        );
        seed_v1(
            &db,
            &[(
                "/Users/dev/other-project".into(),
                conv(
                    "conv-2",
                    vec![turn(
                        1_780_000_100_000,
                        Some("claude-sonnet-4.5"),
                        &"u".repeat(800),
                        "",
                        400,
                    )],
                )
                .to_string(),
            )],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        let ids: HashSet<_> = entries.iter().map(|e| e.id.clone()).collect();
        assert_eq!(
            ids,
            [
                "kiro|conv-1|1780000000000".to_string(),
                "kiro|conv-2|1780000100000".to_string()
            ]
            .into_iter()
            .collect()
        );
    }

    // MARK: roots

    #[test]
    fn accepts_a_direct_database_path_as_root() {
        let home = temp_home();
        std::fs::create_dir_all(&home).unwrap();
        let db = home.join("kiro.sqlite3");
        seed_v2(
            &db,
            &[(
                "conv-1".into(),
                conv(
                    "conv-1",
                    vec![turn(
                        1_780_000_000_000,
                        Some("claude-sonnet-4.5"),
                        &"u".repeat(400),
                        "",
                        200,
                    )],
                )
                .to_string(),
            )],
        );
        let mut env = HashMap::new();
        env.insert("KIRO_CLI_HOME".to_string(), db.display().to_string());
        let ctx = ProviderCtx {
            home: home.clone(),
            env,
            tz: FixedOffset::east_opt(0).unwrap(),
        };
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn nonexistent_root_returns_nothing() {
        let home = temp_home();
        let (ctx, _db) = setup(&home);
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        assert!(entries.is_empty());
    }

    #[test]
    fn default_root_is_the_kiro_cli_home() {
        let home = PathBuf::from("/home/testkit");
        let ctx = ProviderCtx::for_test(home.clone(), FixedOffset::east_opt(0).unwrap());
        let roots = kiro_dbs(&ctx);
        assert!(!roots.is_empty());
        let expected = {
            #[cfg(target_os = "macos")]
            home.join("Library/Application Support/kiro-cli")
                .join(DB_NAME);
            #[cfg(not(target_os = "macos"))]
            home.join(".config/kiro-cli").join(DB_NAME)
        };
        assert_eq!(roots, vec![expected]);
    }

    // MARK: aggregation

    #[test]
    fn daily_aggregates_every_turn_of_the_day() {
        let home = temp_home();
        let (ctx, db) = setup(&home);
        seed_v2(
            &db,
            &[(
                "conv-1".into(),
                conv(
                    "conv-1",
                    vec![
                        turn(
                            1_780_000_000_000,
                            Some("claude-sonnet-4.5"),
                            &"u".repeat(400),
                            "",
                            200,
                        ),
                        turn(1_780_000_100_000, Some("claude-sonnet-4.5"), "", "", 40),
                    ],
                )
                .to_string(),
            )],
        );
        let entries = kiro_entries(&ctx, since_default(), None);
        cleanup(&home);
        let day = entries[0].local_day.clone();
        let daily = aggregate::daily(&entries, &day).unwrap();
        assert_eq!(
            daily.total_tokens,
            entries.iter().map(|e| e.total()).sum::<i64>()
        );
    }

    #[test]
    fn in_place_conversation_updates_are_seen_via_the_signature_change() {
        use crate::usage_cache::UsageCache;
        let home = temp_home();
        let (ctx, db) = setup(&home);
        let one_turn = vec![turn(
            1_780_000_000_000,
            Some("claude-sonnet-4.5"),
            &"u".repeat(400),
            &"a".repeat(200),
            200,
        )];
        seed_v2(&db, &[("conv-1".into(), conv("conv-1", one_turn).to_string())]);
        let cache = UsageCache::open(&home.join("usage-cache.sqlite")).unwrap();
        let since = since_default();

        assert_eq!(kiro_entries(&ctx, since, Some(&cache)).len(), 1);
        // Unchanged: served from the cache.
        assert_eq!(kiro_entries(&ctx, since, Some(&cache)).len(), 1);

        // A turn lands in an existing conversation in place (UPDATE, no new rowid): the
        // changed signature must force a full re-read — a rowid watermark cannot see it.
        let two_turns = vec![
            turn(
                1_780_000_000_000,
                Some("claude-sonnet-4.5"),
                &"u".repeat(400),
                &"a".repeat(200),
                200,
            ),
            turn(1_780_000_100_000, Some("claude-sonnet-4.5"), &"u".repeat(40), "", 40),
        ];
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE conversations_v2 SET value = ?1 WHERE conversation_id = 'conv-1'",
            rusqlite::params![conv("conv-1", two_turns).to_string()],
        )
        .unwrap();
        drop(conn);

        let next = kiro_entries(&ctx, since, Some(&cache));
        let plain = kiro_entries(&ctx, since, None);
        assert_eq!(next.len(), 2, "the in-place appended turn must be re-read");
        assert_eq!(next, plain, "re-read after an in-place update must equal a full read");
        cleanup(&home);
    }

    // MARK: provider identity

    #[test]
    fn provider_identity() {
        let p = KiroProvider;
        assert_eq!(p.id(), "kiro");
        assert_eq!(p.display_name(), "Kiro");
        assert!(
            !p.reports_cost(),
            "tokens are a bytes/4 estimate — no real dollar cost"
        );
    }
}
