//! Filesystem locations. XDG-based on Linux; the companion state dir honours `PTB_STATE_DIR`
//! (mirrors the macOS app's `PTB_STATE_DIR` override for tests/diagnostics).

use std::path::PathBuf;

/// OS home directory (`HOME`).
pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Where companion state + config live. `$PTB_STATE_DIR` (non-empty) else
/// `$XDG_DATA_HOME/PokeTokenBar` else `~/.local/share/PokeTokenBar`.
pub fn data_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("PTB_STATE_DIR") {
        if !dir.trim().is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::data_dir().map(|d| d.join("PokeTokenBar"))
}

/// Where fetched sprites are cached.
pub fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("PokeTokenBar"))
}

/// The companion state file (created by Phase 1b).
pub fn state_file() -> Option<PathBuf> {
    data_dir().map(|d| d.join("companion-state.json"))
}

/// Config file.
pub fn config_file() -> Option<PathBuf> {
    data_dir().map(|d| d.join("config.json"))
}
