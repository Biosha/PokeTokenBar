# PokeTokenBar (GNOME / Linux port)

Port of [PokeTokenBar](https://github.com/chattymin/PokeTokenBar) — the macOS
menu-bar app that turns your AI-coding CLI token usage (Claude Code, Codex,
Gemini CLI, …) into a growing Pokémon companion — to Ubuntu / GNOME.

> **Credits** — thanks to [chattymin](https://github.com/chattymin), author of
> the original app. This port builds directly on his code: the core logic
> (usage windows, evolution thresholds, providers, i18n) is a faithful port of
> a remarkably clean implementation — short modules, systematic unit tests, no
> UI dependencies.

## What the app does

A single Rust process, always in your session:

- **Local log reading** of your AI CLIs — no external usage service:
  `~/.claude/projects`, `~/.codex/sessions`, `~/.gemini/tmp`, `~/.gemini/antigravity-cli`,
  `~/.local/share/opencode`, `~/.hermes`, `~/.config/Cursor`, `~/.grok/sessions`,
  `~/.copilot`, `~/.config/kiro-cli` (10 providers: Claude, Codex, Gemini, Grok,
  OpenCode, Hermes, Cursor, Copilot, Kiro, Antigravity).
- **Aggregation** into day / week / month windows (local time) + Claude's 5-hour
  burn window, with a per-model cost table and entry deduplication.
- **Official limits**, live: Claude's 5-hour / weekly quota (Anthropic OAuth)
  and Codex windows (via `codex app-server`) — degrading gracefully to
  "not available" when credentials or the binary are missing or the token is
  expired.
- **Pokémon companion**: an egg hatches at 5M cumulative tokens, then evolves
  and graduates according to exact per-rarity thresholds, with surplus carry.
  Shop (candy, mint, premium eggs), bag (passive items), natures, shiny odds,
  Pokédex. Official PokéAPI artwork (animated sprites, disk-cached, emoji
  fallback offline). Localized UI (en / ko / ja / es). The hatch pool is the
  full Gen I–V evolution-line set, **resolved at runtime from PokéAPI** (see
  below) — not compiled in.
- **GNOME interface**:
  - tray icon (StatusNotifierItem): a Pokéball — left-click toggles the
    **floating pet** (sprite on your desktop), right-click: Open / Quit menu;
  - libadwaita window (mirror of the macOS popover): 4 tabs — **Home**
    (companion, usage, limits, per-provider list), **Shop**, **Bag**,
    **Collection** — plus a **Settings** page (language, first day of week);
  - usage refresh every 15 s, limits every 60 s;
  - **single-instance** app (bus name `io.github.poketoken.app`): a second
    launch forwards to the existing instance.

## Architecture (brief)

A 3-crate Rust workspace:

| Crate | Role |
|---|---|
| `crates/core` | The portable core, **no UI at all**: providers (`src/providers/`), windows + costs, limits, companion, i18n, incremental-watermark SQLite cache, PokéAPI clients (sprites via `rustemon`, **runtime pool** via `src/pokeapi.rs`), config and XDG paths. |
| `crates/cli` | `poke-token-bar`: `snapshot` / `companion` / `watch` / `limits`, headless and testable — the core's front door. |
| `crates/app` | `poketoken-app`: the GTK4/libadwaita window, the floating pet and the SNI tray in the **same process** (D-Bus via `zbus`, pure Rust — no libdbus, no GTK3). |

Threading rule: GTK4 widgets are main-thread-affine; the workers (sprites,
limits, tray) publish results into `Arc<Mutex<_>>` queues that a main-thread
timer drains. Usage reading is incremental (per-provider watermarks +
`usage-cache.sqlite`): ~0.05 s in steady state instead of ~1.5 s for a full
 re-read.

## The Pokémon pool (runtime, no recompile)

Like the original macOS app, the hatch pool is **not compiled in** — it is
resolved at runtime from PokéAPI, so a change upstream (capture rates, names,
a new base) reaches users without a recompile or a new release.

Resolution ladder on first access (always local and instant):

1. **In-memory** pool (once loaded for the process);
2. **Disk snapshot** `~/.cache/PokeTokenBar/pool-cache.json` — a stale
   snapshot is used too (the macOS app serves its expired disk index the same
   way);
3. **Bundled fallback** — a generated Gen I–V snapshot
   (`crates/core/src/pool_gen.rs`) that keeps the app fully functional offline.

`pool::init_live()` (called at app/CLI start) spawns a background refresh:
when the disk snapshot is older than the **30-day TTL** it re-fetches the base
index (one GraphQL query), the 649 species rows, and every base's evolution
chain (8-way, chain URLs pinned to `https://pokeapi.co`), persists the new
snapshot, and hot-swaps it in. A partially-failed fetch degrades per-species
onto the bundled data; a full offline launch simply stays on the fallback.

- Disable the live refresh entirely with `PTB_POOL_OFFLINE=1`.
- `scripts/gen_pool.py` regenerates the bundled fallback only (it no longer
  defines the pool).

## Build

```bash
# core + CLI (no GUI dependencies)
cargo build
cargo test
cargo clippy --all-targets -- -D warnings

# GUI (requires GTK4 + libadwaita headers)
sudo apt install libgtk-4-dev libadwaita-1-dev
cargo run -p poketoken-app --features gui
```

### Run the app

```bash
cargo run -p poketoken-app --features gui
```

Troubleshooting a "stale window": the app is single-instance — if a rebuild
changes nothing, an older instance is still the primary one. `pkill -x
poketoken-app` then relaunch. `PTB_APP_ID=…` lets you run an independent test
instance alongside.

### CLI (headless)

```bash
cargo run -p poketoken-cli -- snapshot            # day / week / month + 5h burn
cargo run -p poketoken-cli -- companion           # evolves the companion
cargo run -p poketoken-cli -- watch --interval 15 # live ticker
cargo run -p poketoken-cli -- limits              # Claude + Codex windows (--json available)
```

## `.deb` packaging

```bash
sudo apt install dpkg-dev libgtk-4-dev libadwaita-1-dev curl
scripts/make-deb.sh                 # release build + assembly + dpkg-deb
sudo dpkg -i dist/poketoken_0.1.0_amd64.deb
```

Releases are cut from GitHub Actions (**Release** workflow → pick patch/minor/major): it bumps
the version, tags, and attaches the `.deb` and a binary tarball to the release. Every push to
`main` also uploads a freshly built `.deb` as a CI artifact.

The package installs `/usr/bin/poketoken` (app) and `/usr/bin/poke-token-bar`
(CLI), the `.desktop` file (named `io.github.poketoken.app` so gnome-shell
binds the titlebar icon) and the hicolor icon. `PTB_SKIP_BUILD=1` repackages
without recompiling; `PTB_ICON_URL` overrides the downloaded icon;
`PTB_MAINTAINER` sets the `Maintainer:` control field.

Cutting a release locally is one command — same gates as the workflow, run on your
machine (test gate → doc checks → bump → build → tag/push → GitHub release):

```bash
scripts/release.sh patch            # or minor / major / an explicit 1.4.0
scripts/release.sh patch --dry-run  # build everything, push nothing
scripts/release.sh --check-only     # doc consistency only
PTB_NOTES_FILE=notes.md scripts/release.sh minor   # custom release notes
```

## Data & environment variables

- Companion state: `~/.local/share/PokeTokenBar/` — override with `PTB_STATE_DIR`.
- Usage cache: `~/.cache/PokeTokenBar/usage-cache.sqlite` — `PTB_USAGE_CACHE=off`
  forces full re-reads (byte-identical parity, covered by a test).
- Pokémon pool snapshot: `~/.cache/PokeTokenBar/pool-cache.json` (30-day TTL,
  refreshed in the background) — `PTB_POOL_OFFLINE=1` disables the live refresh
  and serves the bundled fallback.
- Provider search root: `POKE_TOKEN_BAR_HOME` (defaults to `$HOME`).
- UI language override: `PTB_LANG=en|ko|ja|es` (wins over the saved setting).
- App instance: `PTB_APP_ID` (bus name, to run a second instance side by side);
  `PTB_NO_PET=1` builds the floating pet inert (diagnostics).

## License

MIT (this port's code). Unofficial, non-commercial Pokémon fan project — see
the upstream disclaimer.
