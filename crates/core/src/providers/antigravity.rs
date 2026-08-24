//! Antigravity CLI — `~/.gemini/antigravity-cli/conversations/<conversation>.db`, one SQLite
//! database per conversation. The per-call token ledger lives in `gen_metadata.data`, a
//! length-delimited protobuf blob walked field by field (there is no schema to decode against;
//! the field numbers below are the writer's own contract, read from the CLI's embedded proto
//! pool):
//!
//! ```text
//! gen_metadata.data              CortexStepGeneratorMetadata
//!   1     chat_model             ChatModelMetadata
//!   1.4     usage                ModelUsageStats
//!   1.4.2     input_tokens       prompt tokens, cache reads NOT included
//!   1.4.3     output_tokens      thinking + response (already summed)
//!   1.4.4     cache_write_tokens declared, never written by this CLI
//!   1.4.5     cache_read_tokens  prompt cache hit
//!   1.4.11    response_id        globally unique per call
//!   1.9     chat_start_metadata ChatStartMetadata
//!   1.9.4     created_at         google.protobuf.Timestamp (1: seconds, 2: nanos)
//!   1.19    response_model       e.g. "gemini-3.6-flash"
//! ```
//!
//! `input_tokens` is already net of the cache read and `output_tokens` already sums its two
//! siblings, so neither is adjusted the way the Gemini CLI parser adjusts its own fields.
//! Model names carry an `antigravity/` prefix, which also keeps them out of the pricing
//! table — this CLI is a subscription and the wire carries no amount.

use crate::entry::{dedup_keep_max, Entry};
use crate::provider::{ProviderCtx, UsageProvider};
use crate::{usage_cache, windows};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub struct AntigravityProvider;

const SQL: &str = "SELECT idx, data FROM gen_metadata WHERE data IS NOT NULL";
/// "no such table" and friends surface as the generic error code from `sqlite3_prepare_v2`.
const SQLITE_ERROR: i32 = 1;

impl UsageProvider for AntigravityProvider {
    fn id(&self) -> &'static str {
        "antigravity"
    }
    fn display_name(&self) -> &'static str {
        "Antigravity"
    }
    fn reports_cost(&self) -> bool {
        true
    }
    fn available(&self, ctx: &ProviderCtx) -> bool {
        let root = default_root(ctx);
        let Ok(dir) = fs::read_dir(&root) else {
            return false;
        };
        dir.flatten().any(|e| {
            e.path().is_file() && e.path().extension().and_then(|s| s.to_str()) == Some("db")
        })
    }
    fn read_entries(&self, ctx: &ProviderCtx, since: DateTime<Utc>) -> anyhow::Result<Vec<Entry>> {
        let cache = usage_cache::UsageCache::resolve();
        Ok(antigravity_entries(ctx, since, cache.as_ref()))
    }
}

/// The read path, with the cache injected so tests can pin it to a temp file. The per-store
/// blob cache that used to live for a single call is persisted between passes, keyed by the
/// very `(mtime, size)` signature the sweep already trusts; a store that changed is re-read
/// whole, one that did not is never reopened.
fn antigravity_entries(
    ctx: &ProviderCtx,
    since: DateTime<Utc>,
    cache: Option<&usage_cache::UsageCache>,
) -> Vec<Entry> {
    let root = default_root(ctx);
    let full = cache
        .map(|c| c.full_rescan_due("antigravity", &ctx.tz))
        .unwrap_or(true);
    let known = cache.map(|c| load_known(c, full)).unwrap_or_default();
    let scanned = scan(&root, since, &ctx.tz, known);
    let entries = assemble(&scanned.blobs, since);
    if let Some(c) = cache {
        commit_blobs(c, &scanned.blobs);
        c.prune_entries_before("antigravity", since);
        if full {
            c.mark_full_scanned("antigravity", &ctx.tz);
        }
    }
    entries
}

/// Rebuild the sweep's `known` blobs from the cache: the signature rides in the source's
/// payload (a `DbState` with no tables), the rows ride in the entries table.
fn load_known(cache: &usage_cache::UsageCache, full: bool) -> HashMap<String, Blob> {
    if full {
        return HashMap::new();
    }
    let mut known: HashMap<String, Blob> = HashMap::new();
    let Ok(sources) = cache.sources("antigravity") else {
        return known;
    };
    for (key, state) in sources {
        let Some(sig) = state
            .payload
            .as_deref()
            .and_then(|p| serde_json::from_str::<usage_cache::DbState>(p).ok())
            .and_then(|s| s.signature())
        else {
            continue;
        };
        let entries = cache.load_entries("antigravity", &key).unwrap_or_default();
        known.insert(
            key,
            Blob {
                mtime: sig.0,
                size: sig.1,
                entries,
            },
        );
    }
    known
}

/// Persist the sweep's surviving blobs (signature + rows) and drop the sources the sweep no
/// longer visits (stores that left the window or vanished).
fn commit_blobs(cache: &usage_cache::UsageCache, blobs: &HashMap<String, Blob>) {
    let mut keep: Vec<String> = Vec::with_capacity(blobs.len());
    for (key, blob) in blobs {
        let state = usage_cache::DbState::new((blob.mtime, blob.size), HashMap::new());
        let payload = serde_json::to_string(&state).ok();
        cache.store_entries("antigravity", key, &blob.entries);
        cache.upsert_source("antigravity", key, 0, payload);
        keep.push(key.clone());
    }
    cache.prune_sources("antigravity", &keep);
}

/// One conversation store's rows, valid for as long as its `(mtime, size)` hold. The rows are
/// deliberately *unfiltered*: the window is applied after the lookup.
#[derive(Debug, Clone)]
struct Blob {
    mtime: DateTime<Utc>,
    size: u64,
    entries: Vec<Entry>,
}

/// What one sweep produced: the surviving blobs keyed by path.
#[derive(Debug)]
struct Scan {
    blobs: HashMap<String, Blob>,
}

/// What one conversation store yielded. Three of these four produce no rows, and only one of
/// those is a fact about the user's usage — collapsing them all to `[]` is what let a store
/// that could never be read pass for a store that was never used.
#[derive(Debug, Clone)]
pub enum ConversationRead {
    /// Read through to `SQLITE_DONE`: these rows are all of them.
    Complete {
        entries: Vec<Entry>,
        discarded_counters: usize,
    },
    /// The scan stopped early — BUSY because the CLI was writing, or a damaged page. Half a
    /// conversation must not pass for the whole of it, so the rows read so far are dropped.
    IncompleteScan { status: i32, rows: usize },
    /// Neither read-only form could open it, or the query failed for a reason other than the
    /// table being absent.
    Unreadable { status: Option<i32> },
    /// No `gen_metadata`: this file is not a conversation store. A permanent, legitimate
    /// empty — the directory may hold databases we don't read.
    NotAConversation,
}

impl ConversationRead {
    /// Rows only exist for a scan that ran through to `SQLITE_DONE`.
    pub fn entries(&self) -> Vec<Entry> {
        match self {
            Self::Complete { entries, .. } => entries.clone(),
            _ => Vec::new(),
        }
    }
}

/// At most this many stores are named per scan; the rest are counted. A store that cannot be
/// read is re-read on every refresh, so one line per store per scan could rotate the log
/// several times a day on a machine whose conversation directory went bad.
pub const NAMED_LOSS_LIMIT: usize = 5;

/// The conversation root. The directory is absent unless Antigravity CLI ran.
fn default_root(ctx: &ProviderCtx) -> PathBuf {
    ctx.home.join(".gemini/antigravity-cli/conversations")
}

/// Rows from every blob, narrowed to the window and deduplicated. `response_id` is unique per
/// call, so the dedup only ever collapses the same turn copied into a second store.
fn assemble(blobs: &HashMap<String, Blob>, since: DateTime<Utc>) -> Vec<Entry> {
    let all: Vec<Entry> = blobs
        .values()
        .flat_map(|b| b.entries.iter().cloned())
        .filter(|e| e.date >= since)
        .collect();
    dedup_keep_max(all)
}

/// Reads every conversation store the window admits, reusing any blob in `known` whose
/// signature still matches.
fn scan(
    root: &Path,
    modified_since: DateTime<Utc>,
    tz: &chrono::FixedOffset,
    known: HashMap<String, Blob>,
) -> Scan {
    let mut blobs: HashMap<String, Blob> = HashMap::new();
    for database in databases(root) {
        // Stat before the read, never after. A commit that lands mid-read then differs from
        // the stored signature on the next sweep and is re-read; stat afterwards and that
        // same commit is frozen into a signature that already looks current.
        let Some(signature) = signature(&database) else {
            continue;
        };
        if signature.0 < modified_since {
            continue;
        }
        let key = database.display().to_string();
        if let Some(blob) = known
            .get(&key)
            .filter(|b| b.mtime == signature.0 && b.size == signature.1)
        {
            blobs.insert(key.clone(), blob.clone());
            continue;
        }
        let read = conversation_entries(&database, tz);
        match &read {
            ConversationRead::Complete { entries, .. } => {
                blobs.insert(
                    key,
                    Blob {
                        mtime: signature.0,
                        size: signature.1,
                        entries: entries.clone(),
                    },
                );
            }
            // A permanent property of the file, so cache the empty — otherwise every database
            // in the directory that is not a conversation store is reopened on every refresh.
            ConversationRead::NotAConversation => {
                blobs.insert(
                    key,
                    Blob {
                        mtime: signature.0,
                        size: signature.1,
                        entries: Vec::new(),
                    },
                );
            }
            // Never file a failed read under the current signature: the store would read as
            // no usage for as long as it sat still. Carry the previous rows forward under
            // their old signature so the next sweep tries again.
            _ => {
                if let Some(stale) = known.get(&key) {
                    blobs.insert(key, stale.clone());
                }
            }
        }
    }
    Scan { blobs }
}

fn databases(root: &Path) -> Vec<PathBuf> {
    let Ok(dir) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("db"))
        .collect();
    out.sort();
    out
}

/// The store's cache key, and the value the scan window is tested against. A WAL commit
/// lands in the `-wal` sibling and leaves the main file's timestamp and length alone, so the
/// newest of the two decides. `-shm` is deliberately excluded: it carries no committed data,
/// and a read-only WAL connection writes read marks into it.
fn signature(database: &Path) -> Option<(DateTime<Utc>, u64)> {
    let mut newest: Option<DateTime<Utc>> = None;
    let mut size: u64 = 0;
    let mut wal = database.as_os_str().to_os_string();
    wal.push("-wal");
    for path in [database, Path::new(&wal)] {
        let Ok(meta) = fs::metadata(path) else {
            continue;
        };
        if let Ok(mtime) = meta.modified() {
            let mtime = DateTime::<Utc>::from(mtime);
            newest = Some(newest.map(|n| n.max(mtime)).unwrap_or(mtime));
        }
        size += meta.len();
    }
    newest.map(|mtime| (mtime, size))
}

/// Reads every row in the store. The window is *not* applied here: the rows go into a blob
/// that outlives the call that produced it, and a cutoff baked into a cached unit is wrong
/// the moment the window moves.
fn conversation_entries(database: &Path, tz: &chrono::FixedOffset) -> ConversationRead {
    let conn = match open_readonly(database) {
        Some(conn) => conn,
        None => return ConversationRead::Unreadable { status: None },
    };
    let mut stmt = match conn.prepare(SQL) {
        Ok(stmt) => stmt,
        Err(e) => {
            // `SQLITE_ERROR` here is "no such table", which is a fact about the file and will
            // never change; anything else (BUSY, I/O) is this moment failing to read a store
            // that may well be a conversation.
            return if sqlite_code(&e) == Some(SQLITE_ERROR) {
                ConversationRead::NotAConversation
            } else {
                ConversationRead::Unreadable {
                    status: sqlite_code(&e),
                }
            };
        }
    };
    let conversation = database
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let mut entries: Vec<Entry> = Vec::new();
    let mut discarded_counters = 0usize;
    let mut rows = 0usize;
    let mut rs = match stmt.query([]) {
        Ok(rs) => rs,
        Err(e) => {
            return ConversationRead::IncompleteScan {
                status: sqlite_code(&e).unwrap_or(-1),
                rows,
            }
        }
    };
    loop {
        match rs.next() {
            Ok(Some(row)) => {
                rows += 1;
                let index: i64 = row.get(0).unwrap_or(0);
                let Some(blob) = row
                    .get::<_, Option<Vec<u8>>>(1)
                    .ok()
                    .flatten()
                    .filter(|b| !b.is_empty())
                else {
                    continue;
                };
                let record = parse_generation_metadata(&blob, conversation, index, tz);
                discarded_counters += record.discarded_counters;
                if let Some(entry) = record.entry {
                    entries.push(entry);
                }
            }
            // The CLI writes while the app polls, so a scan can end on BUSY rather than on
            // DONE. Half a conversation would otherwise be cached as the whole of it; drop it
            // instead and let the next refresh read it entire.
            Ok(None) => {
                return ConversationRead::Complete {
                    entries,
                    discarded_counters,
                }
            }
            Err(e) => {
                return ConversationRead::IncompleteScan {
                    status: sqlite_code(&e).unwrap_or(-1),
                    rows,
                }
            }
        }
    }
}

/// `mode=ro` cannot create the `-shm` file a WAL database needs, so it fails outright on
/// conversations that have no `-wal` sibling yet. `immutable=1` reads those, and is only
/// reached when there is no uncommitted tail to miss. Using `immutable=1` first would
/// silently drop the newest turns of an active conversation.
fn open_readonly(database: &Path) -> Option<Connection> {
    if !database.exists() {
        return None;
    }
    // Opening is lazy: plain read-only reports success on a checkpointed WAL database and
    // only fails once something reads, which is late enough to look like an empty
    // conversation instead of a failed open. Force the read here so the fallback runs.
    for (uri, use_uri) in [
        (database.display().to_string(), false),
        (format!("file:{}?immutable=1", uri_path(database)), true),
    ] {
        let flags = if use_uri {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        };
        let conn = match Connection::open_with_flags(&uri, flags) {
            Ok(conn) => conn,
            Err(_) => continue,
        };
        if conn
            .execute_batch("SELECT count(*) FROM sqlite_master")
            .is_ok()
        {
            return Some(conn);
        }
    }
    None
}

/// Percent-encode a path for use inside a SQLite `file:` URI (`/` separators kept).
fn uri_path(path: &Path) -> String {
    let mut out = String::new();
    let mut rooted = false;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => rooted = true,
            other => {
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(&percent_encode(&other.as_os_str().to_string_lossy()));
            }
        }
    }
    if rooted {
        out.insert(0, '/');
    }
    out
}

fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' || b == b'~' {
            out.push(b as char);
        } else {
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// The primary SQLite result code (low byte of the extended code) for a failure.
fn sqlite_code(e: &rusqlite::Error) -> Option<i32> {
    match e {
        rusqlite::Error::SqliteFailure(err, _) => Some(err.extended_code & 0xff),
        _ => None,
    }
}

/// What one `gen_metadata` row yielded. `discarded_counters` travels with the entry because
/// `token_count` runs four times per row and a live store holds thousands of them — the scan
/// totals these rather than reporting each one where it happens.
struct Record {
    entry: Option<Entry>,
    discarded_counters: usize,
}

fn parse_generation_metadata(
    blob: &[u8],
    conversation: &str,
    index: i64,
    tz: &chrono::FixedOffset,
) -> Record {
    let Some(chat_model) = proto::message(blob, 1) else {
        return Record {
            entry: None,
            discarded_counters: 0,
        };
    };
    let Some(usage) = proto::message(&chat_model, 4) else {
        return Record {
            entry: None,
            discarded_counters: 0,
        };
    };
    let Some(date) = created_at(&chat_model) else {
        return Record {
            entry: None,
            discarded_counters: 0,
        };
    };

    // The turn's own id, not the file it happens to sit in — a copied conversation must not
    // read as fresh spend. `response_id` is populated on every recorded call.
    let identity = proto::string(&usage, 11)
        .map(|s| format!("antigravity|{s}"))
        .unwrap_or_else(|| format!("antigravity|{conversation}|{index}"));
    // `response_model` names the model that answered; the prefix keeps the name out of the
    // rate table as well.
    let model = proto::string(&chat_model, 19).unwrap_or_else(|| "unknown".into());

    let counters: [Option<i64>; 4] = [
        proto::token_count(&usage, 2),
        proto::token_count(&usage, 3),
        proto::token_count(&usage, 4),
        proto::token_count(&usage, 5),
    ];
    let discarded_counters = counters.iter().filter(|c| c.is_none()).count();
    let [input, output, cache_write, cache_read] = counters.map(|c| c.unwrap_or(0));
    if input + output + cache_write + cache_read <= 0 {
        return Record {
            entry: None,
            discarded_counters,
        };
    }
    Record {
        entry: Some(Entry {
            id: identity,
            date,
            local_day: windows::local_day(date, tz),
            model: format!("antigravity/{model}"),
            input,
            output,
            cache_write,
            cache_read,
            explicit_cost: None,
        }),
        discarded_counters,
    }
}

/// `chat_start_metadata.created_at`, a `google.protobuf.Timestamp`.
fn created_at(chat_model: &[u8]) -> Option<DateTime<Utc>> {
    let start = proto::message(chat_model, 9)?;
    let stamp = proto::message(&start, 4)?;
    let seconds = proto::varint(&stamp, 1)? as i64;
    // A malformed varint can carry the whole uint64 range; a date built from it would
    // overflow downstream arithmetic. Anything outside a plausible window is not a time.
    if !(1_000_000_000..=4_102_444_800).contains(&seconds) {
        return None;
    }
    let nanos = match proto::varint(&stamp, 2) {
        Some(n) if n < 1_000_000_000 => n as u32,
        _ => 0,
    };
    Utc.timestamp_opt(seconds, nanos).single()
}

/// The loss lines one scan would leave behind. Pure on purpose: the decision is tested on its
/// own, and emitting them is the app layer's side effect (the core crate has no log sink).
pub fn loss_log(reads: &[(String, ConversationRead)]) -> Vec<String> {
    let lines: Vec<String> = reads
        .iter()
        .filter_map(|(conversation, read)| match read {
            ConversationRead::Complete { .. } | ConversationRead::NotAConversation => None,
            ConversationRead::IncompleteScan { status, rows } => Some(format!(
                "antigravity: lost conversation={conversation} reason=scan-incomplete status={status} rows={rows}"
            )),
            ConversationRead::Unreadable { status } => Some(format!(
                "antigravity: lost conversation={conversation} reason=unreadable{}",
                status
                    .map(|status| format!(" status={status}"))
                    .unwrap_or_default()
            )),
        })
        .collect();
    capped(lines, |total, hidden| {
        format!("antigravity: lost {total} conversation(s) this scan ({hidden} not named)")
    })
}

/// The discard lines one scan would leave behind. Pure, for the same reason as `loss_log`.
pub fn discard_log(reads: &[(String, ConversationRead)]) -> Vec<String> {
    let lines: Vec<String> = reads
        .iter()
        .filter_map(|(conversation, read)| match read {
            ConversationRead::Complete {
                discarded_counters,
                ..
            } if *discarded_counters > 0 => Some(format!(
                "antigravity: conversation={conversation} discarded={discarded_counters} token counter(s) over {}",
                proto::TOKEN_CEILING
            )),
            _ => None,
        })
        .collect();
    capped(lines, |total, hidden| {
        format!(
            "antigravity: discarded token counters in {total} conversation(s) ({hidden} not named)"
        )
    })
}

fn capped(lines: Vec<String>, summary: impl Fn(usize, usize) -> String) -> Vec<String> {
    if lines.len() <= NAMED_LOSS_LIMIT {
        return lines;
    }
    let total = lines.len();
    let mut out: Vec<String> = lines.into_iter().take(NAMED_LOSS_LIMIT).collect();
    out.push(summary(total, total - NAMED_LOSS_LIMIT));
    out
}

/// Just enough of the protobuf wire format to walk the Cascade metadata blobs. Reading an
/// external file, a malformed byte is an expected outcome rather than an error: every helper
/// stops at the first thing it cannot parse and reports what it read up to that point.
mod proto {
    /// Tokens are `uint64` on the wire. Widening an out-of-range value straight into
    /// arithmetic would trap (or, clamped, dominate every aggregate it reaches). A count this
    /// large is a sentinel: discarding it loses one counter and leaves the rest of the record
    /// intact.
    pub const TOKEN_CEILING: u64 = 1_000_000_000;

    /// `Some(0)` for an absent field (a legitimate zero — `cache_write_tokens` is declared
    /// and never written); `None` when the field was present and its value cannot be a count.
    pub fn token_count(data: &[u8], field: u64) -> Option<i64> {
        match varint(data, field) {
            Some(value) if value <= TOKEN_CEILING => Some(value as i64),
            Some(_) => None,
            None => Some(0),
        }
    }

    pub fn varint(data: &[u8], field: u64) -> Option<u64> {
        let mut result: Option<u64> = None;
        walk(data, &mut |number: u64,
                         value: u64,
                         payload: Option<&[u8]>| {
            if number == field && payload.is_none() {
                result = Some(value);
                false
            } else {
                true
            }
        });
        result
    }

    pub fn string(data: &[u8], field: u64) -> Option<String> {
        let payload = message(data, field)?;
        if payload.is_empty() {
            return None;
        }
        let text = String::from_utf8(payload).ok()?;
        if text.is_empty() {
            return None;
        }
        Some(text)
    }

    pub fn message(data: &[u8], field: u64) -> Option<Vec<u8>> {
        let mut result: Option<Vec<u8>> = None;
        walk(data, &mut |number: u64,
                         _value: u64,
                         payload: Option<&[u8]>| {
            match payload {
                Some(payload) if number == field => {
                    result = Some(payload.to_vec());
                    false
                }
                _ => true,
            }
        });
        result
    }

    /// Visits each field in order until `visit` returns false or the bytes stop making sense.
    /// Length-delimited fields arrive as `payload`; varints arrive as `value`. Fixed-width
    /// fields are skipped — nothing this reader wants is encoded that way. A group tag (or
    /// any other unknown wire type) means these bytes are not the message we took them for,
    /// so the walk stops there.
    pub fn walk<F: FnMut(u64, u64, Option<&[u8]>) -> bool>(data: &[u8], mut visit: F) {
        let mut index = 0usize;
        while index < data.len() {
            let (key, after_key) = match read_varint(data, index) {
                Some(v) => v,
                None => return,
            };
            index = after_key;
            let field = key >> 3;
            if field == 0 {
                return;
            }
            match key & 7 {
                0 => {
                    let (value, after_value) = match read_varint(data, index) {
                        Some(v) => v,
                        None => return,
                    };
                    index = after_value;
                    if !visit(field, value, None) {
                        return;
                    }
                }
                1 => {
                    if data.len() - index < 8 {
                        return;
                    }
                    index += 8;
                }
                2 => {
                    let (length, after_length) = match read_varint(data, index) {
                        Some(v) => v,
                        None => return,
                    };
                    if length > data.len() as u64 - after_length as u64 {
                        return;
                    }
                    let end = after_length + length as usize;
                    if !visit(field, 0, Some(&data[after_length..end])) {
                        return;
                    }
                    index = end;
                }
                5 => {
                    if data.len() - index < 4 {
                        return;
                    }
                    index += 4;
                }
                _ => return,
            }
        }
    }

    fn read_varint(data: &[u8], start: usize) -> Option<(u64, usize)> {
        let mut value: u64 = 0;
        let mut shift: u32 = 0;
        let mut index = start;
        while index < data.len() {
            let byte = data[index];
            index += 1;
            if shift < 64 {
                value |= u64::from(byte & 0x7f) << shift;
            }
            if byte & 0x80 == 0 {
                return Some((value, index));
            }
            shift += 7;
            if shift > 63 {
                return None; // a varint is at most ten bytes
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::ModelPricing;
    use crate::provider::ProviderCtx;
    use chrono::FixedOffset;
    use std::fmt::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

    /// `sqlite3_step` has finished executing.
    const SQLITE_DONE: i32 = 100;

    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn date(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn distant_past() -> DateTime<Utc> {
        date("1970-01-01T00:00:00Z")
    }

    /// One scratch directory per test case (tests in this crate run threaded).
    fn fresh_dir() -> PathBuf {
        let seq = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir()
            .join(format!("antigravity-test-{}", std::process::id()))
            .join(format!("case-{seq}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // -- Wire format encoding, test side only --

    fn encode_raw_varint(mut value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                bytes.push(byte);
                return bytes;
            }
            bytes.push(byte | 0x80);
        }
    }

    fn encode_varint(field: u32, value: u64) -> Vec<u8> {
        let mut out = encode_raw_varint(u64::from(field) << 3);
        out.extend(encode_raw_varint(value));
        out
    }

    fn encode_message(field: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = encode_raw_varint((u64::from(field) << 3) | 2);
        out.extend(encode_raw_varint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn encode_string(field: u32, text: &str) -> Vec<u8> {
        encode_message(field, text.as_bytes())
    }

    /// `CortexStepGeneratorMetadata { 1 chat_model { 4 usage, 9 chat_start_metadata,
    /// 19 response_model } }`
    #[allow(clippy::too_many_arguments)] // one arg per encoded wire field
    fn make_record(
        response_id: Option<&str>,
        model: &str,
        created_at_seconds: u64,
        input: u64,
        output: u64,
        cache_read: u64,
        thinking: Option<u64>,
        response: Option<u64>,
    ) -> Vec<u8> {
        let mut usage = encode_varint(1, 1071); // model enum
        usage.extend(encode_varint(2, input)); // input_tokens
        usage.extend(encode_varint(3, output)); // output_tokens
        usage.extend(encode_varint(5, cache_read)); // cache_read_tokens
        usage.extend(encode_varint(6, 24)); // api_provider
        if let Some(thinking) = thinking {
            usage.extend(encode_varint(9, thinking));
        }
        if let Some(response) = response {
            usage.extend(encode_varint(10, response));
        }
        if let Some(response_id) = response_id {
            usage.extend(encode_string(11, response_id));
        }
        let timestamp = encode_varint(1, created_at_seconds);
        let chat_start = encode_message(4, &timestamp);
        let mut chat_model = encode_varint(3, 1071);
        chat_model.extend(encode_message(4, &usage));
        chat_model.extend(encode_message(9, &chat_start));
        chat_model.extend(encode_string(19, model));
        encode_message(1, &chat_model)
    }

    fn write_conversation(
        dir: &Path,
        name: &str,
        blobs: &[Vec<u8>],
        wal_mode: bool,
        page_size: Option<u32>,
    ) -> PathBuf {
        let db = dir.join(format!("{name}.db"));
        let conn = Connection::open(&db).unwrap();
        let mut sql = String::new();
        // `page_size` only takes effect before the first table exists.
        if let Some(size) = page_size {
            let _ = writeln!(sql, "PRAGMA page_size={size};");
        }
        if wal_mode {
            sql.push_str("PRAGMA journal_mode=WAL;\n");
        }
        sql.push_str(
            "CREATE TABLE gen_metadata (idx integer, data blob, size integer NOT NULL DEFAULT 0, PRIMARY KEY (idx));\n",
        );
        for (index, blob) in blobs.iter().enumerate() {
            let hex: String = blob.iter().map(|b| format!("{b:02x}")).collect();
            let _ = writeln!(
                sql,
                "INSERT INTO gen_metadata VALUES ({index}, X'{hex}', {});",
                blob.len()
            );
        }
        conn.execute_batch(&sql).unwrap();
        db
    }

    fn read_all(dir: &Path) -> Vec<Entry> {
        let scanned = scan(dir, distant_past(), &utc(), HashMap::new());
        assemble(&scanned.blobs, distant_past())
    }

    fn read_conversation(dir: &Path, name: &str) -> ConversationRead {
        conversation_entries(&dir.join(format!("{name}.db")), &utc())
    }

    fn scan_known(dir: &Path, known: HashMap<String, Blob>, modified_since: DateTime<Utc>) -> Scan {
        scan(dir, modified_since, &utc(), known)
    }

    fn set_mtime(path: &Path, when: &str) {
        assert!(
            std::process::Command::new("touch")
                .args(["-d", when])
                .arg(path)
                .status()
                .unwrap()
                .success(),
            "touch -d {when}"
        );
    }

    /// A blob that does not match the file it is filed under, so a test can tell a cache hit
    /// (the planted rows come back) from a re-read (the file's own rows do).
    fn planted_blob(database: &Path, id: &str) -> Blob {
        let (mtime, size) = signature(database).unwrap();
        Blob {
            mtime,
            size,
            entries: vec![Entry {
                id: id.to_string(),
                date: date("2026-03-04T10:00:00Z"),
                local_day: "2026-03-04".into(),
                model: "antigravity/planted".into(),
                input: 1,
                output: 0,
                cache_write: 0,
                cache_read: 0,
                explicit_cost: None,
            }],
        }
    }

    /// A store spanning several pages, so damaging the last one still leaves earlier rows
    /// readable — which is the shape a scan that stops half way actually has.
    fn write_many_records(dir: &Path, name: &str, count: usize, page_size: u32) {
        let base = date("2026-03-04T10:00:00Z").timestamp() as u64;
        let blobs: Vec<Vec<u8>> = (0..count)
            .map(|index| {
                make_record(
                    Some(&format!("r{index}")),
                    "gemini-3.6-flash",
                    base + index as u64,
                    100,
                    20,
                    300,
                    None,
                    None,
                )
            })
            .collect();
        write_conversation(dir, name, &blobs, false, Some(page_size));
    }

    /// Damages the b-tree page-type byte of the last page. Page 1 — the schema — is
    /// untouched on purpose, so the open probe still succeeds and the failure can only come
    /// from the step loop.
    fn corrupt_last_page(database: &Path, page_size: u32) {
        let mut bytes = fs::read(database).unwrap();
        let num_pages = bytes.len() as u64 / u64::from(page_size);
        assert!(num_pages > 2, "the fixture needs more than two pages");
        bytes[(num_pages - 1) as usize * page_size as usize] = 0x00;
        fs::write(database, bytes).unwrap();
    }

    /// Walks the reader's own query so a test can assert that rows really do arrive before
    /// the failure — i.e. that it is exercising the partial-scan branch and not a failed
    /// open.
    fn step_until_not_a_row(database: &Path) -> (usize, i32) {
        let conn = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut stmt = conn.prepare(SQL).unwrap();
        let mut rs = stmt.query([]).unwrap();
        let mut rows = 0usize;
        loop {
            match rs.next() {
                Ok(Some(_)) => rows += 1,
                Ok(None) => return (rows, SQLITE_DONE),
                Err(e) => return (rows, sqlite_code(&e).unwrap_or(-1)),
            }
        }
    }

    /// Reproduces a conversation that has been fully checkpointed: the rows are in the main
    /// file and the siblings are gone.
    fn checkpoint_and_drop_wal_siblings(database: &Path) {
        let conn = Connection::open(database).unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(conn);
        for suffix in ["-wal", "-shm"] {
            let mut p = database.as_os_str().to_os_string();
            p.push(suffix);
            let _ = fs::remove_file(Path::new(&p));
        }
    }

    /// The read-only attempt alone, without the `immutable=1` fallback.
    fn open_readonly_without_fallback(database: &Path) -> Option<Connection> {
        if !database.exists() {
            return None;
        }
        let conn = match Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(conn) => conn,
            Err(_) => return None,
        };
        let ok = {
            let mut stmt = match conn.prepare("SELECT count(*) FROM gen_metadata") {
                Ok(stmt) => stmt,
                Err(_) => return None,
            };
            let query = stmt.query([]);
            match query {
                Ok(mut rs) => rs.next().ok().flatten().is_some(),
                Err(_) => false,
            }
        };
        ok.then_some(conn)
    }

    // -- Token mapping --

    /// `input_tokens` excludes cache reads and `output_tokens` already contains the thinking
    /// half, so neither may be adjusted the way the Gemini CLI parser adjusts its own fields.
    #[test]
    fn token_mapping_keeps_the_writer_semantics() {
        let dir = fresh_dir();
        write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                4667,
                462,
                52968,
                Some(398),
                Some(64),
            )],
            false,
            None,
        );
        let entry = &read_all(&dir)[0];
        assert_eq!(
            entry.input, 4667,
            "input_tokens is already net of the cache read"
        );
        assert_eq!(entry.cache_read, 52968);
        assert_eq!(
            entry.output, 462,
            "output_tokens already sums thinking and response"
        );
        assert_eq!(entry.cache_write, 0);
        assert_eq!(entry.model, "antigravity/gemini-3.6-flash");
    }

    /// The schema has no total field, so the total is whatever the counters add up to.
    #[test]
    fn total_is_the_sum_of_the_counters() {
        let dir = fresh_dir();
        write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        assert_eq!(read_all(&dir)[0].total(), 420);
    }

    #[test]
    fn thinking_and_response_are_not_added_on_top_of_output() {
        let dir = fresh_dir();
        write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                10,
                900,
                0,
                Some(800),
                Some(100),
            )],
            false,
            None,
        );
        assert_eq!(
            read_all(&dir)[0].output,
            900,
            "adding the siblings to their own sum would double the output"
        );
    }

    #[test]
    fn row_with_no_tokens_produces_no_entry() {
        let dir = fresh_dir();
        write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                0,
                0,
                0,
                None,
                None,
            )],
            false,
            None,
        );
        assert!(read_all(&dir).is_empty());
    }

    // -- Identity --

    /// The turn's own id, so the same call copied into a second conversation store stays one
    /// charge rather than becoming two.
    #[test]
    fn response_id_deduplicates_across_conversations() {
        let dir = fresh_dir();
        let shared = make_record(
            Some("same-call"),
            "gemini-3.6-flash",
            date("2026-03-04T10:00:00Z").timestamp() as u64,
            100,
            20,
            300,
            None,
            None,
        );
        write_conversation(&dir, "c1", std::slice::from_ref(&shared), false, None);
        write_conversation(&dir, "c2", &[shared], false, None);
        let entries = read_all(&dir);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "antigravity|same-call");
    }

    #[test]
    fn record_without_response_id_falls_back_to_conversation_and_index() {
        let dir = fresh_dir();
        write_conversation(
            &dir,
            "c1",
            &[make_record(
                None,
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        assert_eq!(read_all(&dir)[0].id, "antigravity|c1|0");
    }

    // -- Time --

    #[test]
    fn created_at_drives_the_local_day() {
        let dir = fresh_dir();
        let created = date("2026-03-04T10:00:00Z");
        write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                created.timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        let entry = &read_all(&dir)[0];
        assert_eq!(entry.date, created);
        assert_eq!(entry.local_day, windows::local_day(created, &utc()));
    }

    #[test]
    fn records_before_the_window_are_excluded() {
        let dir = fresh_dir();
        write_conversation(
            &dir,
            "c1",
            &[
                make_record(
                    Some("old"),
                    "gemini-3.6-flash",
                    date("2026-03-01T10:00:00Z").timestamp() as u64,
                    100,
                    20,
                    300,
                    None,
                    None,
                ),
                make_record(
                    Some("new"),
                    "gemini-3.6-flash",
                    date("2026-03-09T10:00:00Z").timestamp() as u64,
                    100,
                    20,
                    300,
                    None,
                    None,
                ),
            ],
            false,
            None,
        );
        let since = date("2026-03-05T00:00:00Z");
        let scanned = scan(&dir, since, &utc(), HashMap::new());
        let entries = assemble(&scanned.blobs, since);
        assert_eq!(
            entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["antigravity|new"]
        );
    }

    /// A timestamp outside any plausible window is a misread varint, not a date.
    #[test]
    fn implausible_timestamp_is_rejected() {
        let dir = fresh_dir();
        write_conversation(
            &dir,
            "c1",
            &[
                make_record(
                    Some("r1"),
                    "gemini-3.6-flash",
                    u64::MAX,
                    100,
                    20,
                    300,
                    None,
                    None,
                ),
                make_record(Some("r2"), "gemini-3.6-flash", 0, 100, 20, 300, None, None),
            ],
            false,
            None,
        );
        assert!(read_all(&dir).is_empty());
    }

    // -- Hostile input --

    /// A `uint64` sentinel widened into a count would trap (or dominate) downstream
    /// arithmetic on every refresh because the file never changes. Dropping it leaves the
    /// rest of the record intact.
    #[test]
    fn sentinel_token_count_is_discarded_rather_than_trapping() {
        let dir = fresh_dir();
        write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                u64::MAX,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        let entry = &read_all(&dir)[0];
        assert_eq!(entry.input, 0);
        assert_eq!(entry.total(), 320, "the rest of the record still counts");
        let read = read_conversation(&dir, "c1");
        match &read {
            ConversationRead::Complete {
                discarded_counters, ..
            } => assert_eq!(*discarded_counters, 1),
            other => panic!("expected a complete read, got {other:?}"),
        }
        let line = discard_log(&[("c1".into(), read)])
            .into_iter()
            .next()
            .unwrap();
        assert!(line.contains("conversation=c1"), "{line}");
        assert!(line.contains("discarded=1"), "{line}");
    }

    /// A counter the writer never sets is a zero, not a bad value — `cache_write_tokens` is
    /// declared and never written, so treating "absent" as a discard would report every
    /// record on every scan.
    #[test]
    fn absent_counter_is_not_a_discard() {
        let dir = fresh_dir();
        write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        let read = read_conversation(&dir, "c1");
        match &read {
            ConversationRead::Complete {
                entries,
                discarded_counters,
            } => {
                assert_eq!(entries.first().map(|e| e.cache_write), Some(0));
                assert_eq!(*discarded_counters, 0);
            }
            other => panic!("expected a complete read, got {other:?}"),
        }
        assert!(discard_log(&[("c1".into(), read)]).is_empty());
    }

    /// Same bound as the loss lines, and for the same reason.
    #[test]
    fn discard_log_names_a_few_stores_and_counts_the_rest() {
        let limit = NAMED_LOSS_LIMIT;
        let reads: Vec<(String, ConversationRead)> = (0..(limit + 2))
            .map(|i| {
                (
                    format!("c{i}"),
                    ConversationRead::Complete {
                        entries: Vec::new(),
                        discarded_counters: 3,
                    },
                )
            })
            .collect();
        let lines = discard_log(&reads);
        assert_eq!(lines.len(), limit + 1);
        let last = lines.last().unwrap();
        assert!(
            last.contains(&format!("in {} conversation(s)", limit + 2)),
            "{last}"
        );
        assert!(last.contains("(2 not named)"), "{last}");
    }

    #[test]
    fn malformed_blob_is_ignored() {
        let dir = fresh_dir();
        write_conversation(&dir, "c1", &[vec![0xFF; 11]], false, None);
        assert!(read_all(&dir).is_empty());
    }

    #[test]
    fn truncated_blob_is_ignored() {
        let dir = fresh_dir();
        let mut bytes = make_record(
            Some("r1"),
            "gemini-3.6-flash",
            1_772_618_400,
            100,
            20,
            300,
            None,
            None,
        );
        bytes.truncate(bytes.len() / 2);
        write_conversation(&dir, "c1", &[bytes], false, None);
        assert!(read_all(&dir).is_empty());
    }

    #[test]
    fn missing_directory_yields_no_entries() {
        let dir = fresh_dir();
        let absent = dir.join("not-here");
        let scanned = scan(&absent, distant_past(), &utc(), HashMap::new());
        assert!(assemble(&scanned.blobs, distant_past()).is_empty());
    }

    #[test]
    fn database_without_the_expected_table_is_ignored() {
        let dir = fresh_dir();
        let db = dir.join("c1.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE something_else (a INTEGER);")
            .unwrap();
        drop(conn);
        assert!(read_all(&dir).is_empty());
    }

    // -- A lost conversation leaves a trace --

    /// A scan that cannot finish drops the rows it did read, and names the reason: the result
    /// is indistinguishable from a conversation with no usage otherwise.
    #[test]
    fn incomplete_scan_drops_the_conversation_and_names_the_reason() {
        let dir = fresh_dir();
        write_many_records(&dir, "c1", 60, 512);
        write_conversation(
            &dir,
            "c2",
            &[make_record(
                Some("survivor"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        let database = dir.join("c1.db");
        corrupt_last_page(&database, 512);

        // Assert the branch this test exists for is the one being walked: the store still
        // opens and still hands back rows, so the loss can only be coming from the step loop.
        let (probe_rows, probe_status) = step_until_not_a_row(&database);
        assert!(
            probe_rows > 0,
            "no rows read — the failure moved to open/prepare"
        );
        assert_ne!(
            probe_status, SQLITE_DONE,
            "the scan finished — this no longer covers the drop"
        );

        let read = read_conversation(&dir, "c1");
        let (status, rows, entries) = match &read {
            ConversationRead::IncompleteScan { status, rows } => (*status, *rows, read.entries()),
            other => panic!("expected an incomplete scan, got {other:?}"),
        };
        assert_eq!(status, probe_status);
        assert!(rows > 0);
        assert!(
            entries.is_empty(),
            "half a conversation must not pass for the whole of it"
        );

        let lines = loss_log(&[("c1".into(), read)]);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("conversation=c1"), "{}", lines[0]);
        assert!(lines[0].contains("reason=scan-incomplete"), "{}", lines[0]);
        assert!(
            lines[0].contains(&format!("status={status}")),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains(&format!("rows={rows}")), "{}", lines[0]);

        // The rest of the directory is unaffected — one bad store, not a bad scan.
        let survivors = read_all(&dir);
        assert_eq!(survivors.len(), 1);
        assert_eq!(survivors[0].id, "antigravity|survivor");
    }

    /// A file that is not a database at all must read as unreadable rather than as an empty
    /// conversation — the two are the same zero, and only one of them is about usage.
    #[test]
    fn unreadable_store_is_not_an_empty_conversation() {
        let dir = fresh_dir();
        let database = dir.join("c1.db");
        fs::write(&database, b"not a sqlite database").unwrap();
        let read = read_conversation(&dir, "c1");
        match &read {
            ConversationRead::Unreadable { .. } => {}
            other => panic!("expected unreadable, got {other:?}"),
        }
        assert!(read.entries().is_empty());
        let line = loss_log(&[("c1".into(), read)]).into_iter().next().unwrap();
        assert!(line.contains("conversation=c1"), "{line}");
        assert!(line.contains("reason=unreadable"), "{line}");
    }

    /// The opposite case: a database without `gen_metadata` will never grow one, so naming it
    /// would repeat every refresh forever.
    #[test]
    fn database_without_the_expected_table_is_not_reported_as_a_loss() {
        let dir = fresh_dir();
        let db = dir.join("c1.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE something_else (a INTEGER);")
            .unwrap();
        drop(conn);
        let read = read_conversation(&dir, "c1");
        match &read {
            ConversationRead::NotAConversation => {}
            other => panic!("expected notAConversation, got {other:?}"),
        }
        assert!(loss_log(&[("c1".into(), read)]).is_empty());
    }

    /// A directory that has gone bad wholesale must not be able to rotate the log.
    #[test]
    fn loss_log_names_a_few_stores_and_counts_the_rest() {
        let limit = NAMED_LOSS_LIMIT;
        let reads: Vec<(String, ConversationRead)> = (0..(limit + 4))
            .map(|i| {
                (
                    format!("c{i}"),
                    ConversationRead::Unreadable { status: None },
                )
            })
            .collect();
        let lines = loss_log(&reads);
        assert_eq!(lines.len(), limit + 1);
        let summary = lines.last().unwrap();
        assert!(
            summary.contains(&format!("lost {} conversation(s)", limit + 4)),
            "{summary}"
        );
        assert!(summary.contains("(4 not named)"), "{summary}");
    }

    /// A clean scan says nothing at all.
    #[test]
    fn complete_scan_logs_nothing() {
        let dir = fresh_dir();
        write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        let read = read_conversation(&dir, "c1");
        assert_eq!(read.entries().len(), 1);
        let lineless = &[("c1".into(), read)];
        assert!(loss_log(lineless).is_empty());
        assert!(discard_log(lineless).is_empty());
    }

    // -- Opening WAL databases read-only --

    /// Every conversation store is in WAL mode. A fully checkpointed one has no `-wal` sibling
    /// and its rows all live in the main file, so a read-only open (or the `immutable=1`
    /// fallback, depending on the SQLite build) must yield the rows.
    #[test]
    fn checkpointed_wal_database_is_still_readable() {
        let dir = fresh_dir();
        let database = write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            true,
            None,
        );
        checkpoint_and_drop_wal_siblings(&database);
        assert_eq!(
            read_all(&dir).len(),
            1,
            "a checkpointed WAL store must still read"
        );
    }

    /// The state the `immutable=1` fallback exists for: an *active* WAL store (a `-wal`
    /// sibling with committed-but-uncheckpointed data) that a read-only connection cannot
    /// index because it cannot create the `-shm` file. Plain read-only must fail here, and the
    /// reader must recover the committed rows via `immutable=1` (which reads the main file and
    /// — by construction — sees no uncommitted tail to miss).
    #[cfg(unix)]
    #[test]
    fn reader_falls_back_to_immutable_when_read_only_cannot_index_the_wal() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fresh_dir();
        let database = write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            true,
            None,
        );
        // Rows land in the main file; the store stays in WAL mode.
        checkpoint_and_drop_wal_siblings(&database);
        // Simulate a live WAL tail: a `-wal` sibling is present, `-shm` is not, and the
        // directory is not writable so a read-only connection cannot create `-shm`.
        let mut wal = database.as_os_str().to_os_string();
        wal.push("-wal");
        fs::write(Path::new(&wal), [0u8; 64]).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();

        // Assert the branch is really walked: without the fallback the store is unreadable.
        assert!(
            open_readonly_without_fallback(&database).is_none(),
            "plain read-only must fail here — otherwise this no longer exercises the fallback"
        );
        let entries = read_all(&dir);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            entries.len(),
            1,
            "the fallback must recover the committed row"
        );
    }

    /// A WAL commit lands in the sibling and leaves the main file's timestamp untouched, so a
    /// scan keyed on the `.db` alone would skip exactly the conversations that just moved.
    #[test]
    fn wal_sibling_timestamp_selects_the_database() {
        let dir = fresh_dir();
        let database = write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        set_mtime(&database, "2020-01-01 00:00:00 UTC");
        let mut wal = database.as_os_str().to_os_string();
        wal.push("-wal");
        fs::write(Path::new(&wal), [0]).unwrap();

        let stale = date("2020-01-01T00:00:00Z");
        let signature = signature(&database).unwrap();
        assert!(
            signature.0 > stale,
            "the -wal sibling must move the signature"
        );
        let since = date("2026-01-01T00:00:00Z");
        let scanned = scan(&dir, since, &utc(), HashMap::new());
        assert_eq!(assemble(&scanned.blobs, since).len(), 1);
    }

    // -- Not re-reading what has not changed --

    /// The blob stands in for the store while its signature holds — the point of keying on the
    /// signature at all is that an unchanged store is never reopened.
    #[test]
    fn store_with_an_unchanged_signature_is_not_reread() {
        let dir = fresh_dir();
        let database = write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("onDisk"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        let key = database.display().to_string();
        let scanned = scan_known(
            &dir,
            HashMap::from([(key.clone(), planted_blob(&database, "planted"))]),
            distant_past(),
        );
        assert_eq!(
            scanned.blobs[&key]
                .entries
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["planted"],
            "the store was reopened even though nothing about it had changed"
        );
    }

    /// The commit a WAL database actually makes: the `.db` is untouched and the `-wal`
    /// appears.
    #[test]
    fn wal_commit_invalidates_the_blob() {
        let dir = fresh_dir();
        let database = write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("onDisk"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        let planted = planted_blob(&database, "planted");
        let mut wal = database.as_os_str().to_os_string();
        wal.push("-wal");
        fs::write(Path::new(&wal), [0, 1, 2, 3]).unwrap();
        let key = database.display().to_string();
        let scanned = scan_known(
            &dir,
            HashMap::from([(key.clone(), planted)]),
            distant_past(),
        );
        assert_eq!(
            scanned.blobs[&key]
                .entries
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["antigravity|onDisk"],
            "a WAL commit must invalidate the blob — the .db alone never moves"
        );
    }

    /// Reason `-shm` is not in the key: a read-only connection writes read marks into it, so
    /// a signature that included it would be invalidated by this very reader, on every scan.
    #[test]
    fn shm_churn_does_not_invalidate_the_blob() {
        let dir = fresh_dir();
        let database = write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("onDisk"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        let planted = planted_blob(&database, "planted");
        let mut shm = database.as_os_str().to_os_string();
        shm.push("-shm");
        fs::write(Path::new(&shm), [9, 9, 9, 9]).unwrap();
        let key = database.display().to_string();
        let scanned = scan_known(
            &dir,
            HashMap::from([(key.clone(), planted)]),
            distant_past(),
        );
        assert_eq!(
            scanned.blobs[&key]
                .entries
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["planted"],
            "-shm churn is somebody reading the store, not somebody writing to it"
        );
    }

    /// A read that did not finish must not be filed under the current signature: the store
    /// would then read as no usage for as long as it sat still, and the next refresh — the
    /// whole reason the partial read was discarded — would never come.
    #[test]
    fn incomplete_scan_keeps_the_previous_rows_under_their_old_signature() {
        let dir = fresh_dir();
        write_many_records(&dir, "c1", 60, 512);
        let database = dir.join("c1.db");
        corrupt_last_page(&database, 512);
        let stale = Blob {
            mtime: distant_past(),
            size: 0,
            entries: vec![Entry {
                id: "planted".into(),
                date: date("2026-03-04T10:00:00Z"),
                local_day: "2026-03-04".into(),
                model: "antigravity/planted".into(),
                input: 1,
                output: 0,
                cache_write: 0,
                cache_read: 0,
                explicit_cost: None,
            }],
        };
        let key = database.display().to_string();
        let scanned = scan_known(&dir, HashMap::from([(key.clone(), stale)]), distant_past());
        let blob = &scanned.blobs[&key];
        assert_eq!(
            blob.entries
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["planted"],
            "the rows already known were thrown away"
        );
        assert_eq!(
            blob.mtime,
            distant_past(),
            "the old signature must survive so the next scan retries"
        );
    }

    /// With nothing to carry forward there must be no blob at all — an empty one would freeze
    /// the store as "no usage" until something happened to change it.
    #[test]
    fn unreadable_store_is_not_cached_as_an_empty_conversation() {
        let dir = fresh_dir();
        let database = dir.join("c1.db");
        fs::write(&database, b"not a sqlite database").unwrap();
        let key = database.display().to_string();
        let scanned = scan_known(&dir, HashMap::new(), distant_past());
        assert!(!scanned.blobs.contains_key(&key));
    }

    /// A database with no `gen_metadata` will never grow one, so caching its empty is what
    /// stops it being reopened on every refresh for the life of the install.
    #[test]
    fn database_without_the_expected_table_is_cached_as_empty() {
        let dir = fresh_dir();
        let database = dir.join("c1.db");
        let conn = Connection::open(&database).unwrap();
        conn.execute_batch("CREATE TABLE something_else (a INTEGER);")
            .unwrap();
        drop(conn);
        let key = database.display().to_string();
        let scanned = scan_known(&dir, HashMap::new(), distant_past());
        let blob = &scanned.blobs[&key];
        assert!(blob.entries.is_empty());
        assert_eq!(blob.mtime, signature(&database).unwrap().0);
    }

    /// A store that drops out of the window leaves the cache with it — the sweep rebuilds
    /// from what it visited, so there is no separate prune to forget to run.
    #[test]
    fn stores_outside_the_window_leave_the_cache() {
        let dir = fresh_dir();
        let database = write_conversation(
            &dir,
            "c1",
            &[make_record(
                Some("old"),
                "gemini-3.6-flash",
                date("2020-01-01T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        set_mtime(&database, "2020-01-02 00:00:00 UTC");
        let key = database.display().to_string();
        let scanned = scan_known(
            &dir,
            HashMap::from([(key, planted_blob(&database, "planted"))]),
            date("2026-03-01T00:00:00Z"),
        );
        assert!(scanned.blobs.is_empty());
    }

    // -- Pricing --

    /// Antigravity is a subscription and reports no amount, so an estimate would be an
    /// invented bill. The prefix keeps the names out of the exact table as well — this CLI
    /// really does call `claude-sonnet-4-6`, which the table prices.
    #[test]
    fn antigravity_usage_is_not_priced() {
        for model in [
            "gemini-3.6-flash",
            "gemini-3-flash-e",
            "gemini-default",
            "claude-sonnet-4-6",
        ] {
            assert_eq!(
                ModelPricing::cost(
                    &format!("antigravity/{model}"),
                    1_000_000,
                    1_000_000,
                    1_000_000,
                    1_000_000
                ),
                0.0,
                "antigravity/{model} must not be priced"
            );
        }
        assert!(
            ModelPricing::cost("claude-sonnet-4-6", 1_000_000, 0, 0, 0) > 0.0,
            "the unprefixed name must keep its rate"
        );
    }

    // -- Provider --

    /// Registered in its own right, so someone who runs only Antigravity sees their own tab
    /// rather than their spend labelled "Gemini".
    #[test]
    fn default_registry_includes_antigravity() {
        let p = AntigravityProvider;
        assert!(crate::provider::all()
            .iter()
            .any(|provider| provider.id() == p.id()));
    }

    /// Someone who has never run Antigravity must get silence rather than an error: the
    /// provider is skipped and reads nothing.
    #[test]
    fn silent_without_any_conversation_store() {
        let dir = fresh_dir();
        let ctx = ProviderCtx::for_test(dir.clone(), utc());
        let p = AntigravityProvider;
        assert!(!p.available(&ctx));
        assert!(antigravity_entries(&ctx, distant_past(), None).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    /// `available` turns on the directory existing and holding a `.db`, not on the DB being
    /// readable (that is the reader's job, and it degrades per store).
    #[test]
    fn available_requires_a_conversation_database() {
        let dir = fresh_dir();
        let root = dir.join(".gemini/antigravity-cli/conversations");
        let ctx = ProviderCtx::for_test(dir.clone(), utc());
        let p = AntigravityProvider;
        assert!(!p.available(&ctx), "no directory");
        fs::create_dir_all(&root).unwrap();
        assert!(!p.available(&ctx), "empty directory");
        let _ = write_conversation(
            &root,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                1,
                1,
                0,
                None,
                None,
            )],
            false,
            None,
        );
        assert!(p.available(&ctx));
        let entries = antigravity_entries(&ctx, distant_past(), None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "antigravity|r1");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The blob cache outlives the call that built it: a second pass over an unchanged store
    /// must be served from the persisted rows, and the persistence must be verifiable on disk.
    #[test]
    fn blob_cache_persists_across_calls() {
        use crate::usage_cache::UsageCache;
        let dir = fresh_dir();
        let root = dir.join(".gemini/antigravity-cli/conversations");
        fs::create_dir_all(&root).unwrap();
        let database = write_conversation(
            &root,
            "c1",
            &[make_record(
                Some("r1"),
                "gemini-3.6-flash",
                date("2026-03-04T10:00:00Z").timestamp() as u64,
                100,
                20,
                300,
                None,
                None,
            )],
            false,
            None,
        );
        let ctx = ProviderCtx::for_test(dir.clone(), utc());
        let cache = UsageCache::open(&dir.join("cache/usage-cache.sqlite")).unwrap();
        let since = distant_past();

        let first = antigravity_entries(&ctx, since, Some(&cache));
        assert_eq!(first.len(), 1);
        // The store's signature + rows must have landed in the cache.
        let key = database.display().to_string();
        let state = cache
            .source("antigravity", &key)
            .unwrap()
            .expect("the store was not persisted");
        assert!(state.payload.is_some(), "the signature payload is missing");
        assert_eq!(
            cache.load_entries("antigravity", &key).unwrap().len(),
            1,
            "the rows were not persisted"
        );

        // A second pass over the unchanged store, and a fresh full read, must agree.
        let second = antigravity_entries(&ctx, since, Some(&cache));
        let plain = antigravity_entries(&ctx, since, None);
        assert_eq!(second, plain);
        let _ = fs::remove_dir_all(&dir);
    }
}
