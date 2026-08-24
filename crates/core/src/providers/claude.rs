//! Claude Code — `~/.claude/projects/**/*.jsonl`.
//!
//! Reads `type:"assistant"` lines with a `message.usage` (4 token fields), `message.model`,
//! `message.id` + `requestId`, and `timestamp`. Same message can appear across resumed/sidechain
//! sessions → global dedup on `(message.id, requestId)`, keeping the largest total.
//!
//! Incremental (see `usage_cache`): each file is a byte-offset-marked source; a grown file
//! contributes only its appended complete lines, and the per-file keep-max dedup is the merge
//! that makes the incremental result identical to a full re-parse.

use crate::entry::{dedup_keep_max, Entry};
use crate::provider::{ProviderCtx, UsageProvider};
use crate::{fsutil, iso8601, usage_cache, util, windows};
use chrono::{DateTime, FixedOffset, Utc};
use serde_json::Value;
use std::path::PathBuf;

pub struct ClaudeProvider;

impl UsageProvider for ClaudeProvider {
    fn id(&self) -> &'static str {
        "claude_code"
    }
    fn display_name(&self) -> &'static str {
        "Claude Code"
    }
    fn available(&self, ctx: &ProviderCtx) -> bool {
        claude_roots(ctx).iter().any(|r| r.is_dir())
    }
    fn read_entries(&self, ctx: &ProviderCtx, since: DateTime<Utc>) -> anyhow::Result<Vec<Entry>> {
        let cache = usage_cache::UsageCache::resolve();
        Ok(claude_entries(ctx, since, cache.as_ref()))
    }
}

/// The read path, with the cache injected so tests can pin it to a temp file.
fn claude_entries(
    ctx: &ProviderCtx,
    since: DateTime<Utc>,
    cache: Option<&usage_cache::UsageCache>,
) -> Vec<Entry> {
    let full = cache
        .map(|c| c.full_rescan_due("claude_code", &ctx.tz))
        .unwrap_or(true);
    let mut all: Vec<Entry> = Vec::new();
    let mut keep: Vec<String> = Vec::new();
    for root in claude_roots(ctx) {
        for f in fsutil::walk_modified(&root, &["jsonl"], since, false, 24) {
            let path = f.path;
            let key = usage_cache::source_key(&path);
            let entries = match cache {
                Some(c) => c.read_file_source(
                    "claude_code",
                    &path,
                    &key,
                    full,
                    || parse_claude_file(&path, ctx.tz),
                    |bytes| parse_claude_lines(bytes, ctx.tz),
                    |mut a, b| {
                        a.extend(b);
                        dedup_keep_max(a)
                    },
                ),
                None => parse_claude_file(&path, ctx.tz),
            };
            keep.push(key);
            all.extend(entries);
        }
    }
    if let Some(c) = cache {
        c.prune_sources("claude_code", &keep);
        c.prune_entries_before("claude_code", since);
        if full {
            c.mark_full_scanned("claude_code", &ctx.tz);
        }
    }
    dedup_keep_max(all)
}

/// Every possible Claude `projects` root: `$CLAUDE_CONFIG_DIR/*/projects` (comma list),
/// `~/.config/claude/projects`, `~/.claude/projects`. (The macOS Desktop-embedded roots do not
/// exist on Linux.)
pub fn claude_roots(ctx: &ProviderCtx) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(v) = ctx.var("CLAUDE_CONFIG_DIR") {
        for part in v.split(',') {
            let p = part.trim();
            if !p.is_empty() {
                roots.push(fsutil::expand_tilde(p, &ctx.home).join("projects"));
            }
        }
    }
    roots.push(ctx.home.join(".config/claude/projects"));
    roots.push(ctx.home.join(".claude/projects"));
    fsutil::normalized_roots(roots)
}

/// Parse one Claude file (file-level dedup). Returns on read failure (empty); a marker line
/// that is not valid JSON is skipped (the file stays on disk, so this degrades, never traps).
pub fn parse_claude_file(path: &std::path::Path, tz: FixedOffset) -> Vec<Entry> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    // The lenient path never returns `Err` (only the strict tail path does).
    let out = parse_claude_text(&text, tz, false).unwrap_or_default();
    dedup_keep_max(out)
}

/// Parse complete lines. `strict` (the incremental tail path) turns a marker line that is not
/// valid JSON into `Err` so the caller re-reads the whole file — the tail never silently drops
/// a row. The full path is lenient: the same line is skipped there, which is what a
/// cache-off read has always done.
fn parse_claude_text(text: &str, tz: FixedOffset, strict: bool) -> Result<Vec<Entry>, ()> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.contains("\"usage\"") && line.contains("\"assistant\"") {
            if serde_json::from_str::<Value>(line).is_err() && strict {
                return Err(());
            }
            if let Some(e) = parse_claude_line(line, tz) {
                out.push(e);
            }
        }
    }
    Ok(out)
}

/// Strict (tail) form over bytes: invalid UTF-8 is also a parse error.
fn parse_claude_lines(bytes: &[u8], tz: FixedOffset) -> Result<Vec<Entry>, ()> {
    let text = std::str::from_utf8(bytes).map_err(|_| ())?;
    parse_claude_text(text, tz, true)
}

fn parse_claude_line(line: &str, tz: FixedOffset) -> Option<Entry> {
    let v: Value = serde_json::from_str(line).ok()?;
    if util::get_str(&v, "type") != Some("assistant") {
        return None;
    }
    let msg = v.get("message")?;
    let usage = msg.get("usage")?;
    let ts = util::get_str(&v, "timestamp")?;
    let date = iso8601::parse(ts)?;
    let model = util::get_str(msg, "model").unwrap_or("unknown").to_string();
    let id = format!(
        "{}|{}",
        util::get_str(msg, "id").unwrap_or(""),
        util::get_str(&v, "requestId").unwrap_or("")
    );
    Some(Entry {
        id,
        date,
        local_day: windows::local_day(date, &tz),
        model,
        input: util::get_int(usage, "input_tokens"),
        output: util::get_int(usage, "output_tokens"),
        cache_write: util::get_int(usage, "cache_creation_input_tokens"),
        cache_read: util::get_int(usage, "cache_read_input_tokens"),
        explicit_cost: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: &str, request: &str, ts: &str, model: &str, in_tok: i64, out_tok: i64) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "requestId": request,
            "message": {
                "id": id,
                "model": model,
                "usage": {
                    "input_tokens": in_tok,
                    "output_tokens": out_tok,
                    "cache_creation_input_tokens": 10,
                    "cache_read_input_tokens": 5,
                }
            }
        })
        .to_string()
    }

    #[test]
    fn dedup_keeps_max_per_id() {
        let entries = vec![
            parse_claude_line(
                &line(
                    "m1",
                    "r1",
                    "2026-01-02T03:00:00Z",
                    "claude-sonnet-4-6",
                    100,
                    10,
                ),
                FixedOffset::east_opt(0).unwrap(),
            ),
            // same (id, requestId) re-logged with a larger completed output
            parse_claude_line(
                &line(
                    "m1",
                    "r1",
                    "2026-01-02T03:00:01Z",
                    "claude-sonnet-4-6",
                    100,
                    40,
                ),
                FixedOffset::east_opt(0).unwrap(),
            ),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let deduped = dedup_keep_max(entries);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].output, 40);
    }

    #[test]
    fn ignores_non_assistant() {
        let e = parse_claude_line(
            &serde_json::json!({"type": "user", "timestamp": "2026-01-02T03:00:00Z"}).to_string(),
            FixedOffset::east_opt(0).unwrap(),
        );
        assert!(e.is_none());
    }

    // ----- incremental watermark -----

    use crate::usage_cache::UsageCache;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CTX_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// Temp home (with a `.claude/projects` root) + temp cache; `since` at epoch.
    fn watermark_env() -> (ProviderCtx, PathBuf, PathBuf, UsageCache, DateTime<Utc>) {
        let n = CTX_SEQ.fetch_add(1, Ordering::SeqCst);
        let home = std::env::temp_dir().join(format!("ptb-claude-{}-{n}", std::process::id()));
        let projects = home.join(".claude/projects/proj");
        std::fs::create_dir_all(&projects).unwrap();
        let cache_dir = home.join("cache");
        let cache = UsageCache::open(&cache_dir.join("usage-cache.sqlite")).unwrap();
        let ctx = ProviderCtx::for_test(home.clone(), FixedOffset::east_opt(0).unwrap());
        let since = DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        (ctx, projects, home, cache, since)
    }

    fn session_file(projects: &Path, ts: &str, input: i64, output: i64) -> PathBuf {
        let path = projects.join("s.jsonl");
        let rec = line("m1", "r1", ts, "claude-sonnet-4-6", input, output);
        std::fs::write(&path, format!("{rec}\n")).unwrap();
        path
    }

    fn append_session_file(projects: &Path, ts: &str, input: i64, output: i64) {
        let path = projects.join("s.jsonl");
        let rec = line("m2", "r2", ts, "claude-sonnet-4-6", input, output);
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(format!("{rec}\n").as_bytes()).unwrap();
    }

    fn ids(entries: &[Entry]) -> Vec<String> {
        entries.iter().map(|e| e.id.clone()).collect()
    }

    #[test]
    fn incremental_tail_matches_full_read() {
        let (ctx, projects, home, cache, since) = watermark_env();
        session_file(&projects, "2026-08-20T10:00:00Z", 100, 10);
        let first = claude_entries(&ctx, since, Some(&cache));
        assert_eq!(first.len(), 1);

        append_session_file(&projects, "2026-08-20T11:00:00Z", 200, 20);
        let second = claude_entries(&ctx, since, Some(&cache));
        assert_eq!(ids(&second), vec!["m1|r1".to_string(), "m2|r2".to_string()]);

        let expected = dedup_keep_max(parse_claude_file(&projects.join("s.jsonl"), ctx.tz));
        assert_eq!(
            ids(&second),
            ids(&expected),
            "incremental must equal a full parse"
        );
        let marker = cache
            .source(
                "claude_code",
                &usage_cache::source_key(&projects.join("s.jsonl")),
            )
            .unwrap()
            .unwrap()
            .marker;
        assert_eq!(
            marker as u64,
            std::fs::metadata(projects.join("s.jsonl")).unwrap().len()
        );

        // Unchanged file: same result, no reparse needed.
        let third = claude_entries(&ctx, since, Some(&cache));
        assert_eq!(ids(&third), ids(&expected));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn truncated_file_is_fully_reread() {
        let (ctx, projects, home, cache, since) = watermark_env();
        session_file(&projects, "2026-08-20T10:00:00Z", 100, 10);
        append_session_file(&projects, "2026-08-20T11:00:00Z", 200, 20);
        assert_eq!(claude_entries(&ctx, since, Some(&cache)).len(), 2);

        // Rotation: the file is replaced by a shorter one.
        let rec = line(
            "m3",
            "r3",
            "2026-08-21T09:00:00Z",
            "claude-sonnet-4-6",
            5,
            5,
        );
        std::fs::write(projects.join("s.jsonl"), format!("{rec}\n")).unwrap();
        let reread = claude_entries(&ctx, since, Some(&cache));
        assert_eq!(
            ids(&reread),
            vec!["m3|r3".to_string()],
            "a shrunken file must be re-read whole"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn bad_tail_line_forces_full_reread() {
        let (ctx, projects, home, cache, since) = watermark_env();
        session_file(&projects, "2026-08-20T10:00:00Z", 100, 10);
        assert_eq!(claude_entries(&ctx, since, Some(&cache)).len(), 1);

        // A marker line that is not valid JSON: the tail parse must reject it and the file
        // must be re-read whole (which skips the bad line, as a cache-off read does).
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(projects.join("s.jsonl"))
            .unwrap();
        f.write_all(b"\"usage\" \"assistant\" {broken\n").unwrap();
        append_session_file(&projects, "2026-08-20T12:00:00Z", 300, 30);
        let reread = claude_entries(&ctx, since, Some(&cache));
        assert_eq!(ids(&reread), vec!["m1|r1".to_string(), "m2|r2".to_string()]);

        // The bad line is now behind the marker; good appends flow again (m2|r2 re-logged
        // with a larger total wins the keep-max dedup).
        let rec = line(
            "m2",
            "r2",
            "2026-08-20T13:00:00Z",
            "claude-sonnet-4-6",
            400,
            40,
        );
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(projects.join("s.jsonl"))
            .unwrap();
        f.write_all(format!("{rec}\n").as_bytes()).unwrap();
        let next = claude_entries(&ctx, since, Some(&cache));
        assert_eq!(ids(&next), vec!["m1|r1".to_string(), "m2|r2".to_string()]);
        assert_eq!(next.iter().find(|e| e.id == "m2|r2").unwrap().input, 400);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn new_file_is_discovered_and_cached() {
        let (ctx, projects, home, cache, since) = watermark_env();
        session_file(&projects, "2026-08-20T10:00:00Z", 100, 10);
        assert_eq!(claude_entries(&ctx, since, Some(&cache)).len(), 1);

        let rec = line(
            "m9",
            "r9",
            "2026-08-21T09:00:00Z",
            "claude-sonnet-4-6",
            7,
            7,
        );
        std::fs::write(projects.join("s2.jsonl"), format!("{rec}\n")).unwrap();
        let next = claude_entries(&ctx, since, Some(&cache));
        assert_eq!(next.len(), 2, "a new file must be parsed and merged");
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn partial_trailing_line_is_not_lost() {
        let (ctx, projects, home, cache, since) = watermark_env();
        // Valid record without its terminating newline: a writer mid-flight.
        let rec = line(
            "m1",
            "r1",
            "2026-08-20T10:00:00Z",
            "claude-sonnet-4-6",
            100,
            10,
        );
        std::fs::write(projects.join("s.jsonl"), rec).unwrap();
        let first = claude_entries(&ctx, since, Some(&cache));
        assert_eq!(first.len(), 1, "full read sees the un-terminated line");

        // Still un-terminated: the marker must not advance past it.
        let second = claude_entries(&ctx, since, Some(&cache));
        assert_eq!(second.len(), 1);

        // Now terminated, plus a new record: both count exactly once.
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(projects.join("s.jsonl"))
            .unwrap();
        f.write_all(
            format!(
                "\n{}\n",
                line(
                    "m2",
                    "r2",
                    "2026-08-20T11:00:00Z",
                    "claude-sonnet-4-6",
                    200,
                    20
                )
            )
            .as_bytes(),
        )
        .unwrap();
        let third = claude_entries(&ctx, since, Some(&cache));
        assert_eq!(ids(&third), vec!["m1|r1".to_string(), "m2|r2".to_string()]);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn day_rollover_forces_full_reread() {
        let (ctx, projects, home, cache, since) = watermark_env();
        session_file(&projects, "2026-08-20T10:00:00Z", 100, 10);
        assert_eq!(claude_entries(&ctx, since, Some(&cache)).len(), 1);

        // Rewrite with the SAME size (a same-length edit is invisible to a size watermark).
        let rec = line(
            "m1",
            "r1",
            "2026-08-20T10:00:00Z",
            "claude-sonnet-4-6",
            200,
            10,
        );
        std::fs::write(projects.join("s.jsonl"), format!("{rec}\n")).unwrap();
        let same = claude_entries(&ctx, since, Some(&cache));
        assert_eq!(
            same[0].input, 100,
            "same-size rewrite stays cached within the day"
        );

        // The next local day's first pass must rebuild from source.
        cache.meta_set("full_day:claude_code", "1999-01-01");
        let fresh = claude_entries(&ctx, since, Some(&cache));
        assert_eq!(
            fresh[0].input, 200,
            "the daily full rescan heals same-size rewrites"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
