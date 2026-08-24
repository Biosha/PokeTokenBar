//! Save transfer for device migration — the port of the macOS `SaveTransfer`.
//!
//! The state is exported inside an *envelope* rather than written raw: `CompanionState`
//! decoding is deliberately lenient (`migrate()` clamps and repairs corrupt fields so one
//! broken field never nukes the whole dex), which means **any JSON decodes "successfully"**.
//! Without the envelope, importing a foreign JSON would report success and leave an empty
//! dex — the user would read that as "the app deleted my progress". The envelope's
//! `format`/`schema` fields carry no defaults, so they reject that misread first.

use crate::companion::{self, CompanionState};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Envelope format id — the file-identity marker.
pub const FORMAT_ID: &str = "poketokenbar.save";
/// Envelope schema version this build speaks.
pub const SCHEMA_VERSION: u32 = 1;
/// File size cap (bytes). A real save is a few KB; 8 MB is far beyond that and keeps a
/// pathologically large file from stalling the UI thread while parsing.
pub const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
/// Pre-import backups kept in the state dir (oldest pruned first).
pub const BACKUPS_TO_KEEP: usize = 5;
const BACKUP_PREFIX: &str = "companion-state.pre-import-";

/// The export envelope: identity metadata around the actual state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveEnvelope {
    pub format: String,
    pub schema: u32,
    pub app_version: String,
    /// ISO 8601 UTC — stored as a string so the file stays human-readable without a
    /// chrono serde feature (a user should be able to open the save and see what moves).
    pub exported_at: String,
    pub source_device: String,
    pub state: CompanionState,
}

/// Header-only view: readable even when the body's schema is newer than this build, so the
/// "newer version" guidance can be precise instead of a generic "not a save".
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveHeader {
    format: String,
    schema: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveTransferError {
    /// Not our format at all (or corrupt beyond our schema).
    NotASaveFile,
    /// The save was made by a newer build than this one.
    NewerSchema { found: u32, supported: u32 },
    /// Too large to be a save — parsing refused before it happens.
    FileTooLarge { bytes: usize, limit: usize },
}

impl std::fmt::Display for SaveTransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotASaveFile => write!(f, "not a PokeTokenBar save file"),
            Self::NewerSchema { found, supported } => write!(
                f,
                "save schema {found} is newer than this build ({supported})"
            ),
            Self::FileTooLarge { bytes, limit } => {
                write!(f, "file is {bytes} bytes, over the {limit} byte save limit")
            }
        }
    }
}

impl std::error::Error for SaveTransferError {}

/// A summary of what an import would replace — the confirmation dialog shows numbers,
/// because "what exactly gets replaced" is more useful than a bare "are you sure?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveSummary {
    pub dex_count: usize,
    pub lifetime_tokens: i64,
}

impl SaveSummary {
    pub fn of(state: &CompanionState) -> Self {
        Self {
            // The lean Pokédex: distinct base species ever hatched.
            dex_count: state.dex.len(),
            lifetime_tokens: state.used_since_install,
        }
    }
}

/// Suggested export file name — dated, so repeated exports never overwrite each other.
pub fn suggested_filename(now: &DateTime<Utc>) -> String {
    format!("PokeTokenBar-Save-{}.json", now.format("%Y-%m-%d"))
}

fn backup_filename(now: &DateTime<Utc>) -> String {
    format!("{BACKUP_PREFIX}{}.json", now.format("%Y-%m-%d-%H%M%S"))
}

/// Encode a state into an export envelope (pretty-printed: a save file should be
/// readable when a user opens it to check what is moving).
pub fn encode(
    state: &CompanionState,
    app_version: &str,
    device: &str,
    now: &DateTime<Utc>,
) -> anyhow::Result<Vec<u8>> {
    let envelope = SaveEnvelope {
        format: FORMAT_ID.to_string(),
        schema: SCHEMA_VERSION,
        app_version: app_version.to_string(),
        exported_at: now.to_rfc3339(),
        source_device: device.to_string(),
        state: state.clone(),
    };
    let mut bytes = serde_json::to_string_pretty(&envelope)?;
    bytes.push('\n');
    Ok(bytes.into_bytes())
}

/// Size gate, split out so it is testable without allocating the cap.
fn size_error(bytes: usize) -> Option<SaveTransferError> {
    (bytes > MAX_FILE_BYTES).then_some(SaveTransferError::FileTooLarge {
        bytes,
        limit: MAX_FILE_BYTES,
    })
}

/// Decode and validate an export envelope. The returned state has already passed the
/// trust-boundary normalization (`migrate`): the save arrives from *outside* the app
/// (hand-edits, a transfer copy, a different build), and the lenient decoder would
/// otherwise let arithmetic-breaking values through.
pub fn decode(data: &[u8]) -> Result<CompanionState, SaveTransferError> {
    if let Some(err) = size_error(data.len()) {
        return Err(err);
    }
    // Header first — the body may be unreadable (newer schema) and we still want to
    // name the exact problem.
    let header: SaveHeader =
        serde_json::from_slice(data).map_err(|_| SaveTransferError::NotASaveFile)?;
    if header.format != FORMAT_ID {
        return Err(SaveTransferError::NotASaveFile);
    }
    if header.schema > SCHEMA_VERSION {
        return Err(SaveTransferError::NewerSchema {
            found: header.schema,
            supported: SCHEMA_VERSION,
        });
    }
    let envelope: SaveEnvelope =
        serde_json::from_slice(data).map_err(|_| SaveTransferError::NotASaveFile)?;
    let mut state = envelope.state;
    state.migrate();
    Ok(state)
}

/// Rebase an imported state against this device (port of `rebasedForThisDevice`).
///
/// State fields split into three classes from the importing device's viewpoint:
///  - **progress** (dex, companion, guarantees, lifetime counters) → carried over as-is;
///  - **local ledger** (`last_day`/`day_applied`) → *this* device's day reconciliation is
///    dropped so it re-reconciles today's usage from its own zero (`day_delta` handles a
///    foreign or empty `last_day` by resetting the day counter);
///  - **device setting** (`language`) → this device keeps its own.
///
/// The account-wide `candy_grant_tier` ledger is merged per-key by **max**, not replaced:
/// the window keys are account-scoped, so a plain replace would forget an already-granted
/// tier and let the candy be re-granted.
pub fn rebase(imported: CompanionState, current: &CompanionState) -> CompanionState {
    let mut state = imported;
    state.language = current.language.clone();
    for (key, &tier) in &current.candy_grant_tier {
        match state.candy_grant_tier.get_mut(key) {
            Some(existing) => *existing = (*existing).max(tier),
            None => {
                state.candy_grant_tier.insert(key.clone(), tier);
            }
        }
    }
    state.candy_feature_seeded = state.candy_feature_seeded || current.candy_feature_seeded;
    state.last_day.clear();
    state.day_applied = 0;
    state
}

/// Back up the current state file before an import overwrites it. Every import gets a new
/// slot: keeping only one backup would let a second import clobber the very file the first
/// import promised to keep for rollback. Returns `None` when there is no state yet.
pub fn backup_current(now: &DateTime<Utc>) -> anyhow::Result<Option<PathBuf>> {
    let Some(src) = companion::state_path() else {
        return Ok(None);
    };
    if !src.exists() {
        return Ok(None);
    }
    let dir = src
        .parent()
        .ok_or_else(|| anyhow::anyhow!("state file has no parent directory"))?;
    let dst = dir.join(backup_filename(now));
    fs::copy(&src, &dst)?;
    prune_backups(dir)?;
    Ok(Some(dst))
}

/// Drop the oldest pre-import backups beyond [`BACKUPS_TO_KEEP`].
pub fn prune_backups(dir: &Path) -> anyhow::Result<()> {
    let mut backups: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_backup_file(p))
        .collect();
    // The timestamp is in the name, so lexicographic order is chronological order.
    backups.sort();
    let excess = backups.len().saturating_sub(BACKUPS_TO_KEEP);
    for old in backups.into_iter().take(excess) {
        let _ = fs::remove_file(old);
    }
    Ok(())
}

fn is_backup_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with(BACKUP_PREFIX) && n.ends_with(".json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with_dex() -> CompanionState {
        // Two known slugs so the dex count is non-trivial.
        let mut s = CompanionState {
            dex: vec!["charmander".to_string(), "squirtle".to_string()],
            used_since_install: 12_345_678,
            spent_tokens: 100,
            language: "ja".to_string(),
            ..CompanionState::default()
        };
        s.candy_grant_tier.insert("claude-5h".to_string(), 2);
        s
    }

    #[test]
    fn roundtrip_preserves_state() {
        let state = state_with_dex();
        let bytes = encode(&state, "0.1.0", "test-host", &Utc::now()).expect("encode");
        let decoded = decode(&bytes).expect("decode");
        assert_eq!(decoded.dex, state.dex);
        assert_eq!(decoded.used_since_install, state.used_since_install);
        assert_eq!(decoded.language, state.language);
        assert_eq!(decoded.candy_grant_tier, state.candy_grant_tier);
    }

    #[test]
    fn rejects_foreign_json() {
        let foreign = br#"{"format":"something.else","schema":1,"state":{}}"#;
        assert!(matches!(
            decode(foreign),
            Err(SaveTransferError::NotASaveFile)
        ));
    }

    #[test]
    fn rejects_non_json() {
        assert!(matches!(
            decode(b"not json at all"),
            Err(SaveTransferError::NotASaveFile)
        ));
    }

    #[test]
    fn rejects_newer_schema_with_exact_message() {
        let state = state_with_dex();
        let bytes = encode(&state, "0.1.0", "test-host", &Utc::now()).unwrap();
        // Bump the schema the way a future build would.
        let newer = std::str::from_utf8(&bytes)
            .unwrap()
            .replace(r#""schema": 1"#, r#""schema": 99"#);
        assert!(matches!(
            decode(newer.as_bytes()),
            Err(SaveTransferError::NewerSchema {
                found: 99,
                supported
            }) if supported == SCHEMA_VERSION
        ));
    }

    #[test]
    fn rejects_oversized_files_before_parsing() {
        assert!(
            size_error(MAX_FILE_BYTES).is_none(),
            "at the cap is allowed"
        );
        assert!(matches!(
            size_error(MAX_FILE_BYTES + 1),
            Some(SaveTransferError::FileTooLarge { bytes, limit })
                if bytes == MAX_FILE_BYTES + 1 && limit == MAX_FILE_BYTES
        ));
    }

    #[test]
    fn decode_sanitizes_untrusted_values() {
        // Hand-built envelope with out-of-bounds counters: the trust boundary must clamp.
        // 9e18 fits i64 but is far past the token clamp ceiling.
        let envelope = r#"{
            "format": "poketokenbar.save",
            "schema": 1,
            "appVersion": "0.0.1",
            "exportedAt": "2026-01-01T00:00:00Z",
            "sourceDevice": "lab",
            "state": {
                "usedSinceInstall": -5,
                "spentTokens": 9000000000000000000,
                "lineKey": "charmander",
                "eggTier": "uncommon"
            }
        }"#;
        let decoded = decode(envelope.as_bytes()).expect("decode");
        assert_eq!(decoded.used_since_install, 0, "negative clamps to 0");
        // An active companion cannot carry an egg guarantee (it would leak into the
        // next egg as a permanent premium) — the sanitizer drops it.
        assert_eq!(decoded.line_key, "charmander");
        assert_eq!(decoded.egg_tier, None);
    }

    #[test]
    fn rebase_keeps_device_language_and_merges_ledger() {
        let mut imported = state_with_dex();
        imported
            .candy_grant_tier
            .insert("codex-week".to_string(), 3);
        imported.last_day = "2020-01-01".to_string();
        imported.day_applied = 42;

        let mut current = CompanionState {
            language: "ko".to_string(),
            ..CompanionState::default()
        };
        current.candy_grant_tier.insert("claude-5h".to_string(), 5);
        current.candy_feature_seeded = true;

        let rebased = rebase(imported, &current);
        assert_eq!(rebased.language, "ko", "device language wins");
        // Per-key max: 5 from current beats 2 from imported; the imported-only key stays.
        assert_eq!(rebased.candy_grant_tier.get("claude-5h"), Some(&5));
        assert_eq!(rebased.candy_grant_tier.get("codex-week"), Some(&3));
        assert!(rebased.candy_feature_seeded);
        assert!(rebased.last_day.is_empty(), "local ledger resets");
        assert_eq!(rebased.day_applied, 0);
        // Progress carried over untouched.
        assert_eq!(rebased.dex.len(), 2);
    }

    #[test]
    fn summary_counts_dex_and_lifetime() {
        let state = state_with_dex();
        let summary = SaveSummary::of(&state);
        assert_eq!(summary.dex_count, 2);
        assert_eq!(summary.lifetime_tokens, 12_345_678);
    }

    #[test]
    fn backup_roundtrip_and_pruning() {
        let dir = std::env::temp_dir().join(format!(
            "ptb-save-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        // Fake a pre-existing state file in the PTB state dir? No — backup_current reads
        // the real companion state path. Instead test the pure pieces: names + pruning.
        let now = Utc::now();
        let name = backup_filename(&now);
        assert!(is_backup_file(&dir.join(&name)));
        assert!(!is_backup_file(&dir.join("companion-state.json")));

        for i in 0..7u32 {
            let f = format!("{BACKUP_PREFIX}2026-01-0{i:01}-00000{i:01}.json");
            fs::write(dir.join(&f), "{}").unwrap();
        }
        prune_backups(&dir).unwrap();
        let remaining = fs::read_dir(&dir).unwrap().count();
        assert_eq!(remaining, BACKUPS_TO_KEEP, "oldest backups pruned");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn suggested_name_is_dated() {
        let now = DateTime::parse_from_rfc3339("2026-08-22T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            suggested_filename(&now),
            "PokeTokenBar-Save-2026-08-22.json"
        );
    }
}
