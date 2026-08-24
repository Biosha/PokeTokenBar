//! App configuration (the port of the macOS `UserDefaults` layer). Kept intentionally small
//! for Phase 1; the companion/i18n/limits settings land with Phases 1b–2.

use crate::paths;
use chrono::Weekday;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    /// UI language: "en" | "ko" | "ja" | "es". The full localization table arrives with the UI.
    pub language: String,
    /// First weekday of the week window. macOS follows the user locale; Unix default is Monday.
    pub first_weekday_is_monday: bool,
    /// Usage refresh interval in minutes (1–15), used by the Phase 2 app loop.
    pub refresh_minutes: u32,
    pub show_cost: bool,
    pub show_limit: bool,
    pub show_took: bool,
    /// Floating desktop pet (opt-in overlay showing the companion sprite).
    pub floating_pet_enabled: bool,
    /// Pet sprite size in px (clamped to 48–160 by the UI).
    pub floating_pet_size: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: "en".into(),
            first_weekday_is_monday: true,
            refresh_minutes: 5,
            show_cost: true,
            show_limit: true,
            show_took: true,
            floating_pet_enabled: false,
            floating_pet_size: 96,
        }
    }
}

impl Config {
    pub fn path() -> Option<PathBuf> {
        paths::config_file()
    }

    /// Load from disk, falling back to defaults for a missing/corrupt file.
    pub fn load() -> Config {
        match paths::config_file().and_then(|p| fs::read_to_string(p).ok()) {
            Some(text) => serde_json::from_str(&text).unwrap_or_default(),
            None => Config::default(),
        }
    }

    /// Atomic save (write temp, then rename) under the data dir.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = paths::config_file().ok_or_else(|| anyhow::anyhow!("no data dir"))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn first_weekday(&self) -> Weekday {
        if self.first_weekday_is_monday {
            Weekday::Mon
        } else {
            Weekday::Sun
        }
    }
}
