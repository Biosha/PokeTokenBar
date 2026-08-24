//! Thin read-only rusqlite helpers shared by the SQLite-backed providers.

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OpenFlags, Row};
use std::path::Path;

/// Open a SQLite database read-only (the sources are other apps' live files; never write).
pub fn open_ro(path: &Path) -> anyhow::Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(conn)
}

/// Run `sql` (no params) and collect the non-`None` results of `f` per row.
pub fn rows<T>(
    conn: &Connection,
    sql: &str,
    mut f: impl FnMut(&Row<'_>) -> anyhow::Result<Option<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let mut out = Vec::new();
    let mut rs = stmt.query([])?;
    while let Some(row) = rs.next()? {
        if let Some(v) = f(row)? {
            out.push(v);
        }
    }
    Ok(out)
}

/// Run `sql` (with params) and collect the non-`None` results of `f` per row.
pub fn rows_p<T>(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
    mut f: impl FnMut(&Row<'_>) -> anyhow::Result<Option<T>>,
) -> anyhow::Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let mut out = Vec::new();
    let mut rs = stmt.query(params)?;
    while let Some(row) = rs.next()? {
        if let Some(v) = f(row)? {
            out.push(v);
        }
    }
    Ok(out)
}

/// `(mtime, size)` of a database file, counting its `-wal` sibling: a WAL commit can land in
/// the `-wal` file and leave the main file's timestamp and length untouched, so the newest of
/// the two decides. `-shm` carries no committed data and is excluded.
pub fn db_signature(path: &Path) -> Option<(DateTime<Utc>, u64)> {
    let mut newest: Option<DateTime<Utc>> = None;
    let mut size: u64 = 0;
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    for p in [path, Path::new(&wal)] {
        let Ok(meta) = std::fs::metadata(p) else {
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

/// Current max `rowid` of `table`: `0` for an empty or unreadable table.
pub fn max_rowid(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT MAX(rowid) FROM {table}"), params![], |r| {
        r.get::<_, Option<i64>>(0)
    })
    .ok()
    .flatten()
    .unwrap_or(0)
}

/// Text column, `None` when absent/NULL.
pub fn col_text(row: &Row<'_>, idx: usize) -> Option<String> {
    row.get::<_, String>(idx).ok()
}

/// Integer column, `0` when absent/NULL (usage counters are non-negative and missing is 0).
pub fn col_i64(row: &Row<'_>, idx: usize) -> i64 {
    row.get::<_, i64>(idx).unwrap_or(0)
}
