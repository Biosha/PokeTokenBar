#!/usr/bin/env bash
#
# release.sh — one-command version release (the Linux port of the upstream macOS
# `scripts/release.sh`): test gate → doc consistency → version bump → build → commit/tag/push
# → GitHub release with the .deb + tarball attached.
#
# Usage:
#   scripts/release.sh patch                 # 0.1.0 → 0.1.1
#   scripts/release.sh minor|major
#   scripts/release.sh 1.4.0                 # explicit version
#   PTB_NOTES_FILE=/tmp/notes.md scripts/release.sh patch
#   scripts/release.sh --check-only          # doc consistency only, releases nothing
#   scripts/release.sh patch --dry-run       # everything except push + release
#   scripts/release.sh patch --yes           # never prompt (used by the Release workflow)
#
# Every step aborts the release on failure (set -e). Nothing is pushed until the build of the
# bumped version has succeeded, so a failed run leaves origin/main untouched.

set -euo pipefail
cd "$(dirname "$0")/.."

REPO="Biosha/PokeTokenBar"
# Vars that legitimately live only in test code / CI and need no README entry.
SKIP_VARS="PTB_TEST_CLAUDE_TOKEN"

YES=0
DRY_RUN=0
DRAFT=""
BUMP=""
for arg in "$@"; do
    case "$arg" in
        --yes|-y) YES=1 ;;
        --dry-run) DRY_RUN=1 ;;
        --draft) DRAFT="--draft" ;;
        --check-only) BUMP="--check-only" ;;
        -*) echo "✗ unknown flag: $arg" >&2; exit 1 ;;
        *) BUMP="$arg" ;;
    esac
done
[[ -t 0 ]] || YES=1   # no tty (CI): prompts would read EOF and abort

confirm() { # confirm <question> — abort unless the user says yes (auto-yes when --yes)
    [[ "$YES" == "1" ]] && return 0
    read -r -p "  $1 [y/N] " a
    [[ "$a" == "y" || "$a" == "Y" ]] || { echo "aborted."; exit 1; }
}

CURRENT="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

# ── Doc consistency (mechanical checks only; content is on the human) ──────────────────────
# `$1` = the version the docs should name (the current one in --check-only, the new one when
# releasing). Returns 1 on warnings.
doc_check() {
    local want="$1" warn=0
    echo "▶ doc consistency"

    # 1. Env vars used in code must be documented in the README (this port grows them fast).
    local documented missing=""
    for var in $(grep -rhoE 'PTB_[A-Z_]+' crates/*/src crates/core/src/providers scripts \
                 | sort -u); do
        [[ " $SKIP_VARS " == *" $var "* ]] && continue
        grep -q "$var" README.md || missing+=" $var"
    done
    if [[ -n "$missing" ]]; then
        echo "  ⚠ env vars used in code but absent from README.md:$missing"
        warn=1
    fi

    # 2. Hardcoded versions in the docs (the `dpkg -i dist/poketoken_X.Y.Z_amd64.deb` line).
    documented="$(grep -oE 'poketoken_[0-9]+\.[0-9]+\.[0-9]+_' README.md | head -1 \
                  | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' || true)"
    if [[ -n "$documented" && "$documented" != "$want" ]]; then
        echo "  ⚠ README names version $documented, expected $want"
        warn=1
    fi

    # 3. UI changed since the last tag but the README feature list did not → stale docs.
    local last_tag
    last_tag="$(git describe --tags --match 'v*' --abbrev=0 2>/dev/null || true)"
    if [[ -n "$last_tag" ]]; then
        local ui_changed doc_changed
        ui_changed="$(git diff --name-only "$last_tag"..HEAD -- crates/app/src crates/core/src/i18n.rs)"
        doc_changed="$(git diff --name-only "$last_tag"..HEAD -- README.md)"
        if [[ -n "$ui_changed" && -z "$doc_changed" ]]; then
            echo "  ⚠ UI/i18n changed since $last_tag with no README update:"
            echo "$ui_changed" | sed 's/^/       /'
            warn=1
        fi
    fi

    [[ "$warn" == "0" ]] && echo "  ✓ clean"
    return $warn
}

if [[ "$BUMP" == "--check-only" ]]; then
    doc_check "$CURRENT" || true
    exit 0
fi

[[ -n "$BUMP" ]] || { echo "usage: release.sh <patch|minor|major|x.y.z> [--dry-run] [--yes]" >&2; exit 1; }

# ── 1/6 preflight ─────────────────────────────────────────────────────────────────────────
echo "▶ 1/6 preflight"
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
[[ "$BRANCH" == "main" ]] || { echo "✗ run from main (on: $BRANCH) — commit/push target must match"; exit 1; }
[[ -z "$(git status --porcelain)" ]] || { echo "✗ working tree is dirty — commit or stash first"; exit 1; }
[[ "$DRY_RUN" == "1" ]] || command -v gh >/dev/null \
    || { echo "✗ gh CLI not found — needed to publish the release"; exit 1; }
command -v dpkg-deb >/dev/null || { echo "✗ dpkg-deb not found — apt install dpkg-dev"; exit 1; }

VERSION="$(scripts/bump-version.sh "$BUMP")"      # bumps Cargo.toml in place
git rev-parse -q --verify "refs/tags/v$VERSION" >/dev/null \
    && { git checkout -- Cargo.toml; echo "✗ tag v$VERSION already exists"; exit 1; }
# From here on a failure leaves an edited Cargo.toml; restore it so a retry starts clean.
trap 'git checkout -- Cargo.toml Cargo.lock README.md 2>/dev/null || true' ERR
echo "  ✓ $CURRENT → $VERSION on main"

# ── 2/6 test gate ─────────────────────────────────────────────────────────────────────────
echo "▶ 2/6 test gate (fmt, clippy, tests — same commands as CI)"
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --quiet -- -D warnings
cargo test --workspace --quiet
scripts/bump-version.sh --self-test
echo "  ✓ green"

# ── 3/6 docs ──────────────────────────────────────────────────────────────────────────────
# The install line is mechanical, so it is rewritten rather than reported.
sed -i -E "s/poketoken_[0-9]+\.[0-9]+\.[0-9]+_/poketoken_${VERSION}_/g" README.md
doc_rc=0; doc_check "$VERSION" || doc_rc=$?
[[ $doc_rc -eq 0 ]] || confirm "doc warnings above. Release anyway?"

# ── 4/6 build (before any push: a failure here costs nothing) ──────────────────────────────
echo "▶ 4/6 build .deb + tarball"
cargo update --workspace --quiet          # re-lock the workspace crates at $VERSION
scripts/make-deb.sh >/dev/null
DEB="dist/poketoken_${VERSION}_$(dpkg --print-architecture).deb"
[[ -f "$DEB" ]] || { echo "✗ expected $DEB, not produced"; exit 1; }
BUILT="$(dpkg-deb --field "$DEB" Version)"
[[ "$BUILT" == "$VERSION" ]] || { echo "✗ package version mismatch: $BUILT ≠ $VERSION"; exit 1; }
TARBALL="dist/poketoken-${VERSION}-$(uname -m)-linux.tar.gz"
tar -czf "$TARBALL" -C target/release poketoken-app poke-token-bar
echo "  ✓ $DEB + $TARBALL"

if [[ "$DRY_RUN" == "1" ]]; then
    echo "▶ dry run — reverting the bump, nothing pushed"
    git checkout -- Cargo.toml Cargo.lock README.md
    echo "✓ dry run for v$VERSION ok"
    exit 0
fi

# ── 5/6 commit, tag, push ─────────────────────────────────────────────────────────────────
echo "▶ 5/6 commit + tag + push"
git add Cargo.toml Cargo.lock README.md
git commit -q -m "release: bump version to $VERSION"
git tag -a "v$VERSION" -m "v$VERSION"
git push -q origin main "v$VERSION"
trap - ERR

# ── 6/6 publish ───────────────────────────────────────────────────────────────────────────
echo "▶ 6/6 GitHub release v$VERSION"
NOTES=(--generate-notes)
[[ -n "${PTB_NOTES_FILE:-}" && -f "${PTB_NOTES_FILE:-}" ]] && NOTES=(--notes-file "$PTB_NOTES_FILE")
gh release create "v$VERSION" "$DEB" "$TARBALL" --repo "$REPO" \
    --title "PokeTokenBar v$VERSION" --target main $DRAFT "${NOTES[@]}"

echo "✓ v$VERSION released. Verify: gh release view v$VERSION --repo $REPO"
