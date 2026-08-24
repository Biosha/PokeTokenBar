//! Runtime PokéAPI pool fetch — the Rust port of the macOS app's `PokeAPIClient`
//! (runtime fetch + parse; the app deliberately bundles no Pokémon data).
//!
//! Mirrors the original's data sources and fallbacks:
//! - **base index**: one GraphQL query (`graphql.pokeapi.co`): `evolves_from_species
//!   IS NULL`, `id <= 649` (the Gen-V animated-sprite ceiling), Ditto #132 excluded
//!   (reserved for the disguise reveal);
//! - **species rows**: REST `pokemon-species/{id}` for 1..=649 — official names
//!   (ko/en/ja-Hrkt-with-ja-fallback/es), capture rate, legendary/mythical flags,
//!   evolution-chain URL;
//! - **evolution chains**: REST per base; children outside 1..=649 are dropped
//!   (`keepingAnimatedSprites`), and chain URLs are pinned to `https://pokeapi.co`
//!   (SSRF guard — the URL is server-controlled, as in `validatedChainURL`).
//!
//! Fetched rows **overlay** the bundled snapshot ([`crate::pool_gen`]): every
//! successfully fetched species replaces its bundled row, every base in the live
//! index replaces its bundled base, and every parent with a successfully fetched
//! chain replaces its bundled edges. A partial fetch therefore degrades
//! per-species instead of all-or-nothing, and full offline still serves the
//! bundled data (the one deliberate extension over the macOS app, which keeps no
//! bundled snapshot and shows an error until the next tick).
//!
//! Concurrency is deliberately small (8 workers), as in the macOS REST index
//! build, out of consideration for PokéAPI.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::companion::{PokemonOdds, Rarity};
use crate::pool::{BaseRec, PoolData, SpeciesRec, CACHE_TTL_SECS, MAX_SPECIES_ID};

const REST_BASE: &str = "https://pokeapi.co/api/v2";
const GRAPHQL_URL: &str = "https://graphql.pokeapi.co/v1beta2";
/// Same per-request timeout as the macOS client.
const TIMEOUT: Duration = Duration::from_secs(15);
const WORKERS: usize = 8;

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// True when a disk snapshot with that fetch timestamp is still within the TTL
/// (30 days, as in the macOS `base-index.json` cache).
fn cache_is_fresh(fetched_at: i64, now: i64) -> bool {
    now - fetched_at < CACHE_TTL_SECS
}

/// True when the snapshot carries every language's names. A snapshot written before a
/// language was added (French) has empty names for it, and would otherwise be served —
/// English — for the rest of its 30-day TTL; refetch instead.
fn has_localized_names(data: &PoolData) -> bool {
    data.species(1).is_some_and(|s| !s.fr.is_empty())
}

/// Background refresh entry point (see [`crate::pool::init_live`]): serve fresh
/// disk data when within the TTL, otherwise fetch the live pool, persist it, and
/// hot-swap it into the process pool.
pub(crate) fn refresh_pool() -> anyhow::Result<String> {
    if let Some(cache) = crate::pool::read_cache() {
        if cache_is_fresh(cache.fetched_at, now_secs()) && has_localized_names(&cache.data) {
            return Ok("disk snapshot within 30-day TTL — no fetch".to_string());
        }
    }
    let live = fetch_live_pool()?;
    let count = live.base_count();
    crate::pool::persist_cache(&live)?;
    crate::pool::swap(Arc::new(live));
    Ok(format!(
        "live pool fetched from PokéAPI ({count} bases), cached 30 days"
    ))
}

/// Fetch the full live pool and overlay it on the bundled snapshot.
pub fn fetch_live_pool() -> anyhow::Result<PoolData> {
    let agent = ureq::Agent::new();

    let species_rows = fetch_all_species(&agent)?;
    let fetched_any = species_rows.iter().any(Option::is_some);
    anyhow::ensure!(
        fetched_any,
        "no species fetched — staying on the bundled pool"
    );

    // Base index: GraphQL first; the macOS app's REST fallback derives it from the
    // species rows (evolves_from null, Ditto excluded) when the GraphQL endpoint is down.
    let index: Vec<(u16, u16)> = match fetch_base_index(&agent) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!(
                "pokeapi: GraphQL base index failed ({err:#}) — deriving the index from REST species rows"
            );
            (1..=MAX_SPECIES_ID)
                .filter(|&id| {
                    id != PokemonOdds::DITTO_SPECIES_ID
                        && species_rows[id as usize]
                            .as_ref()
                            .is_some_and(|d| d.evolves_from_species.is_none())
                })
                .map(|id| {
                    let rate = species_rows[id as usize]
                        .as_ref()
                        .expect("filtered above")
                        .capture_rate
                        .max(0) as u16;
                    (id, rate)
                })
                .collect()
        }
    };
    anyhow::ensure!(!index.is_empty(), "empty base index");

    // Evolution chains for every base (parallel), each overlaid on the bundled edges.
    let chain_urls: Vec<(u16, String)> = index
        .iter()
        .filter_map(|&(id, _)| {
            let url = species_rows
                .get(id as usize)?
                .as_ref()?
                .evolution_chain
                .url
                .clone();
            validated_chain_url(&url).map(|u| (id, u))
        })
        .collect();
    let live_edges = fetch_all_chains(&agent, &chain_urls);

    build_pool(&species_rows, &index, &live_edges)
}

/// Build the overlaid pool: live species rows over the bundled rows, the live index
/// (with rarity derived from the official capture rate + legendary/mythical flags,
/// as in `Rarity.from`) over the bundled bases, and live chain edges over the
/// bundled edges.
fn build_pool(
    species_rows: &[Option<SpeciesDto>],
    index: &[(u16, u16)],
    live_edges: &HashMap<u16, Vec<u16>>,
) -> anyhow::Result<PoolData> {
    let bundled = crate::pool::bundled();
    let size = (MAX_SPECIES_ID + 1) as usize;

    let mut species = bundled.species;
    for (slot, row) in species_rows.iter().enumerate().skip(1) {
        if let Some(dto) = row {
            let rec = to_species_rec(slot as u16, dto);
            if !rec.slug.is_empty() {
                species[slot] = Some(rec); // unusable rows keep the bundled one
            }
        }
    }

    // `species` starts from the bundled table, so a missing live row already
    // falls back to the bundled one.
    let mut bases = vec![None; size];
    for &(id, capture_rate) in index {
        let slot = id as usize;
        if slot >= size {
            continue;
        }
        let sp = match species.get(slot).and_then(Option::as_ref) {
            Some(s) => s,
            None => continue,
        };
        let rarity = Rarity::from_capture_rate(capture_rate as i32, sp.legendary, sp.mythical);
        bases[slot] = Some(BaseRec {
            id,
            capture_rate,
            rarity,
        });
    }

    let bundled_children = bundled.children;
    let mut children = vec![Vec::new(); size];
    for id in 1..=MAX_SPECIES_ID {
        let slot = id as usize;
        children[slot] = live_edges
            .get(&id)
            .cloned()
            .or_else(|| bundled_children.get(slot).cloned())
            .unwrap_or_default();
    }

    Ok(PoolData {
        species,
        bases,
        children,
    })
}

fn to_species_rec(id: u16, d: &SpeciesDto) -> SpeciesRec {
    let mut ko = String::new();
    let mut en = String::new();
    let mut ja_hrkt = String::new();
    let mut ja = String::new();
    let mut es = String::new();
    let mut fr = String::new();
    for n in &d.names {
        match n.language.name.as_str() {
            "ko" => ko = n.name.clone(),
            "en" => en = n.name.clone(),
            "ja-Hrkt" => ja_hrkt = n.name.clone(),
            "ja" => ja = n.name.clone(),
            "es" => es = n.name.clone(),
            "fr" => fr = n.name.clone(),
            _ => {}
        }
    }
    if ja_hrkt.is_empty() {
        ja_hrkt = ja;
    }
    SpeciesRec {
        id,
        slug: d.name.clone(),
        en,
        ko,
        ja: ja_hrkt,
        es,
        fr,
        capture_rate: d.capture_rate.max(0) as u16,
        legendary: d.is_legendary,
        mythical: d.is_mythical,
    }
}

// ---------------------------------------------------------------------------
// Fetches
// ---------------------------------------------------------------------------

fn fetch_all_species(agent: &ureq::Agent) -> anyhow::Result<Vec<Option<SpeciesDto>>> {
    let next = Arc::new(AtomicUsize::new(1));
    let mut initial: Vec<Option<SpeciesDto>> = Vec::with_capacity((MAX_SPECIES_ID + 1) as usize);
    initial.resize_with(initial.capacity(), || None);
    let results = Arc::new(Mutex::new(initial));
    std::thread::scope(|s| {
        for _ in 0..WORKERS {
            let next = next.clone();
            let results = results.clone();
            s.spawn(move || loop {
                let id = next.fetch_add(1, Ordering::Relaxed);
                if id > MAX_SPECIES_ID as usize {
                    return;
                }
                let dto = fetch_species(agent, id as u16).ok();
                results.lock().unwrap_or_else(|p| p.into_inner())[id] = dto;
            });
        }
    });
    let mut guard = results.lock().unwrap_or_else(|p| p.into_inner());
    Ok(std::mem::take(&mut *guard))
}

fn fetch_species(agent: &ureq::Agent, id: u16) -> anyhow::Result<SpeciesDto> {
    let mut bytes = Vec::new();
    agent
        .get(&format!("{REST_BASE}/pokemon-species/{id}"))
        .timeout(TIMEOUT)
        .call()?
        .into_reader()
        .read_to_end(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// The original's one-query base index: `evolves_from_species IS NULL`,
/// `id <= 649`, Ditto excluded, ordered by id.
fn fetch_base_index(agent: &ureq::Agent) -> anyhow::Result<Vec<(u16, u16)>> {
    let ditto = PokemonOdds::DITTO_SPECIES_ID;
    let query = format!(
        "{{ pokemonspecies(where: {{evolves_from_species_id: {{_is_null: true}}, \
         id: {{_lte: {MAX_SPECIES_ID}, _neq: {ditto}}}}}, \
         order_by: {{id: asc}}) {{ id capture_rate }} }}"
    );
    let mut bytes = Vec::new();
    agent
        .post(GRAPHQL_URL)
        .timeout(TIMEOUT)
        .set("Content-Type", "application/json")
        .send_string(&serde_json::json!({ "query": query }).to_string())?
        .into_reader()
        .read_to_end(&mut bytes)?;
    #[derive(serde::Deserialize)]
    struct Gql {
        data: GqlData,
    }
    #[derive(serde::Deserialize)]
    struct GqlData {
        pokemonspecies: Vec<GqlRow>,
    }
    #[derive(serde::Deserialize)]
    struct GqlRow {
        id: i32,
        capture_rate: i32,
    }
    let g: Gql = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(!g.data.pokemonspecies.is_empty(), "empty base index");
    Ok(g.data
        .pokemonspecies
        .into_iter()
        .map(|r| (r.id.max(0) as u16, r.capture_rate.max(0) as u16))
        .collect())
}

fn fetch_all_chains(agent: &ureq::Agent, jobs: &[(u16, String)]) -> HashMap<u16, Vec<u16>> {
    let next = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(Mutex::new(HashMap::<u16, Vec<u16>>::new()));
    std::thread::scope(|s| {
        for _ in 0..WORKERS {
            let next = next.clone();
            let results = results.clone();
            s.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= jobs.len() {
                    return;
                }
                let (base_id, url) = &jobs[i];
                match fetch_chain(agent, url) {
                    Ok(chain) => {
                        let mut edges: Vec<(u16, Vec<u16>)> = Vec::new();
                        walk_chain(&chain.chain, &mut edges);
                        let mut map = results.lock().unwrap_or_else(|p| p.into_inner());
                        for (parent, kids) in edges {
                            map.entry(parent).or_default().extend(kids);
                        }
                    }
                    Err(err) => eprintln!(
                        "pokeapi: chain {base_id} failed ({err:#}) — keeping bundled edges"
                    ),
                }
            });
        }
    });
    let mut guard = results.lock().unwrap_or_else(|p| p.into_inner());
    let mut map = std::mem::take(&mut *guard);
    // Two chains can cover the same parent: order + dedup the merged child lists.
    for kids in map.values_mut() {
        kids.sort_unstable();
        kids.dedup();
    }
    map
}

fn fetch_chain(agent: &ureq::Agent, url: &str) -> anyhow::Result<ChainDto> {
    let mut bytes = Vec::new();
    agent
        .get(url)
        .timeout(TIMEOUT)
        .call()?
        .into_reader()
        .read_to_end(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// SSRF guard (port of `validatedChainURL`): evolution-chain URLs come from the
/// API response, so they are pinned to `https://pokeapi.co` before any fetch.
fn validated_chain_url(raw: &str) -> Option<String> {
    let rest = raw.strip_prefix("https://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (host == "pokeapi.co").then(|| raw.to_string())
}

/// Species id from a `.../pokemon-species/{id}/` URL.
fn id_from_species_url(url: &str) -> u16 {
    url.rsplit('/')
        .nth(1)
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(0)
}

/// Collect `(species_id, in-range direct children)` for every in-range node of a
/// chain (port of `chain_ids` in the pool generator / `keepingAnimatedSprites`):
/// out-of-range nodes and their subtrees are dropped.
fn walk_chain(link: &ChainLink, out: &mut Vec<(u16, Vec<u16>)>) {
    let sid = id_from_species_url(link.species.url.as_deref().unwrap_or(""));
    if !(1..=MAX_SPECIES_ID).contains(&sid) {
        return;
    }
    let kids: Vec<u16> = link
        .evolves_to
        .iter()
        .map(|c| id_from_species_url(c.species.url.as_deref().unwrap_or("")))
        .filter(|c| (1..=MAX_SPECIES_ID).contains(c))
        .collect();
    out.push((sid, kids));
    for c in &link.evolves_to {
        walk_chain(c, out);
    }
}

#[derive(serde::Deserialize)]
struct SpeciesDto {
    name: String,
    capture_rate: i32,
    is_legendary: bool,
    is_mythical: bool,
    evolves_from_species: Option<NamedRef>,
    evolution_chain: UrlRef,
    names: Vec<NameDto>,
}

#[derive(serde::Deserialize)]
struct NamedRef {
    name: String,
    #[serde(default)]
    url: Option<String>,
}

#[derive(serde::Deserialize)]
struct UrlRef {
    url: String,
}

#[derive(serde::Deserialize)]
struct NameDto {
    name: String,
    language: NamedRef,
}

#[derive(serde::Deserialize)]
struct ChainDto {
    chain: ChainLink,
}

#[derive(serde::Deserialize)]
struct ChainLink {
    species: NamedRef,
    #[serde(default)]
    evolves_to: Vec<ChainLink>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(species_id: u16, kids: &[u16]) -> ChainLink {
        ChainLink {
            species: NamedRef {
                name: String::new(),
                url: Some(format!(
                    "https://pokeapi.co/api/v2/pokemon-species/{species_id}/"
                )),
            },
            evolves_to: kids.iter().map(|k| link(*k, &[])).collect(),
        }
    }

    #[test]
    fn chain_url_guard_pins_https_pokeapi() {
        assert!(validated_chain_url("https://pokeapi.co/api/v2/evolution-chain/1/").is_some());
        assert!(validated_chain_url("https://pokeapi.co/x?y=1#z").is_some());
        assert!(
            validated_chain_url("http://pokeapi.co/x").is_none(),
            "http must be rejected"
        );
        assert!(
            validated_chain_url("https://evil.example.com/x").is_none(),
            "other host rejected"
        );
        assert!(
            validated_chain_url("https://pokeapi.co.evil.com/x").is_none(),
            "lookalike host rejected"
        );
        assert!(validated_chain_url("not a url").is_none());
    }

    #[test]
    fn species_url_id_parsing() {
        assert_eq!(
            id_from_species_url("https://pokeapi.co/api/v2/pokemon-species/649/"),
            649
        );
        assert_eq!(id_from_species_url("garbage"), 0);
    }

    #[test]
    fn walk_drops_out_of_range_subtrees() {
        let mut out = Vec::new();
        // Hand-build: 1 -> [2, 650], 650 -> [100]
        let chain = ChainLink {
            species: NamedRef {
                name: String::new(),
                url: Some("https://pokeapi.co/api/v2/pokemon-species/1/".to_string()),
            },
            evolves_to: vec![
                link(2, &[]),
                ChainLink {
                    species: NamedRef {
                        name: String::new(),
                        url: Some("https://pokeapi.co/api/v2/pokemon-species/650/".to_string()),
                    },
                    evolves_to: vec![link(100, &[])],
                },
            ],
        };
        walk_chain(&chain, &mut out);
        let map: HashMap<u16, Vec<u16>> = out.into_iter().collect();
        assert_eq!(map.get(&1), Some(&vec![2]), "650 is not listed as a child");
        assert!(map.contains_key(&2));
        assert!(!map.contains_key(&650), "out-of-range node dropped");
        assert!(
            !map.contains_key(&100),
            "subtree of an out-of-range node dropped"
        );
    }

    #[test]
    fn freshness_ttl_is_30_days() {
        assert!(cache_is_fresh(1_000_000, 1_000_000 + 29 * 86_400));
        assert!(!cache_is_fresh(1_000_000, 1_000_000 + 31 * 86_400));
    }

    #[test]
    fn build_pool_overlays_live_rows_on_bundled() {
        // A live row for species 1 with a changed capture rate + a live index.
        let mut rows: Vec<Option<SpeciesDto>> = Vec::new();
        rows.resize_with(650, || None);
        rows[1] = Some(SpeciesDto {
            name: "bulbasaur".into(),
            capture_rate: 45,
            is_legendary: false,
            is_mythical: false,
            evolves_from_species: None,
            evolution_chain: UrlRef {
                url: "https://pokeapi.co/api/v2/evolution-chain/1/".into(),
            },
            names: vec![NameDto {
                name: "이상해씨".into(),
                language: NamedRef {
                    name: "ko".into(),
                    url: None,
                },
            }],
        });
        let index = vec![(1u16, 45u16), (4u16, 45)];
        let edges: HashMap<u16, Vec<u16>> = [(1u16, vec![2u16, 3])].into_iter().collect();
        let pool = build_pool(&rows, &index, &edges).expect("build");
        // Species 1 is the live row; species 4 falls back to bundled.
        assert_eq!(pool.species[1].as_ref().unwrap().ko, "이상해씨");
        assert_eq!(pool.species[4].as_ref().unwrap().en, "Charmander");
        // Bases come from the live index; rarity derives from capture rate.
        assert_eq!(pool.bases.iter().flatten().count(), 2);
        assert_eq!(pool.bases[1].unwrap().rarity, Rarity::Rare);
        // Live edges replace the bundled ones for species 1.
        assert_eq!(pool.children[1], vec![2, 3]);
        // Species 2 keeps its bundled edges.
        assert_eq!(pool.children[2], vec![3]);
        assert_eq!(pool.base_count(), 2);
    }

    /// End-to-end live fetch against PokéAPI: the pool resolves, is non-trivial, and
    /// matches the bundled snapshot's shape (328 Gen I–V bases, 649 species, Ditto out).
    #[test]
    #[ignore = "live network (requires internet)"]
    fn fetches_live_pool_end_to_end() {
        let pool = fetch_live_pool().expect("live pool fetch");
        assert!(
            pool.base_count() > 300,
            "expected ~328 bases, got {}",
            pool.base_count()
        );
        assert_eq!(
            pool.species.iter().filter(|s| s.is_some()).count(),
            649,
            "all Gen I–V species present"
        );
        // Ditto is excluded from the hatch pool.
        assert!(pool.base(PokemonOdds::DITTO_SPECIES_ID).is_none());
        // A known line: Bulbasaur (1) → Ivysaur (2) → Venusaur (3).
        assert_eq!(pool.children(1), vec![2]);
        assert_eq!(pool.children(2), vec![3]);
        // Legendaries derive Legendary rarity.
        assert_eq!(pool.base(150).expect("mewtwo").rarity, Rarity::Legendary);
        // Official names resolve (English + a CJK language).
        let pikachu = pool.species(25).expect("pikachu");
        assert_eq!(pikachu.en, "Pikachu");
        assert!(!pikachu.ko.is_empty());
    }

    /// The full "no recompile" flow: `refresh_pool` fetches live, persists the disk
    /// snapshot, and a second call within the 30-day TTL skips the network. The cache
    /// dir is isolated via `XDG_CACHE_HOME` so the user's real cache is untouched.
    #[test]
    #[ignore = "live network (requires internet)"]
    fn refresh_pool_persists_and_serves_fresh_from_disk() {
        let tmp = std::env::temp_dir().join(format!("ptb-pool-live-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        std::env::set_var("XDG_CACHE_HOME", &tmp);

        // First refresh: no cache → fetches live and writes the disk snapshot.
        let note = refresh_pool().expect("first refresh");
        assert!(note.contains("live pool fetched"), "unexpected: {note}");
        let cache_path = crate::pool::cache_path().expect("cache path");
        assert!(
            cache_path.exists(),
            "disk snapshot must be persisted at {cache_path:?}"
        );
        let cache = crate::pool::read_cache().expect("cache readable");
        assert!(cache.data.base_count() > 300);
        assert!(cache.fetched_at > 0);

        // Second refresh within the TTL: no network, serves the fresh disk snapshot.
        let note2 = refresh_pool().expect("second refresh");
        assert!(note2.contains("within 30-day TTL"), "unexpected: {note2}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_snapshot_without_french_names_is_not_served() {
        let mut data = crate::pool::bundled();
        assert!(has_localized_names(&data), "bundled snapshot has fr names");
        // A cache written before French support: fr empty everywhere.
        for s in data.species.iter_mut().flatten() {
            s.fr.clear();
        }
        assert!(!has_localized_names(&data));
    }
}
