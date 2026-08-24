#!/usr/bin/env bash
#
# Bump the workspace version in Cargo.toml (the single source of truth: make-deb.sh and
# every crate read `[workspace.package] version`).
#
#   scripts/bump-version.sh patch|minor|major   # bump in place, print the new version
#   scripts/bump-version.sh 1.4.0               # set an explicit version
#   scripts/bump-version.sh --self-test         # check the arithmetic, touch nothing

set -euo pipefail

next() { # next <current> <patch|minor|major|explicit>
    local cur=$1 kind=$2 major minor patch
    IFS=. read -r major minor patch <<<"$cur"
    case "$kind" in
        major) echo "$((major + 1)).0.0" ;;
        minor) echo "$major.$((minor + 1)).0" ;;
        patch) echo "$major.$minor.$((patch + 1))" ;;
        [0-9]*.[0-9]*.[0-9]*) echo "$kind" ;;
        *) echo "error: bump must be patch|minor|major or an x.y.z version, got '$kind'" >&2; return 1 ;;
    esac
}

if [[ "${1:-}" == "--self-test" ]]; then
    [[ "$(next 0.1.0 patch)" == "0.1.1" ]]
    [[ "$(next 0.1.9 minor)" == "0.2.0" ]]
    [[ "$(next 1.2.3 major)" == "2.0.0" ]]
    [[ "$(next 0.1.0 9.9.9)" == "9.9.9" ]]
    ! next 0.1.0 sideways 2>/dev/null
    echo "bump-version self-test ok"
    exit 0
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$REPO_ROOT/Cargo.toml"

CURRENT="$(sed -n 's/^version = "\(.*\)"/\1/p' "$MANIFEST" | head -1)"
[[ -n "$CURRENT" ]] || { echo "error: no version found in $MANIFEST" >&2; exit 1; }

NEW="$(next "$CURRENT" "${1:?usage: bump-version.sh patch|minor|major|x.y.z}")"
# Only the first `version = ` line, which is the one under [workspace.package].
sed -i "0,/^version = \".*\"/s//version = \"$NEW\"/" "$MANIFEST"
echo "$NEW"
