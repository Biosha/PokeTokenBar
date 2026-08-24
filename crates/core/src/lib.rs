//! Portable core of PokeTokenBar — token-usage readers + aggregation, no UI.
//!
//! This crate is deliberately UI- and platform-agnostic: the same code backs the headless
//! CLI (Phase 1) and the tray + GTK application (Phase 2).

pub mod aggregate;
pub mod autostart;
pub mod companion;
pub mod config;
pub mod cost;
pub mod entry;
pub mod fsutil;
pub mod i18n;
pub mod iso8601;
pub mod limits;
pub mod nature;
pub mod paths;
mod pokeapi;
pub mod pool;
pub mod pool_gen;
pub mod provider;
pub mod providers;
pub mod save_transfer;
pub mod sprite;
pub mod sqld;
pub mod types;
pub mod usage_cache;
pub mod usage_store;
pub mod util;
pub mod windows;

pub use cost::{ModelPricing, ModelRate};
pub use entry::{Bucket, Entry};
pub use limits::{
    ClaudeLimitsProvider, CodexLimitsProvider, CodexRateLimitStatus, LimitStatus, LimitsError,
};
pub use provider::{ProviderCtx, UsageProvider};
pub use types::{BlockUsage, DailyUsage, PeriodUsage, ProviderSnapshot, UsageSnapshot};
pub use usage_store::build_snapshot;
