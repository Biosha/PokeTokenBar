//! Recursive directory walk collecting files by extension and modification time — the port of
//! `LocalUsageReader`'s mtime-windowed scanner. Append-only logs mean "modified after `since`"
//! bounds the scan to files that could hold in-range entries.

use chrono::{DateTime, Utc};
use std::fs;
use std::path::{Path, PathBuf};

pub struct FileRec {
    pub path: PathBuf,
    pub mtime: DateTime<Utc>,
}

/// Directories never worth descending into (user worktrees, VCS internals).
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".hg",
    ".svn",
    "venv",
    ".venv",
    "__pycache__",
];

/// Recursively collect files under `root` whose extension ∈ `exts` and mtime ≥ `modified_since`.
/// Hidden entries are skipped unless `allow_hidden`; `max_depth` caps traversal.
pub fn walk_modified(
    root: &Path,
    exts: &[&str],
    modified_since: DateTime<Utc>,
    allow_hidden: bool,
    max_depth: usize,
) -> Vec<FileRec> {
    let mut out = Vec::new();
    walk(
        root,
        exts,
        modified_since,
        allow_hidden,
        max_depth,
        0,
        &mut out,
    );
    out
}

fn walk(
    dir: &Path,
    exts: &[&str],
    since: DateTime<Utc>,
    allow_hidden: bool,
    max_depth: usize,
    depth: usize,
    out: &mut Vec<FileRec>,
) {
    let rd = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let file_name = entry.file_name().to_string_lossy().to_string();
        let hidden = file_name.starts_with('.');
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_dir() {
            if hidden && !allow_hidden {
                continue;
            }
            if SKIP_DIRS.contains(&file_name.as_str()) {
                continue;
            }
            if depth + 1 > max_depth {
                continue;
            }
            walk(&path, exts, since, allow_hidden, max_depth, depth + 1, out);
        } else if ft.is_file() {
            if hidden && !allow_hidden {
                continue;
            }
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if !exts.contains(&ext) {
                continue;
            }
            if let Some(mtime) = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .map(DateTime::<Utc>::from)
                .filter(|mt| *mt >= since)
            {
                out.push(FileRec { path, mtime });
            }
        }
    }
}

/// Replace a leading `~` with the given home.
pub fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Canonicalize, dedup, and drop nested roots (mirrors Swift `normalizedRoots`), preserving
/// input order. Overlapping roots (e.g. `CLAUDE_CONFIG_DIR` pointing at a default) must not be
/// scanned twice.
pub fn normalized_roots(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen: Vec<String> = Vec::new();
    let mut unique: Vec<PathBuf> = Vec::new();
    for r in roots {
        let c = match r.canonicalize() {
            Ok(c) => c,
            Err(_) => r,
        };
        let s = c.to_string_lossy().to_lowercase();
        if !seen.contains(&s) {
            seen.push(s);
            unique.push(c);
        }
    }
    let mut by_len: Vec<&PathBuf> = unique.iter().collect();
    by_len.sort_by_key(|p| p.to_string_lossy().len());
    let mut kept: Vec<String> = Vec::new();
    for p in by_len.iter() {
        let s = p.to_string_lossy().to_lowercase();
        if !kept
            .iter()
            .any(|k| s == *k || s.starts_with(&format!("{}/", k)))
        {
            kept.push(s);
        }
    }
    unique
        .into_iter()
        .filter(|p| kept.contains(&p.to_string_lossy().to_lowercase()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tilde_variants() {
        let home = PathBuf::from("/home/u");
        assert_eq!(expand_tilde("~", &home), PathBuf::from("/home/u"));
        assert_eq!(
            expand_tilde("~/.claude/projects", &home),
            PathBuf::from("/home/u/.claude/projects")
        );
        assert_eq!(expand_tilde("/abs/path", &home), PathBuf::from("/abs/path"));
    }
}
