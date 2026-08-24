//! Codex — `~/.codex/sessions/**/rollout-*.jsonl` + `~/.codex/archived_sessions/`.
//!
//! A rollout is a JSONL stream of log records. Cumulative token usage arrives from
//! `event_msg` records with `payload.type == "token_count"`:
//! `info.total_token_usage` (cumulative since session start) plus
//! `info.last_token_usage` (that turn's delta). The ported `Entry` is built from the delta:
//! `input = (input_tokens − cached_input_tokens)`, `output = output_tokens`
//! (reasoning is already inside output), `cache_read = cached_input_tokens`, `cache_write = 0`.
//!
//! Two bug-sensitive behaviors carry over verbatim from the macOS reader:
//! 1. **Same-state rerecord drop**: within one file, a consecutive `token_count` with an
//!    identical full usage state (cumulative *and* last vector) carries no new tokens and is
//!    dropped once.
//! 2. **Fork / subagent replay trim**: a forked rollout re-inserts its parent's `token_count`
//!    records at the top. `resolve_codex_rollouts` compares the child's usage-state prefix
//!    against the matched parent's resolved history and drops exactly the replayed prefix; a
//!    manual fork with no usable parent falls back to a 1-second gap heuristic. Subagents with
//!    no overlapping prefix keep all of their own usage.
//!
//! Canonical entries are `codex|<owner>|<epoch>|<state-fingerprint>`; dedup keeps the **earliest
//!** date per id (a token vector is part of the id, so keep-earliest, not keep-max, fits Codex).

use crate::entry::Entry;
use crate::provider::{ProviderCtx, UsageProvider};
use crate::{fsutil, iso8601, usage_cache, util, windows};
use chrono::{DateTime, Duration, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};

// Fork replay is logged in the low milliseconds. Anything after this first gap is a real
// child turn (fallback heuristic only; the structural parent-compare is the primary path).
const FORK_REPLAY_MAXIMUM_GAP: i64 = 1;
// Cap on the metadata-only probe (1 MiB). `session_meta` is typically 22–46 KB, so ~22× headroom.
const PROBE_BYTE_LIMIT: usize = 1 << 20;
const PROBE_CHUNK: usize = 64 * 1024;
// Cheap byte-short-circuit markers (mirrors the Swift `Data` markers) — most lines are
// response_item/delta and none of these three, so we only JSON-parse the interesting kinds.
const SESSION_META_MARKER: &str = "session_meta";
const MODEL_MARKER: &str = "\"model\"";
const TOKEN_COUNT_MARKER: &str = "token_count";

pub struct CodexProvider;

// MARK: - Normalized types (ports of the Swift `Codex*` structs)

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexUsageVector {
    input: i64,
    cached_input: i64,
    cache_write_input: i64,
    output: i64,
    reasoning_output: i64,
    total: i64,
}

impl CodexUsageVector {
    fn from_obj(raw: &Value) -> Self {
        // Read via the clamping accessor so a corrupt `1e30` degrades instead of trapping.
        Self {
            input: util::get_int(raw, "input_tokens"),
            cached_input: util::get_int(raw, "cached_input_tokens"),
            cache_write_input: util::get_int(raw, "cache_write_input_tokens"),
            output: util::get_int(raw, "output_tokens"),
            reasoning_output: util::get_int(raw, "reasoning_output_tokens"),
            total: util::get_int(raw, "total_tokens"),
        }
    }

    fn fingerprint(&self) -> String {
        format!(
            "{},{},{},{},{},{}",
            self.input,
            self.cached_input,
            self.cache_write_input,
            self.output,
            self.reasoning_output,
            self.total
        )
    }

    /// Any component strictly below the previous snapshot (a reset ⇒ new canonical epoch).
    fn is_lower_than(&self, prev: &Self) -> bool {
        self.input < prev.input
            || self.cached_input < prev.cached_input
            || self.cache_write_input < prev.cache_write_input
            || self.output < prev.output
            || self.reasoning_output < prev.reasoning_output
            || self.total < prev.total
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexUsageState {
    cumulative: CodexUsageVector,
    last: CodexUsageVector,
}

impl CodexUsageState {
    fn fingerprint(&self) -> String {
        format!(
            "{}|{}",
            self.cumulative.fingerprint(),
            self.last.fingerprint()
        )
    }
}

struct ParsedCodexToken {
    entry: Entry,
    /// Old records lack `total_token_usage`; those are never same-state-compared.
    usage_state: Option<CodexUsageState>,
}

#[derive(Debug)]
struct CodexSessionMeta {
    id: Option<String>,
    parent_id: Option<String>,
    date: Option<DateTime<Utc>>,
    is_subagent: bool,
}

#[derive(Debug)]
struct CodexUsageEvent {
    entry: Entry,
    usage_state: Option<CodexUsageState>,
    session_id: Option<String>,
}

#[derive(Debug)]
struct CodexParsedRollout {
    path: PathBuf,
    session_id: Option<String>,
    parent_session_id: Option<String>,
    /// Carried for parity with the Swift `Codable` model; the solver never reads it.
    #[allow(dead_code)]
    forked_at: Option<DateTime<Utc>>,
    is_subagent: bool,
    events: Vec<CodexUsageEvent>,
}

#[derive(Debug, Clone)]
struct CodexResolvedEvent {
    /// Carried for parity with the Swift model; only `usage_state` feeds the replay comparison.
    #[allow(dead_code)]
    entry: Entry,
    usage_state: Option<CodexUsageState>,
}

#[derive(Clone)]
struct CodexResolvedRollout {
    history: Vec<CodexResolvedEvent>,
    owned_entries: Vec<Entry>,
}

// MARK: - Parsed-rollout cache state (persisted per file in the usage cache)

/// The parse state one rollout file needs across passes: everything the replay resolver
/// consumes. A file whose size is unchanged serves this instead of re-reading its content.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedRollout {
    session_id: Option<String>,
    parent_session_id: Option<String>,
    is_subagent: bool,
    events: Vec<CachedEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedEvent {
    id: String,
    date_ns: i64,
    local_day: String,
    model: String,
    input: i64,
    output: i64,
    cache_write: i64,
    cache_read: i64,
    explicit_cost: Option<f64>,
    session_id: Option<String>,
    state: Option<CachedState>,
}

/// Both usage vectors as flat `[i64; 6]` in `CodexUsageVector` field order.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct CachedState {
    cum: [i64; 6],
    last: [i64; 6],
}

fn vector_to_array(v: &CodexUsageVector) -> [i64; 6] {
    [
        v.input,
        v.cached_input,
        v.cache_write_input,
        v.output,
        v.reasoning_output,
        v.total,
    ]
}

fn vector_from_array(a: [i64; 6]) -> CodexUsageVector {
    CodexUsageVector {
        input: a[0],
        cached_input: a[1],
        cache_write_input: a[2],
        output: a[3],
        reasoning_output: a[4],
        total: a[5],
    }
}

fn encode_rollout(r: &CodexParsedRollout) -> String {
    let cached = CachedRollout {
        session_id: r.session_id.clone(),
        parent_session_id: r.parent_session_id.clone(),
        is_subagent: r.is_subagent,
        events: r
            .events
            .iter()
            .map(|e| CachedEvent {
                id: e.entry.id.clone(),
                date_ns: e
                    .entry
                    .date
                    .timestamp_nanos_opt()
                    .unwrap_or_else(|| e.entry.date.timestamp_millis() * 1_000_000),
                local_day: e.entry.local_day.clone(),
                model: e.entry.model.clone(),
                input: e.entry.input,
                output: e.entry.output,
                cache_write: e.entry.cache_write,
                cache_read: e.entry.cache_read,
                explicit_cost: e.entry.explicit_cost,
                session_id: e.session_id.clone(),
                state: e.usage_state.as_ref().map(|s| CachedState {
                    cum: vector_to_array(&s.cumulative),
                    last: vector_to_array(&s.last),
                }),
            })
            .collect(),
    };
    serde_json::to_string(&cached).unwrap_or_else(|_| "{}".into())
}

/// `path` is not part of the persisted payload (the source row is keyed by it) but the replay
/// resolver keys everything on it, so a decoded rollout must always carry its file's path.
fn decode_rollout(json: &str, path: &Path) -> Option<CodexParsedRollout> {
    let c: CachedRollout = serde_json::from_str(json).ok()?;
    Some(CodexParsedRollout {
        path: path.to_path_buf(),
        session_id: c.session_id,
        parent_session_id: c.parent_session_id,
        forked_at: None,
        is_subagent: c.is_subagent,
        events: c
            .events
            .into_iter()
            .map(|e| {
                let entry = Entry {
                    id: e.id,
                    date: DateTime::<Utc>::from_timestamp_nanos(e.date_ns),
                    local_day: e.local_day,
                    model: e.model,
                    input: e.input,
                    output: e.output,
                    cache_write: e.cache_write,
                    cache_read: e.cache_read,
                    explicit_cost: e.explicit_cost,
                };
                let usage_state = e.state.map(|s| CodexUsageState {
                    cumulative: vector_from_array(s.cum),
                    last: vector_from_array(s.last),
                });
                CodexUsageEvent {
                    entry,
                    usage_state,
                    session_id: e.session_id,
                }
            })
            .collect(),
    })
}

/// A rollout file on disk (path + mtime). mtime splits the enrichment window and the parent
/// closure; unchanged files are served from the usage cache.
#[derive(Debug, Clone)]
struct RolloutFile {
    path: PathBuf,
    mtime: DateTime<Utc>,
}

// MARK: - Provider

impl UsageProvider for CodexProvider {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn display_name(&self) -> &'static str {
        "Codex"
    }
    fn reports_cost(&self) -> bool {
        true
    }
    fn available(&self, ctx: &ProviderCtx) -> bool {
        codex_scan_roots(ctx).iter().any(|r| r.is_dir())
    }
    fn read_entries(&self, ctx: &ProviderCtx, since: DateTime<Utc>) -> anyhow::Result<Vec<Entry>> {
        let cache = usage_cache::UsageCache::resolve();
        let full = cache
            .as_ref()
            .map(|c| c.full_rescan_due("codex", &ctx.tz))
            .unwrap_or(true);
        let roots = codex_scan_roots(ctx);
        let mut entries = codex_entries_with_cache(&roots, since, ctx.tz, cache.as_ref(), full);
        // Enrichment floor: entries older than the (month/week/5h) window are outside every
        // display bucket. The mtime floor above already bounds the scan; this drops stragglers
        // from a recently-touched file whose early turns predate the window.
        entries.retain(|e| e.date >= since);
        if let Some(c) = &cache {
            if full {
                c.mark_full_scanned("codex", &ctx.tz);
            }
        }
        Ok(entries)
    }
}

// MARK: - Roots (Codex owns its root discovery; the two home-relative paths are the single source)

const SESSIONS_REL: &str = ".codex/sessions";
const ARCHIVED_REL: &str = ".codex/archived_sessions";

/// The two default Codex roots, normalized (symlink/nested dedup). `CODEX_HOME` is not read:
/// the source of truth (the macOS reader) only ever uses these two home-relative paths.
fn codex_scan_roots(ctx: &ProviderCtx) -> Vec<PathBuf> {
    let home = &ctx.home;
    fsutil::normalized_roots(vec![home.join(SESSIONS_REL), home.join(ARCHIVED_REL)])
}

// MARK: - File discovery

/// Every rollout `.jsonl` under the roots (no mtime filter — old files are still parent
/// candidates), sorted by path.
fn codex_rollout_files(roots: &[PathBuf]) -> Vec<RolloutFile> {
    // Floor far in the past so nothing is filtered by mtime; `since` is applied later.
    let floor = DateTime::from_timestamp(0, 0).expect("epoch");
    const MAX_DEPTH: usize = 24;
    let mut files: Vec<RolloutFile> = Vec::new();
    for root in fsutil::normalized_roots(roots.to_vec()) {
        for f in fsutil::walk_modified(&root, &["jsonl"], floor, false, MAX_DEPTH) {
            files.push(RolloutFile {
                path: f.path,
                mtime: f.mtime,
            });
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

// MARK: - Filename hint guard

/// Whether an id is usable to narrow parent candidates by filename substring match.
/// Degenerate values (empty, or only separators like `"-"`) match almost every rollout
/// filename, turning the cheap pre-filter into a full-parse of everything (0.009s → 18.2s on
/// a measured 300-file tree). Passing here does NOT skip content verification.
fn is_usable_filename_hint(id: &str) -> bool {
    id.chars().count() >= 4 && id.chars().any(|c| c.is_ascii_alphanumeric())
}

// MARK: - session_meta / model decode

fn first_nonempty(obj: &Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        if let Some(s) = util::get_str(obj, k) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn codex_session_meta(obj: &Value) -> Option<CodexSessionMeta> {
    if util::get_str(obj, "type") != Some("session_meta") {
        return None;
    }
    let payload = obj.get("payload")?;
    if !payload.is_object() {
        return None;
    }
    // A subagent `session_meta` may carry `id` = child and `session_id` = parent, so prefer `id`.
    let id = first_nonempty(payload, &["id", "session_id"]);
    let parent_id = first_nonempty(payload, &["forked_from_id", "parent_thread_id"]);
    let date = util::get_str(obj, "timestamp").and_then(iso8601::parse);
    let thread_source = util::get_str(payload, "thread_source");
    let has_subagent_source = payload
        .get("source")
        .and_then(|s| s.get("subagent"))
        .is_some();
    let is_subagent = thread_source == Some("subagent") || has_subagent_source;
    Some(CodexSessionMeta {
        id,
        parent_id,
        date,
        is_subagent,
    })
}

fn codex_model(obj: &Value) -> Option<String> {
    let payload = obj.get("payload")?;
    if let Some(m) = util::get_str(payload, "model") {
        return Some(m.to_string());
    }
    if let Some(tc) = payload.get("turn_context") {
        if let Some(m) = util::get_str(tc, "model") {
            return Some(m.to_string());
        }
    }
    None
}

// MARK: - Single line → token entry

fn parse_codex_line(
    line: &str,
    file: &str,
    turn: i64,
    model: &str,
    tz: FixedOffset,
) -> Option<ParsedCodexToken> {
    let obj: Value = serde_json::from_str(line).ok()?;
    let payload = obj.get("payload")?;
    if !payload.is_object() || util::get_str(payload, "type") != Some("token_count") {
        return None;
    }
    let info = payload.get("info")?;
    if !info.is_object() {
        return None;
    }
    let last = info.get("last_token_usage")?;
    if !last.is_object() {
        return None;
    }
    let date = util::get_str(&obj, "timestamp").and_then(iso8601::parse)?;

    let input_total = util::get_int(last, "input_tokens");
    let cached = util::get_int(last, "cached_input_tokens");
    let output = util::get_int(last, "output_tokens");
    let non_cached_input = (input_total - cached).max(0);

    let entry = Entry {
        id: format!("codex|{file}|{turn}"),
        date,
        local_day: windows::local_day(date, &tz),
        model: model.to_string(),
        input: non_cached_input,
        output,
        cache_write: 0,
        cache_read: cached,
        explicit_cost: None,
    };

    // Cumulative state only when present (old CLIs omit `total_token_usage`).
    let usage_state = info
        .get("total_token_usage")
        .filter(|v| v.is_object())
        .map(|cum| CodexUsageState {
            cumulative: CodexUsageVector::from_obj(cum),
            last: CodexUsageVector::from_obj(last),
        });

    Some(ParsedCodexToken { entry, usage_state })
}

// MARK: - Rollout parse (file-internal only; fork replay is resolved against other files)

fn parse_codex_rollout(path: &Path, tz: FixedOffset) -> CodexParsedRollout {
    let empty = CodexParsedRollout {
        path: path.to_path_buf(),
        session_id: None,
        parent_session_id: None,
        forked_at: None,
        is_subagent: false,
        events: Vec::new(),
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return empty,
    };
    let file = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("rollout")
        .to_string();

    let mut events: Vec<CodexUsageEvent> = Vec::new();
    let mut turn = 0i64;
    let mut session_id: Option<String> = None;
    let mut parent_session_id: Option<String> = None;
    let mut forked_at: Option<DateTime<Utc>> = None;
    let mut is_subagent = false;
    let mut current_session_id: Option<String> = None;
    let mut previous_usage_state: Option<(String, CodexUsageState)> = None;
    // Fallback only when a session has no model line at all; Codex cost is always 0 so the
    // display number is unaffected. The real model is extracted dynamically via `codex_model`.
    let mut model = "codex".to_string();

    for line in text.lines() {
        if line.contains(SESSION_META_MARKER) {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(meta) = codex_session_meta(&v) {
                    if session_id.is_none() {
                        session_id = meta.id.clone();
                        parent_session_id = meta.parent_id.clone();
                        forked_at = meta.date;
                        is_subagent = meta.is_subagent;
                    }
                    // A later (e.g. embedded parent) meta resets the same-state comparison.
                    if let Some(id) = &meta.id {
                        if current_session_id.as_deref() != Some(id.as_str()) {
                            current_session_id = Some(id.clone());
                            previous_usage_state = None;
                        }
                    }
                }
            }
        }
        if line.contains(MODEL_MARKER) {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(m) = codex_model(&v) {
                    model = m;
                }
            }
        }
        if !line.contains(TOKEN_COUNT_MARKER) {
            continue;
        }
        let Some(parsed) = parse_codex_line(line, &file, turn, &model, tz) else {
            continue;
        };
        turn += 1;

        // Codex may re-record the exact same cumulative/last state. Normalize within the file
        // BEFORE replay trimming: a consecutive token_count with an identical full state
        // (cumulative and last) has no new token contribution, so keep it once.
        if let (Some(state), Some(sid)) = (&parsed.usage_state, &current_session_id) {
            if let Some(prev) = &previous_usage_state {
                if prev.0 == *sid && prev.1 == *state {
                    continue;
                }
            }
            previous_usage_state = Some((sid.clone(), state.clone()));
        } else {
            previous_usage_state = None;
        }

        events.push(CodexUsageEvent {
            entry: parsed.entry,
            usage_state: parsed.usage_state,
            session_id: current_session_id.clone(),
        });
    }

    CodexParsedRollout {
        path: path.to_path_buf(),
        session_id,
        parent_session_id,
        forked_at,
        is_subagent,
        events,
    }
}

/// Parse + resolve a single rollout against only itself (the test/diagnostic path with no
/// cross-file parent knowledge). Mirrors the Swift `parseCodexFile`.
pub fn parse_codex_file(path: &Path, tz: FixedOffset) -> Vec<Entry> {
    let rollout = parse_codex_rollout(path, tz);
    let included = HashSet::from([rollout.path.clone()]);
    resolve_codex_rollouts(&[rollout], &included)
}

// MARK: - Metadata-only probe (find an old parent dependency without reading the whole rollout)

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeOutcome {
    /// `session_meta` found — id may be `None` if empty (preserves prior behavior).
    SessionID(Option<String>),
    /// Reached `token_count` first — no usable metadata precedes it.
    Stop,
    /// A non-empty line that is not valid UTF-8 — stop so we do not misread a later parent meta.
    Invalid,
    KeepScanning,
}

/// Outcome of one completed (newline-terminated or EOF-terminated) line.
fn probe_outcome_bytes(line: &[u8]) -> ProbeOutcome {
    if line.is_empty() {
        return ProbeOutcome::KeepScanning;
    }
    // Skipping a corrupt line could mis-attribute a later re-inserted parent meta as this file's
    // id, so stop. The 1 MiB cap bounds this UTF-8 validation cost.
    let s = match std::str::from_utf8(line) {
        Ok(s) => s,
        Err(_) => return ProbeOutcome::Invalid,
    };
    if s.contains(SESSION_META_MARKER) {
        if let Ok(v) = serde_json::from_str::<Value>(s) {
            if let Some(meta) = codex_session_meta(&v) {
                return ProbeOutcome::SessionID(meta.id);
            }
        }
    }
    if s.contains(TOKEN_COUNT_MARKER) {
        return ProbeOutcome::Stop;
    }
    ProbeOutcome::KeepScanning
}

/// Only accept a trailing (possibly unterminated) line if it is a `session_meta` line; else `None`.
fn finish_trailing(buffer: &[u8]) -> Option<String> {
    match probe_outcome_bytes(buffer) {
        ProbeOutcome::SessionID(id) => id,
        _ => None,
    }
}

/// Read at most `byte_limit` bytes, decoding only newline-complete lines (a fixed-prefix strict
/// decode fails when the cut lands mid-multibyte char). Returns `Ok(None)` for "read fine but no
/// usable metadata" and `Err` for I/O failure — those must not be conflated by callers caching.
fn probe_codex_rollout_session_id(path: &Path, byte_limit: usize) -> io::Result<Option<String>> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut buffer: Vec<u8> = Vec::new();
    let mut read_total: usize = 0;
    loop {
        if read_total >= byte_limit {
            break;
        }
        let want = std::cmp::min(PROBE_CHUNK, byte_limit - read_total);
        let mut chunk = vec![0u8; want];
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            // EOF — a trailing line without a newline also counts as a complete line.
            return Ok(finish_trailing(&buffer));
        }
        chunk.truncate(n);
        read_total += n;
        buffer.extend_from_slice(&chunk);

        let mut start = 0usize;
        while start < buffer.len() {
            match buffer[start..].iter().position(|&b| b == b'\n') {
                Some(rel) => {
                    let line_end = start + rel;
                    match probe_outcome_bytes(&buffer[start..line_end]) {
                        ProbeOutcome::SessionID(id) => return Ok(id),
                        ProbeOutcome::Stop | ProbeOutcome::Invalid => return Ok(None),
                        ProbeOutcome::KeepScanning => {}
                    }
                    start = line_end + 1;
                }
                None => break,
            }
        }
        if start > 0 {
            buffer.drain(..start);
        }
    }
    // Hit the limit: a final unterminated-but-complete `session_meta` line is still accepted.
    Ok(finish_trailing(&buffer))
}

/// Convenience wrapper that folds read failure into `None`. Callers that persist the result must
/// use `probe_codex_rollout_session_id` directly so transient I/O error does not hard-settle.
fn codex_rollout_session_id(path: &Path) -> Option<String> {
    probe_codex_rollout_session_id(path, PROBE_BYTE_LIMIT)
        .ok()
        .flatten()
}

// MARK: - Parent closure (pull replay-comparison parents — and their parents — into scope)

fn adopt_candidates(
    candidates: &[&RolloutFile],
    parent_id: &str,
    rollouts_by_path: &mut HashMap<PathBuf, CodexParsedRollout>,
    pending_parent_ids: &mut HashSet<String>,
    load: &dyn Fn(&RolloutFile) -> CodexParsedRollout,
) -> bool {
    // A hint only picks candidates; adoption is judged solely by the loaded payload's session id.
    let mut resolved = false;
    for c in candidates {
        let parent = (*load)(c);
        if parent.session_id.as_deref() != Some(parent_id) {
            continue;
        }
        if let Some(ancestor) = parent.parent_session_id.clone() {
            pending_parent_ids.insert(ancestor);
        }
        rollouts_by_path.insert(parent.path.clone(), parent);
        resolved = true;
    }
    resolved
}

fn expand_codex_parent_closure(
    window_files: &[RolloutFile],
    all_files: &[RolloutFile],
    load: impl Fn(&RolloutFile) -> CodexParsedRollout,
    probe: impl Fn(&RolloutFile) -> Option<String>,
) -> (Vec<CodexParsedRollout>, HashSet<PathBuf>) {
    let mut rollouts_by_path: HashMap<PathBuf, CodexParsedRollout> = HashMap::new();
    for f in window_files {
        let rollout = load(f);
        rollouts_by_path.insert(rollout.path.clone(), rollout);
    }
    let included_paths: HashSet<PathBuf> = window_files.iter().map(|f| f.path.clone()).collect();

    let mut pending_parent_ids: HashSet<String> = rollouts_by_path
        .values()
        .filter_map(|r| r.parent_session_id.clone())
        .collect();
    let mut searched_parent_ids: HashSet<String> = HashSet::new();

    while let Some(p) = pending_parent_ids
        .iter()
        .filter(|p| !searched_parent_ids.contains(*p))
        .min()
    {
        // Deterministic choice (the Swift `Set.first` is arbitrary).
        let parent_id = p.clone();
        searched_parent_ids.insert(parent_id.clone());
        if rollouts_by_path
            .values()
            .any(|r| r.session_id.as_deref() == Some(parent_id.as_str()))
        {
            continue;
        }

        let unresolved: Vec<&RolloutFile> = all_files
            .iter()
            .filter(|f| !rollouts_by_path.contains_key(&f.path))
            .collect();

        // No cache here ⇒ session-id knowledge is always `Unknown`: narrow by filename hint first.
        let hinted: Vec<&RolloutFile> = unresolved
            .iter()
            .copied()
            .filter(|f| {
                is_usable_filename_hint(&parent_id)
                    && f.path
                        .file_name()
                        .map(|b| b.to_string_lossy().contains(parent_id.as_str()))
                        .unwrap_or(false)
            })
            .collect();
        if adopt_candidates(
            &hinted,
            &parent_id,
            &mut rollouts_by_path,
            &mut pending_parent_ids,
            &load,
        ) {
            continue;
        }

        // No hint, or hints failed verification ⇒ only open files whose content reveals the id.
        let hinted_paths: HashSet<PathBuf> = hinted.iter().map(|f| f.path.clone()).collect();
        let probe_candidates: Vec<&RolloutFile> = unresolved
            .iter()
            .copied()
            .filter(|f| {
                !hinted_paths.contains(&f.path) && probe(f).as_deref() == Some(parent_id.as_str())
            })
            .collect();
        adopt_candidates(
            &probe_candidates,
            &parent_id,
            &mut rollouts_by_path,
            &mut pending_parent_ids,
            &load,
        );
    }

    (rollouts_by_path.into_values().collect(), included_paths)
}

// MARK: - Replay comparison + owned-event mapping

/// Length of the common full-usage-state prefix between a child and a (resolved) parent.
/// `None` means the cumulative state is absent, so structural comparison is impossible (not 0).
fn comparable_usage_prefix_count(
    child: &[CodexUsageEvent],
    parent: &[CodexResolvedEvent],
) -> Option<usize> {
    if child.is_empty() {
        return Some(0);
    }
    if parent.is_empty() {
        return None;
    }
    let mut count = 0;
    while count < child.len() && count < parent.len() {
        let child_state = child[count].usage_state.as_ref()?;
        let parent_state = parent[count].usage_state.as_ref()?;
        if child_state != parent_state {
            break;
        }
        count += 1;
    }
    Some(count)
}

/// Confirmed 0.142.5/0.145.0 subagents insert only parent metadata and never replay
/// `token_count`; do not drop their first real turn on account of a missing parent file.
fn fallback_replay_count(rollout: &CodexParsedRollout) -> usize {
    if rollout.is_subagent {
        return 0;
    }
    let events = &rollout.events;
    if events.len() <= 1 {
        return usize::from(events.len() == 1);
    }
    let mut count = 1;
    while count < events.len() {
        let gap = events[count].entry.date - events[count - 1].entry.date;
        if gap >= Duration::seconds(FORK_REPLAY_MAXIMUM_GAP) {
            break;
        }
        count += 1;
    }
    count
}

fn replace_id(entry: &Entry, id: &str) -> Entry {
    Entry {
        id: id.to_string(),
        date: entry.date,
        local_day: entry.local_day.clone(),
        model: entry.model.clone(),
        input: entry.input,
        output: entry.output,
        cache_write: entry.cache_write,
        cache_read: entry.cache_read,
        explicit_cost: entry.explicit_cost,
    }
}

fn resolve_owned_events(
    rollout: &CodexParsedRollout,
    replay_count: usize,
    inherited_history: Vec<CodexResolvedEvent>,
) -> CodexResolvedRollout {
    let mut history = inherited_history;
    let mut owned_entries: Vec<Entry> = Vec::new();
    let mut epoch = 0i64;
    let mut previous_cumulative: Option<CodexUsageVector> = None;
    let mut previous_owner: Option<String> = None;

    for event in rollout.events.iter().skip(replay_count) {
        // A fork file's unmatched suffix sits after an embedded parent meta but is still owned by
        // the child. A non-fork file's real session switch follows the event's own session id.
        let owner = if rollout.parent_session_id.is_none() {
            event
                .session_id
                .clone()
                .or_else(|| rollout.session_id.clone())
        } else {
            rollout.session_id.clone()
        };
        if owner != previous_owner {
            epoch = 0;
            previous_cumulative = None;
            previous_owner = owner.clone();
        }
        match event.usage_state.as_ref().map(|s| &s.cumulative) {
            Some(cumulative) => {
                if let Some(prev) = &previous_cumulative {
                    if cumulative.is_lower_than(prev) {
                        epoch += 1;
                    }
                }
                previous_cumulative = Some(cumulative.clone());
            }
            None => previous_cumulative = None,
        }

        let entry = match (&owner, &event.usage_state) {
            (Some(owner), Some(state)) => replace_id(
                &event.entry,
                &format!("codex|{owner}|{epoch}|{}", state.fingerprint()),
            ),
            // Legacy records without a cumulative state or session id keep their positional id.
            _ => event.entry.clone(),
        };
        owned_entries.push(entry.clone());
        history.push(CodexResolvedEvent {
            entry,
            usage_state: event.usage_state.clone(),
        });
    }

    CodexResolvedRollout {
        history,
        owned_entries,
    }
}

fn resolve_codex_rollouts<'a>(
    rollouts: &'a [CodexParsedRollout],
    included_paths: &HashSet<PathBuf>,
) -> Vec<Entry> {
    // Group by session id, each group sorted by path (deterministic candidate order).
    let mut by_session: HashMap<String, Vec<&'a CodexParsedRollout>> = HashMap::new();
    for r in rollouts.iter() {
        if let Some(sid) = &r.session_id {
            by_session.entry(sid.clone()).or_default().push(r);
        }
    }
    for group in by_session.values_mut() {
        group.sort_by(|a, b| a.path.cmp(&b.path));
    }
    let by_path: HashMap<&PathBuf, &'a CodexParsedRollout> =
        rollouts.iter().map(|r| (&r.path, r)).collect();

    let mut memo: HashMap<PathBuf, CodexResolvedRollout> = HashMap::new();

    fn resolve_one<'a>(
        rollout: &'a CodexParsedRollout,
        visiting: &mut HashSet<PathBuf>,
        by_session: &'a HashMap<String, Vec<&'a CodexParsedRollout>>,
        memo: &mut HashMap<PathBuf, CodexResolvedRollout>,
    ) -> CodexResolvedRollout {
        if let Some(cached) = memo.get(&rollout.path) {
            return cached.clone();
        }
        if !visiting.insert(rollout.path.clone()) {
            return resolve_owned_events(rollout, fallback_replay_count(rollout), Vec::new());
        }

        let mut best_parent_match: Option<(usize, Vec<CodexResolvedEvent>)> = None;
        if let Some(parent_id) = &rollout.parent_session_id {
            if let Some(candidates) = by_session.get(parent_id) {
                for candidate in candidates {
                    if candidate.path == rollout.path {
                        continue;
                    }
                    let resolved_parent = resolve_one(candidate, visiting, by_session, memo);
                    // A zero-length overlap means we found the parent but have no basis to
                    // compare — counting it would trim nothing yet skip the timing fallback
                    // (worse than not finding the parent). Keep only parents with a real prefix.
                    let Some(replay_count) =
                        comparable_usage_prefix_count(&rollout.events, &resolved_parent.history)
                    else {
                        continue;
                    };
                    if replay_count == 0 {
                        continue;
                    }
                    match best_parent_match.as_ref() {
                        Some(cur) if replay_count > cur.0 => {
                            best_parent_match = Some((replay_count, resolved_parent.history));
                        }
                        Some(_) => {}
                        None => best_parent_match = Some((replay_count, resolved_parent.history)),
                    }
                }
            }
        }

        let resolved = match best_parent_match {
            Some((replay_count, history)) => {
                let inherited: Vec<CodexResolvedEvent> =
                    history.iter().take(replay_count).cloned().collect();
                resolve_owned_events(rollout, replay_count, inherited)
            }
            // Parent not found, or legacy cumulative state forbids structural compare.
            // A real subagent is preserved by fallback_replay_count; only a manual fork
            // falls back to the timing heuristic.
            None if rollout.parent_session_id.is_some() => {
                resolve_owned_events(rollout, fallback_replay_count(rollout), Vec::new())
            }
            None => resolve_owned_events(rollout, 0, Vec::new()),
        };

        visiting.remove(&rollout.path);
        let out = resolved.clone();
        memo.insert(rollout.path.clone(), resolved);
        out
    }

    let mut result: Vec<Entry> = Vec::new();
    let mut included: Vec<&PathBuf> = included_paths.iter().collect();
    included.sort();
    for path in included {
        if let Some(rollout) = by_path.get(path) {
            let mut visiting: HashSet<PathBuf> = HashSet::new();
            let resolved = resolve_one(rollout, &mut visiting, &by_session, &mut memo);
            result.extend(resolved.owned_entries);
        }
    }

    dedup_codex_canonical_entries(result)
}

// MARK: - Canonical dedup

/// The same canonical state may survive in several files; keep the record closest in time to the
/// original (the earliest date), not the largest total — a token vector is part of the id, so
/// keep-earliest is what fits Codex's date semantics.
fn dedup_codex_canonical_entries(entries: Vec<Entry>) -> Vec<Entry> {
    let mut by_id: HashMap<String, Entry> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for entry in entries {
        match by_id.get(&entry.id) {
            Some(existing) if entry.date < existing.date => {
                by_id.insert(entry.id.clone(), entry);
            }
            Some(_) => {}
            None => {
                order.push(entry.id.clone());
                by_id.insert(entry.id.clone(), entry);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|id| by_id.remove(&id))
        .collect()
}

// MARK: - Pipeline (the port of `codexEntries`)

#[cfg(test)]
fn codex_entries(roots: &[PathBuf], since: DateTime<Utc>, tz: FixedOffset) -> Vec<Entry> {
    codex_entries_with_cache(roots, since, tz, None, true)
}

/// The pipeline with the usage cache: every rollout file is a size-marked source. A file that
/// is unchanged since the last pass is served from its cached parse (no disk I/O on the
/// content); a changed file is re-parsed whole — a rollout's same-state drop and replay trim
/// span the whole file, so the per-file parse is the atomic unit. This is what turns the
/// parent-closure walk from "re-parse hundreds of rollouts" into "load cached rollouts".
fn codex_entries_with_cache(
    roots: &[PathBuf],
    since: DateTime<Utc>,
    tz: FixedOffset,
    cache: Option<&usage_cache::UsageCache>,
    full: bool,
) -> Vec<Entry> {
    let all_files = codex_rollout_files(roots);
    let window_files: Vec<RolloutFile> = all_files
        .iter()
        .filter(|f| f.mtime >= since)
        .cloned()
        .collect();

    let (rollouts, included_paths) = expand_codex_parent_closure(
        &window_files,
        &all_files,
        |f| load_rollout(f, tz, cache, full),
        |f| codex_rollout_session_id(&f.path),
    );
    if let Some(cache) = cache {
        // Parents outside the mtime window must stay cached: they are the replay-comparison
        // basis for future forks.
        let keep: Vec<String> = all_files
            .iter()
            .map(|f| usage_cache::source_key(&f.path))
            .collect();
        cache.prune_sources("codex", &keep);
    }
    resolve_codex_rollouts(&rollouts, &included_paths)
}

/// A rollout file's parse, served from the cache while its size is unchanged.
fn load_rollout(
    f: &RolloutFile,
    tz: FixedOffset,
    cache: Option<&usage_cache::UsageCache>,
    full: bool,
) -> CodexParsedRollout {
    if !full {
        if let Some(cache) = cache {
            let key = usage_cache::source_key(&f.path);
            if let Ok(Some(state)) = cache.source("codex", &key) {
                if let Some(size) = std::fs::metadata(&f.path).ok().map(|m| m.len()) {
                    if state.marker == size as i64 {
                        if let Some(rollout) = state
                            .payload
                            .as_deref()
                            .and_then(|p| decode_rollout(p, &f.path))
                        {
                            return rollout;
                        }
                    }
                }
            }
        }
    }
    let rollout = parse_codex_rollout(&f.path, tz);
    if let Some(cache) = cache {
        let key = usage_cache::source_key(&f.path);
        let marker = std::fs::metadata(&f.path)
            .map(|m| m.len())
            .unwrap_or(0) as i64;
        cache.upsert_source("codex", &key, marker, Some(encode_rollout(&rollout)));
    }
    rollout
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate;
    use chrono::FixedOffset;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TZ: i32 = 0;
    fn tz() -> FixedOffset {
        FixedOffset::east_opt(TZ).unwrap()
    }
    fn distant_past() -> DateTime<Utc> {
        DateTime::from_timestamp(0, 0).expect("epoch")
    }

    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("ptb-codex-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).expect("mkdir temp dir");
        d
    }

    /// Write `lines` joined by newline (no trailing newline, like the real rollout files) to
    /// `dir[/sub]/name`.
    fn write(dir: &Path, name: &str, sub: Option<&str>, lines: &[String]) -> PathBuf {
        let folder = sub
            .map(|s| dir.join(s))
            .unwrap_or_else(|| dir.to_path_buf());
        std::fs::create_dir_all(&folder).expect("mkdir");
        let path = folder.join(name);
        std::fs::write(&path, lines.join("\n")).expect("write");
        path
    }

    // --- line builders (mirror the Swift test helpers byte-for-byte) ---

    fn codex_line(
        ts: &str,
        input: i64,
        cached: i64,
        output: i64,
        reasoning: i64,
        cw: i64,
    ) -> String {
        serde_json::json!({
            "type": "event_msg",
            "timestamp": ts,
            "payload": {
                "type": "token_count",
                "info": {
                    "last_token_usage": {
                        "input_tokens": input,
                        "cached_input_tokens": cached,
                        "cache_write_input_tokens": cw,
                        "output_tokens": output,
                        "reasoning_output_tokens": reasoning,
                        "total_tokens": input + output,
                    }
                }
            }
        })
        .to_string()
    }
    // Default 1000/200/50/10/0 like the Swift helper.
    fn codex_line_d(ts: &str, output: i64) -> String {
        codex_line(ts, 1_000, 200, output, 10, 0)
    }

    fn codex_session_meta_line(id: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "session_meta",
            "timestamp": ts,
            "payload": { "id": id, "session_id": id }
        })
        .to_string()
    }

    // Mirrors the 10-field Swift `codexStateLine` helper verbatim.
    #[allow(clippy::too_many_arguments)]
    fn codex_state_line(
        ts: &str,
        cum_input: i64,
        cum_cached: i64,
        cum_output: i64,
        cum_reasoning: i64,
        last_input: i64,
        last_cached: i64,
        last_output: i64,
        last_reasoning: i64,
        last_total: Option<i64>,
        cache_write: Option<i64>,
    ) -> String {
        let cum_total = cum_input + cum_output;
        let reported_last_total = last_total.unwrap_or(last_input + last_output);
        // Field order is irrelevant to the parser (keyed lookup); a `cache_write_input_tokens`
        // is present in both blocks or absent from both, matching the Swift helper.
        let mut cum = serde_json::json!({
            "input_tokens": cum_input,
            "cached_input_tokens": cum_cached,
            "output_tokens": cum_output,
            "reasoning_output_tokens": cum_reasoning,
            "total_tokens": cum_total,
        });
        let mut last = serde_json::json!({
            "input_tokens": last_input,
            "cached_input_tokens": last_cached,
            "output_tokens": last_output,
            "reasoning_output_tokens": last_reasoning,
            "total_tokens": reported_last_total,
        });
        if let Some(cw) = cache_write {
            cum["cache_write_input_tokens"] = serde_json::json!(cw);
            last["cache_write_input_tokens"] = serde_json::json!(cw);
        }
        serde_json::json!({
            "type": "event_msg",
            "timestamp": ts,
            "payload": {
                "type": "token_count",
                "info": { "total_token_usage": cum, "last_token_usage": last }
            }
        })
        .to_string()
    }
    // Convenience: cumulative == last, cached/reasoning default 0, no cache_write.
    fn state(
        ts: &str,
        cum_input: i64,
        cum_output: i64,
        last_input: i64,
        last_output: i64,
    ) -> String {
        codex_state_line(
            ts,
            cum_input,
            0,
            cum_output,
            0,
            last_input,
            0,
            last_output,
            0,
            None,
            None,
        )
    }

    fn forked_session_meta(ts: &str) -> String {
        serde_json::json!({
            "type": "session_meta",
            "timestamp": ts,
            "payload": {
                "id": "child",
                "forked_from_id": "parent",
                "parent_thread_id": "parent",
                "thread_source": "user",
            }
        })
        .to_string()
    }

    fn forked_meta(id: &str, parent_id: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "session_meta",
            "timestamp": ts,
            "payload": {
                "id": id,
                "session_id": parent_id,
                "forked_from_id": parent_id,
                "parent_thread_id": parent_id,
                "thread_source": "subagent",
            }
        })
        .to_string()
    }

    // --- fixture helpers ---

    fn fixture(subdir: &str, name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join(subdir)
            .join(format!("{name}.jsonl"))
    }

    fn copy_fixture(dir: &Path, subdir: &str, name: &str) -> PathBuf {
        let src = fixture(subdir, name);
        let dst = dir.join(format!("{name}.jsonl"));
        std::fs::copy(&src, &dst).expect("copy fixture");
        dst
    }

    fn totals(entries: &[Entry]) -> Vec<i64> {
        entries.iter().map(|e| e.total()).collect()
    }
    fn sum(entries: &[Entry]) -> i64 {
        entries.iter().map(|e| e.total()).sum()
    }

    // ==========================================================================
    // Parity tests
    // ==========================================================================

    #[test]
    fn codex_parsing() {
        let dir = temp_dir();
        write(
            &dir,
            "rollout-x.jsonl",
            Some("2026/06/30"),
            &[codex_line_d("2026-06-30T11:00:00.000Z", 50)],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.input, 800); // 1000 - 200
        assert_eq!(e.cache_read, 200);
        assert_eq!(e.output, 50);
        assert_eq!(e.cache_write, 0);
    }

    #[test]
    fn non_fork_resolver_preserves_parsed_entries_except_canonical_ids() {
        let dir = temp_dir();
        let file = write(
            &dir,
            "rollout.jsonl",
            None,
            &[
                codex_session_meta_line("session-a", "2026-07-29T01:00:00.000Z"),
                codex_state_line(
                    "2026-07-29T01:00:01.000Z",
                    100,
                    20,
                    10,
                    0,
                    100,
                    20,
                    10,
                    0,
                    None,
                    None,
                ),
                codex_state_line(
                    "2026-07-29T01:00:02.000Z",
                    300,
                    120,
                    30,
                    0,
                    200,
                    100,
                    20,
                    0,
                    None,
                    None,
                ),
                codex_state_line(
                    "2026-07-29T01:00:03.000Z",
                    450,
                    170,
                    45,
                    0,
                    150,
                    50,
                    15,
                    0,
                    None,
                    None,
                ),
            ],
        );
        let rollout = parse_codex_rollout(&file, tz());
        let parsed: Vec<Entry> = rollout.events.iter().map(|e| e.entry.clone()).collect();
        assert_eq!(parsed.len(), 3);

        let resolved = resolve_codex_rollouts(&[rollout], &HashSet::from([file.clone()]));
        assert_eq!(resolved.len(), parsed.len());
        for (before, after) in parsed.iter().zip(resolved.iter()) {
            assert_ne!(after.id, before.id);
            assert!(
                after.id.starts_with("codex|session-a|0|"),
                "id was {}",
                after.id
            );
            assert_eq!(after.date, before.date);
            assert_eq!(after.local_day, before.local_day);
            assert_eq!(after.model, before.model);
            assert_eq!(after.input, before.input);
            assert_eq!(after.output, before.output);
            assert_eq!(after.cache_write, before.cache_write);
            assert_eq!(after.cache_read, before.cache_read);
            assert_eq!(after.explicit_cost, before.explicit_cost);
        }
    }

    #[test]
    fn drops_consecutive_same_state_rerecords_and_matches_cumulative_total() {
        let dir = temp_dir();
        write(
            &dir,
            "s.jsonl",
            None,
            &[
                codex_session_meta_line("session-a", "2026-07-29T01:00:00.000Z"),
                state("2026-07-29T01:00:01.000Z", 100, 10, 100, 10),
                // simple rerecord of the same snapshot.
                state("2026-07-29T01:00:02.000Z", 100, 10, 100, 10),
                state("2026-07-29T01:00:03.000Z", 300, 30, 200, 20),
                codex_session_meta_line("session-a", "2026-07-29T01:00:04.000Z"),
                state("2026-07-29T01:00:05.000Z", 300, 30, 200, 20),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(totals(&entries), vec![110, 220]);
        assert_eq!(sum(&entries), 330);
    }

    #[test]
    fn same_scalar_totals_with_different_full_vectors_are_preserved() {
        let dir = temp_dir();
        write(
            &dir,
            "s.jsonl",
            None,
            &[
                codex_session_meta_line("session-a", "2026-07-29T01:00:00.000Z"),
                state("2026-07-29T01:00:01.000Z", 100, 10, 100, 10),
                state("2026-07-29T01:00:02.000Z", 90, 20, 90, 20),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(entries.len(), 2);
        assert_eq!(totals(&entries), vec![110, 110]);
    }

    #[test]
    fn unchanged_cumulative_with_different_last_vector_is_preserved() {
        let dir = temp_dir();
        write(
            &dir,
            "s.jsonl",
            None,
            &[
                codex_session_meta_line("session-a", "2026-07-29T01:00:00.000Z"),
                state("2026-07-29T01:00:01.000Z", 100, 10, 100, 10),
                codex_state_line(
                    "2026-07-29T01:00:02.000Z",
                    100,
                    0,
                    10,
                    0,
                    0,
                    0,
                    0,
                    0,
                    Some(6_742),
                    None,
                ),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(totals(&entries), vec![110, 0]);
    }

    #[test]
    fn session_change_resets_same_state_comparison() {
        let dir = temp_dir();
        let state_a = state("2026-07-29T01:00:01.000Z", 100, 10, 100, 10);
        let state_b = state("2026-07-29T01:00:03.000Z", 100, 10, 100, 10);
        write(
            &dir,
            "s.jsonl",
            None,
            &[
                codex_session_meta_line("session-a", "2026-07-29T01:00:00.000Z"),
                state_a,
                codex_session_meta_line("session-b", "2026-07-29T01:00:02.000Z"),
                state_b,
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(totals(&entries), vec![110, 110]);
    }

    #[test]
    fn missing_cumulative_usage_preserves_repeated_records() {
        let dir = temp_dir();
        write(
            &dir,
            "s.jsonl",
            None,
            &[
                codex_session_meta_line("session-a", "2026-07-29T01:00:00.000Z"),
                codex_line_d("2026-07-29T01:00:01.000Z", 50),
                codex_line_d("2026-07-29T01:00:02.000Z", 50),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn manual_fork_falls_back_when_parent_usage_state_is_unavailable() {
        let dir = temp_dir();
        write(
            &dir,
            "parent.jsonl",
            None,
            &[
                codex_session_meta_line("parent", "2026-07-29T01:00:00.000Z"),
                codex_line_d("2026-07-29T01:00:00.010Z", 50),
                codex_line_d("2026-07-29T01:00:00.020Z", 51),
            ],
        );
        write(
            &dir,
            "child.jsonl",
            None,
            &[
                forked_session_meta("2026-07-30T01:00:00.000Z"),
                codex_session_meta_line("parent", "2026-07-30T01:00:00.001Z"),
                codex_line_d("2026-07-30T01:00:03.000Z", 50),
                codex_line_d("2026-07-30T01:00:03.010Z", 51),
                codex_line_d("2026-07-30T01:00:06.000Z", 99),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        let mut outputs: Vec<i64> = entries.iter().map(|e| e.output).collect();
        outputs.sort();
        assert_eq!(outputs, vec![50, 51, 99]);
    }

    #[test]
    fn manual_fork_falls_back_when_found_parent_prefix_does_not_match() {
        let dir = temp_dir();
        write(
            &dir,
            "parent.jsonl",
            None,
            &[
                codex_session_meta_line("parent", "2026-07-29T01:00:00.000Z"),
                state("2026-07-29T01:00:01.000Z", 100, 10, 100, 10),
                state("2026-07-29T01:00:02.000Z", 300, 30, 200, 20),
            ],
        );
        write(
            &dir,
            "child.jsonl",
            None,
            &[
                forked_session_meta("2026-07-30T01:00:00.000Z"),
                codex_session_meta_line("parent", "2026-07-30T01:00:00.001Z"),
                codex_state_line(
                    "2026-07-30T01:00:00.010Z",
                    100,
                    0,
                    10,
                    0,
                    100,
                    0,
                    10,
                    0,
                    None,
                    Some(7),
                ),
                codex_state_line(
                    "2026-07-30T01:00:00.020Z",
                    300,
                    0,
                    30,
                    0,
                    200,
                    0,
                    20,
                    0,
                    None,
                    Some(7),
                ),
                codex_state_line(
                    "2026-07-30T01:00:03.000Z",
                    1_300,
                    0,
                    128,
                    0,
                    1_000,
                    0,
                    98,
                    0,
                    None,
                    Some(7),
                ),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        let mut tot: Vec<i64> = totals(&entries);
        tot.sort();
        assert_eq!(tot, vec![110, 220, 1_098]);
    }

    #[test]
    fn cumulative_usage_clamps_out_of_range_number() {
        let dir = temp_dir();
        let absurd = r#"{"type":"event_msg","timestamp":"2026-07-30T01:00:01.000Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1e30,"cached_input_tokens":0,"output_tokens":10,"total_tokens":1e30},"last_token_usage":{"input_tokens":100,"cached_input_tokens":0,"output_tokens":10,"total_tokens":110}}}}"#;
        write(
            &dir,
            "rollout-huge.jsonl",
            None,
            &[
                codex_session_meta_line("huge", "2026-07-30T01:00:00.000Z"),
                absurd.to_string(),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(totals(&entries), vec![110]);
    }

    #[test]
    fn last_usage_clamps_out_of_range_number() {
        let dir = temp_dir();
        let line = r#"{"type":"event_msg","timestamp":"2026-07-29T01:00:00.000Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15},"last_token_usage":{"input_tokens":1e30,"cached_input_tokens":0,"output_tokens":1e30,"reasoning_output_tokens":0,"total_tokens":1e30}}}}"#;
        write(&dir, "rollout.jsonl", Some("huge"), &[line.to_string()]);
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].input, util::MAX_PARSED_TOKEN);
    }

    #[test]
    fn fork_trims_replay_before_dropping_actual_same_state_rerecord() {
        let dir = temp_dir();
        write(
            &dir,
            "rollout-child.jsonl",
            Some("child"),
            &[
                forked_session_meta("2026-07-29T01:00:00.000Z"),
                state("2026-07-29T01:00:00.010Z", 100, 10, 100, 10),
                state("2026-07-29T01:00:03.000Z", 300, 30, 200, 20),
                state("2026-07-29T01:00:04.000Z", 300, 30, 200, 20),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(totals(&entries), vec![220]);
    }

    #[test]
    fn forked_rollout_drops_leading_replay_burst() {
        let dir = temp_dir();
        write(
            &dir,
            "rollout-child.jsonl",
            Some("child"),
            &[
                forked_session_meta("2026-07-29T01:00:00.000Z"),
                codex_line_d("2026-07-29T01:00:00.010Z", 50),
                codex_line_d("2026-07-29T01:00:00.020Z", 51),
                codex_line_d("2026-07-29T01:00:03.000Z", 52),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].output, 52);
    }

    #[test]
    fn fork_drops_replay_burst_that_starts_after_metadata_delay() {
        let dir = temp_dir();
        write(
            &dir,
            "rollout-child.jsonl",
            Some("child"),
            &[
                forked_session_meta("2026-07-29T01:00:00.000Z"),
                codex_line_d("2026-07-29T01:00:03.000Z", 1),
                codex_line_d("2026-07-29T01:00:03.010Z", 2),
                codex_line_d("2026-07-29T01:00:03.020Z", 3),
                codex_line_d("2026-07-29T01:00:43.000Z", 99),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        let outputs: Vec<i64> = entries.iter().map(|e| e.output).collect();
        assert_eq!(outputs, vec![99]);
    }

    #[test]
    fn fork_keeps_real_turns_after_replay_burst_when_they_are_less_than_two_seconds_apart() {
        let dir = temp_dir();
        write(
            &dir,
            "rollout-child.jsonl",
            Some("child"),
            &[
                forked_session_meta("2026-07-29T01:00:00.000Z"),
                codex_line_d("2026-07-29T01:00:00.010Z", 1),
                codex_line_d("2026-07-29T01:00:00.020Z", 2),
                codex_line_d("2026-07-29T01:00:00.030Z", 3),
                codex_line_d("2026-07-29T01:00:01.530Z", 11),
                codex_line_d("2026-07-29T01:00:03.030Z", 22),
                codex_line_d("2026-07-29T01:00:04.530Z", 33),
                codex_line_d("2026-07-29T01:01:00.000Z", 44),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        let outputs: Vec<i64> = entries.iter().map(|e| e.output).collect();
        assert_eq!(outputs, vec![11, 22, 33, 44]);
    }

    #[test]
    fn fork_detects_metadata_after_leading_non_token_record() {
        let dir = temp_dir();
        write(
            &dir,
            "rollout-child.jsonl",
            Some("child"),
            &[
                r#"{"type":"turn_context","timestamp":"2026-07-29T01:00:00.000Z","payload":{}}"#
                    .to_string(),
                forked_session_meta("2026-07-29T01:00:00.001Z"),
                codex_line_d("2026-07-29T01:00:00.010Z", 1),
                codex_line_d("2026-07-29T01:00:03.000Z", 99),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        let outputs: Vec<i64> = entries.iter().map(|e| e.output).collect();
        assert_eq!(outputs, vec![99]);
    }

    // --- canonical id / epoch / dedup ---

    #[test]
    fn cumulative_reset_starts_new_canonical_epoch() {
        let dir = temp_dir();
        write(
            &dir,
            "s.jsonl",
            None,
            &[
                codex_session_meta_line("session-a", "2026-07-30T01:00:00.000Z"),
                state("2026-07-30T01:00:01.000Z", 100, 10, 100, 10),
                state("2026-07-30T01:00:02.000Z", 10, 1, 10, 1),
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(entries.len(), 2);
        assert!(
            entries[0].id.starts_with("codex|session-a|0|"),
            "{}",
            entries[0].id
        );
        assert!(
            entries[1].id.starts_with("codex|session-a|1|"),
            "{}",
            entries[1].id
        );
    }

    #[test]
    fn canonical_id_collapses_same_session_state_across_files_keeping_earliest_date() {
        let dir = temp_dir();
        for (name, ts) in [
            ("later.jsonl", "2026-07-30T02:00:00.000Z"),
            ("earlier.jsonl", "2026-07-30T01:00:00.000Z"),
        ] {
            write(
                &dir,
                name,
                None,
                &[
                    codex_session_meta_line("session-a", ts),
                    state(ts, 100, 10, 100, 10),
                ],
            );
        }
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(entries.len(), 1);
        let expected = iso8601::parse("2026-07-30T01:00:00.000Z").unwrap();
        assert_eq!(entries[0].date, expected);
    }

    // --- filename-hint guard ---

    #[test]
    fn usable_filename_hint_rules() {
        assert!(!is_usable_filename_hint("-"));
        assert!(!is_usable_filename_hint(""));
        assert!(!is_usable_filename_hint("----"));
        assert!(is_usable_filename_hint("parent"));
        assert!(is_usable_filename_hint(
            "00000000-0000-7000-8000-000000000001"
        ));
    }

    #[test]
    fn fork_of_fork_reuses_resolved_ancestor_history() {
        let dir = temp_dir();
        let first = state("2026-07-30T01:00:01.000Z", 100, 10, 100, 10);
        let second = state("2026-07-30T01:00:02.000Z", 200, 20, 100, 10);
        let third = state("2026-07-30T01:00:03.000Z", 300, 30, 100, 10);
        let fourth = state("2026-07-30T01:00:04.000Z", 400, 40, 100, 10);
        write(
            &dir,
            "root.jsonl",
            None,
            &[
                codex_session_meta_line("root", "2026-07-30T01:00:00.000Z"),
                first.clone(),
                second.clone(),
            ],
        );
        write(
            &dir,
            "child.jsonl",
            None,
            &[
                forked_meta("child", "root", "2026-07-30T02:00:00.000Z"),
                codex_session_meta_line("root", "2026-07-30T02:00:00.001Z"),
                first.clone(),
                second.clone(),
                third.clone(),
            ],
        );
        write(
            &dir,
            "grandchild.jsonl",
            None,
            &[
                forked_meta("grandchild", "child", "2026-07-30T03:00:00.000Z"),
                codex_session_meta_line("child", "2026-07-30T03:00:00.001Z"),
                first.clone(),
                second,
                third,
                fourth,
            ],
        );
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(totals(&entries), vec![110, 110, 110, 110]);
        let c = |prefix: &str| entries.iter().filter(|e| e.id.starts_with(prefix)).count();
        assert_eq!(c("codex|root|"), 2);
        assert_eq!(c("codex|child|"), 1);
        assert_eq!(c("codex|grandchild|"), 1);
    }

    #[test]
    fn sibling_forks_with_identical_own_usage_keep_distinct_ids() {
        let dir = temp_dir();
        let replay = state("2026-07-30T01:00:01.000Z", 100, 10, 100, 10);
        let own = state("2026-07-30T02:00:01.000Z", 200, 20, 100, 10);
        write(
            &dir,
            "root.jsonl",
            None,
            &[
                codex_session_meta_line("root", "2026-07-30T01:00:00.000Z"),
                replay.clone(),
            ],
        );
        for child_id in ["left", "right"] {
            write(
                &dir,
                &format!("{child_id}.jsonl"),
                None,
                &[
                    forked_meta(child_id, "root", "2026-07-30T02:00:00.000Z"),
                    codex_session_meta_line("root", "2026-07-30T02:00:00.001Z"),
                    replay.clone(),
                    own.clone(),
                ],
            );
        }
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        assert_eq!(entries.len(), 3);
        let ids: HashSet<String> = entries.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids.len(), 3);
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.id.starts_with("codex|left|"))
                .count(),
            1
        );
        assert_eq!(
            entries
                .iter()
                .filter(|e| e.id.starts_with("codex|right|"))
                .count(),
            1
        );
    }

    // --- fixtures (acceptance criterion) ---

    #[test]
    fn manual_fork_fixture_keeps_only_post_replay_usage() {
        let dir = temp_dir();
        let child = copy_fixture(&dir, "CodexFork", "child");
        let entries = parse_codex_file(&child, tz());
        assert_eq!(totals(&entries), vec![0, 28_138]);
    }

    #[test]
    fn manual_fork_fixture_keeps_parent_and_child_usage_on_their_own_days() {
        let dir = temp_dir();
        let parent = copy_fixture(&dir, "CodexFork", "parent");
        copy_fixture(&dir, "CodexFork", "child");

        let parent_entries = parse_codex_file(&parent, tz());
        let parent_day = parent_entries
            .first()
            .expect("parent entries")
            .local_day
            .clone();
        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        let child_day = entries
            .iter()
            .find(|e| e.total() == 28_138)
            .expect("child new turn")
            .local_day
            .clone();

        assert_eq!(
            aggregate::daily(&entries, &parent_day).map(|d| d.total_tokens),
            Some(312_814)
        );
        assert_eq!(
            aggregate::daily(&entries, &child_day).map(|d| d.total_tokens),
            Some(28_138)
        );
        assert_eq!(
            aggregate::period(&entries, "fixture", &parent_day, &child_day).total_tokens,
            340_952
        );
        let ids: HashSet<String> = entries.iter().map(|e| e.id.clone()).collect();
        assert_eq!(ids.len(), entries.len());
    }

    #[test]
    fn sibling_fork_fixtures_keep_independent_post_replay_usage() {
        let dir = temp_dir();
        copy_fixture(&dir, "CodexFork", "parent");
        copy_fixture(&dir, "CodexFork", "child");
        copy_fixture(&dir, "CodexFork", "sibling");

        let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
        let fork_totals: Vec<i64> = entries
            .iter()
            .map(|e| e.total())
            .filter(|&t| t == 28_138 || t == 28_263)
            .collect();
        let mut sorted = fork_totals.clone();
        sorted.sort();
        assert_eq!(sorted, vec![28_138, 28_263]);
        assert_eq!(sum(&entries), 369_215);
    }

    #[test]
    fn subagent_fixtures_keep_all_own_usage_without_replay_prefix() {
        let fixtures = [
            (
                "parent",
                "child",
                [22_992, 23_043, 23_062, 23_219, 23_291],
                115_607,
                "00000000-0000-7000-8000-000000000002",
            ),
            (
                "parent-v145",
                "child-v145",
                [20_863, 21_175, 21_365, 21_458, 21_722],
                106_583,
                "00000000-0000-7000-8000-000000000146",
            ),
        ];
        for (parent_fx, child_fx, expected, combined, child_id) in fixtures {
            let dir = temp_dir();
            copy_fixture(&dir, "CodexSubagent", parent_fx);
            copy_fixture(&dir, "CodexSubagent", child_fx);
            let entries = codex_entries(std::slice::from_ref(&dir), distant_past(), tz());
            let mut sorted = totals(&entries);
            sorted.sort();
            assert_eq!(sorted, expected, "{child_fx}");
            assert_eq!(sum(&entries), combined, "{child_fx}");
            let child_count = entries
                .iter()
                .filter(|e| e.id.starts_with(&format!("codex|{child_id}|")))
                .count();
            assert_eq!(child_count, 2, "{child_fx}");
        }
    }

    #[test]
    fn subagent_child_fixtures_keep_first_turn_when_parent_is_missing() {
        let fixtures = [
            (
                "child",
                "00000000-0000-7000-8000-000000000002",
                "00000000-0000-7000-8000-000000000001",
                [23_062, 23_291],
            ),
            (
                "child-v145",
                "00000000-0000-7000-8000-000000000146",
                "00000000-0000-7000-8000-000000000145",
                [21_458, 21_722],
            ),
        ];
        for (name, child_id, parent_id, expected) in fixtures {
            let dir = temp_dir();
            let child = copy_fixture(&dir, "CodexSubagent", name);
            let rollout = parse_codex_rollout(&child, tz());
            assert_eq!(rollout.session_id.as_deref(), Some(child_id), "{name}");
            assert_eq!(
                rollout.parent_session_id.as_deref(),
                Some(parent_id),
                "{name}"
            );
            assert!(rollout.is_subagent, "{name}");

            let entries = parse_codex_file(&child, tz());
            assert_eq!(totals(&entries), expected, "{name}");
            assert!(
                entries
                    .iter()
                    .all(|e| e.id.starts_with(&format!("codex|{child_id}|"))),
                "{name}"
            );
        }
    }

    // --- usage-cache watermark ---

    use crate::usage_cache::UsageCache;

    fn cached_env() -> (PathBuf, UsageCache) {
        let dir = temp_dir();
        let cache = UsageCache::open(&dir.join("usage-cache.sqlite")).unwrap();
        (dir, cache)
    }

    fn fork_tree(dir: &Path) -> (PathBuf, PathBuf) {
        let parent = write(
            dir,
            "parent.jsonl",
            None,
            &[
                codex_session_meta_line("parent", "2026-08-19T01:00:00.000Z"),
                state("2026-08-19T01:00:01.000Z", 100, 10, 100, 10),
                state("2026-08-19T01:00:02.000Z", 300, 30, 200, 20),
            ],
        );
        let child = write(
            dir,
            "child.jsonl",
            None,
            &[
                forked_meta("child", "parent", "2026-08-20T02:00:00.000Z"),
                codex_session_meta_line("parent", "2026-08-20T02:00:00.001Z"),
                state("2026-08-20T02:00:00.010Z", 100, 10, 100, 10),
                state("2026-08-20T02:00:00.020Z", 300, 30, 200, 20),
                state("2026-08-20T02:00:03.000Z", 1_300, 100, 1_000, 98),
            ],
        );
        (parent, child)
    }

    #[test]
    fn unchanged_rollouts_are_served_from_the_cache() {
        let (dir, cache) = cached_env();
        fork_tree(&dir);
        let since = distant_past();
        let first =
            codex_entries_with_cache(std::slice::from_ref(&dir), since, tz(), Some(&cache), true);
        assert!(!first.is_empty());

        let state = cache
            .source("codex", &crate::usage_cache::source_key(&dir.join("child.jsonl")))
            .unwrap()
            .expect("the child rollout must be cached");
        assert!(state.payload.is_some(), "parse state must be persisted");

        let second =
            codex_entries_with_cache(std::slice::from_ref(&dir), since, tz(), Some(&cache), false);
        assert_eq!(totals(&second), totals(&first), "cached pass must equal the full pass");
        // And equal to the no-cache baseline.
        let plain = codex_entries(std::slice::from_ref(&dir), since, tz());
        assert_eq!(totals(&second), totals(&plain));
    }

    #[test]
    fn changed_window_rollout_is_reparsed_and_matches_full_read() {
        let (dir, cache) = cached_env();
        let (_parent, child) = fork_tree(&dir);
        let since = distant_past();
        let first =
            codex_entries_with_cache(std::slice::from_ref(&dir), since, tz(), Some(&cache), true);

        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).open(&child).unwrap();
        // The fixture file has no trailing newline, so the append must start with one.
        f.write_all(
            format!("\n{}\n", state("2026-08-20T02:00:04.000Z", 2_000, 120, 700, 20))
                .as_bytes(),
        )
        .unwrap();
        let second =
            codex_entries_with_cache(std::slice::from_ref(&dir), since, tz(), Some(&cache), false);
        let plain = codex_entries(std::slice::from_ref(&dir), since, tz());
        assert!(totals(&second).len() > totals(&first).len(), "the new turn must be picked up");
        assert_eq!(
            totals(&second),
            totals(&plain),
            "reparse of the changed file must equal a full read"
        );
    }

    #[test]
    fn cached_rollout_roundtrip_preserves_replay_resolution() {
        let dir = temp_dir();
        let child = copy_fixture(&dir, "CodexFork", "child");
        let rollout = parse_codex_rollout(&child, tz());
        let decoded = decode_rollout(&encode_rollout(&rollout), &child).unwrap();
        assert_eq!(decoded.session_id, rollout.session_id);
        assert_eq!(decoded.parent_session_id, rollout.parent_session_id);
        assert_eq!(decoded.events.len(), rollout.events.len());
        for (a, b) in rollout.events.iter().zip(&decoded.events) {
            assert_eq!(a.entry, b.entry);
            assert_eq!(a.usage_state, b.usage_state);
            assert_eq!(a.session_id, b.session_id);
        }
        let a = resolve_codex_rollouts(&[rollout], &HashSet::from([child.clone()]));
        let b = resolve_codex_rollouts(&[decoded], &HashSet::from([child.clone()]));
        assert_eq!(a, b, "replay resolution must be cache-invariant");
    }
}
