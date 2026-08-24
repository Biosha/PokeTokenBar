//! The provider extension point (port of the `UsageProvider` protocol) plus the context passed
//! to every provider and the registry of installed providers.
//!
//! Convention carried over from the macOS app (see `docs/reference/provider-extension.md`):
//! **each provider owns its root discovery**, including per-OS roots and its own override
//! env var. No generic `== "claude_code"` branch may live in shared code.

use crate::entry::Entry;
use chrono::{DateTime, FixedOffset, Local, Utc};
use std::collections::HashMap;
use std::path::PathBuf;

/// Override variables providers may read, resolved once up front (the port of `UsageEnvironment`).
/// A CLI inherits the shell environment, so no login-shell lookup (a GUI-only concern) is needed.
pub const OVERRIDE_VARS: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    "OPENCODE_DATA_DIR",
    "HERMES_HOME",
    "COPILOT_HOME",
    "GROK_HOME",
    "KIRO_CLI_HOME",
    "CURSOR_DATA_DIR",
];

/// Shared inputs handed to every provider.
#[derive(Clone)]
pub struct ProviderCtx {
    /// OS home directory.
    pub home: PathBuf,
    /// Resolved override vars (empty map when none set).
    pub env: HashMap<String, String>,
    /// Local timezone offset at startup, applied to local-day bucketing.
    pub tz: FixedOffset,
}

impl ProviderCtx {
    /// Build from the current process environment and system home.
    pub fn system() -> Self {
        let home = crate::paths::home();
        let mut env = HashMap::new();
        for key in OVERRIDE_VARS {
            if let Ok(v) = std::env::var(key) {
                if !v.trim().is_empty() {
                    env.insert((*key).to_string(), v);
                }
            }
        }
        let tz = *Local::now().offset();
        Self { home, env, tz }
    }

    /// A deterministic context for tests (pinned home + offset, no env).
    pub fn for_test(home: PathBuf, tz: FixedOffset) -> Self {
        Self {
            home,
            env: HashMap::new(),
            tz,
        }
    }

    pub fn var(&self, key: &str) -> Option<String> {
        self.env.get(key).cloned().filter(|s| !s.trim().is_empty())
    }
}

/// A token-usage source. Providers are cheap, stateless singletons.
pub trait UsageProvider: Send {
    /// Stable id (e.g. `"claude_code"`), used in state and per-provider ledgers.
    fn id(&self) -> &'static str;
    /// Human name shown in the UI.
    fn display_name(&self) -> &'static str;
    /// Whether this provider contributes cost (flat-rate subscriptions report tokens only).
    fn reports_cost(&self) -> bool {
        true
    }
    /// Whether the data source is present. When false the provider is skipped in the snapshot.
    fn available(&self, _ctx: &ProviderCtx) -> bool {
        true
    }
    /// Read normalized entries created/modified since `since` (UTC).
    fn read_entries(&self, ctx: &ProviderCtx, since: DateTime<Utc>) -> anyhow::Result<Vec<Entry>>;
}

/// All installed providers, in display order.
pub fn all() -> Vec<Box<dyn UsageProvider>> {
    vec![
        Box::new(crate::providers::claude::ClaudeProvider),
        Box::new(crate::providers::codex::CodexProvider),
        Box::new(crate::providers::gemini::GeminiProvider),
        Box::new(crate::providers::grok::GrokProvider),
        Box::new(crate::providers::opencode::OpenCodeProvider),
        Box::new(crate::providers::hermes::HermesProvider),
        Box::new(crate::providers::cursor::CursorProvider),
        Box::new(crate::providers::copilot::CopilotProvider),
        Box::new(crate::providers::kiro::KiroProvider),
        Box::new(crate::providers::antigravity::AntigravityProvider),
    ]
}
