//! Pokémon companion core — full Phase 1b port of the macOS app's companion scope:
//! - the complete Gen I–V evolution-line pool with official capture-rate weights
//!   ([`crate::pool`], resolved at runtime from PokéAPI as in the macOS app's
//!   `PokeAPIClient` — 30-day disk cache, bundled offline fallback);
//! - the egg → hatch → evolve → graduate loop with the exact [`PokemonBalance`] thresholds
//!   (egg hatch 5M, per-rarity graduation totals, per-form phase costs that sum to the total);
//! - shiny (1/64, 1/48 with the Shiny Charm) and 25-nature rolls at hatch, Ditto disguise
//!   (1/128, common ≥2-form) and its reveal at the first evolution threshold;
//! - the shop (Rare Candy / Mint / Shiny Charm), premium-egg rerolls with rarity guarantees,
//!   and the limit-window Rare-Candy grant logic;
//! - per-language i18n for all companion strings ([`crate::i18n`], default English) and the
//!   exact macOS display-state rule (egg / idle / working / focus / tired / sleep / levelUp).
//!
//! Persistence stays backward compatible: legacy `companion-state.json` files (eggProgress,
//! lineKey, formIndex, phaseProgress, graduated, dex, seed, lastDay, dayApplied) load via
//! serde defaults + [`CompanionState::migrate`]. New fields are appended, never renamed.

use crate::i18n::{Language, L};
use crate::nature::Nature;
use crate::pool::{self, Roll};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Tokens required to crack the egg (surplus carries into the hatched form's first phase).
pub const EGG_HATCH_THRESHOLD: i64 = 5_000_000;

/// Display state, derived from recent usage/burn (drives sprite motion + status copy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateKind {
    Egg,
    Idle,
    Working,
    Focus,
    Tired,
    Sleep,
    LevelUp,
}

impl StateKind {
    pub fn label(self) -> &'static str {
        match self {
            StateKind::Egg => "egg",
            StateKind::Idle => "idle",
            StateKind::Working => "working",
            StateKind::Focus => "focus",
            StateKind::Tired => "tired",
            StateKind::Sleep => "sleep",
            StateKind::LevelUp => "levelup",
        }
    }
}

impl std::fmt::Display for StateKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rarity {
    Common,
    Uncommon,
    Rare,
    Legendary,
}

impl Rarity {
    pub fn sort_rank(self) -> u8 {
        match self {
            Rarity::Common => 0,
            Rarity::Uncommon => 1,
            Rarity::Rare => 2,
            Rarity::Legendary => 3,
        }
    }
    /// This tier's `capture_rate` ceiling; `capture_rate <= ceiling` ⇒ at least this tier.
    /// `None` = not expressible via capture rate (legendaries are flag-only) — a legendary
    /// guarantee would make the egg unhatchable, so it is dropped at load.
    pub fn capture_rate_ceiling(self) -> Option<i32> {
        match self {
            Rarity::Common => Some(255),
            Rarity::Uncommon => Some(120),
            Rarity::Rare => Some(45),
            Rarity::Legendary => None,
        }
    }
    pub fn includes(self, capture_rate: i32) -> bool {
        self.capture_rate_ceiling()
            .is_some_and(|c| capture_rate <= c)
    }
    pub fn from_capture_rate(capture_rate: i32, is_legendary: bool, is_mythical: bool) -> Self {
        if is_legendary || is_mythical {
            Rarity::Legendary
        } else if Rarity::Rare.includes(capture_rate) {
            Rarity::Rare
        } else if Rarity::Uncommon.includes(capture_rate) {
            Rarity::Uncommon
        } else {
            Rarity::Common
        }
    }
}

// ---------------------------------------------------------------------------
// Balance (exact ports of `PokemonBalance` and the item constant blocks)
// ---------------------------------------------------------------------------

/// Graduation total T — identical for a given rarity regardless of line length.
pub fn graduation_total(rarity: Rarity) -> i64 {
    match rarity {
        Rarity::Common => 750_000_000,
        Rarity::Uncommon => 1_875_000_000,
        Rarity::Rare => 3_000_000_000,
        Rarity::Legendary => 6_000_000_000,
    }
}

/// Cost to grow from `stage_index` (0-based) to the next form / graduate. The k stage costs of a
/// k-form line sum to exactly `graduation_total(rarity)`.
pub fn phase_threshold(rarity: Rarity, total_forms: i64, stage_index: i64) -> i64 {
    let kk = total_forms.max(1);
    let i = stage_index + 1;
    let total = graduation_total(rarity) as f64;
    let denom = (kk * (kk + 1)) as f64 / 2.0;
    (total * i as f64 / denom).round() as i64
}

/// Individual-roll odds (port of `PokemonOdds`).
pub struct PokemonOdds;
impl PokemonOdds {
    /// Shiny hatch odds denominator — 1/64 (the original 1/4096 is a lifetime at this scale).
    pub const SHINY_DENOMINATOR: u64 = 64;
    /// Ditto disguise odds denominator — 1/128, common ≥2-form hatches only.
    pub const DITTO_DISGUISE_DENOMINATOR: u64 = 128;
    /// Ditto species id — disguise reveal only (excluded from the hatch pool).
    pub const DITTO_SPECIES_ID: u16 = 132;
}

/// Rare Candy balance (port of `RareCandy`).
pub struct RareCandy;
impl RareCandy {
    /// XP (token equivalent) injected on use — below the minimum one-stage threshold, so one
    /// candy raises at most one form (no cascade/graduation runaway).
    pub const XP: i64 = 100_000_000;
    /// Count granted when a weekly limit window hits 100% (session windows grant 1).
    pub const WEEKLY_GRANT: i64 = 5;
    /// Shop price (currency = tokens used).
    pub const PRICE: i64 = 500_000_000;
}

/// Mint balance (port of `Mint`).
pub struct Mint;
impl Mint {
    /// Shop price — cosmetic nature reroll, kept light at 1/5 of the candy.
    pub const PRICE: i64 = 100_000_000;
}

/// Shiny Charm balance (port of `ShinyCharm`) — passive, one-time, permanent.
pub struct ShinyCharm;
impl ShinyCharm {
    /// Shop price (premium: one rare's graduation worth).
    pub const PRICE: i64 = 3_000_000_000;
    /// Shiny denominator while owned: 1/64 → 1/48 (+33%). No retroactive hatches.
    pub const SHINY_DENOMINATOR: u64 = 48;
}

/// Fresh-egg (reroll) balance (port of `FreshEgg`).
pub struct FreshEgg;
impl FreshEgg {
    /// Base price (no guarantee).
    pub const PRICE: i64 = 1_000_000_000;
    /// Tiers sold: no guarantee → uncommon+ → rare+. No legendary-only egg (the guarantee
    /// cannot be expressed via capture rate, and the top tier is not a fixed product).
    pub const SHOP_TIERS: [Option<Rarity>; 3] = [None, Some(Rarity::Uncommon), Some(Rarity::Rare)];

    /// Tiered price = base × (graduationTotal(tier) / graduationTotal(common)), reusing the
    /// graduation table: 1B / 2.5B / 4B (1 : 2.5 : 4).
    pub fn price(guaranteeing: Option<Rarity>) -> i64 {
        let Some(tier) = guaranteeing else {
            return Self::PRICE;
        };
        let multiplier = graduation_total(tier) as f64 / graduation_total(Rarity::Common) as f64;
        (Self::PRICE as f64 * multiplier).round() as i64
    }
}

/// Inventory item kind (port of `ItemKind`); raw values persist in `inventory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemKind {
    RareCandy,
    Mint,
    ShinyCharm,
}

impl ItemKind {
    pub const ALL: [ItemKind; 3] = [ItemKind::RareCandy, ItemKind::Mint, ItemKind::ShinyCharm];

    /// Persistent raw value (JSON key in `inventory`).
    pub fn raw(self) -> &'static str {
        match self {
            ItemKind::RareCandy => "rareCandy",
            ItemKind::Mint => "mint",
            ItemKind::ShinyCharm => "shinyCharm",
        }
    }

    /// PokéAPI item sprite filename (None → emoji fallback; mints have no Gen-8 sprite).
    pub fn sprite_name(self) -> Option<&'static str> {
        match self {
            ItemKind::RareCandy => Some("rare-candy"),
            ItemKind::Mint => None,
            ItemKind::ShinyCharm => Some("shiny-charm"),
        }
    }

    pub fn fallback_emoji(self) -> &'static str {
        match self {
            ItemKind::RareCandy => "🍬",
            ItemKind::Mint => "🌿",
            ItemKind::ShinyCharm => "✨",
        }
    }

    /// Shop price; None = not sold.
    pub fn shop_price(self) -> Option<i64> {
        match self {
            ItemKind::RareCandy => Some(RareCandy::PRICE),
            ItemKind::Mint => Some(Mint::PRICE),
            ItemKind::ShinyCharm => Some(ShinyCharm::PRICE),
        }
    }

    /// Passive (owned) item — never consumed, one purchase.
    pub fn is_passive(self) -> bool {
        matches!(self, ItemKind::ShinyCharm)
    }
}

/// One shop row — a sold item or an egg reroll (port of `ShopEntry`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShopEntry {
    Item(ItemKind),
    Egg(Option<Rarity>),
}

impl ShopEntry {
    pub fn price(self) -> i64 {
        match self {
            ShopEntry::Item(kind) => kind.shop_price().unwrap_or(0),
            ShopEntry::Egg(tier) => FreshEgg::price(tier),
        }
    }
}

// ---------------------------------------------------------------------------
// Candy grants (pure decision logic, port of `evaluateCandyGrants` / `grantCandies`)
// ---------------------------------------------------------------------------

/// Limit-window class: session grants 1 candy, weekly grants [`RareCandy::WEEKLY_GRANT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowClass {
    Session,
    Weekly,
}

/// One provider-independent limit window (input to the candy decision).
#[derive(Debug, Clone, PartialEq)]
pub struct CandyWindow {
    /// Stable identifier (no volatile fields like resets_at).
    pub key: String,
    /// Display name ("why am I getting this").
    pub name: String,
    pub kind: WindowClass,
    /// 0–100+ utilization.
    pub utilization: f64,
}

/// One granted candy batch (pure decision result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandyGrant {
    pub window_key: String,
    pub window_name: String,
    pub count: i64,
}

/// Edge-triggered candy decision (port of `CompanionStore.evaluateCandyGrants`):
/// grants only when a window *crosses* 100%; a drop below 100% re-arms it; already-granted
/// windows (tier ≥ 1) never re-grant.
pub fn evaluate_candy_grants(
    windows: &[CandyWindow],
    grant_tier: &mut HashMap<String, i64>,
) -> Vec<CandyGrant> {
    let mut grants = Vec::new();
    for w in windows {
        if w.utilization < 100.0 {
            grant_tier.remove(&w.key);
            continue;
        }
        let previous = grant_tier.get(&w.key).copied().unwrap_or(0);
        if previous >= 1 {
            continue;
        }
        grant_tier.insert(w.key.clone(), 1);
        let count = if w.kind == WindowClass::Weekly {
            RareCandy::WEEKLY_GRANT
        } else {
            1
        };
        grants.push(CandyGrant {
            window_key: w.key.clone(),
            window_name: w.name.clone(),
            count,
        });
    }
    grants
}

// ---------------------------------------------------------------------------
// Odds (pure roll checks, port of `rollsShiny` / `dittoDisguiseHit`)
// ---------------------------------------------------------------------------

/// Shiny hatch check: `roll % (48 with charm, else 64) == 0`.
pub fn rolls_shiny(roll: u64, charm_owned: bool) -> bool {
    let denom = if charm_owned {
        ShinyCharm::SHINY_DENOMINATOR
    } else {
        PokemonOdds::SHINY_DENOMINATOR
    };
    roll.is_multiple_of(denom)
}

/// Ditto disguise hit: common rarity, ≥2 forms (tree depth), `roll % 128 == 0`.
pub fn ditto_disguise_hit(rarity: Rarity, total_forms: u16, roll: u64) -> bool {
    rarity == Rarity::Common
        && total_forms >= 2
        && roll.is_multiple_of(PokemonOdds::DITTO_DISGUISE_DENOMINATOR)
}

// ---------------------------------------------------------------------------
// Dex + events + state
// ---------------------------------------------------------------------------

/// Graduated companion record (port of `DexEntry`; per-chain multilingual names are omitted —
/// the static pool already resolves them, so there is no network to backfill).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DexEntry {
    pub base_id: u16,
    pub final_id: u16,
    /// Chain order, base → final (species ids).
    pub chain_order: Vec<u16>,
    pub rarity: Rarity,
    /// RFC-3339 catch time (None when unavailable, e.g. headless grants).
    pub caught_at: Option<String>,
    pub is_shiny: bool,
    pub nature: Option<Nature>,
}

impl Default for DexEntry {
    fn default() -> Self {
        Self {
            base_id: 0,
            final_id: 0,
            chain_order: Vec::new(),
            rarity: Rarity::Common,
            caught_at: None,
            is_shiny: false,
            nature: None,
        }
    }
}

/// Transient progression events emitted by [`CompanionState::add_tokens`] / item use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompanionEvent {
    Hatched {
        /// Base slug (stable id for the line).
        slug: String,
        /// English species name (display localizes via i18n/pool).
        species: String,
        is_shiny: bool,
    },
    Evolved {
        to: String,
    },
    Graduated {
        species: String,
    },
    DittoRevealed {
        /// English name of the species it was disguised as.
        disguise: String,
    },
}

impl CompanionEvent {
    /// True when this event opens a short "levelUp" display window (hatch/evolve/graduate/reveal).
    pub fn is_celebration(&self) -> bool {
        matches!(
            self,
            CompanionEvent::Hatched { .. }
                | CompanionEvent::Evolved { .. }
                | CompanionEvent::Graduated { .. }
                | CompanionEvent::DittoRevealed { .. }
        )
    }
}

/// Result of using a Rare Candy (port of `CandyUseResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandyUseResult {
    Evolved,
    Graduated,
    Progressed,
    Unavailable,
}

/// Persisted companion state.
///
/// The legacy flat fields (`eggProgress`, `lineKey`, `formIndex`, `phaseProgress`,
/// `graduated`, `dex`, `seed`, `lastDay`, `dayApplied`) keep their names and meanings; the
/// Phase 1b additions all default, so old files load as-is and are normalized in
/// [`CompanionState::migrate`]. `lineKey` holds a base species slug from the full pool
/// (`""` = still an egg).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompanionState {
    /// Tokens accumulated toward cracking the egg (0 once hatched).
    pub egg_progress: i64,
    /// Hatched line slug ("" while still an egg).
    pub line_key: String,
    /// 0-based stage index within the realized path.
    pub form_index: i64,
    /// Tokens accumulated at the current form toward the next form/graduation.
    pub phase_progress: i64,
    /// (Port-specific, kept from Phase 1b step 1) the final form was reached and is kept on
    /// display instead of immediately receiving a new egg.
    pub graduated: bool,
    /// Base slugs ever hatched (a lean Pokédex).
    pub dex: Vec<String>,
    /// LCG seed — persisted so all rolls stay deterministic per install.
    pub seed: u64,
    /// Day key ("YYYY-MM-DD") the counter was last reconciled against + that day's applied total.
    pub last_day: String,
    pub day_applied: i64,

    // ---- Phase 1b additions (all defaulted; absent from legacy files) ----
    /// Lifetime tokens used (wallet source; growth meter, never rewound by purchases).
    pub used_since_install: i64,
    /// Lifetime tokens spent in the shop (currency = usedSinceInstall − spentTokens).
    pub spent_tokens: i64,
    /// Rarity floor the current egg guarantees (premium egg); consumed at hatch.
    /// `None` = no guarantee. Never coexists with an active companion.
    pub egg_tier: Option<Rarity>,
    /// Shiny, fixed at hatch, kept through evolutions.
    pub is_shiny: bool,
    /// Nature, fixed at hatch (None only on legacy/undetermined individuals).
    pub nature: Option<Nature>,
    /// Realized evolution path (species ids); empty pre-migration.
    pub path: Vec<u16>,
    /// Full planned route to a leaf (chosen at hatch, repaired when invalid).
    pub planned_path: Vec<u16>,
    /// Owned "base:final" pairs — branch diversity + hatch-weight halving.
    pub collected_finals: Vec<String>,
    /// Graduated companions (permanent dex).
    pub dex_entries: Vec<DexEntry>,
    /// Inventory (ItemKind raw → count).
    pub inventory: HashMap<String, i64>,
    /// Candy grant edge state (window key → granted tier); persisted to stop infinite
    /// re-grants across restarts.
    pub candy_grant_tier: HashMap<String, i64>,
    /// First candy-grant seed completed (blocks retroactive grants).
    pub candy_feature_seeded: bool,
    /// UI language code ("en" default; "ko"/"ja"/"es").
    pub language: String,
    /// Ditto disguise: None = normal; Some(id) = disguised as that species.
    pub ditto_disguise: Option<u16>,
    /// Disguise → reveal switch performed.
    pub ditto_revealed: bool,
}

impl Default for CompanionState {
    fn default() -> Self {
        Self {
            egg_progress: 0,
            line_key: String::new(),
            form_index: 0,
            phase_progress: 0,
            graduated: false,
            dex: Vec::new(),
            seed: 0x9e3779b97f4a7c15,
            last_day: String::new(),
            day_applied: 0,
            used_since_install: 0,
            spent_tokens: 0,
            egg_tier: None,
            is_shiny: false,
            nature: None,
            path: Vec::new(),
            planned_path: Vec::new(),
            collected_finals: Vec::new(),
            dex_entries: Vec::new(),
            inventory: HashMap::new(),
            candy_grant_tier: HashMap::new(),
            candy_feature_seeded: false,
            language: "en".into(),
            ditto_disguise: None,
            ditto_revealed: false,
        }
    }
}

/// The old 9-line representative pool's form lists, keyed by base slug — used only to migrate
/// legacy `formIndex` values onto the real evolution trees.
const LEGACY_LINES: &[(&str, &[&str])] = &[
    ("bulbasaur", &["bulbasaur", "ivysaur", "venusaur"]),
    ("charmander", &["charmander", "charmeleon", "charizard"]),
    ("squirtle", &["squirtle", "wartortle", "blastoise"]),
    ("pikachu", &["pikachu", "raichu"]),
    ("eevee", &["eevee", "vaporeon", "jolteon", "flareon"]),
    ("gastly", &["gastly", "haunter", "gengar"]),
    ("dratini", &["dratini", "dragonair", "dragonite"]),
    ("mewtwo", &["mewtwo"]),
    ("mew", &["mew"]),
];

impl CompanionState {
    pub fn is_egg(&self) -> bool {
        self.line_key.is_empty()
    }

    /// The hatched line's metadata (None for an egg or an unresolvable slug).
    pub fn line(&self) -> Option<pool::LineMeta> {
        if self.is_egg() {
            return None;
        }
        pool::line_by_slug(&self.line_key)
    }

    pub fn base_id(&self) -> Option<u16> {
        self.line().map(|l| l.base_id)
    }

    /// Current species id (last realized path id; base when the path is empty).
    pub fn current_id(&self) -> u16 {
        self.path
            .last()
            .copied()
            .unwrap_or_else(|| self.line().map(|l| l.base_id).unwrap_or(0))
    }

    /// Current species name in the state's language (the egg placeholder when hatching).
    pub fn species(&self) -> String {
        if self.is_egg() {
            return "Egg".to_string();
        }
        let Some(s) = pool::species_by_id(self.current_id()) else {
            return "??".to_string();
        };
        let lang = Language::from_code(&self.language).unwrap_or(Language::En);
        let name = match lang {
            Language::Ko => s.ko,
            Language::Ja => s.ja,
            Language::Es => s.es,
            Language::Fr => s.fr,
            Language::En => s.en.clone(),
        };
        if name.is_empty() {
            s.en
        } else {
            name
        }
    }

    /// English name — sprite lookups must use this (the PokéAPI slug is English-only).
    pub fn species_en(&self) -> String {
        if self.is_egg() {
            return "Egg".to_string();
        }
        pool::species_by_id(self.current_id())
            .map(|s| s.en)
            .unwrap_or_else(|| "??".to_string())
    }

    pub fn total_forms(&self) -> i64 {
        self.planned_path.len() as i64
    }

    pub fn rarity(&self) -> Option<Rarity> {
        self.line().map(|l| l.rarity)
    }

    /// True when disguised and not yet revealed (the shiny is hidden until the reveal).
    pub fn is_disguised(&self) -> bool {
        self.ditto_disguise.is_some() && !self.ditto_revealed
    }

    /// Shiny as displayed (hidden while disguised, mirroring `currentIsShiny`).
    pub fn current_is_shiny(&self) -> bool {
        self.is_shiny && !self.is_disguised()
    }

    pub fn current_nature(&self) -> Option<Nature> {
        self.nature
    }

    /// Cost to reach the next form / graduation, `None` when graduated or still an egg.
    pub fn next_cost(&self) -> Option<i64> {
        let line = self.line()?;
        if self.graduated {
            return None;
        }
        Some(phase_threshold(
            line.rarity,
            self.total_forms(),
            self.form_index,
        ))
    }

    /// Progress fraction toward the next transition in `0.0..=1.0` (1.0 when an egg is full).
    pub fn progress_fraction(&self) -> f64 {
        if self.is_egg() {
            return (self.egg_progress as f64 / EGG_HATCH_THRESHOLD as f64).clamp(0.0, 1.0);
        }
        match self.next_cost() {
            Some(c) if c > 0 => (self.phase_progress as f64 / c as f64).clamp(0.0, 1.0),
            _ => 1.0,
        }
    }

    /// Add tokens (from the day counter, or a candy's XP via item use): feeds the egg, then the
    /// current form; hatches / evolves / reveals / graduates in a burst with surplus carry.
    pub fn add_tokens(&mut self, tokens: i64) -> Vec<CompanionEvent> {
        let mut events: Vec<CompanionEvent> = Vec::new();
        if tokens > 0 {
            self.used_since_install = self.used_since_install.saturating_add(tokens);
            let mut tokens = tokens;
            // 1) Egg: pour tokens in until it hatches; the leftover carries into the hatched form.
            while self.is_egg() && tokens > 0 {
                let need = EGG_HATCH_THRESHOLD - self.egg_progress;
                if need <= 0 {
                    break;
                }
                if tokens < need {
                    self.egg_progress += tokens;
                    tokens = 0;
                    break;
                }
                self.egg_progress = need;
                tokens -= need;
                match self.hatch() {
                    Some(ev) => {
                        self.egg_progress = 0;
                        events.push(ev);
                    }
                    // No candidate (unsatisfiable guarantee): keep the egg; sanitize()
                    // drops such tiers at load, so this is unreachable in practice.
                    None => {
                        self.egg_progress = 0;
                        break;
                    }
                }
            }
            // 2) Hatched: remaining tokens feed the current phase.
            if !self.is_egg() {
                self.phase_progress = self.phase_progress.saturating_add(tokens);
            }
        }
        // 3) Resolve any reached thresholds (also covers a disguised mon whose reveal
        //    threshold was crossed by an earlier save).
        events.extend(self.apply_growth());
        events
    }

    /// Apply threshold crossings (port of `applyUsage`): evolve along the planned path, reveal a
    /// disguised Ditto at its first evolution threshold, graduate at the leaf.
    fn apply_growth(&mut self) -> Vec<CompanionEvent> {
        let mut events: Vec<CompanionEvent> = Vec::new();
        let mut guard = 0;
        while !self.graduated && guard < 50 {
            guard += 1;
            let Some(line) = self.line() else {
                break;
            };
            let k = self.total_forms();
            let thr = phase_threshold(line.rarity, k, self.form_index);
            if thr <= 0 || self.phase_progress < thr {
                break;
            }
            // A disguised Ditto reveals at the first evolution threshold instead of evolving.
            if let Some(disguise) = self.ditto_disguise {
                if !self.ditto_revealed {
                    events.push(self.reveal_ditto(thr, disguise));
                    continue;
                }
            }
            let cur = self.current_id();
            if pool::children_of(cur).is_empty() || self.form_index >= k - 1 {
                let species = self.species_en();
                self.graduate();
                events.push(CompanionEvent::Graduated { species });
                break;
            }
            let (next, repaired) = pick_next_form_computed(
                &self.line_key,
                self.form_index,
                &self.path,
                &self.planned_path,
                &self.collected_finals,
                cur,
                &mut self.seed,
            );
            if let Some(plan) = repaired {
                self.planned_path = plan;
            }
            let to = pool::en_name(next);
            self.path.push(next);
            self.form_index += 1;
            self.phase_progress -= thr;
            events.push(CompanionEvent::Evolved { to });
        }
        events
    }

    /// Disguise → reveal (port of `revealDitto`): switch to Ditto (rare, single form), carry the
    /// overflow past the first evolution threshold, keep shiny/nature.
    fn reveal_ditto(&mut self, first_thr: i64, disguise: u16) -> CompanionEvent {
        let carry = (self.phase_progress - first_thr).max(0);
        let disguise_en = pool::en_name(disguise);
        let ditto = PokemonOdds::DITTO_SPECIES_ID;
        self.line_key = pool::species_by_id(ditto)
            .map(|s| s.slug.to_string())
            .unwrap_or_default();
        self.path = vec![ditto];
        self.planned_path = vec![ditto];
        self.form_index = 0;
        self.phase_progress = carry;
        self.graduated = false;
        self.ditto_revealed = true;
        // ditto_disguise keeps the original species id (display "you thought it was …").
        CompanionEvent::DittoRevealed {
            disguise: disguise_en,
        }
    }

    /// Graduate (port of `graduate`): record the permanent dex entry + collected final.
    /// Port difference (kept from Phase 1b step 1): the companion is retained with
    /// `graduated = true` instead of immediately receiving a new egg — a shop fresh egg
    /// discards it.
    fn graduate(&mut self) {
        let line = match self.line() {
            Some(l) => l,
            None => return,
        };
        let final_id = self.current_id();
        let key = format!("{}:{}", line.base_id, final_id);
        if !self.collected_finals.contains(&key) {
            self.collected_finals.push(key);
        }
        self.dex_entries.push(DexEntry {
            base_id: line.base_id,
            final_id,
            chain_order: self.path.clone(),
            rarity: line.rarity,
            caught_at: None,
            is_shiny: self.is_shiny,
            nature: self.nature,
        });
        self.graduated = true;
    }

    /// Roll a new companion (port of `chooseBase` + the hatch rolls in `hatchCore`).
    /// RNG order: base → shiny → nature → ditto disguise → plan branch picks.
    fn hatch(&mut self) -> Option<CompanionEvent> {
        // Extract Copy data before creating the seed roll (avoids double-borrow).
        let charm = self.owns_shiny_charm();
        let tier = self.egg_tier;
        let collected: Vec<String> = self.collected_finals.clone();
        let mut roll = SeedRoll(&mut self.seed);
        let mut base = pool::choose_base(tier, &collected, &mut roll)?;
        // Guarantee safety gate (hatchCore): the static pool's tier pre-filter already
        // satisfies the guarantee; this bounded re-roll is a defensive net only.
        for _ in 0..64 {
            let below = match tier {
                Some(t) => pool::line_by_id(base)
                    .map(|l| l.rarity.sort_rank() < t.sort_rank())
                    .unwrap_or(false),
                None => false,
            };
            if !below {
                break;
            }
            base = pool::choose_base(tier, &collected, &mut roll)?;
        }
        let line = pool::line_by_id(base)?;
        let is_shiny = rolls_shiny(roll.next(), charm);
        let nature = Nature::from_index(roll.next());
        // Ditto disguise (always enabled in this port; macOS gates it on the app bundle).
        let ditto = if ditto_disguise_hit(line.rarity, pool::tree_depth(base), roll.next()) {
            Some(base)
        } else {
            None
        };
        let plan = pool::make_evolution_plan(base, &collected, &mut roll);
        self.line_key = line.slug.to_string();
        self.path = vec![base];
        self.planned_path = plan;
        self.form_index = 0;
        self.phase_progress = 0;
        self.graduated = false;
        self.is_shiny = is_shiny;
        self.nature = Some(nature);
        self.ditto_disguise = ditto;
        self.ditto_revealed = false;
        self.egg_tier = None; // the guarantee is consumed by this hatch
        if !self.dex.iter().any(|s| s == &line.slug) {
            self.dex.push(line.slug.to_string());
        }
        Some(CompanionEvent::Hatched {
            slug: line.slug,
            species: line.en.to_string(),
            is_shiny,
        })
    }

    // ---- wallet / shop (ports of the CompanionStore shop section) ----

    /// Spendable tokens = lifetime used − lifetime spent (purchases never rewind growth).
    pub fn available_tokens(&self) -> i64 {
        (self.used_since_install - self.spent_tokens).max(0)
    }

    pub fn item_count(&self, kind: ItemKind) -> i64 {
        self.inventory.get(kind.raw()).copied().unwrap_or(0)
    }

    /// Shiny Charm owned (passive: count > 0 lowers the shiny denominator for future hatches).
    pub fn owns_shiny_charm(&self) -> bool {
        self.item_count(ItemKind::ShinyCharm) > 0
    }

    /// Owned items in canonical ItemKind order (the bag list).
    pub fn owned_items(&self) -> Vec<(ItemKind, i64)> {
        ItemKind::ALL
            .iter()
            .copied()
            .map(|k| (k, self.item_count(k)))
            .filter(|(_, c)| *c > 0)
            .collect()
    }

    /// Shop rows — sold items + (with an active companion) the three egg rerolls, merged
    /// price-ascending with purchased passives last (port of `shopEntries`).
    pub fn shop_entries(&self) -> Vec<ShopEntry> {
        let mut entries: Vec<ShopEntry> = ItemKind::ALL
            .iter()
            .copied()
            .filter(|k| k.shop_price().is_some())
            .map(ShopEntry::Item)
            .collect();
        if !self.is_egg() {
            entries.extend(FreshEgg::SHOP_TIERS.map(ShopEntry::Egg));
        }
        let purchased_passive = |e: ShopEntry| -> bool {
            matches!(e, ShopEntry::Item(k) if k.is_passive() && self.item_count(k) > 0)
        };
        entries.sort_by_key(|e| (purchased_passive(*e), e.price()));
        entries
    }

    pub fn can_buy(&self, kind: ItemKind) -> bool {
        let Some(price) = kind.shop_price() else {
            return false;
        };
        if kind.is_passive() && self.item_count(kind) > 0 {
            return false; // one-time
        }
        self.available_tokens() >= price
    }

    /// Buy one item: wallet −= price, inventory +1; no effect on growth/statistics.
    pub fn buy(&mut self, kind: ItemKind) -> bool {
        let Some(price) = kind.shop_price() else {
            return false;
        };
        if self.available_tokens() < price {
            return false;
        }
        if kind.is_passive() && self.item_count(kind) > 0 {
            return false;
        }
        self.spent_tokens += price;
        *self.inventory.entry(kind.raw().to_string()).or_insert(0) += 1;
        true
    }

    /// Egg reroll availability: must have a companion to discard and enough for the tier.
    pub fn can_buy_egg(&self, tier: Option<Rarity>) -> bool {
        if !FreshEgg::SHOP_TIERS.contains(&tier) {
            return false;
        }
        !self.is_egg() && self.available_tokens() >= FreshEgg::price(tier)
    }

    /// Buy an egg: discard the current companion (NOT a graduation — dex/collectedFinals
    /// untouched, "as if never rolled") and start a fresh egg with the tier guarantee.
    pub fn buy_egg(&mut self, tier: Option<Rarity>) -> bool {
        if !self.can_buy_egg(tier) {
            return false;
        }
        self.spent_tokens += FreshEgg::price(tier);
        self.line_key.clear();
        self.path.clear();
        self.planned_path.clear();
        self.form_index = 0;
        self.phase_progress = 0;
        self.graduated = false;
        self.is_shiny = false;
        self.nature = None;
        self.ditto_disguise = None;
        self.ditto_revealed = false;
        self.egg_progress = 0; // re-incubate from zero
        self.egg_tier = tier;
        true
    }

    // ---- items (ports of `useRareCandy` / `useMint`) ----

    /// Rare Candy availability: active companion + stock (a graduated companion has no
    /// further growth, mirroring macOS where the mon is already gone after graduation).
    pub fn can_use_rare_candy(&self) -> bool {
        !self.is_egg()
            && self.line().is_some()
            && !self.graduated
            && self.item_count(ItemKind::RareCandy) > 0
    }

    /// Use one Rare Candy: +[`RareCandy::XP`] into the current form; evolution/graduation via
    /// the normal threshold path (no effect on real usage statistics).
    pub fn use_rare_candy(&mut self) -> CandyUseResult {
        if !self.can_use_rare_candy() {
            return CandyUseResult::Unavailable;
        }
        self.inventory.insert(
            ItemKind::RareCandy.raw().to_string(),
            self.item_count(ItemKind::RareCandy) - 1,
        );
        let before_stage = self.form_index;
        self.phase_progress = self.phase_progress.saturating_add(RareCandy::XP);
        let _ = self.apply_growth();
        if self.graduated {
            return CandyUseResult::Graduated;
        }
        if self.form_index > before_stage {
            return CandyUseResult::Evolved;
        }
        CandyUseResult::Progressed
    }

    pub fn can_use_mint(&self) -> bool {
        !self.is_egg() && self.item_count(ItemKind::Mint) > 0
    }

    /// Use one Mint: replace the current nature with a *different* random one (purely
    /// cosmetic). Returns the new nature, or None when unavailable (no consumption).
    pub fn use_mint(&mut self) -> Option<Nature> {
        if !self.can_use_mint() {
            return None;
        }
        let current = self.nature;
        let candidates: Vec<Nature> = Nature::ALL
            .iter()
            .copied()
            .filter(|n| Some(*n) != current)
            .collect();
        let new = {
            let mut roll = SeedRoll(&mut self.seed);
            candidates[(roll.next() % candidates.len() as u64) as usize]
        };
        self.nature = Some(new);
        self.inventory.insert(
            ItemKind::Mint.raw().to_string(),
            self.item_count(ItemKind::Mint) - 1,
        );
        Some(new)
    }

    /// Grant candies from limit windows (port of `grantCandies`): first run only seeds
    /// already-100% windows (no retroactive grant); afterwards the edge-triggered decision
    /// applies and re-arms (100% → below) must persist.
    pub fn grant_candies(
        &mut self,
        windows: &[CandyWindow],
        limits_ready: bool,
    ) -> Vec<CandyGrant> {
        if !limits_ready {
            return Vec::new();
        }
        if !self.candy_feature_seeded {
            for w in windows.iter().filter(|w| w.utilization >= 100.0) {
                self.candy_grant_tier.insert(w.key.clone(), 1);
            }
            self.candy_feature_seeded = true;
            return Vec::new();
        }
        let grants = evaluate_candy_grants(windows, &mut self.candy_grant_tier);
        for g in &grants {
            *self
                .inventory
                .entry(ItemKind::RareCandy.raw().to_string())
                .or_insert(0) += g.count;
        }
        grants
    }

    // ---- migration / sanitization ----

    /// Normalize a loaded state (legacy flat files, hand-edits, corrupt fields).
    /// Clamps token counters, drops guarantees that cannot hatch, and rebuilds the realized /
    /// planned evolution paths against the real trees (port of `SaveTransfer.sanitized` +
    /// `normalizedEvolutionState`).
    pub fn migrate(&mut self) {
        const MAX_TOKEN: i64 = i64::MAX / 4;
        let clamp = |v: i64| v.clamp(0, MAX_TOKEN);
        self.used_since_install = clamp(self.used_since_install);
        self.spent_tokens = clamp(self.spent_tokens);
        self.egg_progress = clamp(self.egg_progress);
        self.phase_progress = clamp(self.phase_progress);
        self.day_applied = clamp(self.day_applied);
        // A guarantee only rides on an egg; an unsatisfiable one (legendary) would make the
        // egg forever unhatchable.
        if !self.is_egg() {
            self.egg_tier = None;
        }
        if self.egg_tier == Some(Rarity::Legendary) {
            self.egg_tier = None;
        }

        if self.is_egg() {
            return;
        }
        let Some(line) = pool::line_by_slug(&self.line_key) else {
            // Unknown slug (unknown/corrupt line) → back to an egg. Nothing else to recover.
            self.line_key.clear();
            self.path.clear();
            self.planned_path.clear();
            self.form_index = 0;
            self.phase_progress = 0;
            self.graduated = false;
            self.is_shiny = false;
            self.nature = None;
            self.ditto_disguise = None;
            self.ditto_revealed = false;
            return;
        };
        let base = line.base_id;

        let (realized, mut planned, _stage) = if self.path.is_empty() {
            // Legacy flat file: the realized path was the old table's linear form list.
            let old_slug = LEGACY_LINES
                .iter()
                .find(|(slug, _)| *slug == line.slug)
                .and_then(|(_, forms)| {
                    forms.get(self.form_index.clamp(0, forms.len() as i64 - 1) as usize)
                })
                .copied();
            let old_id = old_slug.and_then(pool::species_by_slug).map(|s| s.id);
            match old_id.and_then(|id| pool::path_from_root_to(base, id)) {
                Some(path) => (path, Vec::new(), 0),
                None => (vec![base], Vec::new(), 0),
            }
        } else {
            let mut roll = SeedRoll(&mut self.seed);
            pool::normalize_evolution(
                &self.path,
                &self.planned_path,
                base,
                &self.collected_finals,
                &mut roll,
            )
        };

        if planned.is_empty() {
            // Legacy: no stored plan — re-plan from the realized node (Swift: realized +
            // suffix, which is exactly this for an empty plan).
            let mut roll = SeedRoll(&mut self.seed);
            let suffix = pool::make_evolution_plan(
                *realized.last().unwrap_or(&base),
                &self.collected_finals,
                &mut roll,
            );
            planned = realized
                .iter()
                .chain(suffix.iter().skip(1))
                .copied()
                .collect();
        }

        self.path = realized;
        self.planned_path = planned;
        let last = (self.path.len() - 1) as i64;
        self.form_index = (self.form_index.clamp(0, 12)).min(last);

        // Legacy dex (hatched slugs) → permanent entries for lines other than the current one
        // (the current companion's line is already represented by the active state).
        if self.dex_entries.is_empty() && !self.dex.is_empty() {
            for slug in self.dex.clone() {
                if slug == line.slug {
                    continue;
                }
                if let Some(l) = pool::line_by_slug(&slug) {
                    let plan = canonical_plan(l.base_id);
                    let final_id = *plan.last().unwrap_or(&l.base_id);
                    let key = format!("{}:{}", l.base_id, final_id);
                    if !self.collected_finals.contains(&key) {
                        self.collected_finals.push(key);
                    }
                    self.dex_entries.push(DexEntry {
                        base_id: l.base_id,
                        final_id,
                        chain_order: plan,
                        rarity: l.rarity,
                        caught_at: None,
                        is_shiny: false,
                        nature: None,
                    });
                }
            }
        }
    }
}

/// Deterministic default plan for migration (smallest-id child at each branch; consumes no
/// persisted RNG).
fn canonical_plan(root: u16) -> Vec<u16> {
    let mut plan = vec![root];
    let mut node = root;
    while !pool::children_of(node).is_empty() {
        node = pool::children_of(node).iter().copied().min().unwrap();
        plan.push(node);
    }
    plan
}

/// Choose the next form (port of the evolve branch of `applyUsage`): the planned child when
/// still valid, otherwise a fresh pick plus plan repair. Free function to avoid double-borrow
/// of `self.seed` through a method call. Returns (next_id, optional_repaired_plan).
fn pick_next_form_computed(
    line_key: &str,
    form_index: i64,
    path: &[u16],
    planned_path: &[u16],
    collected_finals: &[String],
    cur: u16,
    seed: &mut u64,
) -> (u16, Option<Vec<u16>>) {
    let kids = pool::children_of(cur);
    let base = pool::line_by_slug(line_key)
        .map(|l| l.base_id)
        .unwrap_or_else(|| pool::species_by_id(cur).map(|s| s.id).unwrap_or(cur));
    let next_index = form_index + 1;
    if let Some(&planned) = planned_path.get(next_index as usize) {
        if kids.contains(&planned) {
            return (planned, None);
        }
    }
    let mut roll = SeedRoll(seed);
    let next = pool::pick_planned_child(cur, base, collected_finals, roll.next());
    let fallback_route: Vec<u16> = std::iter::once(cur)
        .chain(pool::make_evolution_plan(next, collected_finals, &mut roll))
        .collect();
    let prefix: Vec<u16> = path.iter().take(form_index as usize + 1).copied().collect();
    let repaired = if !prefix.is_empty() && fallback_route.first() == prefix.last() {
        let mut p = prefix;
        p.extend(fallback_route.iter().skip(1));
        Some(p)
    } else {
        None
    };
    (next, repaired)
}

// ---------------------------------------------------------------------------
// Day counter + display state
// ---------------------------------------------------------------------------

/// Reconcile a *day counter* (e.g. today's total) into a non-negative forward delta, so repeated
/// snapshot calls for the same day apply only the growth since the last call. Returns the delta
/// to feed to [`add_tokens`] (the state's `last_day`/`day_applied` are updated here).
pub fn day_delta(state: &mut CompanionState, today_key: &str, today_total: i64) -> i64 {
    if state.last_day != today_key {
        state.last_day = today_key.to_string();
        state.day_applied = 0;
    }
    let delta = today_total.saturating_sub(state.day_applied).max(0);
    if delta > 0 {
        state.day_applied = today_total;
    }
    delta
}

/// Burn-rate tier (port of `UsageStore.burnTier`, combined tokens/minute across providers):
/// ≤1,000 idle · <100,000 normal · <400,000 fast · else blazing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BurnTier {
    Idle,
    Normal,
    Fast,
    Blazing,
}

pub fn burn_tier(tokens_per_minute: f64) -> BurnTier {
    if tokens_per_minute <= 1_000.0 {
        BurnTier::Idle
    } else if tokens_per_minute < 100_000.0 {
        BurnTier::Normal
    } else if tokens_per_minute < 400_000.0 {
        BurnTier::Fast
    } else {
        BurnTier::Blazing
    }
}

/// Inputs to the display rule (the snapshot the caller already has).
pub struct DisplayInput {
    /// Combined tokens/minute burn; `None` = no active block observed.
    pub tpm: Option<f64>,
    /// A limit window is at/over the warning threshold.
    pub limit_warning: bool,
    /// Any usage snapshot exists for today's view.
    pub has_usage_data: bool,
    /// Today's combined token total.
    pub today_total: i64,
    /// A hatch/evolve/graduate celebration window is alive (the caller owns the window).
    pub celebration: bool,
}

/// The exact macOS display-state rule (port of `CompanionStore.computeState`):
/// egg → levelUp (celebration) → tired (limit warning) → sleep (no data / zero today)
/// → idle / working / focus by burn tier.
pub fn display_state(state: &CompanionState, input: &DisplayInput) -> StateKind {
    if state.is_egg() {
        return StateKind::Egg;
    }
    if input.celebration {
        return StateKind::LevelUp;
    }
    if input.limit_warning {
        return StateKind::Tired;
    }
    if !input.has_usage_data || input.today_total == 0 {
        return StateKind::Sleep;
    }
    match burn_tier(input.tpm.unwrap_or(0.0)) {
        BurnTier::Idle => StateKind::Idle,
        BurnTier::Normal => StateKind::Working,
        BurnTier::Fast | BurnTier::Blazing => StateKind::Focus,
    }
}

/// Localized status line for the current display state (port of the CompanionView status text).
/// For `LevelUp`, the caller composes the "Evolved into X!" variant from the event's `to` name
/// via `l.status_evolved(name)`; this helper returns the generic "it grew" copy.
pub fn status_text(_state: &CompanionState, kind: StateKind, l: &L) -> String {
    match kind {
        StateKind::Egg => l.status_egg().to_string(),
        StateKind::Idle => l.status_idle().to_string(),
        StateKind::Working => l.status_working().to_string(),
        StateKind::Focus => l.status_focus().to_string(),
        StateKind::Tired => l.status_tired().to_string(),
        StateKind::Sleep => l.status_sleep().to_string(),
        StateKind::LevelUp => l.status_grew().to_string(),
    }
}

/// State file under the app data dir (respects `PTB_STATE_DIR` via `paths::state_file`).
pub fn state_path() -> Option<PathBuf> {
    crate::paths::state_file()
}

pub fn load() -> CompanionState {
    let mut state: CompanionState = state_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    state.migrate();
    state
}

pub fn save(state: &CompanionState) -> anyhow::Result<()> {
    let path = state_path().ok_or_else(|| anyhow::anyhow!("no data dir"))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Persisted LCG (xorshift-style) step.
fn next_seed(seed: u64) -> u64 {
    let mut x = seed;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// Adapter exposing the state's persisted seed as a [`pool::Roll`] stream.
struct SeedRoll<'a>(&'a mut u64);

impl pool::Roll for SeedRoll<'_> {
    fn next(&mut self) -> u64 {
        *self.0 = next_seed(*self.0);
        *self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The per-test state dir plus a guard that serializes every companion-state test.
    ///
    /// `PTB_STATE_DIR` is a **process-global** env var read by [`load`]/[`save`] at call time,
    /// so parallel test threads would otherwise stomp on each other's value between a test's
    /// `set_var` and its `load`/`save`. Holding the mutex for the test's whole lifetime (until
    /// this value is dropped) makes those tests run one at a time and keeps the suite stable.
    #[allow(dead_code)] // field 1 (the guard) is intentionally held, never read
    struct Isolated(std::path::PathBuf, std::sync::MutexGuard<'static, ()>);

    impl std::ops::Deref for Isolated {
        type Target = std::path::PathBuf;
        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl AsRef<std::path::Path> for Isolated {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }

    fn isolated() -> Isolated {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ptb-companion-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("PTB_STATE_DIR", &dir);
        Isolated(dir, guard)
    }

    #[test]
    fn balance_thresholds_and_phase_sum() {
        // Phase costs of a k-form line must sum to the rarity's graduation total.
        for k in [1, 2, 3, 4] {
            let sum: i64 = (0..k).map(|f| phase_threshold(Rarity::Common, k, f)).sum();
            assert_eq!(sum, graduation_total(Rarity::Common));
        }
        assert_eq!(phase_threshold(Rarity::Common, 3, 0), 125_000_000);
        assert_eq!(phase_threshold(Rarity::Common, 3, 1), 250_000_000);
        assert_eq!(phase_threshold(Rarity::Common, 3, 2), 375_000_000);
    }

    #[test]
    fn rarity_tiers() {
        assert_eq!(Rarity::from_capture_rate(255, false, false), Rarity::Common);
        assert_eq!(
            Rarity::from_capture_rate(100, false, false),
            Rarity::Uncommon
        );
        assert_eq!(Rarity::from_capture_rate(40, false, false), Rarity::Rare);
        assert_eq!(
            Rarity::from_capture_rate(10, true, false),
            Rarity::Legendary
        );
        assert!(!Rarity::Legendary.includes(1)); // legendaries are flag-only
    }

    #[test]
    fn egg_hatches_with_surplus_carry() {
        let mut s = CompanionState {
            seed: 1,
            ..Default::default()
        };
        let ev = s.add_tokens(EGG_HATCH_THRESHOLD + 1_000);
        assert!(matches!(ev[0], CompanionEvent::Hatched { .. }));
        assert!(!s.is_egg());
        assert_eq!(s.egg_progress, 0); // consumed at hatch
        assert_eq!(s.phase_progress, 1_000); // surplus went into the first phase
        assert_eq!(s.dex.len(), 1);
        assert!(s.nature.is_some(), "nature fixed at hatch");
        assert_eq!(s.used_since_install, EGG_HATCH_THRESHOLD + 1_000);
    }

    #[test]
    fn evolves_through_forms_then_graduates() {
        // A 1-form legendary line (Mewtwo) graduates after exactly the graduation total.
        let mut s = CompanionState {
            line_key: "mewtwo".into(),
            seed: 1,
            ..Default::default()
        };
        s.migrate();
        let cost = phase_threshold(Rarity::Legendary, 1, 0); // == graduation_total(legendary)
        let ev = s.add_tokens(cost);
        assert!(s.graduated);
        assert!(matches!(ev.last(), Some(CompanionEvent::Graduated { .. })));
        assert_eq!(s.dex_entries.len(), 1);
        assert_eq!(s.collected_finals, vec!["150:150".to_string()]);
    }

    #[test]
    fn multi_form_evolution_sequence() {
        let mut s = CompanionState {
            line_key: "caterpie".into(),
            ..Default::default()
        }; // 3 common forms: Caterpie → Metapod → Butterfree
        s.migrate();
        for i in 0..2 {
            let ev = s.add_tokens(phase_threshold(Rarity::Common, 3, i));
            assert!(matches!(ev.last(), Some(CompanionEvent::Evolved { .. })));
        }
        assert_eq!(s.form_index, 2);
        assert_eq!(s.species(), "Butterfree");
        assert!(!s.graduated);
        // The third (final) phase graduates.
        let ev = s.add_tokens(phase_threshold(Rarity::Common, 3, 2));
        assert!(s.graduated);
        assert!(matches!(ev.last(), Some(CompanionEvent::Graduated { .. })));
    }

    #[test]
    fn day_delta_monotonic_within_a_day() {
        let mut s = CompanionState {
            line_key: "pikachu".into(),
            ..Default::default()
        };
        assert_eq!(day_delta(&mut s, "2026-01-02", 1_000), 1_000);
        assert_eq!(day_delta(&mut s, "2026-01-02", 1_600), 600); // growth only
        assert_eq!(day_delta(&mut s, "2026-01-02", 1_500), 0); // never negative
        assert_eq!(day_delta(&mut s, "2026-01-03", 500), 500); // new day resets
    }

    #[test]
    fn roundtrip_json() {
        let mut s = CompanionState {
            line_key: "eevee".into(),
            form_index: 1,
            phase_progress: 123,
            ..Default::default()
        };
        s.migrate();
        let txt = serde_json::to_string(&s).unwrap();
        let back: CompanionState = serde_json::from_str(&txt).unwrap();
        assert_eq!(back.line_key, "eevee");
        // Legacy linear form 1 (Vaporeon) migrates to the real tree path Eevee -> Vaporeon.
        assert_eq!(back.path, vec![133, 134]);
        assert_eq!(back.form_index, 1);
        assert_eq!(back.species(), "Vaporeon");
    }

    // ---- legacy file migration ----

    #[test]
    fn legacy_state_file_migrates() {
        let dir = isolated();
        // The exact shape of the real user file on this machine (Phase 1b step 1 schema).
        let legacy = r#"{
  "eggProgress": 0,
  "lineKey": "charmander",
  "formIndex": 0,
  "phaseProgress": 95940237,
  "graduated": false,
  "dex": ["charmander"],
  "seed": 15860402102123842989,
  "lastDay": "2026-08-21",
  "dayApplied": 51111607
}"#;
        std::fs::write(dir.join("companion-state.json"), legacy).unwrap();
        let s = load();
        assert!(!s.is_egg());
        assert_eq!(s.line_key, "charmander");
        assert_eq!(s.path, vec![4]);
        assert_eq!(s.planned_path, vec![4, 5, 6]);
        assert_eq!(s.form_index, 0);
        assert_eq!(s.phase_progress, 95940237, "growth preserved");
        assert!(!s.graduated);
        assert_eq!(s.last_day, "2026-08-21");
        assert_eq!(s.day_applied, 51111607);
        assert_eq!(s.dex, vec!["charmander".to_string()]);
        // The real pool re-derives rarity from the official capture rate (Charmander 45 = rare).
        assert_eq!(s.rarity(), Some(Rarity::Rare));
        assert_eq!(s.species(), "Charmander");
        // Re-save and reload: stable, no re-rolls.
        save(&s).unwrap();
        let again = load();
        assert_eq!(again.seed, s.seed);
        assert_eq!(again.planned_path, s.planned_path);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_branching_line_migrates_onto_real_tree() {
        let dir = isolated();
        let legacy = r#"{
  "eggProgress": 0,
  "lineKey": "eevee",
  "formIndex": 2,
  "phaseProgress": 1000,
  "graduated": false,
  "dex": ["eevee", "bulbasaur"],
  "seed": 7,
  "lastDay": "2026-01-01",
  "dayApplied": 1000
}"#;
        std::fs::write(dir.join("companion-state.json"), legacy).unwrap();
        let s = load();
        // Old linear form 2 = Jolteon → real path Eevee(133) -> Jolteon(135), stage 1.
        assert_eq!(s.path, vec![133, 135]);
        assert_eq!(s.form_index, 1);
        assert_eq!(s.species(), "Jolteon");
        assert!(
            s.planned_path.starts_with(&[133, 135]),
            "{:?}",
            s.planned_path
        );
        // Legacy dex: the other hatched line becomes a permanent entry.
        assert_eq!(s.dex_entries.len(), 1);
        assert_eq!(s.dex_entries[0].base_id, 1);
        assert!(s.collected_finals.iter().any(|k| k.starts_with("1:")));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_egg_file_loads() {
        let dir = isolated();
        let legacy = r#"{
  "eggProgress": 1234567,
  "lineKey": "",
  "formIndex": 0,
  "phaseProgress": 0,
  "graduated": false,
  "dex": [],
  "seed": 3,
  "lastDay": "",
  "dayApplied": 0
}"#;
        std::fs::write(dir.join("companion-state.json"), legacy).unwrap();
        let s = load();
        assert!(s.is_egg());
        assert_eq!(s.egg_progress, 1_234_567);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- hatch determinism + odds ----

    #[test]
    fn hatch_sequence_is_deterministic_per_seed() {
        let mut a = CompanionState {
            seed: 0xCAFE,
            ..Default::default()
        };
        let mut b = CompanionState {
            seed: 0xCAFE,
            ..Default::default()
        };
        let events_a: Vec<String> = (0..5)
            .map(|_| {
                let ev = a.add_tokens(EGG_HATCH_THRESHOLD + graduation_total(Rarity::Legendary));
                a.buy_egg(None);
                format!("{ev:?}")
            })
            .collect();
        let events_b: Vec<String> = (0..5)
            .map(|_| {
                let ev = b.add_tokens(EGG_HATCH_THRESHOLD + graduation_total(Rarity::Legendary));
                b.buy_egg(None);
                format!("{ev:?}")
            })
            .collect();
        assert_eq!(events_a, events_b, "same seed → same hatches");
    }

    #[test]
    fn shiny_rolls_match_odds() {
        // Statistical: the LCG stream should hit ~1/64 without charm, ~1/48 with.
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            seed = next_seed(seed);
            seed
        };
        let n = 640_000u64;
        let plain = (0..n).filter(|_| rolls_shiny(next(), false)).count();
        let charmed = (0..n).filter(|_| rolls_shiny(next(), true)).count();
        let target_plain = n / 64;
        let target_charmed = n / 48;
        assert!(
            (plain as i64 - target_plain as i64).abs() < target_plain as i64 / 8,
            "plain shiny {plain} vs ~{target_plain}"
        );
        assert!(
            (charmed as i64 - target_charmed as i64).abs() < target_charmed as i64 / 8,
            "charmed shiny {charmed} vs ~{target_charmed}"
        );
        assert!(charmed > plain, "charm must improve the odds");
        // Pure checks: roll 0 is always shiny; the denominators are exact.
        assert!(rolls_shiny(0, false) && rolls_shiny(0, true));
        assert!(rolls_shiny(48, true) && !rolls_shiny(48, false));
        assert!(rolls_shiny(64, false) && !rolls_shiny(64, true)); // 64%48=16
        assert!(!rolls_shiny(48, false)); // 48%64=48
    }

    #[test]
    fn nature_distribution_is_uniform() {
        let mut seed = 42u64;
        let mut counts = [0u32; 25];
        let n = 25_000u32;
        for _ in 0..n {
            seed = next_seed(seed);
            let idx = (seed % 25) as usize;
            counts[idx] += 1;
        }
        let expected = n / 25;
        for (i, c) in counts.iter().enumerate() {
            assert!(
                (*c as i64 - expected as i64).abs() < expected as i64 / 4,
                "nature {i}: {c} vs ~{expected}"
            );
        }
    }

    #[test]
    fn ditto_disguise_gates() {
        assert!(ditto_disguise_hit(Rarity::Common, 2, 0));
        assert!(ditto_disguise_hit(Rarity::Common, 3, 128));
        assert!(
            !ditto_disguise_hit(Rarity::Common, 1, 0),
            "single-form excluded"
        );
        assert!(!ditto_disguise_hit(Rarity::Rare, 3, 0), "rare excluded");
        assert!(!ditto_disguise_hit(Rarity::Legendary, 2, 0));
        assert!(!ditto_disguise_hit(Rarity::Common, 2, 127));
    }

    // ---- premium eggs ----

    #[test]
    fn premium_egg_prices_and_tiers() {
        assert_eq!(FreshEgg::price(None), 1_000_000_000);
        assert_eq!(FreshEgg::price(Some(Rarity::Uncommon)), 2_500_000_000);
        assert_eq!(FreshEgg::price(Some(Rarity::Rare)), 4_000_000_000);
        assert_eq!(
            FreshEgg::SHOP_TIERS,
            [None, Some(Rarity::Uncommon), Some(Rarity::Rare)]
        );
    }

    #[test]
    fn buy_egg_discards_without_dex_impact() {
        let mut s = CompanionState {
            line_key: "charmander".into(),
            used_since_install: 5_000_000_000,
            seed: 7,
            ..Default::default()
        };
        s.migrate();
        s.phase_progress = 200_000_000;
        s.dex_entries.push(DexEntry {
            base_id: 1,
            final_id: 3,
            chain_order: vec![1, 2, 3],
            rarity: Rarity::Common,
            caught_at: None,
            is_shiny: false,
            nature: None,
        });
        let dex_before = s.dex_entries.clone();
        let finals_before = s.collected_finals.clone();

        assert!(s.buy_egg(Some(Rarity::Rare)));
        assert!(s.is_egg());
        assert_eq!(s.egg_progress, 0, "re-incubate from zero");
        assert_eq!(s.egg_tier, Some(Rarity::Rare));
        assert_eq!(s.spent_tokens, FreshEgg::price(Some(Rarity::Rare)));
        assert_eq!(s.available_tokens(), 1_000_000_000);
        assert_eq!(s.dex_entries, dex_before, "permanent dex untouched");
        assert_eq!(
            s.collected_finals, finals_before,
            "probability weights untouched"
        );
        assert_eq!(s.line_key, "");
    }

    #[test]
    fn buy_egg_gates() {
        // No active companion → nothing to discard.
        let mut s = CompanionState {
            used_since_install: 10_000_000_000,
            ..Default::default()
        };
        assert!(!s.can_buy_egg(None));
        assert!(!s.buy_egg(None));
        assert_eq!(s.spent_tokens, 0);
        // Insufficient funds.
        let mut s = CompanionState {
            line_key: "mew".into(),
            used_since_install: 500_000_000,
            ..Default::default()
        };
        s.migrate();
        assert!(!s.can_buy_egg(None));
        assert!(!s.buy_egg(None));
        assert!(!s.is_egg(), "companion kept");
        // Unsold tier (legendary) is never purchasable.
        assert!(!s.can_buy_egg(Some(Rarity::Legendary)));
        assert!(!s.buy_egg(Some(Rarity::Legendary)));
    }

    #[test]
    fn guaranteed_egg_only_hatches_at_or_above_tier() {
        for seed in [1u64, 7, 42, 0xDEAD_BEEF] {
            let mut s = CompanionState {
                egg_tier: Some(Rarity::Rare),
                seed,
                ..Default::default()
            };
            let ev = s.add_tokens(EGG_HATCH_THRESHOLD);
            match ev.first() {
                Some(CompanionEvent::Hatched { .. }) => {}
                other => panic!("expected hatch, got {other:?}"),
            }
            let rarity = s.rarity().unwrap();
            assert!(
                rarity.sort_rank() >= Rarity::Rare.sort_rank(),
                "seed {seed}: hatched {rarity:?} below guaranteed rare"
            );
            assert_eq!(s.egg_tier, None, "guarantee consumed at hatch");
        }
    }

    #[test]
    fn legendary_tier_is_dropped_at_migration() {
        let mut s = CompanionState {
            egg_tier: Some(Rarity::Legendary),
            ..Default::default()
        };
        s.migrate();
        assert_eq!(
            s.egg_tier, None,
            "unsatisfiable guarantee would never hatch"
        );
        // And an egg tier never coexists with an active companion.
        let mut s = CompanionState {
            line_key: "mew".into(),
            egg_tier: Some(Rarity::Rare),
            ..Default::default()
        };
        s.migrate();
        assert_eq!(s.egg_tier, None);
    }

    // ---- shop / items ----

    #[test]
    fn shop_purchase_flow() {
        let mut s = CompanionState {
            used_since_install: 1_000_000_000,
            ..Default::default()
        };
        assert_eq!(s.available_tokens(), 1_000_000_000);
        // Mint (100M) affordable; candy (500M) affordable; charm (3B) not.
        assert!(s.buy(ItemKind::Mint));
        assert!(s.buy(ItemKind::RareCandy));
        assert!(!s.buy(ItemKind::ShinyCharm));
        assert_eq!(s.spent_tokens, Mint::PRICE + RareCandy::PRICE);
        assert_eq!(s.available_tokens(), 400_000_000);
        assert_eq!(s.item_count(ItemKind::Mint), 1);
        assert_eq!(s.item_count(ItemKind::RareCandy), 1);
        // Passive one-time purchase.
        let mut s2 = CompanionState {
            used_since_install: 10_000_000_000,
            ..Default::default()
        };
        assert!(s2.buy(ItemKind::ShinyCharm));
        assert!(!s2.can_buy(ItemKind::ShinyCharm), "no repurchase");
        assert!(!s2.buy(ItemKind::ShinyCharm));
        assert_eq!(s2.spent_tokens, ShinyCharm::PRICE);
        assert!(s2.owns_shiny_charm());
        // Growth meter is untouched by purchases.
        assert_eq!(s2.used_since_install, 10_000_000_000);
    }

    #[test]
    fn shop_entries_sorted_purchased_passive_last() {
        let mut s = CompanionState {
            line_key: "mew".into(),
            used_since_install: 10_000_000_000,
            ..Default::default()
        };
        s.migrate();
        let entries = s.shop_entries();
        // With an active companion: items + 3 eggs, price ascending (passive last only when owned).
        assert_eq!(
            entries.iter().map(|e| e.price()).collect::<Vec<_>>(),
            [
                Mint::PRICE,
                RareCandy::PRICE,
                FreshEgg::PRICE,
                FreshEgg::price(Some(Rarity::Uncommon)),
                ShinyCharm::PRICE,
                FreshEgg::price(Some(Rarity::Rare)),
            ]
        );
        s.buy(ItemKind::ShinyCharm);
        let entries = s.shop_entries();
        assert!(matches!(
            entries.last(),
            Some(ShopEntry::Item(ItemKind::ShinyCharm))
        ));
    }

    #[test]
    fn rare_candy_progression() {
        let mut s = CompanionState {
            line_key: "caterpie".into(),
            seed: 5,
            ..Default::default()
        };
        s.migrate();
        // No stock → unavailable.
        assert_eq!(s.use_rare_candy(), CandyUseResult::Unavailable);
        s.inventory.insert(ItemKind::RareCandy.raw().to_string(), 3);
        // Partial progress (100M < 125M first stage of a common 3-form line).
        assert_eq!(s.use_rare_candy(), CandyUseResult::Progressed);
        assert_eq!(s.phase_progress, RareCandy::XP);
        // Second candy crosses the threshold → evolve.
        assert_eq!(s.use_rare_candy(), CandyUseResult::Evolved);
        assert_eq!(s.form_index, 1);
        assert_eq!(s.item_count(ItemKind::RareCandy), 1);
        // Egg: no companion to use it on.
        let mut egg_state = CompanionState {
            used_since_install: 0,
            ..Default::default()
        };
        egg_state
            .inventory
            .insert(ItemKind::RareCandy.raw().to_string(), 1);
        assert_eq!(egg_state.use_rare_candy(), CandyUseResult::Unavailable);
    }

    #[test]
    fn mint_always_changes_nature() {
        let mut s = CompanionState {
            line_key: "caterpie".into(),
            nature: Some(Nature::Hardy),
            seed: 11,
            ..Default::default()
        };
        s.inventory.insert(ItemKind::Mint.raw().to_string(), 25);
        let mut prev = s.nature.unwrap();
        for _ in 0..25 {
            let new = s.use_mint().unwrap();
            assert_ne!(
                new, prev,
                "must pick a nature different from the current one"
            );
            assert_eq!(s.nature, Some(new));
            prev = new;
        }
        assert_eq!(s.item_count(ItemKind::Mint), 0);
        assert_eq!(s.use_mint(), None, "no stock → no-op");
    }

    // ---- candy grants ----

    fn window(key: &str, kind: WindowClass, utilization: f64) -> CandyWindow {
        CandyWindow {
            key: key.into(),
            name: key.to_string(),
            kind,
            utilization,
        }
    }

    #[test]
    fn candy_grant_edge_trigger() {
        // First run seeds (no grant).
        let mut s = CompanionState::default();
        assert!(s
            .grant_candies(&[window("claude.5h", WindowClass::Session, 100.0)], true)
            .is_empty());
        assert!(s.candy_feature_seeded);
        assert_eq!(s.item_count(ItemKind::RareCandy), 0, "no retroactive grant");
        // A new crossing grants (session = 1).
        let grants = s.grant_candies(&[window("codex.weekly", WindowClass::Weekly, 102.0)], true);
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].count, RareCandy::WEEKLY_GRANT);
        assert_eq!(s.item_count(ItemKind::RareCandy), RareCandy::WEEKLY_GRANT);
        // Same window again: no re-grant.
        assert!(s
            .grant_candies(&[window("codex.weekly", WindowClass::Weekly, 102.0)], true)
            .is_empty());
        // Drop below 100 → re-arm; cross again → grant again.
        assert!(s
            .grant_candies(&[window("codex.weekly", WindowClass::Weekly, 99.0)], true)
            .is_empty());
        let grants = s.grant_candies(&[window("codex.weekly", WindowClass::Weekly, 100.0)], true);
        assert_eq!(grants.len(), 1);
        // limits not ready → nothing.
        assert!(s
            .grant_candies(&[window("x", WindowClass::Session, 100.0)], false)
            .is_empty());
    }

    // ---- display state (CompanionDisplayStateTests port) ----

    fn hatched_common() -> CompanionState {
        let mut s = CompanionState {
            line_key: "bulbasaur".into(),
            seed: 1,
            ..Default::default()
        };
        s.migrate();
        s
    }

    #[test]
    fn display_egg_and_rules() {
        let egg = CompanionState::default();
        let input = DisplayInput {
            tpm: Some(50_000.0),
            limit_warning: false,
            has_usage_data: true,
            today_total: 100,
            celebration: false,
        };
        assert_eq!(display_state(&egg, &input), StateKind::Egg);

        let s = hatched_common();
        assert_eq!(display_state(&s, &input), StateKind::Working);

        let input_celeb = DisplayInput {
            tpm: Some(50_000.0),
            limit_warning: false,
            has_usage_data: true,
            today_total: 100,
            celebration: true,
        };
        assert_eq!(display_state(&s, &input_celeb), StateKind::LevelUp);

        let input_tired = DisplayInput {
            tpm: Some(500_000.0),
            limit_warning: true,
            has_usage_data: true,
            today_total: 100,
            celebration: false,
        };
        assert_eq!(display_state(&s, &input_tired), StateKind::Tired);

        let input_sleep = DisplayInput {
            tpm: None,
            limit_warning: false,
            has_usage_data: false,
            today_total: 0,
            celebration: false,
        };
        assert_eq!(display_state(&s, &input_sleep), StateKind::Sleep);

        let input_zero_today = DisplayInput {
            tpm: Some(200_000.0),
            limit_warning: false,
            has_usage_data: true,
            today_total: 0,
            celebration: false,
        };
        assert_eq!(display_state(&s, &input_zero_today), StateKind::Sleep);
    }

    #[test]
    fn display_burn_tiers() {
        let s = hatched_common();
        let mk = |tpm: f64| DisplayInput {
            tpm: Some(tpm),
            limit_warning: false,
            has_usage_data: true,
            today_total: 1000,
            celebration: false,
        };
        assert_eq!(display_state(&s, &mk(0.0)), StateKind::Idle);
        assert_eq!(display_state(&s, &mk(1_001.0)), StateKind::Working);
        assert_eq!(display_state(&s, &mk(99_999.0)), StateKind::Working);
        assert_eq!(display_state(&s, &mk(100_000.0)), StateKind::Focus);
        assert_eq!(display_state(&s, &mk(399_999.0)), StateKind::Focus);
        assert_eq!(display_state(&s, &mk(400_000.0)), StateKind::Focus);
        assert_eq!(burn_tier(1_000.0), BurnTier::Idle, "≤1000 is idle");
        assert_eq!(burn_tier(100_000.0), BurnTier::Fast);
    }
}
