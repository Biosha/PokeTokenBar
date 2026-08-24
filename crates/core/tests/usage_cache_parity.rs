//! End-to-end cache parity: a full snapshot built with the usage cache enabled must be
//! identical to one built with it disabled — on a cold cache, on a warm unchanged cache,
//! and on a warm cache after the provider data has grown.
//!
//! One test on purpose: it toggles the process-wide `PTB_USAGE_CACHE` variable, so it must
//! not run in parallel with anything else in this binary.

use chrono::{DateTime, FixedOffset, Utc, Weekday};
use poketoken_core::provider::ProviderCtx;
use poketoken_core::usage_store::build_snapshot;
use poketoken_core::types::UsageSnapshot;
use rusqlite::Connection;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQ: AtomicUsize = AtomicUsize::new(0);

const NOW: &str = "2026-08-21T16:27:33Z";

/// 2026-08-20T10:00:00Z / 2026-08-21T10:00:00Z, the two fixture instants (epoch seconds).
const T0: i64 = 1_787_220_000;
const T1: i64 = 1_787_306_400;

fn utc() -> FixedOffset {
    FixedOffset::east_opt(0).unwrap()
}

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(NOW).unwrap().with_timezone(&Utc)
}

fn home() -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("ptb-parity-{}-{}", process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn cache_dir() -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("ptb-parity-cache-{}-{}", process::id(), n));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn append(path: &Path, text: &str) {
    use std::io::Write as _;
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

// ----- fixture builders (one small source per provider, default paths) ----------------

fn seed_claude(h: &Path) -> PathBuf {
    let projects = h.join(".claude/projects/proj");
    std::fs::create_dir_all(&projects).unwrap();
    let path = projects.join("s.jsonl");
    let rec = json!({
        "type": "assistant", "timestamp": "2026-08-20T10:00:00Z", "requestId": "r1",
        "message": { "id": "m1", "model": "claude-sonnet-4-6",
            "usage": { "input_tokens": 100, "output_tokens": 10,
                       "cache_creation_input_tokens": 10, "cache_read_input_tokens": 5 } }
    });
    std::fs::write(&path, format!("{rec}\n")).unwrap();
    path
}

fn seed_grok(h: &Path) -> PathBuf {
    let session = h.join(".grok/sessions/s1");
    std::fs::create_dir_all(&session).unwrap();
    let path = session.join("updates.jsonl");
    let rec = json!({
        "timestamp": T0,
        "params": { "sessionId": "s1", "update": {
            "sessionUpdate": "turn_completed", "prompt_id": "p1",
            "usage": { "inputTokens": 100, "outputTokens": 10, "totalTokens": 110 } } }
    });
    std::fs::write(&path, format!("{rec}\n")).unwrap();
    path
}

fn seed_gemini(h: &Path) -> PathBuf {
    let chats = h.join(".gemini/tmp/h1/chats");
    std::fs::create_dir_all(&chats).unwrap();
    let path = chats.join("session.jsonl");
    let rec = json!({
        "id": "m1", "timestamp": "2026-08-20T10:00:00Z", "model": "gemini-2.5-flash",
        "tokens": { "input": 100, "cached": 40, "tool": 0, "output": 30 }
    });
    std::fs::write(&path, format!("{rec}\n")).unwrap();
    path
}

fn seed_opencode(h: &Path) -> PathBuf {
    let root = h.join(".local/share/opencode");
    std::fs::create_dir_all(root.join("storage/message")).unwrap();
    let db = root.join("opencode.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE message(id TEXT, session_id TEXT, data TEXT, time_created INTEGER);
         INSERT INTO message VALUES
           ('m1','s1','{\"id\":\"m1\",\"modelID\":\"anthropic/claude-3-5-sonnet\",\"providerID\":\"anthropic\",\"time\":{\"created\":1787220000000},\"tokens\":{\"input\":100,\"output\":50,\"cache\":{\"read\":10,\"write\":5}},\"cost\":0.01}',1787220000000);",
    )
    .unwrap();
    drop(conn);
    std::fs::write(
        root.join("storage/message/m1.json"),
        "{\"id\":\"lg1\",\"modelID\":\"anthropic/claude-3-5-sonnet\",\"providerID\":\"anthropic\",\"time\":{\"created\":1787220000000},\"tokens\":{\"input\":7,\"output\":3}}",
    )
    .unwrap();
    db
}

fn seed_copilot(h: &Path) -> PathBuf {
    let dir = h.join(".copilot");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("session-store.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE assistant_usage_events(
           id INTEGER PRIMARY KEY, model TEXT, input_tokens INTEGER, output_tokens INTEGER,
           cache_read_tokens INTEGER, cache_write_tokens INTEGER, created_at TEXT);
         INSERT INTO assistant_usage_events VALUES (1,'gpt-4o',1000,500,300,20,'2026-08-20T10:00:00Z');",
    )
    .unwrap();
    drop(conn);
    db
}

fn seed_hermes(h: &Path) -> PathBuf {
    let dir = h.join(".hermes");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("state.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE sessions (id TEXT PRIMARY KEY, model TEXT, billing_provider TEXT,
           started_at REAL NOT NULL, message_count INTEGER, input_tokens INTEGER,
           output_tokens INTEGER, cache_read_tokens INTEGER, cache_write_tokens INTEGER,
           reasoning_tokens INTEGER, estimated_cost_usd REAL, actual_cost_usd REAL);
         INSERT INTO sessions VALUES
           ('s1','claude-sonnet-4-20250514','anthropic',1787220000,42,100,50,10,20,5,0.12,0.34);",
    )
    .unwrap();
    drop(conn);
    db
}

fn seed_cursor(h: &Path) -> PathBuf {
    let root = h.join(".config/Cursor/User/globalStorage");
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("state.vscdb");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
        rusqlite::params![
            "bubbleId:tab:1",
            json!({"tokenCount":{"inputTokens":10,"outputTokens":5},
                   "createdAt":"2026-08-20T10:00:00.000Z","modelType":"gpt-4o"})
                .to_string()
                .as_bytes()
                .to_vec()
        ],
    )
    .unwrap();
    drop(conn);
    db
}

fn seed_kiro(h: &Path) -> PathBuf {
    let dir = h.join(".config/kiro-cli");
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("data.sqlite3");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE conversations_v2 (
           conversation_id TEXT PRIMARY KEY, key TEXT, created_at INTEGER, updated_at INTEGER, value TEXT);",
    )
    .unwrap();
    let value = json!({
        "conversation_id": "c1", "latest_summary": null,
        "history": [ {
            "user": { "content": "hello" }, "assistant": { "content": "hi there" },
            "request_metadata": {
                "request_start_timestamp_ms": 1_787_220_000_000i64, "response_size": 100,
                "time_between_chunks": [], "tool_use_ids_and_names": [],
                "model_id": "kiro-model-1"
            }
        } ]
    });
    conn.execute(
        "INSERT INTO conversations_v2 (conversation_id, key, created_at, updated_at, value)
         VALUES (?1, ?2, 0, 0, ?3)",
        rusqlite::params!["c1", "/Users/dev/project", value.to_string()],
    )
    .unwrap();
    drop(conn);
    db
}

fn seed_codex(h: &Path) -> PathBuf {
    let dir = h.join(".codex/sessions/2026/08/20");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rollout-2026-08-20T10-00-00-0001.jsonl");
    let meta = json!({
        "type": "session_meta", "timestamp": "2026-08-20T10:00:00Z",
        "payload": { "id": "sess1", "session_id": "sess1" }
    });
    let tc = json!({
        "type": "event_msg", "timestamp": "2026-08-20T10:00:00Z",
        "payload": { "type": "token_count", "info": {
            "total_token_usage": { "input_tokens": 1000, "cached_input_tokens": 100,
                                   "output_tokens": 200, "reasoning_output_tokens": 0,
                                   "total_tokens": 1200 },
            "last_token_usage": { "input_tokens": 1000, "cached_input_tokens": 100,
                                  "output_tokens": 200, "reasoning_output_tokens": 0,
                                  "total_tokens": 1200 } } }
    });
    std::fs::write(&path, format!("{meta}\n{tc}\n")).unwrap();
    path
}

// -- Antigravity's wire format, test side (a copy of the unit-test encoders). ----------

fn raw_varint(mut v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return out;
        }
        out.push(b | 0x80);
    }
}

fn varint(field: u32, v: u64) -> Vec<u8> {
    let mut out = raw_varint(u64::from(field) << 3);
    out.extend(raw_varint(v));
    out
}

fn message(field: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = raw_varint((u64::from(field) << 3) | 2);
    out.extend(raw_varint(payload.len() as u64));
    out.extend_from_slice(payload);
    out
}

fn ag_record(response_id: &str, created_at: u64, input: u64, output: u64) -> Vec<u8> {
    let mut usage = varint(1, 1071);
    usage.extend(varint(2, input));
    usage.extend(varint(3, output));
    usage.extend(varint(5, 0));
    usage.extend(varint(6, 24));
    usage.extend(message(11, response_id.as_bytes()));
    let timestamp = varint(1, created_at);
    let chat_start = message(4, &timestamp);
    let mut chat_model = varint(3, 1071);
    chat_model.extend(message(4, &usage));
    chat_model.extend(message(9, &chat_start));
    chat_model.extend(message(19, b"gemini-3.6-flash"));
    message(1, &chat_model)
}

fn seed_antigravity(h: &Path) -> PathBuf {
    let root = h.join(".gemini/antigravity-cli/conversations");
    std::fs::create_dir_all(&root).unwrap();
    let db = root.join("c1.db");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE gen_metadata (idx integer, data blob, size integer NOT NULL DEFAULT 0, PRIMARY KEY (idx));",
    )
    .unwrap();
    let blob = ag_record("r1", T0 as u64, 100, 10);
    let hex: String = blob.iter().map(|b| format!("{b:02x}")).collect();
    conn.execute(
        format!("INSERT INTO gen_metadata VALUES (0, X'{hex}', {});", blob.len()).as_str(),
        [],
    )
    .unwrap();
    drop(conn);
    db
}

fn append_antigravity(db: &Path, response_id: &str, created_at: u64, input: u64, output: u64) {
    let conn = Connection::open(db).unwrap();
    let idx: i64 = conn
        .query_row("SELECT COALESCE(MAX(idx), -1) + 1 FROM gen_metadata", [], |r| {
            r.get(0)
        })
        .unwrap();
    let blob = ag_record(response_id, created_at, input, output);
    let hex: String = blob.iter().map(|b| format!("{b:02x}")).collect();
    conn.execute(
        format!(
            "INSERT INTO gen_metadata VALUES ({idx}, X'{hex}', {});",
            blob.len()
        )
        .as_str(),
        [],
    )
    .unwrap();
    drop(conn);
}

// ----- the parity test ---------------------------------------------------------------

#[test]
fn cache_on_matches_cache_off() {
    // Silence the helper above; the real body is inline because it needs `h` everywhere.
    let h = home();
    let cache = cache_dir();
    let h_claude = seed_claude(&h);
    let h_grok = seed_grok(&h);
    let h_gemini = seed_gemini(&h);
    let h_opencode = seed_opencode(&h);
    let h_copilot = seed_copilot(&h);
    let h_hermes = seed_hermes(&h);
    let h_cursor = seed_cursor(&h);
    let h_kiro = seed_kiro(&h);
    let h_codex = seed_codex(&h);
    let h_antigravity = seed_antigravity(&h);

    let ctx = ProviderCtx::for_test(h.clone(), utc());
    let snap = |cache: Option<&Path>| -> UsageSnapshot {
        if let Some(dir) = cache {
            std::env::set_var("PTB_USAGE_CACHE", dir);
        } else {
            std::env::set_var("PTB_USAGE_CACHE", "off");
        }
        build_snapshot(&ctx, now(), Weekday::Mon)
    };

    // All ten providers must be detected, otherwise this test is vacuous.
    let base = snap(None);
    let ids: Vec<&str> = base
        .providers
        .iter()
        .map(|p| p.provider_id.as_str())
        .collect();
    for want in [
        "claude_code",
        "codex",
        "copilot",
        "cursor",
        "gemini_cli",
        "grok_cli",
        "hermes",
        "kiro",
        "opencode",
        "antigravity",
    ] {
        assert!(ids.contains(&want), "missing provider {want:?}; saw {ids:?}");
    }

    // Cold cache: identical to a cache-off read.
    assert_eq!(snap(Some(&cache)), base, "cold cache must equal cache-off");
    // Warm, unchanged: still identical.
    assert_eq!(snap(Some(&cache)), base, "warm unchanged cache must equal cache-off");

    // Grow every provider's data by one record.
    let rec = json!({
        "type": "assistant", "timestamp": "2026-08-21T10:00:00Z", "requestId": "r2",
        "message": { "id": "m2", "model": "claude-sonnet-4-6",
            "usage": { "input_tokens": 200, "output_tokens": 20,
                       "cache_creation_input_tokens": 10, "cache_read_input_tokens": 5 } }
    });
    append(&h_claude, &format!("{rec}\n"));
    let rec = json!({
        "timestamp": T1,
        "params": { "sessionId": "s1", "update": {
            "sessionUpdate": "turn_completed", "prompt_id": "p2",
            "usage": { "inputTokens": 300, "outputTokens": 30, "totalTokens": 330 } } }
    });
    append(&h_grok, &format!("{rec}\n"));
    let rec = json!({
        "id": "m2", "timestamp": "2026-08-21T10:00:00Z", "model": "gemini-2.5-flash",
        "tokens": { "input": 400, "cached": 0, "tool": 0, "output": 40 }
    });
    append(&h_gemini, &format!("{rec}\n"));
    {
        let conn = Connection::open(&h_opencode).unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m2','s2','{\"id\":\"m2\",\"modelID\":\"anthropic/claude-3-5-sonnet\",\"providerID\":\"anthropic\",\"time\":{\"created\":1787306400000},\"tokens\":{\"input\":7,\"output\":3}}',1787306400000)",
            [],
        )
        .unwrap();
        drop(conn);
    }
    {
        let conn = Connection::open(&h_copilot).unwrap();
        conn.execute(
            "INSERT INTO assistant_usage_events VALUES (2,'gpt-4o',100,50,0,0,'2026-08-21T10:00:00Z')",
            [],
        )
        .unwrap();
        drop(conn);
    }
    {
        let conn = Connection::open(&h_hermes).unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES ('s2','claude-sonnet-4-20250514','anthropic',1787306400,7,900,90,0,0,0,0.05,0.5)",
            [],
        )
        .unwrap();
        drop(conn);
    }
    {
        let conn = Connection::open(&h_cursor).unwrap();
        conn.execute(
            "INSERT INTO cursorDiskKV VALUES (?1, ?2)",
            rusqlite::params![
                "bubbleId:tab:2",
                json!({"tokenCount":{"inputTokens":60,"outputTokens":6},
                       "createdAt":"2026-08-21T10:00:00.000Z","modelType":"gpt-4o"})
                    .to_string()
                    .as_bytes()
                    .to_vec()
            ],
        )
        .unwrap();
        drop(conn);
    }
    {
        let conn = Connection::open(&h_kiro).unwrap();
        let value = json!({
            "conversation_id": "c2", "latest_summary": null,
            "history": [ {
                "user": { "content": "hello again" }, "assistant": { "content": "hi again" },
                "request_metadata": {
                    "request_start_timestamp_ms": 1_787_306_400_000i64, "response_size": 90,
                    "time_between_chunks": [], "tool_use_ids_and_names": [],
                    "model_id": "kiro-model-1"
                }
            } ]
        });
        conn.execute(
            "INSERT INTO conversations_v2 (conversation_id, key, created_at, updated_at, value)
             VALUES (?1, ?2, 0, 0, ?3)",
            rusqlite::params!["c2", "/Users/dev/project", value.to_string()],
        )
        .unwrap();
        drop(conn);
    }
    let tc = json!({
        "type": "event_msg", "timestamp": "2026-08-21T10:00:00Z",
        "payload": { "type": "token_count", "info": {
            "total_token_usage": { "input_tokens": 1600, "cached_input_tokens": 100,
                                   "output_tokens": 320, "reasoning_output_tokens": 0,
                                   "total_tokens": 1920 },
            "last_token_usage": { "input_tokens": 600, "cached_input_tokens": 0,
                                  "output_tokens": 120, "reasoning_output_tokens": 0,
                                  "total_tokens": 720 } } }
    });
    append(&h_codex, &format!("{tc}\n"));
    append_antigravity(&h_antigravity, "r2", T1 as u64, 500, 50);

    // The grown data must actually change the totals (the appends are in-window).
    let grown_off = snap(None);
    assert_ne!(
        grown_off.combined_today.as_ref().map(|d| d.total_tokens),
        base.combined_today.as_ref().map(|d| d.total_tokens),
        "appends must change the combined total"
    );

    // The warm cache is now stale: it must catch up exactly to a fresh cache-off read.
    assert_eq!(
        snap(Some(&cache)),
        grown_off,
        "warm cache after appends must equal cache-off"
    );

    std::fs::remove_dir_all(&h).ok();
    std::fs::remove_dir_all(&cache).ok();
}
