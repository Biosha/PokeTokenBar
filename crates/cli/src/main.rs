//! `poke-token-bar` — headless CLI for the portable core (Phase 1 deliverable).
//!
//! `snapshot` reads every local usage source and prints the combined result, as human text or
//! JSON. No GUI, no daemon — this is the verifiable "core first" milestone.

use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use poketoken_core::companion::{self, CompanionEvent, CompanionState, DisplayInput, StateKind};
use poketoken_core::config::Config;
use poketoken_core::limits as lim;
use poketoken_core::limits::{ClaudeLimitsProvider, CodexLimitsProvider};
use poketoken_core::{
    build_snapshot, CodexRateLimitStatus, LimitStatus, ProviderCtx, UsageSnapshot,
};
use std::fmt::Write as _;
use std::io::Write as IoWrite;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "poke-token-bar",
    version,
    about = "AI-coding token usage → Pokémon companion (Linux/GNOME port)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print a usage snapshot for now.
    Snapshot {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Restrict to a single provider id (e.g. `claude_code`).
        #[arg(long)]
        provider: Option<String>,
        /// Fixed "now" as RFC-3339 (for reproducible output / tests).
        #[arg(long)]
        now: Option<String>,
    },
    /// Advance + print the Pokémon companion from live usage (persists to the data dir).
    Companion {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Fixed "now" as RFC-3339 (reproducible).
        #[arg(long)]
        now: Option<String>,
    },
    /// Continuously re-read usage and advance the companion every `--interval` seconds (Ctrl-C to stop).
    Watch {
        /// Emit one JSON object per tick.
        #[arg(long)]
        json: bool,
        /// Refresh interval in seconds.
        #[arg(long = "interval", default_value_t = 15)]
        interval_secs: u64,
    },
    /// Print official usage limits (Claude plan windows + Codex rate-limit buckets).
    /// An unavailable source (missing credentials / missing codex binary) prints a
    /// `not available` line and does not fail the command.
    Limits {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}

/// One companion refresh: build snapshot, apply the day-delta token growth, persist, derive display.
struct Tick {
    state: CompanionState,
    kind: StateKind,
    events: Vec<CompanionEvent>,
    applied: i64,
    today_total: i64,
    today_cost: f64,
    recent_tpm: Option<f64>,
}

fn tick(now: DateTime<Utc>) -> anyhow::Result<Tick> {
    let cfg = Config::load();
    let ctx = ProviderCtx::system();
    let snap = build_snapshot(&ctx, now, cfg.first_weekday());
    let today_key = poketoken_core::windows::local_day(now, &ctx.tz);
    let today = snap.combined_today.clone();
    let today_total = today.as_ref().map(|t| t.total_tokens).unwrap_or(0);
    let today_cost = today.as_ref().map(|t| t.total_cost).unwrap_or(0.0);
    let recent_tpm = {
        let sum: f64 = snap
            .providers
            .iter()
            .filter_map(|p| p.active_block.as_ref().and_then(|b| b.tokens_per_minute))
            .sum();
        if sum > 0.0 {
            Some(sum)
        } else {
            None
        }
    };

    let mut state = companion::load();
    let applied = companion::day_delta(&mut state, &today_key, today_total);
    let events = state.add_tokens(applied);
    let celebration = events.iter().any(|e| e.is_celebration());
    let di = DisplayInput {
        tpm: recent_tpm,
        limit_warning: false,
        has_usage_data: today_total > 0,
        today_total,
        celebration,
    };
    let kind = companion::display_state(&state, &di);
    companion::save(&state)?;
    Ok(Tick {
        state,
        kind,
        events,
        applied,
        today_total,
        today_cost,
        recent_tpm,
    })
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Snapshot {
            json,
            provider,
            now,
        } => {
            let cfg = Config::load();
            let ctx = ProviderCtx::system();
            let now: DateTime<Utc> = match &now {
                Some(s) => chrono::DateTime::parse_from_rfc3339(s)
                    .map(|d| d.with_timezone(&Utc))
                    .map_err(|e| anyhow::anyhow!("bad --now: {e}"))?,
                None => Utc::now(),
            };
            let mut snap = build_snapshot(&ctx, now, cfg.first_weekday());
            if let Some(id) = &provider {
                snap.providers.retain(|p| &p.provider_id == id);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&snap)?);
            } else {
                print_human(&snap);
            }
            Ok(())
        }
        Command::Companion { json, now } => {
            let t = tick(parse_now(&now)?)?;
            if json {
                println!("{}", companion_json(&t));
            } else {
                print_companion(&t.state, t.kind, &t.events, t.applied);
            }
            Ok(())
        }
        Command::Watch {
            json,
            interval_secs,
        } => {
            let interval = Duration::from_secs(interval_secs.max(1));
            eprintln!(
                "poke-token-bar: watching every {}s (Ctrl-C to stop)",
                interval_secs.max(1)
            );
            loop {
                let t = tick(Utc::now())?;
                if json {
                    println!("{}", companion_json(&t));
                } else {
                    print_ticker(&t);
                    std::io::stdout().flush()?;
                }
                std::thread::sleep(interval);
            }
        }
        Command::Limits { json } => {
            let claude = ClaudeLimitsProvider::new().fetch();
            let codex = CodexLimitsProvider::new().fetch();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&limits_json(&claude, &codex))?
                );
            } else {
                print_limits_human(&claude, &codex);
            }
            Ok(())
        }
    }
}

/// One provider's fetch result. `Err(e)` → "not available (e)"; the Codex `Ok(None)` is
/// the missing-binary case (the core returns nil there, not an error).
fn limits_json(
    claude: &Result<LimitStatus, lim::LimitsError>,
    codex: &Result<Option<CodexRateLimitStatus>, lim::LimitsError>,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    match claude {
        Ok(status) => {
            obj.insert(
                "claude".into(),
                serde_json::to_value(status).unwrap_or(serde_json::Value::Null),
            );
        }
        Err(e) => {
            obj.insert("claude".into(), serde_json::Value::Null);
            obj.insert("claude_error".into(), e.to_string().into());
        }
    }
    match codex {
        Ok(Some(status)) => {
            obj.insert(
                "codex".into(),
                serde_json::to_value(status).unwrap_or(serde_json::Value::Null),
            );
        }
        Ok(None) => {
            obj.insert("codex".into(), serde_json::Value::Null);
            obj.insert("codex_error".into(), "no codex binary found".into());
        }
        Err(e) => {
            obj.insert("codex".into(), serde_json::Value::Null);
            obj.insert("codex_error".into(), e.to_string().into());
        }
    }
    serde_json::Value::Object(obj)
}

fn print_limits_human(
    claude: &Result<LimitStatus, lim::LimitsError>,
    codex: &Result<Option<CodexRateLimitStatus>, lim::LimitsError>,
) {
    match claude {
        Ok(status) => print!("{}", claude_human(status)),
        Err(e) => println!("claude: not available ({e})"),
    }
    match codex {
        Ok(Some(status)) => print!("{}", codex_human(status)),
        Ok(None) => println!("codex: not available (no codex binary found)"),
        Err(e) => println!("codex: not available ({e})"),
    }
}

fn reset_suffix(d: Option<DateTime<Utc>>) -> String {
    d.map(|d| format!("  (resets {})", d.format("%Y-%m-%d %H:%M UTC")))
        .unwrap_or_default()
}

/// Human rows for the Claude usage response: plan header, the legacy 5h/weekly windows,
/// then any extra `limits[]` entries (e.g. per-model weekly scopes).
fn claude_human(s: &LimitStatus) -> String {
    let mut out = String::new();
    let header = match s.plan_display() {
        Some(plan) => format!("claude ({plan}):"),
        None => "claude:".to_string(),
    };
    let _ = writeln!(out, "{header}");
    for (name, window) in [
        ("5h", s.five_hour.as_ref()),
        ("weekly", s.seven_day.as_ref()),
    ] {
        if let Some(w) = window {
            let used = w
                .utilization
                .map(|u| format!("{u:.0}% used"))
                .unwrap_or_else(|| "—".to_string());
            let _ = writeln!(out, "  {name:<16} {used}{}", reset_suffix(w.reset_date()));
        }
    }
    for e in s.scoped_limit_entries() {
        let name = match e
            .scope
            .as_ref()
            .and_then(|sc| sc.model.as_ref())
            .and_then(|m| m.display_name.as_deref())
            .filter(|n| !n.is_empty())
        {
            Some(model) => format!("{} ({model})", e.kind.as_deref().unwrap_or("limit")),
            None => e.kind.as_deref().unwrap_or("limit").to_string(),
        };
        let used = e
            .percent
            .map(|p| format!("{p:.0}% used"))
            .unwrap_or_else(|| "—".to_string());
        let _ = writeln!(out, "  {name:<16} {used}{}", reset_suffix(e.reset_date()));
    }
    out
}

/// Human rows for the Codex rate-limit status: one block per visible bucket (using the
/// core `bucket_display_name` / `display_name` helpers).
fn codex_human(s: &CodexRateLimitStatus) -> String {
    let visible = s.visible_snapshots();
    if visible.is_empty() {
        return "codex: no visible limits".to_string();
    }
    let mut out = String::new();
    for snap in &visible {
        let _ = writeln!(out, "codex ({}):", snap.bucket_display_name());
        for w in snap
            .primary
            .as_ref()
            .into_iter()
            .chain(snap.secondary.as_ref())
        {
            let _ = writeln!(
                out,
                "  {:<16} {}% used{}",
                w.display_name(),
                w.used_percent,
                reset_suffix(w.reset_date())
            );
        }
        if let Some(ind) = &snap.individual_limit {
            let _ = writeln!(
                out,
                "  {:<16}${} used  (limit ${}){}",
                "spend",
                ind.used,
                ind.limit,
                reset_suffix(ind.reset_date())
            );
        }
    }
    out
}

fn parse_now(s: &Option<String>) -> anyhow::Result<DateTime<Utc>> {
    match s {
        Some(v) => chrono::DateTime::parse_from_rfc3339(v)
            .map(|d| d.with_timezone(&Utc))
            .map_err(|e| anyhow::anyhow!("bad --now: {e}")),
        None => Ok(Utc::now()),
    }
}

fn companion_json(t: &Tick) -> serde_json::Value {
    let events: Vec<String> = t.events.iter().map(companion_event_str).collect();
    serde_json::json!({
        "species": t.state.species(),
        "speciesEn": t.state.species_en(),
        "kind": t.kind.label(),
        "lineKey": t.state.line_key,
        "formIndex": t.state.form_index,
        "graduated": t.state.graduated,
        "eggProgress": if t.state.is_egg() { t.state.egg_progress } else { 0 },
        "progressFraction": t.state.progress_fraction(),
        "appliedToday": t.applied,
        "todayTotal": t.today_total,
        "todayCost": t.today_cost,
        "events": events,
        "dex": t.state.dex,
    })
}

fn print_ticker(t: &Tick) {
    let st = &t.state;
    let clock = Utc::now().format("%H:%M:%S");
    let stage = if st.is_egg() {
        format!("egg {}% to hatch", (st.progress_fraction() * 100.0) as u32)
    } else if st.graduated {
        format!("{} — graduated 🎓", st.species())
    } else {
        format!(
            "{} {}% (form {}/{})",
            st.species(),
            (st.progress_fraction() * 100.0) as u32,
            st.form_index + 1,
            st.total_forms()
        )
    };
    let burn = t
        .recent_tpm
        .map(|b| format!("| {:.0} tok/min", b))
        .unwrap_or_default();
    println!(
        "[{clock}] {stage} [{kind}] · today {tm:.3}M ${cost:.2} {burn}",
        kind = t.kind.label(),
        tm = t.today_total as f64 / 1e6,
        cost = t.today_cost,
        burn = burn,
    );
    for e in &t.events {
        println!("    → {}", companion_event_str(e));
    }
}

fn companion_event_str(e: &poketoken_core::companion::CompanionEvent) -> String {
    match e {
        poketoken_core::companion::CompanionEvent::Hatched { species, .. } => {
            format!("hatched into {species}!")
        }
        poketoken_core::companion::CompanionEvent::Evolved { to } => format!("evolved into {to}!"),
        poketoken_core::companion::CompanionEvent::Graduated { species } => {
            format!("{species} graduated 🎓")
        }
        poketoken_core::companion::CompanionEvent::DittoRevealed { disguise } => {
            format!("it was {disguise}? No — it's Ditto!")
        }
    }
}

fn print_companion(
    state: &poketoken_core::companion::CompanionState,
    kind: poketoken_core::companion::StateKind,
    events: &[poketoken_core::companion::CompanionEvent],
    applied: i64,
) {
    let mut out = String::new();
    let _ = writeln!(out, "companion  {}", state.species());
    let _ = writeln!(out, "state      {}", kind.label());
    if state.is_egg() {
        let pct = (state.progress_fraction() * 100.0) as u32;
        let _ = writeln!(
            out,
            "egg        {pct}% to hatch ({} / {} M)",
            state.egg_progress,
            poketoken_core::companion::EGG_HATCH_THRESHOLD / 1_000_000
        );
    } else if state.graduated {
        let _ = writeln!(out, "stage      GRADUATED 🎓");
    } else {
        let pct = (state.progress_fraction() * 100.0) as u32;
        let _ = writeln!(
            out,
            "stage      form {} of {}  ({pct}% to next)",
            state.form_index + 1,
            state.total_forms()
        );
    }
    for e in events {
        let _ = writeln!(out, "  → {}", companion_event_str(e));
    }
    if applied > 0 {
        let _ = writeln!(out, "applied    {applied} tokens this call");
    }
    print!("{out}");
}

fn print_human(s: &UsageSnapshot) {
    let mut out = String::new();
    let mt = |t: i64| t as f64 / 1e6;
    let _ = writeln!(out, "generated_at  {}", s.generated_at);
    match &s.combined_today {
        Some(t) => {
            let _ = writeln!(
                out,
                "today (total) {:.3}M tokens  ${:.2}",
                mt(t.total_tokens),
                t.total_cost
            );
        }
        None => {
            let _ = writeln!(
                out,
                "today (total) — (no usage today across detected providers)"
            );
        }
    }
    for p in &s.providers {
        let _ = write!(out, "\n{} ({}):", p.display_name, p.provider_id);
        match &p.today {
            Some(t) => {
                let _ = writeln!(
                    out,
                    "\n  today   {:.3}M  ${:.2}",
                    mt(t.total_tokens),
                    t.total_cost
                );
            }
            None => {
                let _ = writeln!(out, "\n  today   —");
            }
        }
        if let Some(b) = &p.active_block {
            let _ = writeln!(
                out,
                "  5h      {:.3}M  ${:.2}  ({:.0} tok/min)",
                mt(b.total_tokens),
                b.cost_usd,
                b.tokens_per_minute.unwrap_or(0.0)
            );
        }
        if let Some(w) = &p.week_total {
            let _ = writeln!(
                out,
                "  week    {:.3}M  ${:.2}",
                mt(w.total_tokens),
                w.total_cost
            );
        }
        if let Some(m) = &p.month_total {
            let _ = writeln!(
                out,
                "  month   {:.3}M  ${:.2}",
                mt(m.total_tokens),
                m.total_cost
            );
        }
    }
    print!("{out}");
}

#[cfg(test)]
mod limits_tests {
    use super::*;
    use poketoken_core::limits::{parse_claude_response, parse_codex_response};
    use std::path::PathBuf;

    const CLAUDE_FIXTURE: &str = r#"{"five_hour":{"utilization":23.0,"resets_at":"2026-06-10T11:10:00.034464+00:00"},
        "seven_day":{"utilization":16.0,"resets_at":"2026-06-14T03:00:01.034496+00:00"},
        "limits":[{"kind":"weekly_scoped","percent":41,"resets_at":"2026-06-14T03:00:02+00:00","scope":{"model":{"display_name":"Fable"}}}]}"#;

    const CODEX_FIXTURE: &str = r#"{
        "rateLimits": {"limitId":"codex","limitName":"codex",
            "primary":{"usedPercent":45,"windowDurationMins":300,"resetsAt":1750000000},
            "secondary":{"usedPercent":12,"windowDurationMins":10080,"resetsAt":1750600000}},
        "rateLimitsByLimitId": {
            "codex": {"limitId":"codex","limitName":"codex",
                "primary":{"usedPercent":45,"windowDurationMins":300,"resetsAt":1750000000},
                "secondary":{"usedPercent":12,"windowDurationMins":10080,"resetsAt":1750600000}},
            "codex_other": {"limitId":"codex_other","limitName":"codex_other",
                "primary":{"usedPercent":7,"windowDurationMins":300,"resetsAt":1750000000}}
        }
    }"#;

    #[test]
    fn claude_human_formats_plan_and_windows() {
        let mut status = parse_claude_response(CLAUDE_FIXTURE).expect("decode");
        status.subscription_type = Some("max".into());
        status.rate_limit_tier = Some("default_claude_max_20x".into());
        let out = claude_human(&status);
        assert!(out.contains("claude (Max 20x):"), "{out}");
        assert!(out.contains("5h"), "{out}");
        assert!(out.contains("23% used"), "{out}");
        assert!(out.contains("2026-06-10 11:10 UTC"), "{out}");
        assert!(out.contains("16% used"), "{out}");
        // Scoped entry (weekly_scoped only, legacy rows already shown above).
        assert!(out.contains("weekly_scoped (Fable)"), "{out}");
        assert!(out.contains("41% used"), "{out}");
    }

    #[test]
    fn claude_human_without_plan_or_resets() {
        // No subscription info, no resets_at: header degrades, "—" for missing usage.
        let status = parse_claude_response(
            r#"{"five_hour":{"utilization":null,"resets_at":null},"seven_day":null}"#,
        )
        .expect("decode");
        let out = claude_human(&status);
        assert!(out.starts_with("claude:\n"), "{out}");
        assert!(out.contains("—"), "{out}");
        assert!(!out.contains("(resets"), "{out}");
    }

    #[test]
    fn codex_human_formats_visible_buckets() {
        let status = parse_codex_response(CODEX_FIXTURE).expect("decode");
        let out = codex_human(&status);
        // Top-level snapshot + the distinct codex_other bucket (the map's "codex" entry dedupes).
        assert!(out.contains("codex (Codex):"), "{out}");
        assert!(out.contains("5h session"), "{out}");
        assert!(out.contains("45% used"), "{out}");
        assert!(out.contains("Weekly"), "{out}");
        assert!(out.contains("12% used"), "{out}");
        assert!(out.contains("codex (Codex other):"), "{out}");
        assert!(out.contains("7% used"), "{out}");
        assert!(out.contains("2025-06-15 15:06 UTC"), "{out}"); // resetsAt 1750000000
        assert_eq!(out.matches("codex (").count(), 2, "{out}");
    }

    #[test]
    fn codex_human_no_visible_limits() {
        let status = parse_codex_response(r#"{"rateLimits":{}}"#).expect("decode");
        assert_eq!(codex_human(&status), "codex: no visible limits");
    }

    #[test]
    fn limits_json_mixes_status_and_errors() {
        let claude: Result<LimitStatus, lim::LimitsError> = Err(lim::LimitsError::NoCredentials {
            path: PathBuf::from("/h/.claude/.credentials.json"),
        });
        let codex: Result<Option<CodexRateLimitStatus>, lim::LimitsError> =
            Ok(Some(parse_codex_response(CODEX_FIXTURE).expect("decode")));
        let v = limits_json(&claude, &codex);
        assert_eq!(v["claude"], serde_json::Value::Null);
        assert!(
            v["claude_error"]
                .as_str()
                .unwrap()
                .contains("no Claude credentials"),
            "{v}"
        );
        assert_eq!(v["codex"]["rateLimits"]["limitId"], "codex");
        assert!(v.get("codex_error").is_none(), "{v}");
    }

    #[test]
    fn limits_json_codex_binary_missing_is_null() {
        let claude: Result<LimitStatus, lim::LimitsError> =
            Ok(parse_claude_response(CLAUDE_FIXTURE).expect("decode"));
        let codex: Result<Option<CodexRateLimitStatus>, lim::LimitsError> = Ok(None);
        let v = limits_json(&claude, &codex);
        assert_eq!(v["claude"]["five_hour"]["utilization"], 23.0);
        assert_eq!(v["codex"], serde_json::Value::Null);
        assert!(
            v["codex_error"]
                .as_str()
                .unwrap()
                .contains("no codex binary"),
            "{v}"
        );
    }
}
