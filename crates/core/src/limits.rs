//! Official usage limits: Claude via the OAuth usage endpoint, Codex via `codex app-server`.
//!
//! Faithful port of the macOS `OAuthLimitsProvider` / `CodexRateLimitsProvider` and limit
//! models (`Sources/PokeTokenBar/Core/`). All response parsing is factored into pure
//! functions ([`parse_claude_response`], [`extract_codex_result`], the credential helpers)
//! so it is unit-testable offline; the providers do only the I/O (one HTTPS GET, one
//! subprocess). Display concerns (used-vs-remaining mode, alert tiers, localization) stay
//! in the CLI/layer that consumes these types — both `utilization` (Claude) and
//! `used_percent` (Codex) are **percent used**, 0–100.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The Claude usage endpoint (unofficial but stable; the macOS app uses the same URL).
pub const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// `anthropic-beta` header required by the usage endpoint.
const ANTHROPIC_BETA: &str = "oauth-2025-04-20";
/// Connect+read timeout for the usage request (macOS uses 15s).
const CLAUDE_USAGE_TIMEOUT: Duration = Duration::from_secs(15);
/// Max wait for the `codex app-server` rate-limits reply (macOS uses 20s).
const CODEX_RPC_TIMEOUT: Duration = Duration::from_secs(20);
/// Response id of the `account/rateLimits/read` request.
const CODEX_RESPONSE_ID: i64 = 1;

// MARK: - Errors

/// Errors from either limits provider (port of the macOS `LimitsError` + `RunnerError`).
#[derive(Debug, thiserror::Error)]
pub enum LimitsError {
    /// No readable Claude credentials file at any candidate path.
    #[error("no Claude credentials file found at {path} — log in with `claude` first")]
    NoCredentials { path: PathBuf },
    /// Valid JSON, but no `claudeAiOauth` object (MCP-server-only credentials, observed on
    /// Claude Code 2.1.x) — re-login is the fix, so it is a distinct case from a format error.
    #[error(
        "Claude credentials at {path} hold no account OAuth (claudeAiOauth) — re-login with `claude`"
    )]
    CredentialMissingAccountOAuth { path: PathBuf },
    /// Unparseable JSON, or a `claudeAiOauth` object without a usable access token.
    #[error("Claude credentials at {path} are malformed (no usable access token)")]
    CredentialFormat { path: PathBuf },
    /// credentials file read fine but the token is expired; there is no silent refresh here.
    #[error("Claude OAuth token in {path} has expired — re-login with `claude`")]
    CredentialExpired { path: PathBuf },
    /// Non-200 status other than 429 from the usage endpoint.
    #[error("Claude usage endpoint returned HTTP {status}")]
    HttpStatus { status: u16 },
    /// 429 — the server-designated Retry-After (seconds), or `None` when the header was
    /// absent/unparseable (caller falls back to its default backoff).
    #[error(
        "Claude usage endpoint rate-limited (HTTP 429) — retry after {}s",
        match retry_after {
            Some(s) => format!("{s}"),
            None => "unknown".to_string(),
        }
    )]
    RateLimited { retry_after: Option<f64> },
    /// Transport-level failure (DNS, TLS, timeout, connection refused).
    #[error("Claude usage request failed: {source}")]
    Http {
        #[source]
        source: Box<ureq::Transport>,
    },
    /// 200 body that does not decode as [`LimitStatus`].
    #[error("Claude usage response is not valid JSON: {0}")]
    ResponseJson(String),
    /// `codex app-server` could not be spawned.
    #[error("failed to spawn codex app-server {binary}: {source}")]
    CodexSpawn {
        binary: String,
        #[source]
        source: std::io::Error,
    },
    /// The temp stdout/stderr capture file for the exchange could not be created.
    #[error("failed to create a temp output file for codex app-server {binary}: {source}")]
    CodexOutputFile {
        binary: String,
        #[source]
        source: std::io::Error,
    },
    /// Timed out waiting for the rate-limits reply.
    #[error("codex app-server ({binary}) timed out waiting for the rate-limits reply")]
    CodexTimeout { binary: String },
    /// The process exited non-zero before replying.
    #[error("codex app-server ({binary}) exited with status {code}")]
    CodexNonZeroExit { code: i32, binary: String },
    /// The process exited 0 (or the reply never matched) without a usable `id: 1` response.
    #[error("codex app-server ({binary}) produced no rate-limits response")]
    CodexMissingResponse { binary: String },
    /// The server answered with a JSON-RPC error object.
    #[error("codex app-server JSON-RPC error: {message}")]
    CodexRpcError { message: String },
}

// MARK: - Claude models (port of `Models.swift` "OAuth limits" section)

/// One limit window from `GET https://api.anthropic.com/api/oauth/usage`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct LimitWindow {
    /// Percent **used** (0–100), not remaining.
    #[serde(default)]
    pub utilization: Option<f64>,
    /// RFC-3339 reset instant (milli- or microsecond precision).
    #[serde(rename = "resets_at", default)]
    pub resets_at: Option<String>,
}

impl LimitWindow {
    pub fn reset_date(&self) -> Option<DateTime<Utc>> {
        self.resets_at.as_deref().and_then(crate::iso8601::parse)
    }
}

/// New-style `limits[]` entry — generalizes the legacy `five_hour`/`seven_day` fields.
/// Model-scoped weekly limits (`kind = "weekly_scoped"`) arrive only here, keyed by
/// `scope.model.display_name`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct OAuthLimitEntry {
    /// `session` (= five_hour) | `weekly_all` (= seven_day) | `weekly_scoped` | …
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
    /// Percent used (0–100).
    #[serde(default)]
    pub percent: Option<f64>,
    #[serde(default)]
    pub severity: Option<String>,
    #[serde(rename = "resets_at", default)]
    pub resets_at: Option<String>,
    #[serde(default)]
    pub scope: Option<LimitEntryScope>,
    #[serde(rename = "is_active", default)]
    pub is_active: Option<bool>,
}

impl OAuthLimitEntry {
    pub fn reset_date(&self) -> Option<DateTime<Utc>> {
        self.resets_at.as_deref().and_then(crate::iso8601::parse)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct LimitEntryScope {
    #[serde(default)]
    pub model: Option<LimitEntryModel>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct LimitEntryModel {
    #[serde(rename = "display_name", default)]
    pub display_name: Option<String>,
}

/// The decoded usage response. `subscription_type`/`rate_limit_tier` are NOT in the HTTP
/// response — [`ClaudeLimitsProvider::fetch`] injects them from the credentials (mirrors the
/// macOS `planInfo()` injection).
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
pub struct LimitStatus {
    #[serde(rename = "five_hour")]
    pub five_hour: Option<LimitWindow>,
    #[serde(rename = "seven_day")]
    pub seven_day: Option<LimitWindow>,
    /// Legacy per-model weekly fields; nulled out in newer responses (use `limits` instead).
    #[serde(rename = "seven_day_opus")]
    pub seven_day_opus: Option<LimitWindow>,
    #[serde(rename = "seven_day_sonnet")]
    pub seven_day_sonnet: Option<LimitWindow>,
    #[serde(default)]
    pub limits: Option<Vec<OAuthLimitEntry>>,
    /// In plan tier (`pro`/`max`/`free`), from the OAuth credentials.
    #[serde(default, skip_deserializing)]
    pub subscription_type: Option<String>,
    /// Rate-limit tier (e.g. `default_claude_max_20x`), from the OAuth credentials.
    #[serde(default, skip_deserializing)]
    pub rate_limit_tier: Option<String>,
}

/// Extract the multiplier token (`"20x"`/`"5x"`) from a rate-limit tier name; `None` when the
/// tier has none (`default_claude_pro` → name only).
pub fn tier_multiplier(tier: &str) -> Option<String> {
    for part in tier.split('_') {
        if part.len() < 2 || !part.ends_with('x') {
            continue;
        }
        let digits = &part[..part.len() - 1];
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return Some(part.to_string());
        }
    }
    None
}

impl LimitStatus {
    /// `subscription_type` + `rate_limit_tier` combined for display, e.g. `max` +
    /// `default_claude_max_20x` → `"Max 20x"`; no multiplier token → tier name alone
    /// (`"Pro"`); no subscription info → `None`.
    pub fn plan_display(&self) -> Option<String> {
        let sub = self.subscription_type.as_deref()?.trim();
        if sub.is_empty() {
            return None;
        }
        let base = capitalize_first(sub);
        if let Some(tier) = self.rate_limit_tier.as_deref() {
            if let Some(mult) = tier_multiplier(tier) {
                return Some(format!("{base} {mult}"));
            }
        }
        Some(base)
    }

    /// Windows the UI must show **in addition to** the legacy rows: `session` (five_hour) and
    /// `weekly_all` (seven_day) are already covered by the legacy fields, so only the rest
    /// (e.g. `weekly_scoped`) qualify — unless the legacy fields are all absent (new-style
    /// response), in which case the whole `limits` list is the display set.
    pub fn scoped_limit_entries(&self) -> Vec<OAuthLimitEntry> {
        let entries = self.limits.as_deref().unwrap_or_default();
        if self.five_hour.is_none() && self.seven_day.is_none() {
            return entries.to_vec();
        }
        entries
            .iter()
            .filter(|e| {
                e.kind.as_deref() != Some("session") && e.kind.as_deref() != Some("weekly_all")
            })
            .cloned()
            .collect()
    }
}

fn capitalize_first(s: &str) -> String {
    match s.chars().next() {
        Some(c) => {
            let mut out: String = c.to_uppercase().collect();
            out.extend(s.chars().skip(1));
            out
        }
        None => String::new(),
    }
}

/// Parse a 200-body from the usage endpoint (pure).
pub fn parse_claude_response(json: &str) -> Result<LimitStatus, LimitsError> {
    serde_json::from_str(json).map_err(|e| LimitsError::ResponseJson(e.to_string()))
}

// MARK: - Claude credentials (port of `OAuthCredentialData`)

/// An extracted Claude OAuth credential from `~/.claude/.credentials.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct Credential {
    pub access_token: String,
    /// Normalized to epoch seconds (the file may carry seconds or milliseconds).
    pub expires_at: Option<DateTime<Utc>>,
    pub subscription_type: Option<String>,
    pub rate_limit_tier: Option<String>,
}

impl Credential {
    /// Swift semantics: expired when within 60s of expiry (a little early so the token is
    /// not used on its final second); no expiry → never expired.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at
            .is_some_and(|e| e <= now + ChronoDuration::seconds(60))
    }
}

/// Valid JSON with a `claudeAiOauth` object (a non-null, non-missing dictionary) but no
/// usable token inside, or broken JSON. Mirrors `OAuthCredentialData.credential(from:)`.
pub fn credential_from(data: &str) -> Option<Credential> {
    let json: serde_json::Value = serde_json::from_str(data).ok()?;
    let oauth = json.get("claudeAiOauth")?;
    if !oauth.is_object() {
        return None;
    }
    let token = oauth
        .get("accessToken")
        .and_then(serde_json::Value::as_str)?;
    if token.is_empty() {
        return None;
    }
    Some(Credential {
        access_token: token.to_string(),
        expires_at: credential_expires_at(oauth.get("expiresAt")),
        subscription_type: oauth
            .get("subscriptionType")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        rate_limit_tier: oauth
            .get("rateLimitTier")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

/// Valid JSON whose top level is an object but whose `claudeAiOauth` is missing or `null`
/// (the logged-out state) — i.e. a re-login case, not a format error. Broken JSON or a
/// top-level array are `false` (mirrors the Swift `as? [String: Any]` casts).
pub fn is_account_oauth_missing(data: &str) -> bool {
    let json = match serde_json::from_str::<serde_json::Value>(data) {
        Ok(j) if j.is_object() => j,
        _ => return false,
    };
    !json
        .get("claudeAiOauth")
        .is_some_and(serde_json::Value::is_object)
}

/// `expiresAt` may be a number (seconds or milliseconds) or a numeric string; `<= 0` →
/// `None`. Values above 10e9 are treated as milliseconds (mirrors the Swift heuristic).
fn credential_expires_at(raw: Option<&serde_json::Value>) -> Option<DateTime<Utc>> {
    let value: f64 = match raw? {
        serde_json::Value::Number(n) => n.as_f64()?,
        serde_json::Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let seconds = if value > 10_000_000_000.0 {
        value / 1000.0
    } else {
        value
    };
    let sec = seconds as i64;
    let micros = ((seconds - sec as f64) * 1_000_000.0).round() as u32;
    if micros == 1_000_000 {
        // Rounding up across the second boundary.
        return DateTime::from_timestamp(sec + 1, 0);
    }
    DateTime::from_timestamp(sec, micros * 1000)
}

/// Retry-After header (seconds form only) → capped seconds, or `None` for absent/date/malformed
/// values (caller uses its default backoff). Overly large values cap at 1 hour.
pub fn retry_after_seconds(raw: &str) -> Option<f64> {
    let seconds: f64 = raw.trim().parse().ok()?;
    if seconds <= 0.0 || !seconds.is_finite() {
        return None;
    }
    Some(seconds.min(3600.0))
}

// MARK: - Claude provider (port of `OAuthLimitsProvider`, file-credentials path only)

pub struct ClaudeLimitsProvider {
    /// Candidate paths in priority order; the first readable one wins.
    credentials_paths: Vec<PathBuf>,
}

impl Default for ClaudeLimitsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeLimitsProvider {
    /// Default construction: `$CLAUDE_CONFIG_DIR` entries (comma list, `~`-expanded) first,
    /// then the stock `~/.claude/.credentials.json`. (The macOS app hard-codes the stock path
    /// because its poller cannot afford a shell lookup; a CLI inherits the env for free, and
    /// Claude Code itself honours `CLAUDE_CONFIG_DIR`.)
    pub fn new() -> Self {
        let home = crate::paths::home();
        let config_dir = std::env::var("CLAUDE_CONFIG_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Self {
            credentials_paths: credential_candidates(&home, config_dir.as_deref()),
        }
    }

    /// Fixed single credentials path (tests/diagnostics).
    pub fn with_credentials_path(path: impl Into<PathBuf>) -> Self {
        Self {
            credentials_paths: vec![path.into()],
        }
    }

    pub fn credentials_paths(&self) -> &[PathBuf] {
        &self.credentials_paths
    }

    /// Read the token, call the usage endpoint, and decorate the result with the plan info
    /// (which travels in the credentials, not the response).
    pub fn fetch(&self) -> Result<LimitStatus, LimitsError> {
        let credential = self.read_credential()?;
        let mut status = self.fetch_with_access_token(&credential.access_token)?;
        status.subscription_type = credential.subscription_type;
        status.rate_limit_tier = credential.rate_limit_tier;
        Ok(status)
    }

    /// The network half of [`fetch`], with an externally supplied token.
    pub fn fetch_with_access_token(&self, access_token: &str) -> Result<LimitStatus, LimitsError> {
        let agent = ureq::AgentBuilder::new()
            .timeout(CLAUDE_USAGE_TIMEOUT)
            .timeout_connect(CLAUDE_USAGE_TIMEOUT)
            .build();
        let outcome = agent
            .get(CLAUDE_USAGE_URL)
            .set("Authorization", &format!("Bearer {access_token}"))
            .set("anthropic-beta", ANTHROPIC_BETA)
            .call();
        match outcome {
            Ok(resp) => parse_claude_response(
                &resp
                    .into_string()
                    .map_err(|e| LimitsError::ResponseJson(format!("body read failed: {e}")))?,
            ),
            Err(ureq::Error::Status(code, resp)) => {
                if code == 429 {
                    let retry_after = resp.header("Retry-After").and_then(retry_after_seconds);
                    Err(LimitsError::RateLimited { retry_after })
                } else {
                    Err(LimitsError::HttpStatus { status: code })
                }
            }
            Err(ureq::Error::Transport(source)) => Err(LimitsError::Http {
                source: Box::new(source),
            }),
        }
    }

    /// First usable candidate file → classified [`Credential`]. A readable-but-unusable file
    /// (malformed / MCP-only / expired) does not mask a later, better candidate — but is the
    /// error reported when no candidate yields a credential (only unreadability falls through
    /// silently to `NoCredentials`).
    fn read_credential(&self) -> Result<Credential, LimitsError> {
        let mut first_error: Option<LimitsError> = None;
        for path in &self.credentials_paths {
            let Ok(data) = std::fs::read_to_string(path) else {
                continue;
            };
            let outcome = if is_account_oauth_missing(&data) {
                Err(LimitsError::CredentialMissingAccountOAuth { path: path.clone() })
            } else {
                match credential_from(&data) {
                    Some(c) if !c.is_expired(Utc::now()) => Ok(c),
                    Some(_) => Err(LimitsError::CredentialExpired { path: path.clone() }),
                    None => Err(LimitsError::CredentialFormat { path: path.clone() }),
                }
            };
            match outcome {
                Ok(c) => return Ok(c),
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        Err(first_error.unwrap_or(LimitsError::NoCredentials {
            path: self
                .credentials_paths
                .first()
                .cloned()
                .unwrap_or_else(|| PathBuf::from("~/.claude/.credentials.json")),
        }))
    }
}

/// Candidate `credentials.json` locations: one per `$CLAUDE_CONFIG_DIR` entry (comma list,
/// `~` expanded), then the stock `~/.claude`.
pub fn credential_candidates(home: &Path, config_dir: Option<&str>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dir) = config_dir {
        for part in dir.split(',') {
            let p = part.trim();
            if !p.is_empty() {
                paths.push(crate::fsutil::expand_tilde(p, home).join(".credentials.json"));
            }
        }
    }
    paths.push(home.join(".claude").join(".credentials.json"));
    // Dedup keeps first (an override dir may alias the stock path).
    let mut out = Vec::new();
    for p in paths {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

// MARK: - Codex models (port of `Models.swift` "Codex app-server rate limits" section)

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CodexRateLimitWindow {
    /// Percent used (0–100). Required in the wire schema.
    #[serde(rename = "usedPercent")]
    pub used_percent: i32,
    /// 300 = the 5-hour session, 10080 = weekly.
    #[serde(rename = "windowDurationMins", default)]
    pub window_duration_mins: Option<i32>,
    /// Unix epoch seconds.
    #[serde(rename = "resetsAt", default)]
    pub resets_at: Option<i64>,
}

impl CodexRateLimitWindow {
    pub fn reset_date(&self) -> Option<DateTime<Utc>> {
        self.resets_at
            .and_then(|ts| DateTime::from_timestamp(ts, 0))
    }

    /// Duration-based label (the macOS app localizes; the core stays English-neutral).
    pub fn display_name(&self) -> String {
        match self.window_duration_mins {
            Some(300) => "5h session".to_string(),
            Some(10_080) => "Weekly".to_string(),
            Some(mins) if mins >= 60 && mins % 60 == 0 => format!("{}h", mins / 60),
            Some(mins) => format!("{mins}m"),
            None => "Limit".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CodexCreditsSnapshot {
    #[serde(rename = "balance", default)]
    pub balance: Option<String>,
    #[serde(rename = "hasCredits")]
    pub has_credits: bool,
    #[serde(rename = "unlimited")]
    pub unlimited: bool,
}

/// Spend-control ("individual") limit — a dollar budget rather than a time window.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CodexSpendControlLimit {
    #[serde(rename = "limit")]
    pub limit: String,
    #[serde(rename = "remainingPercent")]
    pub remaining_percent: i32,
    #[serde(rename = "resetsAt")]
    pub resets_at: i64,
    #[serde(rename = "used")]
    pub used: String,
}

impl CodexSpendControlLimit {
    /// Percent used, clamped to 0–100 (the server may report outside that range).
    pub fn used_percent(&self) -> i32 {
        (100 - self.remaining_percent).clamp(0, 100)
    }

    pub fn reset_date(&self) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(self.resets_at, 0)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CodexRateLimitSnapshot {
    /// Bucket id (`codex`, `codex_other`, …). May be `null` in older responses.
    #[serde(rename = "limitId", default)]
    pub limit_id: Option<String>,
    #[serde(rename = "limitName", default)]
    pub limit_name: Option<String>,
    /// Usually the 5-hour window.
    #[serde(default)]
    pub primary: Option<CodexRateLimitWindow>,
    /// Usually the weekly window.
    #[serde(default)]
    pub secondary: Option<CodexRateLimitWindow>,
    #[serde(default)]
    pub credits: Option<CodexCreditsSnapshot>,
    #[serde(rename = "individualLimit", default)]
    pub individual_limit: Option<CodexSpendControlLimit>,
    #[serde(rename = "planType", default)]
    pub plan_type: Option<String>,
    #[serde(rename = "rateLimitReachedType", default)]
    pub rate_limit_reached_type: Option<String>,
}

impl CodexRateLimitSnapshot {
    pub fn has_visible_limit(&self) -> bool {
        self.primary.is_some() || self.secondary.is_some() || self.individual_limit.is_some()
    }

    /// Bucket label from `limitName`/`limitId` with underscores spaced, e.g. `"codex_other"`
    /// → `"Codex other"`, defaulting to `"Codex"`.
    pub fn bucket_display_name(&self) -> String {
        let raw = self
            .limit_name
            .as_deref()
            .or(self.limit_id.as_deref())
            .unwrap_or("codex");
        capitalize_first(&raw.replace('_', " "))
    }
}

/// The `account/rateLimits/read` result. The top-level `rate_limits` is the `"codex"` bucket;
/// additional buckets (`codex_other`, …) exist only in `rate_limits_by_limit_id`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CodexRateLimitStatus {
    #[serde(rename = "rateLimits")]
    pub rate_limits: CodexRateLimitSnapshot,
    #[serde(rename = "rateLimitsByLimitId", default)]
    pub rate_limits_by_limit_id: Option<std::collections::HashMap<String, CodexRateLimitSnapshot>>,
}

impl CodexRateLimitStatus {
    /// Every bucket — mirrors the Codex TUI's snapshot list: the top-level snapshot first,
    /// then the `rateLimitsByLimitId` entries (sorted by key) minus the duplicates: the
    /// server also stores the no-`limitId` snapshot under the `"codex"` key.
    pub fn snapshots(&self) -> Vec<CodexRateLimitSnapshot> {
        let mut result = vec![self.rate_limits.clone()];
        let Some(by_limit_id) = self.rate_limits_by_limit_id.as_ref() else {
            return result;
        };
        let primary_key = self
            .rate_limits
            .limit_id
            .clone()
            .unwrap_or_else(|| "codex".to_string());
        let mut entries: Vec<(&String, &CodexRateLimitSnapshot)> = by_limit_id.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (limit_id, snapshot) in entries {
            if *limit_id == primary_key {
                continue;
            }
            if let (Some(a), Some(b)) = (
                snapshot.limit_id.as_deref(),
                self.rate_limits.limit_id.as_deref(),
            ) {
                if a == b {
                    continue;
                }
            }
            result.push(snapshot.clone());
        }
        result
    }

    pub fn visible_snapshots(&self) -> Vec<CodexRateLimitSnapshot> {
        self.snapshots()
            .into_iter()
            .filter(|s| s.has_visible_limit())
            .collect()
    }

    pub fn has_visible_limit(&self) -> bool {
        !self.visible_snapshots().is_empty()
    }

    /// Menu/alert headline: the max 5h (primary) usage across all visible buckets.
    pub fn max_primary_used_percent(&self) -> Option<i32> {
        self.visible_snapshots()
            .iter()
            .filter_map(|s| s.primary.as_ref().map(|w| w.used_percent))
            .max()
    }
}

// MARK: - Codex JSON-RPC (port of `ProcessRunner.runJSONRPC` + request lines)

/// The three newline-delimited JSON-RPC lines sent to `codex app-server --stdio`.
/// Pure — split out so the exact wire shape is testable without spawning anything.
pub fn codex_request_lines() -> Vec<String> {
    let initialize = serde_json::json!({
        "method": "initialize",
        "id": 0,
        "params": {
            "clientInfo": {
                "name": "poketokenbar",
                "title": "PokeTokenBar",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "capabilities": {
                "experimentalApi": true,
            },
        },
    });
    vec![
        initialize.to_string(),
        r#"{"method":"initialized","params":{}}"#.to_string(),
        format!(r#"{{"method":"account/rateLimits/read","id":{CODEX_RESPONSE_ID},"params":{{}}}}"#),
    ]
}

/// Decode a `rateLimits` response payload — the `result` field of the JSON-RPC reply
/// (pure). This is the same shape the macOS suite decodes directly.
pub fn parse_codex_response(json: &str) -> Result<CodexRateLimitStatus, LimitsError> {
    serde_json::from_str(json).map_err(|e| LimitsError::ResponseJson(e.to_string()))
}

/// Scan the app-server's stdout (which mixes logs, notifications and responses) for the
/// `id == 1` JSON-RPC reply and decode its `result` (pure). `Ok(None)` when no such reply
/// was seen; `Err(CodexRpcError)` when the server answered with an error object.
pub fn extract_codex_result(text: &str) -> Result<Option<CodexRateLimitStatus>, LimitsError> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(id) = obj.get("id").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        if id != CODEX_RESPONSE_ID {
            continue;
        }
        if let Some(error) = obj.get("error") {
            let message = error
                .get("message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string());
            return Err(LimitsError::CodexRpcError { message });
        }
        if let Some(result) = obj.get("result") {
            let status: CodexRateLimitStatus = serde_json::from_value(result.clone())
                .map_err(|e| LimitsError::ResponseJson(e.to_string()))?;
            return Ok(Some(status));
        }
    }
    Ok(None)
}

// MARK: - Binary discovery (port of `BinaryLocator` for the codex candidate set)

/// Well-known version-manager / package-manager bin dirs, mirroring the macOS
/// `BinaryLocator.commonToolDirectories` (single source for candidate building).
pub fn common_tool_directories(home: &Path) -> Vec<PathBuf> {
    vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".local/share/mise/shims"),
        home.join(".asdf/shims"),
        home.join(".volta/bin"),
        home.join(".bun/bin"),
        home.join(".npm-global/bin"),
        home.join(".local/bin"),
        PathBuf::from("/usr/bin"),
    ]
}

/// Static `codex` candidates in priority order: app-bundle (macOS only), the native
/// installer's `~/.codex/bin/codex`, then one per shared tool directory.
pub fn codex_binary_candidates(home: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from(
            "/Applications/Codex.app/Contents/Resources/codex",
        ));
    }
    candidates.push(home.join(".codex").join("bin").join("codex"));
    candidates.extend(
        common_tool_directories(home)
            .into_iter()
            .map(|d| d.join("codex")),
    );
    candidates
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// First executable candidate, else a PATH scan (a CLI inherits the shell PATH; the macOS
/// app needs a login-shell spawn for this, which a portable core avoids).
pub fn resolve_codex_binary(home: &Path) -> Option<PathBuf> {
    for candidate in codex_binary_candidates(home) {
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("codex");
            if candidate.is_absolute() && is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

// MARK: - Codex provider (port of `CodexRateLimitsProvider`)

pub struct CodexLimitsProvider {
    binary_candidates: Vec<PathBuf>,
    timeout: Duration,
}

impl Default for CodexLimitsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexLimitsProvider {
    /// Default construction: the static [`codex_binary_candidates`] + PATH fallback, 20s
    /// timeout (the macOS app-server exchange budget).
    pub fn new() -> Self {
        let home = crate::paths::home();
        Self {
            binary_candidates: codex_binary_candidates(&home),
            timeout: CODEX_RPC_TIMEOUT,
        }
    }

    /// Fixed binary (tests/diagnostics; skip PATH resolution).
    pub fn with_binary(path: impl Into<PathBuf>) -> Self {
        Self {
            binary_candidates: vec![path.into()],
            timeout: CODEX_RPC_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn resolved_binary(&self) -> Option<PathBuf> {
        let home = crate::paths::home();
        for candidate in &self.binary_candidates {
            if is_executable_file(candidate) {
                return Some(candidate.clone());
            }
        }
        if self.binary_candidates.len() == 1 {
            return None;
        }
        resolve_codex_binary(&home)
    }

    /// `Ok(None)` when no codex binary exists (the macOS equivalent returns nil — limits are
    /// hidden, not errored).
    pub fn fetch(&self) -> Result<Option<CodexRateLimitStatus>, LimitsError> {
        let Some(binary) = self.resolved_binary() else {
            return Ok(None);
        };
        let binary_name = binary
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| binary.display().to_string());
        let stdout = run_codex_app_server(&binary, self.timeout, &binary_name)?;
        match extract_codex_result(&stdout)? {
            Some(status) => Ok(Some(status)),
            None => Err(LimitsError::CodexMissingResponse {
                binary: binary_name,
            }),
        }
    }
}

/// Spawn `codex app-server --stdio`, send the three JSON-RPC lines, and return the full
/// stdout once the reply is seen (or the process exits). Faithful to the macOS runner in the
/// risky detail: stdout/stderr go to **temp files**, not pipes — a hung server (or its
/// children) must never be able to stall us on a blocked read, and a failed run leaves the
/// stderr file's tail in diagnostics instead of a lost pipe.
fn run_codex_app_server(
    binary: &Path,
    timeout: Duration,
    binary_name: &str,
) -> Result<String, LimitsError> {
    let (out_path, err_path) = rpc_output_files();
    let cleanup = |out: &Path, err: &Path| {
        let _ = std::fs::remove_file(out);
        let _ = std::fs::remove_file(err);
    };
    let out_file =
        std::fs::File::create(&out_path).map_err(|source| LimitsError::CodexOutputFile {
            binary: binary_name.to_string(),
            source,
        })?;
    let err_file =
        std::fs::File::create(&err_path).map_err(|source| LimitsError::CodexOutputFile {
            binary: binary_name.to_string(),
            source,
        })?;

    let input = codex_request_lines().join("\n") + "\n";
    let mut child = match std::process::Command::new(binary)
        .arg("app-server")
        .arg("--stdio")
        .stdin(std::process::Stdio::piped())
        .stdout(out_file)
        .stderr(err_file)
        .spawn()
    {
        Ok(c) => c,
        Err(source) => {
            cleanup(&out_path, &err_path);
            return Err(LimitsError::CodexSpawn {
                binary: binary_name.to_string(),
                source,
            });
        }
    };

    // The child may exit before reading stdin (broken pipe) — swallowing EPIPE matches the
    // macOS runner; the poll below then surfaces exit/timeout.
    let mut stdin = child.stdin.take().expect("stdin was piped");
    use std::io::Write;
    let _ = stdin.write_all(input.as_bytes());
    let _ = stdin.flush();
    drop(stdin);

    let deadline = std::time::Instant::now() + timeout;
    let mut exit: Option<std::process::ExitStatus> = None;
    loop {
        // Poll the growing stdout file (the server interleaves logs and notifications, so we
        // scan line-by-line for the id-matched reply).
        if let Ok(raw) = std::fs::read(&out_path) {
            let text = String::from_utf8_lossy(&raw).into_owned();
            match extract_codex_result(&text) {
                Ok(Some(_)) => {
                    let _ = child.kill(); // reply received; don't leave the server running
                    let _ = child.wait();
                    cleanup(&out_path, &err_path);
                    return Ok(text);
                }
                Err(_) => {} // RPC error: report it after the process settles (below)
                Ok(None) => {}
            }
        }
        if exit.is_none() {
            if let Ok(Some(status)) = child.try_wait() {
                exit = Some(status);
            }
        }
        if exit.is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            eprintln!(
                "limits: codex app-server timed out; stderr tail: {}",
                file_tail(&err_path)
            );
            cleanup(&out_path, &err_path);
            return Err(LimitsError::CodexTimeout {
                binary: binary_name.to_string(),
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // The process exited: one final read (a last-second flush lands after the last in-loop
    // poll — the same race the macOS runner re-reads for).
    let stdout_text = std::fs::read_to_string(&out_path).unwrap_or_default();
    match extract_codex_result(&stdout_text) {
        Ok(Some(_status)) => {
            cleanup(&out_path, &err_path);
            return Ok(stdout_text);
        }
        Ok(None) => {}
        Err(e) => {
            cleanup(&out_path, &err_path);
            return Err(e);
        }
    }
    let code = exit.unwrap().code().unwrap_or(-1);
    eprintln!(
        "limits: codex app-server exited {code}; stderr tail: {}",
        file_tail(&err_path)
    );
    cleanup(&out_path, &err_path);
    if code != 0 {
        Err(LimitsError::CodexNonZeroExit {
            code,
            binary: binary_name.to_string(),
        })
    } else {
        Err(LimitsError::CodexMissingResponse {
            binary: binary_name.to_string(),
        })
    }
}

/// Unique temp output files for one app-server exchange (`poketokenbar-<pid>-<n>-<ts>.{out,err}`).
fn rpc_output_files() -> (PathBuf, PathBuf) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let base = std::env::temp_dir().join(format!(
        "poketokenbar-limit-{}-{n}-{ts}",
        std::process::id()
    ));
    (base.with_extension("out"), base.with_extension("err"))
}

/// Last 300 chars of a file's trimmed content ("" when missing/empty) — failure diagnostics.
fn file_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let start = chars.len().saturating_sub(300);
    chars[start..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("ptb-limits-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).expect("mkdir temp dir");
        d
    }

    // ==========================================================================
    // Claude — parse_claude_response (fixtures captured from the macOS suite)
    // ==========================================================================

    #[test]
    fn parses_real_oauth_usage_response() {
        // Real /api/oauth/usage response (2026-06-10): microsecond resets_at, nulled legacy
        // opus field, and unknown keys (seven_day_omelette, extra_usage) that must be ignored.
        let json = r#"{"five_hour":{"utilization":23.0,"resets_at":"2026-06-10T11:10:00.034464+00:00"},
        "seven_day":{"utilization":16.0,"resets_at":"2026-06-14T03:00:01.034496+00:00"},
        "seven_day_opus":null,
        "seven_day_sonnet":{"utilization":0.0,"resets_at":"2026-06-14T03:00:01.034508+00:00"},
        "seven_day_omelette":{"utilization":0.0,"resets_at":null},
        "extra_usage":{"is_enabled":false}}"#;
        let status = parse_claude_response(json).expect("decode");
        assert_eq!(
            status.five_hour.as_ref().and_then(|w| w.utilization),
            Some(23.0)
        );
        assert!(
            status
                .five_hour
                .as_ref()
                .and_then(|w| w.reset_date())
                .is_some(),
            "microsecond-precision resets_at must parse"
        );
        assert!(status.seven_day_opus.is_none());
        assert_eq!(
            status.seven_day.as_ref().and_then(|w| w.utilization),
            Some(16.0)
        );
        assert!(status.seven_day_sonnet.is_some());
        assert!(status.limits.is_none());
    }

    #[test]
    fn parses_limits_entries_and_filters_scoped() {
        // 2026-07 schema: session/weekly_all duplicate the legacy rows, only weekly_scoped
        // (per-model) is the extra display target.
        let json = r#"{"five_hour":{"utilization":32.0,"resets_at":"2026-07-10T04:10:00.497904+00:00"},
        "seven_day":{"utilization":7.0,"resets_at":"2026-07-12T03:00:00.497928+00:00"},
        "seven_day_opus":null,"seven_day_sonnet":null,
        "limits":[
        {"kind":"session","group":"session","percent":32,"severity":"normal","resets_at":"2026-07-10T04:10:00.497904+00:00","scope":null,"is_active":true},
        {"kind":"weekly_all","group":"weekly","percent":7,"severity":"normal","resets_at":"2026-07-12T03:00:00.497928+00:00","scope":null,"is_active":false},
        {"kind":"weekly_scoped","group":"weekly","percent":41,"severity":"normal","resets_at":"2026-07-12T03:00:00.498239+00:00","scope":{"model":{"id":null,"display_name":"Fable"},"surface":null},"is_active":false}
        ]}"#;
        let status = parse_claude_response(json).expect("decode");
        assert_eq!(status.limits.as_ref().map(Vec::len), Some(3));
        let scoped = status.scoped_limit_entries();
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].kind.as_deref(), Some("weekly_scoped"));
        assert_eq!(scoped[0].percent, Some(41.0));
        assert_eq!(
            scoped[0]
                .scope
                .as_ref()
                .and_then(|s| s.model.as_ref())
                .and_then(|m| m.display_name.as_ref()),
            Some(&"Fable".to_string())
        );
        assert!(scoped[0].reset_date().is_some());
    }

    #[test]
    fn legacy_empty_falls_back_to_all_entries() {
        // New-style response with no legacy fields: the whole limits[] list is the set.
        let json = r#"{"five_hour":null,"seven_day":null,
        "limits":[
        {"kind":"session","group":"session","percent":10,"severity":"normal","resets_at":"2026-07-10T04:10:00+00:00","scope":null,"is_active":true},
        {"kind":"weekly_all","group":"weekly","percent":5,"severity":"normal","resets_at":"2026-07-12T03:00:00+00:00","scope":null,"is_active":false}
        ]}"#;
        let status = parse_claude_response(json).expect("decode");
        assert_eq!(status.scoped_limit_entries().len(), 2);
    }

    #[test]
    fn invalid_body_is_response_json_error() {
        let err = parse_claude_response("not json").unwrap_err();
        assert!(matches!(err, LimitsError::ResponseJson(_)), "{err}");
    }

    // ==========================================================================
    // Claude — plan display / tier multiplier
    // ==========================================================================

    #[test]
    fn plan_display_combinations() {
        let mut status = parse_claude_response("{}").unwrap();
        status.subscription_type = Some("max".into());
        status.rate_limit_tier = Some("default_claude_max_20x".into());
        assert_eq!(status.plan_display().as_deref(), Some("Max 20x"));

        status.rate_limit_tier = Some("default_claude_max_5x".into());
        assert_eq!(status.plan_display().as_deref(), Some("Max 5x"));

        status.subscription_type = Some("pro".into());
        status.rate_limit_tier = Some("default_claude_pro".into());
        assert_eq!(status.plan_display().as_deref(), Some("Pro"));

        status.subscription_type = Some("free".into());
        status.rate_limit_tier = None;
        assert_eq!(status.plan_display().as_deref(), Some("Free"));

        status.subscription_type = None;
        assert_eq!(status.plan_display(), None);

        status.subscription_type = Some("".into());
        assert_eq!(status.plan_display(), None);
    }

    #[test]
    fn tier_multiplier_extract() {
        assert_eq!(
            tier_multiplier("default_claude_max_20x").as_deref(),
            Some("20x")
        );
        assert_eq!(
            tier_multiplier("default_claude_max_5x").as_deref(),
            Some("5x")
        );
        assert_eq!(tier_multiplier("default_claude_pro"), None);
        assert_eq!(tier_multiplier("x"), None); // lone "x" has no digits
        assert_eq!(tier_multiplier("20y"), None);
    }

    // ==========================================================================
    // Claude — credential parsing
    // ==========================================================================

    #[test]
    fn credential_parses_token_and_plan() {
        let data = r#"{"claudeAiOauth":{"accessToken":"tok","expiresAt":9999999999999,
        "subscriptionType":"max","rateLimitTier":"default_claude_max_20x"}}"#;
        let c = credential_from(data).expect("credential");
        assert_eq!(c.access_token, "tok");
        assert_eq!(c.subscription_type.as_deref(), Some("max"));
        assert_eq!(c.rate_limit_tier.as_deref(), Some("default_claude_max_20x"));
        // Millisecond epoch normalized to seconds.
        let now = Utc::now();
        assert!(now < c.expires_at.unwrap() - ChronoDuration::seconds(1000));
        assert!(!c.is_expired(now));
    }

    #[test]
    fn credential_legacy_without_plan_fields() {
        let data = r#"{"claudeAiOauth":{"accessToken":"tok","expiresAt":9999999999999}}"#;
        let c = credential_from(data).expect("credential");
        assert_eq!(c.subscription_type, None);
        assert_eq!(c.rate_limit_tier, None);
    }

    #[test]
    fn credential_expires_at_forms() {
        let base = |json: &str| {
            let v: serde_json::Value = serde_json::from_str(json).unwrap();
            credential_expires_at(v.get("expiresAt"))
        };
        // Seconds.
        let s = base(r#"{"expiresAt":1781000000}"#).expect("seconds");
        assert_eq!(s.timestamp(), 1_781_000_000);
        // Milliseconds (> 10e9 heuristic).
        let m = base(r#"{"expiresAt":1781000000123}"#).expect("milliseconds");
        assert_eq!(m.timestamp(), 1_781_000_000);
        assert_eq!(m.timestamp_subsec_millis(), 123);
        // Numeric string.
        assert_eq!(
            base(r#"{"expiresAt":"1781000000"}"#)
                .expect("string")
                .timestamp(),
            1_781_000_000
        );
        // Non-positive / junk / wrong type → None.
        assert_eq!(base(r#"{"expiresAt":0}"#), None);
        assert_eq!(base(r#"{"expiresAt":-5}"#), None);
        assert_eq!(base(r#"{"expiresAt":"junk"}"#), None);
        assert_eq!(base(r#"{"expiresAt":"tok"}"#), None);
        assert_eq!(base(r#"{}"#), None);
    }

    #[test]
    fn credential_missing_or_bogus_returns_none() {
        assert_eq!(credential_from("garbage"), None);
        assert_eq!(credential_from(r#"{"mcpOAuth":{"accessToken":"x"}}"#), None);
        // Explicit JSON null (the logout state) must not parse as a credential.
        assert_eq!(credential_from(r#"{"claudeAiOauth":null}"#), None);
        assert_eq!(
            credential_from(r#"{"claudeAiOauth":{"accessToken":""}}"#),
            None
        );
        assert_eq!(
            credential_from(r#"{"claudeAiOauth":{"refreshToken":"r"}}"#),
            None
        );
    }

    #[test]
    fn is_account_oauth_missing_covers_null_and_missing_only() {
        // Valid JSON, oauth dict present → not missing.
        assert!(!is_account_oauth_missing(
            r#"{"claudeAiOauth":{"accessToken":"tok"}}"#
        ));
        // Explicit null (logout) → missing (the NSNull misread regression).
        assert!(is_account_oauth_missing(r#"{"claudeAiOauth":null}"#));
        // Key absent → missing.
        assert!(is_account_oauth_missing(r#"{"mcpOAuth":{}}"#));
        // Broken JSON → NOT "missing account oauth" (that's a format error instead).
        assert!(!is_account_oauth_missing("not json"));
        // Top-level array → false (mirrors the Swift object cast).
        assert!(!is_account_oauth_missing(r#"[1,2]"#));
    }

    // ==========================================================================
    // Claude — Retry-After (port of the macOS table)
    // ==========================================================================

    #[test]
    fn retry_after_parsing() {
        assert_eq!(retry_after_seconds("120"), Some(120.0));
        assert_eq!(retry_after_seconds(" 60 "), Some(60.0));
        assert_eq!(retry_after_seconds("999999"), Some(3600.0)); // cap
        assert_eq!(retry_after_seconds("0"), None);
        assert_eq!(retry_after_seconds("-3"), None);
        assert_eq!(retry_after_seconds("Wed, 21 Oct 2026 07:28:00 GMT"), None); // date form
        assert_eq!(retry_after_seconds(""), None);
    }

    #[test]
    fn expiry_boundary_uses_60s_buffer() {
        let now = DateTime::from_timestamp(1_781_000_000, 0).unwrap();
        let at = |offset: i64| Credential {
            access_token: "t".into(),
            expires_at: Some(now + ChronoDuration::seconds(offset)),
            subscription_type: None,
            rate_limit_tier: None,
        };
        assert!(at(59).is_expired(now)); // within buffer
        assert!(at(60).is_expired(now)); // boundary
        assert!(!at(61).is_expired(now)); // outside buffer
        let never = Credential {
            access_token: "t".into(),
            expires_at: None,
            subscription_type: None,
            rate_limit_tier: None,
        };
        assert!(!never.is_expired(now));
    }

    // ==========================================================================
    // Claude — provider: candidate paths + credential classification (file only, no network)
    // ==========================================================================

    #[test]
    fn credential_candidates_order_and_dedup() {
        let home = PathBuf::from("/home/u");
        let none = credential_candidates(&home, None);
        assert_eq!(
            none,
            vec![PathBuf::from("/home/u/.claude/.credentials.json")]
        );

        let both = credential_candidates(&home, Some("/alt/claude, ~/more/claude"));
        assert_eq!(
            both,
            vec![
                PathBuf::from("/alt/claude/.credentials.json"),
                PathBuf::from("/home/u/more/claude/.credentials.json"),
                PathBuf::from("/home/u/.claude/.credentials.json"),
            ]
        );

        // Override aliasing the stock dir is not duplicated.
        let dup = credential_candidates(&home, Some("/home/u/.claude"));
        assert_eq!(
            dup,
            vec![PathBuf::from("/home/u/.claude/.credentials.json")]
        );
    }

    fn write_credentials(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).expect("write");
        p
    }

    #[test]
    fn read_credential_classifies_states() {
        let dir = temp_dir();
        // No file at all.
        let p = dir.join("absent/.credentials.json");
        let provider = ClaudeLimitsProvider::with_credentials_path(&p);
        let err = provider.read_credential().unwrap_err();
        assert!(matches!(err, LimitsError::NoCredentials { .. }), "{err}");

        // MCP-only (valid JSON, no account oauth).
        let mcp = write_credentials(&dir, "mcp.json", r#"{"mcpOAuth":{"accessToken":"x"}}"#);
        let provider = ClaudeLimitsProvider::with_credentials_path(&mcp);
        let err = provider.read_credential().unwrap_err();
        assert!(
            matches!(err, LimitsError::CredentialMissingAccountOAuth { .. }),
            "{err}"
        );

        // Broken JSON.
        let broken = write_credentials(&dir, "broken.json", "{oops");
        let provider = ClaudeLimitsProvider::with_credentials_path(&broken);
        let err = provider.read_credential().unwrap_err();
        assert!(matches!(err, LimitsError::CredentialFormat { .. }), "{err}");

        // Expired token.
        let past = Utc::now().timestamp() - 3600;
        let expired = write_credentials(
            &dir,
            "expired.json",
            &format!(r#"{{"claudeAiOauth":{{"accessToken":"tok","expiresAt":{past}}}}}"#),
        );
        let provider = ClaudeLimitsProvider::with_credentials_path(&expired);
        let err = provider.read_credential().unwrap_err();
        assert!(
            matches!(err, LimitsError::CredentialExpired { .. }),
            "{err}"
        );

        // Happy path: token + plan fields.
        let future = Utc::now().timestamp() + 86_400;
        let ok = write_credentials(
            &dir,
            "ok.json",
            &format!(
                r#"{{"claudeAiOauth":{{"accessToken":"tok123","expiresAt":{future},"subscriptionType":"max","rateLimitTier":"default_claude_max_20x"}}}}"#
            ),
        );
        let provider = ClaudeLimitsProvider::with_credentials_path(&ok);
        let c = provider.read_credential().expect("valid credential");
        assert_eq!(c.access_token, "tok123");
        assert_eq!(c.subscription_type.as_deref(), Some("max"));
    }

    #[test]
    fn first_readable_candidate_wins() {
        let dir = temp_dir();
        let broken = write_credentials(&dir, "broken.json", "{oops");
        let future = Utc::now().timestamp() + 86_400;
        let ok = write_credentials(
            &dir,
            "ok.json",
            &format!(r#"{{"claudeAiOauth":{{"accessToken":"second","expiresAt":{future}}}}}"#),
        );
        // Provider with both: the unreadable/malformed first file is skipped to the good one.
        let provider = ClaudeLimitsProvider {
            credentials_paths: vec![dir.join("absent"), broken.clone(), ok.clone()],
        };
        let c = provider
            .read_credential()
            .expect("credential from 2nd candidate");
        assert_eq!(c.access_token, "second");
        assert_eq!(provider.credentials_paths().len(), 3);
    }

    #[test]
    #[ignore = "hits api.anthropic.com; set PTB_TEST_CLAUDE_TOKEN to a live OAuth token to run"]
    fn fetch_claude_limits_live() {
        let token = match std::env::var("PTB_TEST_CLAUDE_TOKEN") {
            Ok(t) if !t.is_empty() => t,
            _ => return,
        };
        let provider = ClaudeLimitsProvider::new();
        let status = provider
            .fetch_with_access_token(&token)
            .expect("live usage fetch");
        eprintln!("live limits: {status:?}");
    }

    // ==========================================================================
    // Codex — parse/extract (fixtures captured from the macOS suite)
    // ==========================================================================

    // The app-server speaks newline-delimited JSON, so the reply is a single line (the
    // macOS suite's multi-line form is whitespace-equivalent as a JSON body, not as NDJSON).
    const SINGLE_BUCKET: &str = r#"{"rateLimits":{"limitId":"codex","limitName":null,"primary":{"usedPercent":86,"windowDurationMins":300,"resetsAt":1781694161},"secondary":{"usedPercent":58,"windowDurationMins":10080,"resetsAt":1781855658},"credits":{"hasCredits":false,"unlimited":false,"balance":null},"individualLimit":null,"planType":"team","rateLimitReachedType":null},"rateLimitsByLimitId":{"codex":{"limitId":"codex","limitName":null,"primary":{"usedPercent":86,"windowDurationMins":300,"resetsAt":1781694161},"secondary":{"usedPercent":58,"windowDurationMins":10080,"resetsAt":1781855658},"credits":{"hasCredits":false,"unlimited":false,"balance":null},"individualLimit":null,"planType":"team","rateLimitReachedType":null}}}"#;

    #[test]
    fn parses_single_bucket() {
        let status = parse_codex_response(SINGLE_BUCKET).unwrap();
        let snaps = status.snapshots();
        assert_eq!(snaps.len(), 1); // top-level and byLimitId["codex"] dedup to one
        let codex = &snaps[0];
        assert_eq!(codex.primary.as_ref().map(|w| w.used_percent), Some(86));
        assert_eq!(
            codex.primary.as_ref().map(|w| w.display_name()),
            Some("5h session".into())
        );
        assert_eq!(
            codex.secondary.as_ref().map(|w| w.display_name()),
            Some("Weekly".into())
        );
        assert_eq!(codex.plan_type.as_deref(), Some("team"));
        assert!(status.has_visible_limit());
        assert!(codex
            .primary
            .as_ref()
            .and_then(|w| w.reset_date())
            .is_some());
        assert_eq!(status.max_primary_used_percent(), Some(86));
    }

    #[test]
    fn parses_multi_bucket() {
        // Reported bug scenario: the "codex" bucket is unused; real usage is in codex_other.
        let other_bucket = r#"{"limitId":"codex_other","limitName":"codex_other","primary":{"usedPercent":41,"windowDurationMins":300,"resetsAt":1781694161},"secondary":{"usedPercent":93,"windowDurationMins":10080,"resetsAt":1781855658},"credits":null,"individualLimit":null,"planType":"plus","rateLimitReachedType":null}"#;
        let codex_bucket = r#"{"limitId":"codex","limitName":null,"primary":{"usedPercent":0,"windowDurationMins":300,"resetsAt":1781694161},"secondary":{"usedPercent":0,"windowDurationMins":10080,"resetsAt":1781855658},"credits":null,"individualLimit":null,"planType":"plus","rateLimitReachedType":null}"#;
        let json = format!(
            r#"{{"rateLimits":{codex_bucket},"rateLimitsByLimitId":{{"codex":{codex_bucket},"codex_other":{other_bucket}}}}}"#
        );
        let status = parse_codex_response(&json).unwrap();
        let snaps = status.snapshots();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].limit_id.as_deref(), Some("codex")); // top-level first
        assert_eq!(snaps[1].limit_id.as_deref(), Some("codex_other"));
        assert_eq!(
            snaps[1].secondary.as_ref().map(|w| w.used_percent),
            Some(93)
        );
        assert_eq!(status.max_primary_used_percent(), Some(41));
        assert_eq!(snaps[0].bucket_display_name(), "Codex");
        assert_eq!(snaps[1].bucket_display_name(), "Codex other");
    }

    #[test]
    fn legacy_nil_limit_id_dedups_codex_key() {
        // Old responses carry a null limitId; the server still keys it under "codex".
        let legacy_bucket = r#"{"limitId":null,"limitName":null,"primary":{"usedPercent":30,"windowDurationMins":300,"resetsAt":1},"secondary":null,"credits":null,"individualLimit":null,"planType":null,"rateLimitReachedType":null}"#;
        let json = format!(
            r#"{{"rateLimits":{legacy_bucket},"rateLimitsByLimitId":{{"codex":{legacy_bucket}}}}}"#
        );
        let status = parse_codex_response(&json).unwrap();
        assert_eq!(status.snapshots().len(), 1);
        assert_eq!(status.max_primary_used_percent(), Some(30));
    }

    #[test]
    fn extract_ignores_noise_and_untargeted_replies() {
        let reply = format!(r#"{{"id":1,"result":{SINGLE_BUCKET}}}"#);
        let text = [
            "startup log line, not json",
            r#"{"method":"item/updated","params":{"status":"idle"}}"#, // notification (no id)
            r#"{"id":0,"result":{"ok":true}}"#, // initialize reply — not our id
            "another log",
            reply.as_str(),
        ]
        .join("\n");
        let status = extract_codex_result(&text)
            .unwrap()
            .expect("reply found despite noise");
        assert_eq!(status.max_primary_used_percent(), Some(86));
    }

    #[test]
    fn extract_rpc_error_surface() {
        let text = r#"{"id":1,"error":{"code":-32601,"message":"method not found"}}"#;
        let err = extract_codex_result(text).unwrap_err();
        match err {
            LimitsError::CodexRpcError { message } => {
                assert_eq!(message, "method not found")
            }
            other => panic!("expected CodexRpcError, got {other}"),
        }
    }

    #[test]
    fn extract_no_reply_is_none() {
        assert_eq!(extract_codex_result("only logs here\n").unwrap(), None);
        assert_eq!(extract_codex_result("").unwrap(), None);
    }

    #[test]
    fn extract_result_bad_shape_is_error() {
        let json = r#"{"id":1,"result":{"rateLimits":"not-an-object"}}"#;
        assert!(matches!(
            extract_codex_result(json).unwrap_err(),
            LimitsError::ResponseJson(_)
        ));
    }

    #[test]
    fn request_lines_wire_shape() {
        let lines = codex_request_lines();
        assert_eq!(lines.len(), 3);

        let init: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["id"], 0);
        assert_eq!(init["params"]["clientInfo"]["name"], "poketokenbar");
        assert_eq!(init["params"]["clientInfo"]["title"], "PokeTokenBar");
        assert_eq!(init["params"]["capabilities"]["experimentalApi"], true);

        let done: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(done["method"], "initialized");
        assert!(done.get("id").is_none());

        let read: serde_json::Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(read["method"], "account/rateLimits/read");
        assert_eq!(read["id"], 1);
    }

    // ==========================================================================
    // Codex — model behavior
    // ==========================================================================

    #[test]
    fn window_display_name_by_duration() {
        let d = |mins: Option<i32>| {
            CodexRateLimitWindow {
                used_percent: 0,
                window_duration_mins: mins,
                resets_at: None,
            }
            .display_name()
        };
        assert_eq!(d(Some(300)), "5h session");
        assert_eq!(d(Some(10_080)), "Weekly");
        assert_eq!(d(Some(120)), "2h");
        assert_eq!(d(Some(90)), "90m");
        assert_eq!(d(None), "Limit");
    }

    #[test]
    fn spend_control_clamps_to_0_100() {
        let mk = |remaining: i32| CodexSpendControlLimit {
            limit: "$20".into(),
            remaining_percent: remaining,
            resets_at: 1_781_000_000,
            used: "$3".into(),
        };
        assert_eq!(mk(40).used_percent(), 60);
        assert_eq!(mk(120).used_percent(), 0); // more than 100% remaining
        assert_eq!(mk(-20).used_percent(), 100); // negative remaining
        assert!(mk(40).reset_date().is_some());
    }

    #[test]
    fn bucket_display_name_defaults() {
        let s = CodexRateLimitSnapshot {
            limit_id: None,
            limit_name: None,
            primary: None,
            secondary: None,
            credits: None,
            individual_limit: None,
            plan_type: None,
            rate_limit_reached_type: None,
        };
        assert_eq!(s.bucket_display_name(), "Codex");
        assert!(!s.has_visible_limit());
    }

    // ==========================================================================
    // Codex — binary discovery
    // ==========================================================================

    #[test]
    fn binary_candidates_cover_installer_and_managers() {
        let home = PathBuf::from("/home/u");
        let candidates = codex_binary_candidates(&home);
        let joined: Vec<String> = candidates
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(joined.contains(&"/home/u/.codex/bin/codex".to_string()));
        assert!(joined.contains(&"/usr/local/bin/codex".to_string()));
        assert!(joined.contains(&"/home/u/.local/share/mise/shims/codex".to_string()));
        assert!(joined.contains(&"/home/u/.local/bin/codex".to_string()));
        // /usr/bin is last among the shared dirs, like the macOS locator.
        assert_eq!(joined.last(), Some(&String::from("/usr/bin/codex")));
    }

    #[test]
    fn find_executable_only() {
        let dir = temp_dir();
        let plain = dir.join("codex");
        std::fs::write(&plain, "#!/bin/sh\n").unwrap();
        assert!(!is_executable_file(&plain));
        let mut m = std::fs::metadata(&plain).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            m.set_mode(0o755);
        }
        std::fs::set_permissions(&plain, m).unwrap();
        assert!(is_executable_file(&plain));
        assert!(!is_executable_file(&dir.join("nope")));
    }

    // ==========================================================================
    // Codex — subprocess runner against a FAKE codex (a shell script, never the real binary)
    // ==========================================================================

    fn write_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, body).unwrap();
        let mut m = std::fs::metadata(&p).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            m.set_mode(0o755);
        }
        std::fs::set_permissions(&p, m).unwrap();
        p
    }

    /// A fake `codex` that logs noise, answers both ids, then exits 0 — exercising the
    /// line-filter + id-match + success path end to end.
    const FAKE_CODEX_OK: &str = r#"#!/bin/sh
cat >/dev/null
echo '{"method":"thread/status","params":{"status":"idle"}}'
echo 'INFO noisy non-json log line'
echo '{"id":0,"result":{"ok":true}}'
echo '{"id":1,"result":{"rateLimits":{"limitId":"codex","limitName":null,"primary":{"usedPercent":86,"windowDurationMins":300,"resetsAt":1781694161},"secondary":{"usedPercent":58,"windowDurationMins":10080,"resetsAt":1781855658},"credits":{"hasCredits":false,"unlimited":false,"balance":null},"individualLimit":null,"planType":"team","rateLimitReachedType":null},"rateLimitsByLimitId":{}}}'
"#;

    #[test]
    fn fake_codex_roundtrip_parses_status() {
        let dir = temp_dir();
        let bin = write_executable(&dir, "codex", FAKE_CODEX_OK);
        let provider = CodexLimitsProvider::with_binary(&bin);
        assert_eq!(provider.resolved_binary(), Some(bin.clone()));
        let status = provider.fetch().expect("fetch").expect("binary found");
        assert_eq!(status.max_primary_used_percent(), Some(86));
        assert_eq!(status.rate_limits.plan_type.as_deref(), Some("team"));
    }

    #[test]
    fn fake_codex_timeout_kills_and_errors() {
        let dir = temp_dir();
        let bin = write_executable(&dir, "codex", "#!/bin/sh\ncat >/dev/null\nsleep 15\n");
        let provider =
            CodexLimitsProvider::with_binary(bin.clone()).with_timeout(Duration::from_secs(1));
        let err = provider.fetch().unwrap_err();
        assert!(matches!(err, LimitsError::CodexTimeout { .. }), "{err}");
    }

    #[test]
    fn fake_codex_nonzero_exit_reports_code_and_stderr() {
        let dir = temp_dir();
        let bin = write_executable(
            &dir,
            "codex",
            "#!/bin/sh\necho 'app-server: config invalid' >&2\nexit 3\n",
        );
        let err = CodexLimitsProvider::with_binary(bin).fetch().unwrap_err();
        match err {
            LimitsError::CodexNonZeroExit { code, .. } => assert_eq!(code, 3),
            other => panic!("expected CodexNonZeroExit, got {other}"),
        }
    }

    #[test]
    fn fake_codex_rpc_error_surfaces_message() {
        let dir = temp_dir();
        let bin = write_executable(
            &dir,
            "codex",
            "#!/bin/sh\ncat >/dev/null\necho '{\"id\":1,\"error\":{\"code\":-1,\"message\":\"unauthorized\"}}'\n",
        );
        let err = CodexLimitsProvider::with_binary(bin).fetch().unwrap_err();
        match err {
            LimitsError::CodexRpcError { message } => assert_eq!(message, "unauthorized"),
            other => panic!("expected CodexRpcError, got {other}"),
        }
    }

    #[test]
    fn fake_codex_silent_exit_is_missing_response() {
        let dir = temp_dir();
        let bin = write_executable(&dir, "codex", "#!/bin/sh\ncat >/dev/null\ntrue\n");
        let err = CodexLimitsProvider::with_binary(bin).fetch().unwrap_err();
        assert!(
            matches!(err, LimitsError::CodexMissingResponse { .. }),
            "{err}"
        );
    }

    #[test]
    fn missing_binary_is_none_not_error() {
        let provider = CodexLimitsProvider::with_binary(PathBuf::from("/nonexistent/codex"));
        assert_eq!(provider.fetch().expect("no binary"), None);
    }

    #[test]
    #[ignore = "runs the real codex app-server; only meaningful on machines with codex installed"]
    fn fetch_codex_limits_live() {
        let provider = CodexLimitsProvider::new();
        if provider.resolved_binary().is_none() {
            return;
        }
        match provider.fetch() {
            Ok(None) => panic!("binary resolved but fetch returned None"),
            Ok(Some(status)) => eprintln!("live codex: {status:?}"),
            Err(e) => panic!("live codex fetch failed: {e}"),
        }
    }
}
