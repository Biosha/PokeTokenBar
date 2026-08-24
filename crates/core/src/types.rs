//! Public data structures shared by the CLI, tests, and (Phase 2) the UI.
//! JSON field names are camelCase to stay consistent with the macOS app's models.

use serde::Serialize;

/// Totals for a single local day.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyUsage {
    /// `yyyy-MM-dd`.
    pub date: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cache_read_tokens: i64,
    pub total_tokens: i64,
    pub total_cost: f64,
}

/// Totals for a week (`period` = `yyyy-MM-dd` of the week start) or month (`yyyy-MM`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeriodUsage {
    pub period: String,
    pub total_tokens: i64,
    pub total_cost: f64,
}

/// A rolling ~5-hour window used for burn-rate and the "active block" bar.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockUsage {
    pub id: String,
    /// RFC-3333 strings.
    pub start_time: String,
    pub end_time: String,
    pub is_active: bool,
    pub total_tokens: i64,
    pub cost_usd: f64,
    pub tokens_per_minute: Option<f64>,
}

/// Everything the UI/CLI shows about one provider for a moment in time.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSnapshot {
    pub provider_id: String,
    pub display_name: String,
    pub reports_cost: bool,
    pub today: Option<DailyUsage>,
    pub active_block: Option<BlockUsage>,
    pub week_total: Option<PeriodUsage>,
    pub month_total: Option<PeriodUsage>,
    /// RFC-3333 instant this snapshot was computed.
    pub fetched_at: String,
}

/// The whole refresh: every detected provider plus the combined today total.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub generated_at: String,
    pub combined_today: Option<DailyUsage>,
    pub providers: Vec<ProviderSnapshot>,
}
