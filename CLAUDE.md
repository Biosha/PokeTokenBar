# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust/GNOME port of the macOS app [PokeTokenBar](https://github.com/chattymin/PokeTokenBar):
it reads AI-coding CLI token usage from local logs and turns it into a Pokémon companion.
`README.md` documents the user-facing behavior (providers, thresholds, packaging) — read it
before changing behavior. **Parity with the macOS app is the design rule**: thresholds, window
math, display-state rules and shop economics are ports, not inventions. Module doc comments cite
the Swift original; keep that convention when porting further.

## Commands

```bash
cargo build                                   # core + CLI (no GTK needed)
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings   # exactly what CI runs

cargo test -p poketoken-core companion::      # one module's tests
cargo test -p poketoken-core --test usage_cache_parity     # the one integration test
cargo test -p poketoken-core -- --nocapture exact_test_name

cargo run -p poketoken-app --features gui     # the GUI (needs libgtk-4-dev libadwaita-1-dev)
cargo run -p poketoken-cli -- snapshot --json # headless core check
scripts/make-deb.sh                           # release .deb into dist/
scripts/bump-version.sh --self-test           # version arithmetic check (also run in CI)
scripts/release.sh patch --dry-run            # full release rehearsal, pushes nothing
scripts/release.sh --check-only               # doc consistency checks only
```

## CI / releases

`.github/workflows/ci.yml` — fmt, clippy `--all-features`, `cargo test --workspace --locked`,
the bump-script self-test; on pushes to `main` it also builds the `.deb` and uploads it as a
workflow artifact. `scripts/release.sh <patch|minor|major|x.y.z>` — the whole release, runnable locally:
preflight (main + clean tree) → test gate (the CI commands) → doc consistency → version bump →
`.deb` + tarball build → commit/tag/push → `gh release create`. Nothing is pushed until the
bumped build succeeds, and `--dry-run` stops right before the push.
`.github/workflows/release.yml` is a thin `workflow_dispatch` wrapper that runs that same
script with `--yes`, so a local release and a CI release are the same code path. **`[workspace.package] version` in
the root `Cargo.toml` is the only place a version is written** — `make-deb.sh` and the
`.desktop` file read it from there.

CI uses `--all-features`, so GUI code must compile even though `gui` is off by default —
run clippy with GTK headers installed before pushing app-crate changes.

The app is single-instance (bus name `io.github.poketoken.app`). If a rebuild seems to change
nothing, an old process is still primary: `pkill -x poketoken-app`, or run a parallel instance
with `PTB_APP_ID=…`. `poketoken-app --screenshot <DIR>` renders one PNG per tab and exits.

## Architecture

Three crates; all logic lives in `core`, which has **no UI dependency**:

- `crates/core` — providers, aggregation, limits, companion, pool, i18n, cache, paths.
- `crates/cli` (`poke-token-bar`) — `snapshot` / `companion` / `watch` / `limits`. The core's
  testable front door; use it to verify core changes without touching GTK.
- `crates/app` (`poketoken-app`) — GTK4/libadwaita window (`app.rs`), floating pet
  (`floating.rs`) and the SNI tray (`sni.rs`) in one process. Everything behind `feature = "gui"`.

### Adding or changing a usage provider

`provider.rs` defines `UsageProvider` and `all()`. Each provider module owns its own root
discovery and its own override env var (listed in `OVERRIDE_VARS`) — **no `== "claude_code"`
style branching may appear in shared code**. `usage_store::build_snapshot` just loops over
`provider::all()` and aggregates; it knows nothing provider-specific.

### Incremental usage cache

`usage_cache.rs` is called **from inside each provider**, not from `build_snapshot`: a provider
resolves the cache, keys each log file / SQLite db by `source_key`, and reads only what changed.
Its module doc lists the safety valves (daily full rescan, moving-floor pruning, per-source
validation) — they exist because the UI shows real dollar amounts. Any change here must keep
`crates/core/tests/usage_cache_parity.rs` green: cached and uncached snapshots must be
byte-identical. `PTB_USAGE_CACHE=off` forces full reads.

### Pokémon pool

Not compiled in. `pool.rs` resolves in-memory → `pool-cache.json` (30-day TTL, stale is still
served) → the generated `pool_gen.rs` fallback. `pool::init_live()` refreshes in the background
and hot-swaps. `scripts/gen_pool.py` regenerates the fallback only.

### GUI threading

GTK4 widgets are main-thread-affine. Workers (sprites, limits, tray, pool refresh) publish into
`Arc<Mutex<_>>` queues that main-thread `glib` timers drain. Never touch a widget off-thread.
Usage refresh 15 s, limits 60 s.

### Persistence compatibility

`companion-state.json` must keep loading older files: add fields with serde defaults, extend
`CompanionState::migrate`, never rename. `config.json` (`config.rs`) follows the same rule.

### i18n

Every user-facing string comes from the `L` table in `i18n.rs` (en/ko/ja/es), resolved on each
refresh so a language switch re-renders. No literal UI strings in `app.rs`.

## Tests

Unit tests are inline `#[cfg(test)] mod tests` next to the code — that is where nearly all of
the coverage lives (providers, limits, companion, pool). Use `ProviderCtx::for_test` with a
tempdir home and a pinned offset; pass `--now` / fixed instants rather than wall-clock time.

## Environment variables

`PTB_STATE_DIR`, `PTB_USAGE_CACHE`, `PTB_POOL_OFFLINE`, `PTB_APP_ID`, `PTB_LANG`, `PTB_NO_PET`,
`PTB_TEST_CLAUDE_TOKEN`, `POKE_TOKEN_BAR_HOME`, plus per-provider overrides in
`provider::OVERRIDE_VARS`. Packaging: `PTB_SKIP_BUILD`, `PTB_ICON_URL`, `PTB_MAINTAINER`.
