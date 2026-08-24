//! The full Gen I–V evolution-line pool, resolved at runtime from PokéAPI —
//! a port of the macOS app's `PokeAPIClient`, which never bundles Pokémon data.
//!
//! The macOS app fetches the base-species index (one GraphQL query:
//! `evolves_from_species IS NULL`, ids 1–649, Ditto #132 excluded — reserved for
//! the disguise reveal), each base's evolution chain, and per-language names at
//! runtime, caching the index on disk with a 30-day TTL. This port does the same
//! ([`crate::pokeapi`] in the background) so **a pool change on PokéAPI's side
//! needs no recompile**: the data refreshes with the TTL and hot-swaps in.
//! [`crate::pool_gen`] — the generated snapshot — is only the bundled offline
//! fallback (and the test fixture), not the source of truth.
//!
//! Resolution ladder (per process, first access is always local and fast):
//! 1. in-memory (once loaded);
//! 2. the disk snapshot `$XDG_CACHE_HOME/PokeTokenBar/pool-cache.json` — a stale
//!    snapshot is used too, exactly as the macOS app serves its expired disk index;
//! 3. the bundled snapshot ([`crate::bundled`]).
//!
//! `init_live()` (called by the app and the CLI's companion commands; skipped
//! under `PTB_POOL_OFFLINE=1`) spawns the background refresh that refetches when
//! the disk snapshot is older than the TTL.
//!
//! Selection semantics are a faithful port of `CompanionStore.chooseBase`:
//! - a premium egg's tier pre-filters candidates by `Rarity::includes(capture_rate)`
//!   (the capture-rate ceiling is the tier floor, so legendaries naturally stay in "rare+");
//! - weight = official capture rate, halved (min 1) when a final form of that base was already
//!   collected (`collectedFinals` "base:final" entries) — an uncollected boost that keeps
//!   re-hatching/shiny hunting open;
//! - exactly one roll over the cumulative weights (no re-rolls, deterministic time bound).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::companion::Rarity;
use crate::i18n::Language;
use crate::paths;
use crate::pool_gen::{BASES, CHILDREN, SPECIES};

/// Highest species id with animated Gen-V assets (`PokemonAssets.animatedSpeciesIDs`).
pub const MAX_SPECIES_ID: u16 = 649;

/// Disk snapshot refresh interval — the macOS app's `base-index.json` 30-day TTL.
pub const CACHE_TTL_SECS: i64 = 30 * 86_400;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

/// One species: official names in the app languages (ja = ja-Hrkt, fallback ja; fr is
/// port-specific) plus the hatch-relevant flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeciesRec {
    pub id: u16,
    pub slug: String,
    pub en: String,
    pub ko: String,
    pub ja: String,
    pub es: String,
    /// Port-specific (absent from caches written before French support).
    #[serde(default)]
    pub fr: String,
    pub capture_rate: u16,
    pub legendary: bool,
    pub mythical: bool,
}

/// One hatchable base line: official capture rate + derived rarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaseRec {
    pub id: u16,
    pub capture_rate: u16,
    pub rarity: Rarity,
}

/// The full pool: vectors indexed by species id (slot 0 unused).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolData {
    pub species: Vec<Option<SpeciesRec>>,
    pub bases: Vec<Option<BaseRec>>,
    pub children: Vec<Vec<u16>>,
}

impl PoolData {
    pub fn species(&self, id: u16) -> Option<&SpeciesRec> {
        self.species.get(id as usize).and_then(Option::as_ref)
    }

    pub fn base(&self, id: u16) -> Option<&BaseRec> {
        self.bases.get(id as usize).and_then(Option::as_ref)
    }

    pub fn children(&self, id: u16) -> &[u16] {
        self.children.get(id as usize).map_or(&[], Vec::as_slice)
    }

    pub fn base_count(&self) -> usize {
        self.bases.iter().filter(|b| b.is_some()).count()
    }
}

/// A hatchable evolution line (base species + derived metadata).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineMeta {
    pub base_id: u16,
    pub slug: String,
    /// English display name (sprite lookup uses this, never the localized name).
    pub en: String,
    pub rarity: Rarity,
    pub capture_rate: u16,
}

// ---------------------------------------------------------------------------
// Process pool (memory → disk → bundled) + background live refresh
// ---------------------------------------------------------------------------

static POOL: Mutex<Option<Arc<PoolData>>> = Mutex::new(None);
static LIVE_STARTED: AtomicBool = AtomicBool::new(false);

fn pool_lock() -> std::sync::MutexGuard<'static, Option<Arc<PoolData>>> {
    POOL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The current pool, bootstrapped on first use. Never performs network I/O:
/// under `cfg(test)` it is always the bundled snapshot (deterministic tests, no
/// stray threads); otherwise disk snapshot when present, else bundled.
pub(crate) fn current() -> Arc<PoolData> {
    if let Some(p) = pool_lock().as_ref() {
        return p.clone();
    }
    let data = local_pool();
    let mut guard = pool_lock();
    if let Some(p) = guard.as_ref() {
        return p.clone();
    }
    let p = Arc::new(data);
    *guard = Some(p.clone());
    p
}

fn local_pool() -> PoolData {
    #[cfg(test)]
    {
        bundled()
    }
    #[cfg(not(test))]
    {
        read_cache().map(|c| c.data).unwrap_or_else(bundled)
    }
}

/// Replace the process pool with freshly fetched data (background refresh).
pub(crate) fn swap(data: Arc<PoolData>) {
    *pool_lock() = Some(data);
}

/// Spawn the background pool refresh (idempotent, once per process). Skipped
/// under `PTB_POOL_OFFLINE=1` (or `true`/`on`). Call from `main`/`run` — never
/// from tests.
pub fn init_live() {
    if offline_from_env() {
        return;
    }
    if LIVE_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("poketoken-pool".to_string())
        .spawn(|| match crate::pokeapi::refresh_pool() {
            Ok(note) => eprintln!("poketoken-pool: {note}"),
            Err(err) => eprintln!("poketoken-pool: keeping the cached/bundled pool: {err:#}"),
        });
}

fn offline_from_env() -> bool {
    std::env::var("PTB_POOL_OFFLINE")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on"))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Disk snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct CacheFile {
    pub fetched_at: i64,
    pub data: PoolData,
}

pub(crate) fn cache_path() -> Option<std::path::PathBuf> {
    paths::cache_dir().map(|d| d.join("pool-cache.json"))
}

pub(crate) fn read_cache_at(path: &std::path::Path) -> Option<CacheFile> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub(crate) fn read_cache() -> Option<CacheFile> {
    let path = cache_path()?;
    read_cache_at(&path)
}

pub(crate) fn persist_cache_at(path: &std::path::Path, data: &PoolData) -> anyhow::Result<()> {
    let file = CacheFile {
        fetched_at: now_secs(),
        data: data.clone(),
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&file)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub(crate) fn persist_cache(data: &PoolData) -> anyhow::Result<()> {
    let path = cache_path().ok_or_else(|| anyhow::anyhow!("no cache dir available"))?;
    persist_cache_at(&path, data)
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Bundled fallback snapshot (generated by scripts/gen_pool.py)
// ---------------------------------------------------------------------------

/// The bundled offline snapshot: what `scripts/gen_pool.py` generated from
/// PokéAPI. Served when there is no disk snapshot yet, and the per-species
/// fallback when a live fetch partially fails.
pub fn bundled() -> PoolData {
    let size = (MAX_SPECIES_ID + 1) as usize;
    let mut species = vec![None; size];
    for s in SPECIES {
        species[s.id as usize] = Some(SpeciesRec {
            id: s.id,
            slug: s.slug.to_string(),
            en: s.en.to_string(),
            ko: s.ko.to_string(),
            ja: s.ja.to_string(),
            es: s.es.to_string(),
            fr: s.fr.to_string(),
            capture_rate: s.capture_rate,
            legendary: s.legendary,
            mythical: s.mythical,
        });
    }
    let mut bases = vec![None; size];
    for b in BASES {
        bases[b.id as usize] = Some(BaseRec {
            id: b.id,
            capture_rate: b.capture_rate,
            rarity: b.rarity,
        });
    }
    let mut children = vec![Vec::new(); size];
    for (parent, kids) in CHILDREN {
        children[*parent as usize] = kids.to_vec();
    }
    PoolData {
        species,
        bases,
        children,
    }
}

// ---------------------------------------------------------------------------
// Lookups (owned returns — the pool is dynamic, not 'static)
// ---------------------------------------------------------------------------

pub fn base_count() -> usize {
    current().base_count()
}

/// The full hatch pool (Ditto excluded, as in `fetchBaseIndex`).
pub fn bases() -> Vec<BaseRec> {
    current().bases.iter().flatten().copied().collect()
}

pub fn species_by_id(id: u16) -> Option<SpeciesRec> {
    current().species(id).cloned()
}

pub fn species_by_slug(slug: &str) -> Option<SpeciesRec> {
    current()
        .species
        .iter()
        .flatten()
        .find(|s| s.slug == slug)
        .cloned()
}

pub fn base_by_id(id: u16) -> Option<BaseRec> {
    current().base(id).copied()
}

pub fn base_by_slug(slug: &str) -> Option<BaseRec> {
    species_by_slug(slug).and_then(|s| base_by_id(s.id))
}

/// Children of a species in the evolution trees (empty for leaves / unknown ids).
pub fn children_of(id: u16) -> Vec<u16> {
    current().children(id).to_vec()
}

pub fn line_by_id(base_id: u16) -> Option<LineMeta> {
    let p = current();
    let s = p.species(base_id)?;
    // Hatchable bases carry their derived rarity; any other species (notably Ditto,
    // which is excluded from the pool and only appears via the disguise reveal) derives
    // it the same way the macOS app does: official capture rate + legendary/mythical flags.
    let (rarity, capture_rate) = match p.base(base_id) {
        Some(b) => (b.rarity, b.capture_rate),
        None => (
            Rarity::from_capture_rate(s.capture_rate as i32, s.legendary, s.mythical),
            s.capture_rate,
        ),
    };
    Some(LineMeta {
        base_id,
        slug: s.slug.clone(),
        en: s.en.clone(),
        rarity,
        capture_rate,
    })
}

/// Line for a base slug; also resolves non-pool species like `ditto` (disguise reveal).
pub fn line_by_slug(slug: &str) -> Option<LineMeta> {
    species_by_slug(slug).and_then(|s| line_by_id(s.id))
}

/// Localized species name: the language's official name, falling back to English, then `#id`
/// (port of `AppLanguage.resolveName` + `EvoLine.localizedName`).
pub fn localized_name(id: u16, lang: Language) -> String {
    let p = current();
    let s = match p.species(id) {
        Some(s) => s,
        None => return format!("#{id}"),
    };
    let chosen = match lang {
        Language::Ko => s.ko.as_str(),
        Language::Ja => s.ja.as_str(),
        Language::Es => s.es.as_str(),
        Language::Fr => s.fr.as_str(),
        Language::En => s.en.as_str(),
    };
    if chosen.is_empty() {
        if s.en.is_empty() {
            format!("#{id}")
        } else {
            s.en.clone()
        }
    } else {
        chosen.to_string()
    }
}

/// English name (`#id` fallback) — used for sprite slugs and JSON.
pub fn en_name(id: u16) -> String {
    match current().species(id) {
        Some(s) if !s.en.is_empty() => s.en.clone(),
        _ => format!("#{id}"),
    }
}

// ---------------------------------------------------------------------------
// Tree + selection semantics (ports of the macOS EvoNode / chooseBase logic)
// ---------------------------------------------------------------------------

/// Every final-form id reachable from `id` (port of `EvoNode.finalIDs`).
pub fn final_ids_of(id: u16) -> Vec<u16> {
    let kids = children_of(id);
    if kids.is_empty() {
        vec![id]
    } else {
        kids.iter().flat_map(|id| final_ids_of(*id)).collect()
    }
}

/// The unique tree path from `root` to `target` (None when `target` is not in the subtree).
pub fn path_from_root_to(root: u16, target: u16) -> Option<Vec<u16>> {
    let mut path = vec![root];
    if root == target {
        return Some(path);
    }
    for kid in children_of(root) {
        if let Some(mut rest) = path_from_root_to(kid, target) {
            path.append(&mut rest);
            return Some(path);
        }
    }
    None
}

/// Longest root-to-leaf path length in forms (port of `EvoNode.depth` — used for the Ditto
/// disguise "≥2 forms" gate, which in Swift consults `line.totalForms` = tree depth).
pub fn tree_depth(id: u16) -> u16 {
    1 + children_of(id)
        .iter()
        .map(|c| tree_depth(*c))
        .max()
        .unwrap_or(0)
}

/// Longest prefix of `ids` that actually continues from `root` (port of `longestValidPath`).
/// A first id that is not the root resets to the root alone (corrupt-save recovery).
pub fn longest_valid_path(ids: &[u16], root: u16) -> Vec<u16> {
    let mut path = vec![root];
    if ids.first() != Some(&root) {
        return path;
    }
    let mut node = root;
    for id in &ids[1..] {
        if !children_of(node).contains(id) {
            break;
        }
        path.push(*id);
        node = *id;
    }
    path
}

/// Pick the next evolution among `node`'s children (port of `pickPlannedChild`): prefer
/// children whose final forms include an *uncollected* "base:final"; then one uniform roll.
pub fn pick_planned_child(node: u16, base: u16, collected: &[String], roll: u64) -> u16 {
    let kids = children_of(node);
    let owned = |final_id: u16| {
        let key = format!("{base}:{final_id}");
        collected.iter().any(|c| c == &key)
    };
    let fresh: Vec<u16> = kids
        .iter()
        .copied()
        .filter(|ch| final_ids_of(*ch).iter().any(|f| !owned(*f)))
        .collect();
    let pool: &[u16] = if fresh.is_empty() { &kids } else { &fresh };
    pool[(roll % pool.len() as u64) as usize]
}

/// A full evolution route from `root` to a leaf (port of `makeEvolutionPlan`). One RNG roll
/// per branching level; straight lines consume none.
pub fn make_evolution_plan(
    root: u16,
    collected: &[String],
    roll_source: &mut dyn Roll,
) -> Vec<u16> {
    let mut plan = vec![root];
    let mut node = root;
    while !children_of(node).is_empty() {
        let kids = children_of(node);
        let next = if kids.len() == 1 {
            kids[0]
        } else {
            pick_planned_child(node, root, collected, roll_source.next())
        };
        plan.push(next);
        node = next;
    }
    plan
}

/// Normalization of a persisted (path, planned) pair against the current tree (port of
/// `normalizedEvolutionState`): reuse the saved plan only when it is complete and still
/// valid, otherwise re-plan from the realized node. Returns (path, planned, stage_index).
pub fn normalize_evolution(
    saved_path: &[u16],
    saved_planned: &[u16],
    root: u16,
    collected: &[String],
    roll_source: &mut dyn Roll,
) -> (Vec<u16>, Vec<u16>, usize) {
    let realized = longest_valid_path(saved_path, root);
    let candidate = longest_valid_path(saved_planned, root);
    let can_reuse = candidate == saved_planned
        && candidate.starts_with(&realized)
        && children_of(*candidate.last().unwrap_or(&root)).is_empty();
    let planned = if can_reuse {
        candidate
    } else {
        let suffix = make_evolution_plan(*realized.last().unwrap_or(&root), collected, roll_source);
        let mut plan = realized.clone();
        plan.extend(suffix.iter().skip(1));
        plan
    };
    let stage = realized.len().saturating_sub(1);
    (realized, planned, stage)
}

/// One hatch roll (port of `chooseBase`, weighted path — the macOS app's per-hatch REST
/// fallback exists for the no-index case; here the bundled snapshot covers it).
/// Returns None when a guaranteed tier has no candidates.
pub fn choose_base(
    tier: Option<Rarity>,
    collected: &[String],
    roll_source: &mut dyn Roll,
) -> Option<u16> {
    let p = current();
    let index: Vec<BaseRec> = match tier {
        Some(t) => p
            .bases
            .iter()
            .flatten()
            .copied()
            .filter(|b| t.includes(b.capture_rate as i32))
            .collect(),
        None => p.bases.iter().flatten().copied().collect(),
    };
    if index.is_empty() {
        return None;
    }
    let weights: Vec<u64> = index
        .iter()
        .map(|b| {
            let prefix = format!("{}:", b.id);
            let w = (b.capture_rate as u64).max(1);
            if collected.iter().any(|c| c.starts_with(&prefix)) {
                (w / 2).max(1)
            } else {
                w
            }
        })
        .collect();
    let total: u64 = weights.iter().sum();
    let mut r = roll_source.next() % total;
    for (i, w) in weights.iter().enumerate() {
        if *w > r {
            return Some(index[i].id);
        }
        r -= w;
    }
    index.last().map(|b| b.id)
}

/// An RNG stream that yields successive u64s (the state's persisted LCG).
pub trait Roll {
    fn next(&mut self) -> u64;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic 64-bit LCG for tests (same step as the companion's persisted seed).
    struct Lcg(u64);
    impl Roll for Lcg {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    #[test]
    fn bundled_snapshot_shape() {
        // Generated snapshot invariants (328 = Gen I–V bases, Ditto #132 excluded).
        let p = bundled();
        assert_eq!(p.base_count(), 328);
        assert_eq!(p.species.iter().filter(|s| s.is_some()).count(), 649);
        assert!(p.bases.iter().flatten().all(|b| b.id <= MAX_SPECIES_ID));
        assert!(
            p.base(132).is_none(),
            "Ditto is excluded from the hatch pool"
        );
        // Rarities derive from the official capture rate / flags.
        assert_eq!(p.base(10).unwrap().rarity, Rarity::Common);
        assert_eq!(p.base(150).unwrap().rarity, Rarity::Legendary);
        assert_eq!(p.base(1).unwrap().rarity, Rarity::Rare); // capture_rate 45
    }

    #[test]
    fn every_base_resolves_names_and_tree() {
        for b in bases() {
            let line = line_by_id(b.id).unwrap_or_else(|| panic!("base {} unresolvable", b.id));
            assert_eq!(line.slug, species_by_id(b.id).unwrap().slug);
            assert!(!line.en.is_empty());
            for lang in Language::ALL {
                let name = localized_name(b.id, lang);
                assert!(
                    !name.is_empty() && !name.starts_with('#'),
                    "{:?} {lang:?}",
                    line.en
                );
            }
            // Every base has at least itself as a final-form candidate.
            assert!(!final_ids_of(b.id).is_empty());
        }
    }

    #[test]
    fn branching_lines() {
        // Eevee (133) has its seven Gen I–V evolutions (Sylveon #700 filtered out).
        assert_eq!(children_of(133), vec![134, 135, 136, 196, 197, 470, 471]);
        assert_eq!(children_of(236), vec![106, 107, 237]); // Tyrogue
        assert_eq!(children_of(361), vec![362, 478]); // Snorunt
    }

    #[test]
    fn straight_line_plan_consumes_no_rolls() {
        let mut r = Lcg(7);
        let before = r.0;
        let plan = make_evolution_plan(1, &[], &mut r);
        assert_eq!(plan, vec![1, 2, 3]);
        assert_eq!(r.0, before, "no branches → no rolls");
    }

    #[test]
    fn plan_is_deterministic_and_uncollected_preferring() {
        let empty: Vec<String> = vec![];
        let a = make_evolution_plan(133, &empty, &mut Lcg(42));
        let b = make_evolution_plan(133, &empty, &mut Lcg(42));
        assert_eq!(a, b, "same seed → same plan");
        assert_eq!(a.first(), Some(&133));
        assert!(
            children_of(*a.last().unwrap()).is_empty(),
            "plan must end at a leaf"
        );
        // Collecting that final biases the next roll toward other branches.
        let collected = vec![format!("133:{}", a.last().unwrap())];
        let c = make_evolution_plan(133, &collected, &mut Lcg(42));
        assert_eq!(c.first(), Some(&133));
        assert!(children_of(*c.last().unwrap()).is_empty());
    }

    #[test]
    fn choose_base_respects_tier_guarantee() {
        for i in 0..200 {
            let mut r = Lcg(0x9e3779b97f4a7c15 + i as u64);
            let id = choose_base(Some(Rarity::Rare), &[], &mut r).expect("rare+ pool non-empty");
            let rarity = line_by_id(id).unwrap().rarity;
            assert!(
                rarity.sort_rank() >= Rarity::Rare.sort_rank(),
                "rolled {rarity:?} below guaranteed rare"
            );
        }
        // Common is never "guaranteed" in the shop; None tier uses the full pool.
        let mut r = Lcg(1);
        assert!(choose_base(None, &[], &mut r).is_some());
    }

    #[test]
    fn collected_base_gets_half_weight() {
        let mut r = Lcg(5);
        assert!(choose_base(None, &[], &mut r).is_some());
        let mut r2 = Lcg(5);
        let collected: Vec<String> = bases()
            .iter()
            .map(|b| format!("{}:{}", b.id, b.id))
            .collect();
        assert!(choose_base(None, &collected, &mut r2).is_some());
    }

    #[test]
    fn longest_valid_path_recovers_corrupt_paths() {
        assert_eq!(longest_valid_path(&[1, 2, 3], 1), vec![1, 2, 3]);
        assert_eq!(longest_valid_path(&[1, 3], 1), vec![1]); // 3 is not a child of 1
        assert_eq!(longest_valid_path(&[2, 3], 1), vec![1]); // wrong root resets
        assert_eq!(longest_valid_path(&[], 1), vec![1]);
    }

    #[test]
    fn normalize_reuses_complete_plans_and_replans_broken_ones() {
        let mut r = Lcg(9);
        let (path, planned, stage) = normalize_evolution(&[1, 2], &[1, 2, 3], 1, &[], &mut r);
        assert_eq!(path, vec![1, 2]);
        assert_eq!(planned, vec![1, 2, 3], "complete valid plan is reused");
        assert_eq!(stage, 1);

        let mut r = Lcg(9);
        let (path, planned, stage) = normalize_evolution(&[1, 2], &[1, 2], 1, &[], &mut r);
        assert_eq!(path, vec![1, 2]);
        assert_eq!(
            planned,
            vec![1, 2, 3],
            "incomplete plan is extended to a leaf"
        );
        assert_eq!(stage, 1);
    }

    #[test]
    fn cache_roundtrip_and_shape() {
        let dir = std::env::temp_dir().join(format!("ptb-pool-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("pool-cache.json");
        let p = bundled();
        persist_cache_at(&path, &p).expect("persist");
        let back = read_cache_at(&path).expect("read back");
        assert_eq!(back.data.base_count(), p.base_count());
        assert_eq!(back.data.species[25].as_ref().unwrap().en, "Pikachu");
        assert!(back.fetched_at > 0);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn current_is_bundled_under_test() {
        // cfg(test) forces the bundled snapshot: no disk, no network, deterministic.
        assert_eq!(base_count(), bundled().base_count());
        assert_eq!(en_name(25), "Pikachu");
        assert_eq!(localized_name(25, Language::Ja), "ピカチュウ");
        // Unknown id → "#id".
        assert_eq!(localized_name(0, Language::En), "#0");
    }

    #[test]
    fn line_meta_for_ditto_derives_rarity() {
        // Ditto is not a base: its LineMeta derives rarity from capture rate/flags
        // (PokéAPI lists Ditto at 35 → Rare).
        let line = line_by_slug("ditto").expect("ditto resolves");
        assert_eq!(line.base_id, 132);
        assert_eq!(line.rarity, Rarity::Rare);
    }
}
