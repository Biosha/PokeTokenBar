//! On-disk usage cache: per-source watermarks plus parsed-entry storage, so a steady-state
//! `snapshot`/`watch` pass reads only what changed since the previous pass instead of
//! re-parsing each provider's full local history.
//!
//! ## Format
//! One SQLite file (`usage-cache.sqlite` in the cache dir; `rusqlite` is already a core
//! dependency). SQLite rather than JSONL because the data is row-addressed (per source, per
//! entry id) and the app polls every few seconds: a single-file WAL database gives atomic
//! updates without rewriting the whole history, and concurrent processes (app + CLI) share it
//! safely. Three tables:
//! * `meta` — small knobs: the local day of each provider's last full rescan.
//! * `sources` — one row per provider source (a log file or a SQLite database): a monotonic
//!   `marker` (byte offset for files; `0` for databases) and a JSON `payload` (a database's
//!   file signature + per-table max-rowid watermarks, or a provider-specific parse state).
//! * `entries` — parsed [`Entry`]s keyed by `(provider, source, id)`; date stored as exact
//!   nanoseconds so the round-trip is lossless.
//!
//! ## Enablement
//! `PTB_USAGE_CACHE` controls it: unset → the standard cache dir; `off`/`0`/`false` → force
//! full reads (the pre-cache behavior, kept for equivalence checks); any other value is used
//! as the cache directory (test hook). Any open or read failure degrades that pass to a full
//! read — the cache never blocks or breaks a snapshot.
//!
//! ## Safety valves (the UI shows real dollar figures)
//! 1. **Once-per-local-day full rescan, per provider.** The first pass of a local day ignores
//!    every watermark and re-reads all sources from scratch, replacing the cached state. This
//!    bounds any incremental drift to one day and also rebuilds after a clock rollback (the
//!    local day changes with the clock).
//! 2. **Moving-floor pruning.** Entries older than the enrichment-window start
//!    (`windows::enrichment_scan_start`) are pruned after every pass. That floor is monotonic
//!    in `now`, so pruned rows are never needed again; the daily full rescan is the backstop.
//! 3. **Per-source validation on the incremental path.**
//!    * Log files: the marker is the byte offset after the last fully processed line. A file
//!      whose size shrank (truncation/rotation) or whose marker is missing is fully re-read;
//!      a grown file contributes only its appended *complete* lines (a half-written trailing
//!      line without its newline is left for the next pass); a line that passes the provider's
//!      cheap substring filter but fails to parse invalidates the incremental result and the
//!      file is fully re-read — rows are never silently skipped by the incremental path.
//!    * SQLite databases: the persisted state holds the file's `(mtime, size)` signature
//!      (the `-wal` sibling included, since a WAL commit can land there) and a per-table
//!      max-rowid watermark. A changed signature means the file was written: append-only
//!      tables then take only rows with `rowid` above the watermark, while tables that update
//!      rows in place (session accumulators, KV stores) are fully re-read. A failing rowid
//!      query falls back to a full re-read.
//! 4. **Source discovery is per pass.** Sources are re-walked on every pass; cached rows for
//!    sources that no longer exist are pruned, and a (re-)appearing source starts with a full
//!    read. A provider flipping available → unavailable → available re-derives everything.

use crate::entry::Entry;
use crate::paths;
use crate::windows;
use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// Environment switch: `off`/`0`/`false` disables the cache; any other value is a directory.
pub const ENV_VAR: &str = "PTB_USAGE_CACHE";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS sources (
    provider TEXT NOT NULL,
    source TEXT NOT NULL,
    marker INTEGER NOT NULL DEFAULT 0,
    payload TEXT,
    PRIMARY KEY (provider, source)
);
CREATE TABLE IF NOT EXISTS entries (
    provider TEXT NOT NULL,
    source TEXT NOT NULL,
    id TEXT NOT NULL,
    date_ns INTEGER NOT NULL,
    local_day TEXT NOT NULL,
    model TEXT NOT NULL,
    input INTEGER NOT NULL,
    output INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    cache_read INTEGER NOT NULL,
    explicit_cost REAL,
    PRIMARY KEY (provider, source, id)
);";

/// One source's watermark state. `payload` is provider-specific JSON (`DbState` for databases,
/// a rollout state for Codex, `None` for plain line-log files).
#[derive(Debug, Clone)]
pub struct SourceState {
    pub marker: i64,
    pub payload: Option<String>,
}

/// Persisted state for a SQLite source: the file signature plus a per-table max-rowid
/// watermark (the table's monotonic column; `rowid` is implicit in every rowid table).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbState {
    pub mtime_ns: i64,
    pub size: u64,
    pub tables: HashMap<String, i64>,
}

impl DbState {
    pub fn new(sig: (DateTime<Utc>, u64), tables: HashMap<String, i64>) -> Self {
        Self {
            mtime_ns: sig
                .0
                .timestamp_nanos_opt()
                .unwrap_or(sig.0.timestamp_millis() * 1_000_000),
            size: sig.1,
            tables,
        }
    }

    /// True when the on-disk file is byte-for-byte the one this state was captured from.
    pub fn matches(&self, sig: (DateTime<Utc>, u64)) -> bool {
        let sig_ns = sig
            .0
            .timestamp_nanos_opt()
            .unwrap_or(sig.0.timestamp_millis() * 1_000_000);
        self.mtime_ns == sig_ns && self.size == sig.1
    }

    pub fn table(&self, name: &str) -> i64 {
        self.tables.get(name).copied().unwrap_or(0)
    }

    /// The file signature this state was captured from, for signature reuse by providers
    /// that key their own blobs on `(mtime, size)`.
    pub fn signature(&self) -> Option<(DateTime<Utc>, u64)> {
        let mtime = ns_to_utc(self.mtime_ns)?;
        Some((mtime, self.size))
    }
}

/// How one database source should be read this pass.
#[derive(Debug, Clone)]
pub struct DbPlan {
    /// When false the source must be fully re-read (fresh state, no cache reuse).
    pub incremental: bool,
    /// Per-table `rowid` floors, valid only when `incremental` is true.
    pub markers: HashMap<String, i64>,
}

impl DbPlan {
    pub fn full() -> Self {
        Self {
            incremental: false,
            markers: HashMap::new(),
        }
    }
}

pub struct UsageCache {
    /// `Connection` is not `Sync`, but the cache is shared behind a shared reference (one
    /// instance per process, passed by `&` into every provider); a single lock serializes the
    /// short read/commit critical sections.
    conn: Mutex<Connection>,
}

/// Stable key for a source path: canonicalized when possible (symlink-stable), else as-is.
pub fn source_key(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// Offset just past the final `\n` of `data` (a line boundary = a UTF-8 char boundary),
/// `0` when `data` holds no complete line.
pub fn complete_line_end(data: &[u8]) -> usize {
    data.iter()
        .rposition(|&b| b == b'\n')
        .map(|p| p + 1)
        .unwrap_or(0)
}

impl UsageCache {
    /// Open (creating if needed) the cache at `path`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", "3000")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Lock the connection; a poisoned lock degrades like any other cache failure.
    fn lock(&self) -> Option<MutexGuard<'_, Connection>> {
        self.conn.lock().ok()
    }

    /// The process-wide cache: `PTB_USAGE_CACHE` (off / directory) else the standard cache
    /// dir. `None` means "read everything, persist nothing" for this pass.
    pub fn resolve() -> Option<Self> {
        match std::env::var(ENV_VAR) {
            Ok(v) => {
                let v = v.trim();
                if v.is_empty() || matches!(v, "off" | "0" | "false" | "no") {
                    return None;
                }
                let dir = PathBuf::from(v);
                Self::open(&dir.join("usage-cache.sqlite")).ok()
            }
            Err(_) => {
                let dir = paths::cache_dir()?;
                Self::open(&dir.join("usage-cache.sqlite")).ok()
            }
        }
    }

    // ----- meta / full-rescan day -----------------------------------------------

    pub fn meta_get(&self, key: &str) -> Option<String> {
        let conn = self.lock()?;
        conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .ok()
        .flatten()
    }

    pub fn meta_set(&self, key: &str, value: &str) {
        let Some(conn) = self.lock() else {
            return;
        };
        let _ = conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        );
    }

    /// Whether this provider must full-rescan this pass (first ever, or the local day moved
    /// since its last full rescan). Uses the wall clock: `build_snapshot`'s `now` is the wall
    /// clock in both the CLI and the app.
    pub fn full_rescan_due(&self, provider: &str, tz: &FixedOffset) -> bool {
        let key = format!("full_day:{provider}");
        let today = windows::local_day(Utc::now(), tz);
        self.meta_get(&key).as_deref() != Some(today.as_str())
    }

    pub fn mark_full_scanned(&self, provider: &str, tz: &FixedOffset) {
        self.meta_set(
            &format!("full_day:{provider}"),
            &windows::local_day(Utc::now(), tz),
        );
    }

    // ----- sources ----------------------------------------------------------------

    /// Every persisted source for this provider (path key + state), for preloading a
    /// provider's in-process blob cache before a sweep.
    pub fn sources(&self, provider: &str) -> anyhow::Result<Vec<(String, SourceState)>> {
        let conn = self
            .lock()
            .ok_or_else(|| anyhow::anyhow!("cache connection poisoned"))?;
        let mut stmt =
            conn.prepare("SELECT source, marker, payload FROM sources WHERE provider = ?1")?;
        let rows = stmt
            .query_map(params![provider], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    SourceState {
                        marker: r.get(1)?,
                        payload: r.get(2)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn source(&self, provider: &str, source: &str) -> anyhow::Result<Option<SourceState>> {
        let conn = self
            .lock()
            .ok_or_else(|| anyhow::anyhow!("cache connection poisoned"))?;
        let row = conn
            .query_row(
                "SELECT marker, payload FROM sources WHERE provider = ?1 AND source = ?2",
                params![provider, source],
                |r| {
                    Ok(SourceState {
                        marker: r.get(0)?,
                        payload: r.get(1)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn upsert_source(
        &self,
        provider: &str,
        source: &str,
        marker: i64,
        payload: Option<String>,
    ) {
        let Some(conn) = self.lock() else {
            return;
        };
        let _ = conn.execute(
            "INSERT INTO sources (provider, source, marker, payload) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(provider, source) DO UPDATE SET marker = excluded.marker, payload = excluded.payload",
            params![provider, source, marker, payload],
        );
    }

    /// Drop this provider's sources no longer discovered this pass — and the entries those
    /// sources stored, so a vanished file never keeps feeding the snapshot.
    pub fn prune_sources(&self, provider: &str, keep: &[String]) {
        let mut conn = match self.lock() {
            Some(c) => c,
            None => return,
        };
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(_) => return,
        };
        if keep.is_empty() {
            let _ = tx.execute("DELETE FROM sources WHERE provider = ?1", params![provider]);
            let _ = tx.execute("DELETE FROM entries WHERE provider = ?1", params![provider]);
        } else {
            let n = keep.len();
            let placeholders: Vec<String> = (0..n).map(|i| format!("?{}", i + 2)).collect();
            let args = |table: &str| -> String {
                format!(
                    "DELETE FROM {table} WHERE provider = ?1 AND source NOT IN ({})",
                    placeholders.join(", ")
                )
            };
            for table in ["sources", "entries"] {
                let _ = tx.execute(
                    &args(table),
                    rusqlite::params_from_iter(
                        std::iter::once(provider.to_string()).chain(keep.iter().cloned()),
                    ),
                );
            }
        }
        let _ = tx.commit();
    }

    // ----- entries ------------------------------------------------------------------

    pub fn load_entries(&self, provider: &str, source: &str) -> anyhow::Result<Vec<Entry>> {
        let conn = self
            .lock()
            .ok_or_else(|| anyhow::anyhow!("cache connection poisoned"))?;
        let mut stmt = conn.prepare(
            "SELECT id, date_ns, local_day, model, input, output, cache_write, cache_read, explicit_cost \
             FROM entries WHERE provider = ?1 AND source = ?2",
        )?;
        type RawRow = (String, i64, String, String, i64, i64, i64, i64, Option<f64>);
        let raw: Vec<RawRow> = stmt
            .query_map(params![provider, source], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // A corrupted row fails the whole load, which makes the provider re-read its source —
        // the cache self-heals instead of panicking the snapshot.
        raw.into_iter()
            .map(
                |(
                    id,
                    date_ns,
                    local_day,
                    model,
                    input,
                    output,
                    cache_write,
                    cache_read,
                    explicit_cost,
                )| {
                    let date = ns_to_utc(date_ns)
                        .ok_or_else(|| anyhow::anyhow!("bad cached date_ns {date_ns}"))?;
                    Ok(Entry {
                        id,
                        date,
                        local_day,
                        model,
                        input,
                        output,
                        cache_write,
                        cache_read,
                        explicit_cost,
                    })
                },
            )
            .collect::<anyhow::Result<Vec<_>>>()
    }

    /// Replace this source's cached entries with `entries`.
    pub fn store_entries(&self, provider: &str, source: &str, entries: &[Entry]) {
        let mut conn = match self.lock() {
            Some(c) => c,
            None => return,
        };
        let Some(tx) = conn.transaction().ok() else {
            return;
        };
        let _ = tx.execute(
            "DELETE FROM entries WHERE provider = ?1 AND source = ?2",
            params![provider, source],
        );
        let sql = "INSERT OR REPLACE INTO entries \
                   (provider, source, id, date_ns, local_day, model, input, output, cache_write, cache_read, explicit_cost) \
                   VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";
        for e in entries {
            let ns = e
                .date
                .timestamp_nanos_opt()
                .unwrap_or_else(|| e.date.timestamp_millis() * 1_000_000);
            let _ = tx.execute(
                sql,
                params![
                    provider,
                    source,
                    e.id,
                    ns,
                    e.local_day,
                    e.model,
                    e.input,
                    e.output,
                    e.cache_write,
                    e.cache_read,
                    e.explicit_cost,
                ],
            );
        }
        let _ = tx.commit();
    }

    /// Drop cached entries older than the (monotonic) enrichment floor.
    pub fn prune_entries_before(&self, provider: &str, floor: DateTime<Utc>) {
        let Some(conn) = self.lock() else {
            return;
        };
        let floor_ns = floor
            .timestamp_nanos_opt()
            .unwrap_or_else(|| floor.timestamp_millis() * 1_000_000);
        let _ = conn.execute(
            "DELETE FROM entries WHERE provider = ?1 AND date_ns < ?2",
            params![provider, floor_ns],
        );
    }

    // ----- database sources ----------------------------------------------------------

    /// Decide how to read one SQLite source: incremental when the persisted signature still
    /// matches (quiescent file) or the source is append-only, full otherwise. `full_day`
    /// (the once-per-day rescan) always forces a full read. `in_place_updates` tables are
    /// never read incrementally across a signature change — a rowid watermark cannot see an
    /// edited row, only an appended one.
    pub fn db_plan(
        &self,
        provider: &str,
        source: &str,
        sig: (DateTime<Utc>, u64),
        full_day: bool,
        in_place_updates: bool,
    ) -> DbPlan {
        if full_day {
            return DbPlan::full();
        }
        let Some(state) = self.source(provider, source).ok().flatten().and_then(|s| {
            s.payload
                .and_then(|p| serde_json::from_str::<DbState>(&p).ok())
        }) else {
            return DbPlan::full();
        };
        if state.matches(sig) {
            DbPlan {
                incremental: true,
                markers: state.tables,
            }
        } else if in_place_updates {
            DbPlan::full()
        } else {
            DbPlan {
                incremental: true,
                markers: state.tables,
            }
        }
    }

    /// Persist one database source's result: signature + per-table rowid watermarks, and the
    /// final entries (full-read or merged-incremental alike).
    pub fn db_commit(
        &self,
        provider: &str,
        source: &str,
        sig: (DateTime<Utc>, u64),
        markers: &HashMap<String, i64>,
        entries: &[Entry],
    ) {
        let state = DbState::new(sig, markers.clone());
        let payload = serde_json::to_string(&state).ok();
        self.store_entries(provider, source, entries);
        self.upsert_source(provider, source, 0, payload);
    }

    /// Cached entries plus every row above the per-table rowid watermarks, re-reading the
    /// last `overlap` rows at or below the watermark so in-place finalization of a recent row
    /// (OpenCode updates a message's `data` blob once the response completes) is picked up;
    /// pass `0` for strictly-append-only tables. The caller must dedupe by id (the providers
    /// keep the max per id), since overlap rows appear both cached and freshly parsed.
    /// `parse` receives the table name (a source's tables may have different shapes) and may
    /// yield several entries per row (one row per conversation, many turns). Any error (a
    /// failed query, a vanished table, a table that shrank below its watermark, a row that
    /// should parse but does not) returns `Err` so the caller falls back to a full read.
    pub fn read_db_incremental(
        &self,
        provider: &str,
        source: &str,
        conn: &Connection,
        tables: &[(&str, &str)],
        overlap: i64,
        parse: impl Fn(&str, &rusqlite::Row<'_>) -> anyhow::Result<Vec<Entry>>,
    ) -> anyhow::Result<(Vec<Entry>, HashMap<String, i64>)> {
        let state = self
            .source(provider, source)?
            .and_then(|s| {
                s.payload
                    .and_then(|p| serde_json::from_str::<DbState>(&p).ok())
            })
            .ok_or_else(|| anyhow::anyhow!("no cached db state"))?;
        let mut entries = self.load_entries(provider, source)?;
        let mut markers = state.tables.clone();
        for (table, sql) in tables {
            let marker = state.table(table);
            let current = crate::sqld::max_rowid(conn, table);
            if current < marker {
                return Err(anyhow::anyhow!(
                    "table {table} shrank below its rowid watermark (rotated file)"
                ));
            }
            let mut stmt = conn.prepare(sql)?;
            let mut rs = stmt.query([marker.saturating_sub(overlap)])?;
            while let Some(row) = rs.next()? {
                entries.extend(parse(table, row)?);
            }
            markers.insert(table.to_string(), current.max(marker));
        }
        Ok((entries, markers))
    }
}

// ----- file-source drivers (shared by the line-log providers) ------------------------

impl UsageCache {
    /// Read one append-only line-log source incrementally.
    ///
    /// `parse_full` is the provider's existing whole-file parse (the exact pre-cache path,
    /// used for new/rotated files and as the fallback on a bad tail). `parse_tail` parses the
    /// appended *complete* lines only and must return `Err` when a line that should parse
    /// (it passed the provider's cheap filter) does not — that triggers the full re-read.
    /// `merge` combines cached and fresh entries and must be idempotent (the providers use
    /// their per-file dedup) so a retried trailing line never double-counts.
    #[allow(clippy::too_many_arguments)] // each argument names one of the source's read phases
    pub fn read_file_source<F, T, M>(
        &self,
        provider: &str,
        path: &Path,
        key: &str,
        full: bool,
        parse_full: F,
        parse_tail: T,
        merge: M,
    ) -> Vec<Entry>
    where
        F: FnOnce() -> Vec<Entry>,
        T: FnOnce(&[u8]) -> Result<Vec<Entry>, ()>,
        M: FnOnce(Vec<Entry>, Vec<Entry>) -> Vec<Entry>,
    {
        let size = match fs::metadata(path).map(|m| m.len()) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let cached: Option<(i64, Vec<Entry>)> = if !full {
            self.source(provider, key)
                .ok()
                .flatten()
                .and_then(|s| self.load_entries(provider, key).ok().map(|e| (s.marker, e)))
        } else {
            None
        };
        if let Some((marker, entries)) = cached {
            if marker as u64 == size {
                return entries;
            }
            if (0..size as i64).contains(&marker) {
                if let Ok(bytes) = read_range(path, marker as u64) {
                    let boundary = complete_line_end(&bytes);
                    if boundary == 0 {
                        // Only an incomplete trailing line was appended: keep the cache,
                        // keep the marker — the line is retried once terminated.
                        return entries;
                    }
                    if let Ok(tail) = parse_tail(&bytes[..boundary]) {
                        let merged = merge(entries, tail);
                        self.commit_file_source(provider, key, marker + boundary as i64, &merged);
                        return merged;
                    }
                }
            }
            // Marker missing, past EOF (truncation/rotation) or the tail was unreadable:
            // fall through to a full re-read.
        }
        let entries = parse_full();
        let marker = read_to_end(path)
            .map(|bytes| complete_line_end(&bytes))
            .unwrap_or(0) as i64;
        self.commit_file_source(provider, key, marker, &entries);
        entries
    }

    /// Read one whole-file source (single JSON documents / re-parse-on-any-change files):
    /// cached while the size is unchanged, fully re-read otherwise.
    pub fn read_file_source_whole<F>(
        &self,
        provider: &str,
        path: &Path,
        key: &str,
        full: bool,
        parse_full: F,
    ) -> Vec<Entry>
    where
        F: FnOnce() -> Vec<Entry>,
    {
        let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if !full {
            if let Ok(Some(state)) = self.source(provider, key) {
                if state.marker as u64 == size {
                    if let Ok(entries) = self.load_entries(provider, key) {
                        return entries;
                    }
                }
            }
        }
        let entries = parse_full();
        self.commit_file_source(provider, key, size as i64, &entries);
        entries
    }

    fn commit_file_source(&self, provider: &str, key: &str, marker: i64, entries: &[Entry]) {
        self.store_entries(provider, key, entries);
        self.upsert_source(provider, key, marker, None);
    }
}

/// Cached nanoseconds → instant. Out of range (a corrupted cache) is `None`, never a panic.
fn ns_to_utc(ns: i64) -> Option<DateTime<Utc>> {
    let secs = ns.div_euclid(1_000_000_000);
    let nanos = ns.rem_euclid(1_000_000_000) as u32;
    Utc.timestamp_opt(secs, nanos).single()
}

/// Read `path` from byte `from` to EOF.
fn read_range(path: &Path, from: u64) -> std::io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

fn read_to_end(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CTR: AtomicUsize = AtomicUsize::new(0);

    fn temp_dir() -> PathBuf {
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("ptb-cache-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn open(dir: &Path) -> UsageCache {
        UsageCache::open(&dir.join("usage-cache.sqlite")).unwrap()
    }

    fn tz() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn entry(id: &str, ns: i64, cost: Option<f64>) -> Entry {
        Entry {
            id: id.to_string(),
            date: DateTime::<Utc>::from_timestamp_nanos(ns),
            local_day: "2026-08-21".to_string(),
            model: "m".to_string(),
            input: 1,
            output: 2,
            cache_write: 3,
            cache_read: 4,
            explicit_cost: cost,
        }
    }

    #[test]
    fn entries_roundtrip_is_lossless() {
        let dir = temp_dir();
        let c = open(&dir);
        let base = DateTime::<Utc>::from_timestamp(1_787_000_000, 0).unwrap();
        let entries = vec![
            entry(
                "a",
                base.timestamp_nanos_opt().unwrap() + 123_456_789,
                Some(0.5),
            ),
            entry("b", base.timestamp_nanos_opt().unwrap(), None),
        ];
        c.store_entries("p", "s", &entries);
        let back = c.load_entries("p", "s").unwrap();
        assert_eq!(back.len(), 2);
        for (orig, loaded) in entries.iter().zip(&back) {
            assert_eq!(orig.id, loaded.id);
            assert_eq!(orig.date, loaded.date, "nanosecond precision lost");
            assert_eq!(orig.local_day, loaded.local_day);
            assert_eq!(orig.model, loaded.model);
            assert_eq!(orig.input, loaded.input);
            assert_eq!(orig.output, loaded.output);
            assert_eq!(orig.cache_write, loaded.cache_write);
            assert_eq!(orig.cache_read, loaded.cache_read);
            assert_eq!(orig.explicit_cost, loaded.explicit_cost);
        }
    }

    #[test]
    fn store_entries_replaces_per_source() {
        let dir = temp_dir();
        let c = open(&dir);
        c.store_entries("p", "s1", &[entry("a", 1, None)]);
        c.store_entries("p", "s2", &[entry("b", 1, None)]);
        c.store_entries("p", "s1", &[entry("c", 1, None), entry("d", 1, None)]);
        assert_eq!(c.load_entries("p", "s1").unwrap().len(), 2);
        assert_eq!(c.load_entries("p", "s2").unwrap().len(), 1);
        c.prune_sources("p", &["s1".to_string()]);
        assert!(c.load_entries("p", "s2").unwrap().is_empty());
        assert_eq!(c.load_entries("p", "s1").unwrap().len(), 2);
    }

    #[test]
    fn full_rescan_due_tracks_the_local_day() {
        let dir = temp_dir();
        let c = open(&dir);
        assert!(c.full_rescan_due("p", &tz()), "first ever is due");
        c.mark_full_scanned("p", &tz());
        assert!(!c.full_rescan_due("p", &tz()), "same day is not due");
        c.meta_set("full_day:p", "1999-01-01");
        assert!(c.full_rescan_due("p", &tz()), "a moved day is due again");
    }

    #[test]
    fn prune_entries_before_drops_only_older_rows() {
        let dir = temp_dir();
        let c = open(&dir);
        let day = DateTime::<Utc>::from_timestamp(1_787_000_000, 0).unwrap();
        let old = day - chrono::Duration::hours(48);
        c.store_entries(
            "p",
            "s",
            &[
                entry("old", old.timestamp_nanos_opt().unwrap(), None),
                entry("new", day.timestamp_nanos_opt().unwrap(), None),
            ],
        );
        c.prune_entries_before("p", day - chrono::Duration::hours(24));
        let back = c.load_entries("p", "s").unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, "new");
    }

    #[test]
    fn db_state_roundtrip_and_signature_match() {
        let dir = temp_dir();
        let c = open(&dir);
        let sig = (
            DateTime::<Utc>::from_timestamp_nanos(1_787_000_000_123_456_789),
            4096,
        );
        let mut tables = HashMap::new();
        tables.insert("t1".to_string(), 42i64);
        let state = DbState::new(sig, tables);
        c.upsert_source("p", "db", 0, Some(serde_json::to_string(&state).unwrap()));
        assert!(c.db_plan("p", "db", sig, false, true).incremental);
        let other = (sig.0 + chrono::Duration::seconds(1), sig.1);
        assert!(
            !c.db_plan("p", "db", other, false, true).incremental,
            "in-place: mtime change forces full"
        );
        let smaller = (sig.0, sig.1 - 1);
        assert!(
            !c.db_plan("p", "db", smaller, false, true).incremental,
            "in-place: size change forces full"
        );
        assert!(
            c.db_plan("p", "db", other, false, false).incremental,
            "append-only: mtime change → rowid watermark"
        );
        assert!(
            c.db_plan("p", "db", smaller, false, false).incremental,
            "append-only: size change → rowid watermark"
        );
        assert!(
            !c.db_plan("p", "db", sig, true, false).incremental,
            "full day forces full"
        );
    }

    #[test]
    fn complete_line_end_finds_the_last_newline() {
        assert_eq!(complete_line_end(b""), 0);
        assert_eq!(complete_line_end(b"abc"), 0);
        assert_eq!(complete_line_end(b"abc\n"), 4);
        assert_eq!(complete_line_end(b"abc\ndef\nghi"), 8);
    }
}
