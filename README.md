# PokeTokenBar (port GNOME / Linux)

Port de [PokeTokenBar](https://github.com/chattymin/PokeTokenBar) — l'application de
barre de menus macOS qui transforme la consommation de tokens de vos CLI de
codage IA (Claude Code, Codex, Gemini CLI, …) en compagnon Pokémon qui grandit —
pour Ubuntu / GNOME.

> **Remerciements** — merci à [chattymin](https://github.com/chattymin), auteur de
> l'application originale. Ce portage s'appuie directement sur son code : la logique
> du cœur (fenêtres de calcul, seuils d'évolution, providers, i18n) est un portage
> fidèle d'une implementation remarquablement propre — modules courts, tests unitaires
> systématiques, aucune dépendance UI.

## Ce que fait l'application

Un unique processus Rust, tout le temps en mémoire de session :

- **Lecture locale des journaux** des CLI d'IA — aucun service externe de usage :
  `~/.claude/projects`, `~/.codex/sessions`, `~/.gemini/tmp`, `~/.gemini/antigravity-cli`,
  `~/.local/share/opencode`, `~/.hermes`, `~/.config/Cursor`, `~/.grok/sessions`,
  `~/.copilot`, `~/.config/kiro-cli` (10 providers : Claude, Codex, Gemini, Grok,
  OpenCode, Hermes, Cursor, Copilot, Kiro, Antigravity).
- **Agrégation** en fenêtres jour / semaine / mois (heure locale) + fenêtre de burn
  5 h de Claude, avec table de coûts par modèle et dédoublonnage des entrées.
- **Limites officielles** en direct : quota 5 h / semaine de Claude (OAuth Anthropic)
  et fenêtres de Codex (via `codex app-server`) — se dégradent en « non disponible »
  sans identifiants, sans binaire ou token expiré.
- **Compagnon Pokémon** : un œuf éclos à 5 M de tokens cumulés, puis évolution et
  graduation selon des seuils exacts par rareté, avec réserve de surplus. Boutique
  (candy, sève, œufs premium), sac (objets passifs), nature, chance shiny, Pokédex.
  Artworks officiels PokéAPI (sprites animés, cachés sur disque, repli emoji hors
  ligne). Interface traduite (en / ko / ja / es).
- **Interface GNOME** :
  - icône de tray (StatusNotifierItem) : pokéball — clic gauche bascule le **pet
    flottant** (sprite sur le bureau), clic droit : menu Ouvrir / Quitter ;
  - fenêtre libadwaita (miroir du popover macOS) : 4 onglets — **Accueil** (companion,
    usage, limites, liste par provider), **Boutique**, **Sac**, **Collection** —
    plus une page **Réglages** (langue, premier jour de semaine) ;
  - rafraîchissement de l'usage toutes les 15 s, limites toutes les 60 s ;
  - application **mono-instance** (bus name `io.github.poketoken.app`) : un second
    lancement transfère à l'instance existante.

## Architecture (brève)

Workspace Rust de 3 crates :

| Crate | Rôle |
|---|---|
| `crates/core` | Le cœur portable, **sans aucune UI** : providers (`src/providers/`), fenêtres + coûts, limites, companion, i18n, cache SQLite à watermarks incrémentaux, client sprites PokéAPI (`rustemon`), config et chemins XDG. |
| `crates/cli` | `poke-token-bar` : `snapshot` / `companion` / `watch` / `limits`, headless et testable — c'est la porte d'entrée du cœur. |
| `crates/app` | `poketoken-app` : la fenêtre GTK4/libadwaita, le pet flottant et le tray SNI en **même processus** (D-Bus via `zbus`, pur Rust — pas de libdbus, pas de GTK3). |

Règle de threading : les widgets GTK4 sont affiliés au thread principal ; les
travailleurs (sprites, limites, tray) publient des résultats dans des files
`Arc<Mutex<_>>` qu'un timer du thread principal vidange. La lecture des usages est
incrémentale (watermarks par provider + cache `usage-cache.sqlite`) : ~0,05 s en
régime établi au lieu de ~1,5 s en relecture complète.

## Build

```bash
# cœur + CLI (aucune dépendance GUI)
cargo build
cargo test
cargo clippy --all-targets -- -D warnings

# GUI (nécessite les headers GTK4 + libadwaita)
sudo apt install libgtk-4-dev libadwaita-1-dev
cargo run -p poketoken-app --features gui
```

### Lancer l'application

```bash
cargo run -p poketoken-app --features gui
```

Dépannage « fenêtre figée » : l'app est mono-instance — si une recompile ne change
rien, une ancienne instance est encore primaire. `pkill -x poketoken-app` puis
relancer. `PTB_APP_ID=…` permet de faire tourner une instance de test à côté.

### CLI (headless)

```bash
cargo run -p poketoken-cli -- snapshot            # jour / semaine / mois + burn 5 h
cargo run -p poketoken-cli -- companion           # fait évoluer le companion
cargo run -p poketoken-cli -- watch --interval 15 # ticker en direct
cargo run -p poketoken-cli -- limits              # fenêtres Claude + Codex (--json dispo)
```

## Packaging `.deb`

```bash
sudo apt install dpkg-dev libgtk-4-dev libadwaita-1-dev curl
scripts/make-deb.sh                 # build release + assemblage + dpkg-deb
sudo dpkg -i dist/poketoken_0.1.0_amd64.deb
```

Le paquet installe `/usr/bin/poketoken` (app) et `/usr/bin/poke-token-bar` (CLI),
le `.desktop` (nommé `io.github.poketoken.app` pour le binding de l'icône de
titlebar par gnome-shell) et l'icône hicolor. `PTB_SKIP_BUILD=1` repackage sans
recompiler ; `PTB_ICON_URL` surcharge l'icône téléchargée.

## Données et variables d'environnement

- État du companion : `~/.local/share/PokeTokenBar/` — surcharge avec `PTB_STATE_DIR`.
- Cache d'usage : `~/.cache/PokeTokenBar/usage-cache.sqlite` — `PTB_USAGE_CACHE=off`
  force les relectures complètes (parité byte-identique, couverte par un test).
- Racine de recherche des providers : `POKE_TOKEN_BAR_HOME` (défaut `$HOME`).

## Licence

MIT (code de ce portage). Projet Pokémon non officiel et non commercial — voir
l'avenant de l'upstream.
