#!/usr/bin/env bash
#
# release.sh — cut a new mousehop release.
#
# Bumps the unified workspace version, commits, tags, pushes, then
# publishes every workspace crate to crates.io in dependency order.
# Pushing the tag triggers the Release workflow, which builds and
# publishes the GitHub Release (signed macOS DMGs + Linux / Windows
# binaries).
#
# Run from a clean working tree on `main`.
#
# Usage:
#   scripts/release.sh 0.11.0
#   scripts/release.sh 0.11.0 --dry-run       print every step, change nothing
#   scripts/release.sh 0.11.0 --skip-publish  bump + tag + push, skip crates.io
#
# Re-running with an existing tag skips straight to crates.io, and any
# crate already published at that version is skipped — so a publish
# that fails partway is safe to resume.
#
# Requirements: cargo, perl, git, and (unless --skip-publish) a
# crates.io token — run `cargo login` or set CARGO_REGISTRY_TOKEN.

set -euo pipefail

DRY_RUN=0
SKIP_PUBLISH=0
NEW_VER=""

while (($#)); do
    case "$1" in
        --dry-run)      DRY_RUN=1; shift ;;
        --skip-publish) SKIP_PUBLISH=1; shift ;;
        -h|--help)      sed -n '2,/^$/p' "$0" | sed -e 's/^#//' -e 's/^ //'; exit 0 ;;
        -*)             echo "unknown flag: $1" >&2; exit 2 ;;
        *)              if [[ -z "$NEW_VER" ]]; then NEW_VER="$1"; shift
                        else echo "unexpected argument: $1" >&2; exit 2; fi ;;
    esac
done

[[ -n "$NEW_VER" ]] \
    || { echo "usage: $0 <version> [--dry-run] [--skip-publish]" >&2; exit 2; }
[[ "$NEW_VER" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.]+)?$ ]] \
    || { echo "'$NEW_VER' is not semver (e.g. 0.11.0 or 0.11.0-rc1)" >&2; exit 2; }

cd "$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
TAG="v$NEW_VER"

# crates.io publish order: every crate appears after the crates it
# depends on, so each dependency is already on the index by the time
# the next crate is verified.
CRATES=(
    mousehop-input-event
    mousehop-ipc
    mousehop-proto
    mousehop-input-emulation
    mousehop-input-capture
    mousehop-cli
    mousehop-gtk
    mousehop
)

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mxx\033[0m %s\n' "$*" >&2; exit 1; }
run() { printf '   \033[2m$\033[0m %s\n' "$*"; (( DRY_RUN )) || "$@"; }

# --- preconditions -------------------------------------------------
say "checking preconditions"
command -v cargo >/dev/null || die "cargo not found"
command -v perl  >/dev/null || die "perl not found"
[[ "$(git rev-parse --abbrev-ref HEAD)" == "main" ]] \
    || die "not on 'main' — releases are cut from main"
if (( ! SKIP_PUBLISH )) && (( ! DRY_RUN )); then
    cargo_home="${CARGO_HOME:-$HOME/.cargo}"
    [[ -f "$cargo_home/credentials.toml" || -n "${CARGO_REGISTRY_TOKEN:-}" ]] \
        || die "no crates.io token — run 'cargo login' or set CARGO_REGISTRY_TOKEN"
fi

# An existing tag means the bump / commit / tag / push already
# happened — resume at the crates.io phase rather than failing.
if git rev-parse -q --verify "refs/tags/$TAG" >/dev/null; then
    say "tag $TAG already exists — resuming at the crates.io step"
else
    if (( ! DRY_RUN )) && [[ -n "$(git status --porcelain)" ]]; then
        git status --short
        die "working tree is dirty — commit or stash first"
    fi

    CUR_VER="$(perl -ne 'if (/^version = "([^"]+)"/) { print $1; exit }' Cargo.toml)"
    say "version: ${CUR_VER:-?} -> $NEW_VER"

    # Bump the single [workspace.package] `version` line and the
    # `version` pin on every [workspace.dependencies] path entry.
    # Every crate inherits via `.workspace = true`, so the root
    # Cargo.toml is the only manifest that changes.
    if (( DRY_RUN )); then
        printf '   \033[2m$\033[0m %s\n' "bump workspace version -> $NEW_VER"
    else
        NEW="$NEW_VER" perl -i -pe \
            's{^version = "[^"]*"}{version = "$ENV{NEW}"}' Cargo.toml
        NEW="$NEW_VER" perl -i -pe \
            's{version = "[^"]*"}{version = "$ENV{NEW}"} if /\bpath = "/' Cargo.toml
    fi

    # Refresh Cargo.lock and confirm the workspace still builds.
    say "cargo build (refresh Cargo.lock + sanity check)"
    run cargo build --quiet

    run git add Cargo.toml Cargo.lock
    run git commit -m "Bump version to $NEW_VER"
    run git tag -a "$TAG" -m "$TAG"

    say "pushing main + $TAG — the tag triggers the Release workflow"
    run git push origin main
    run git push origin "$TAG"
fi

# --- publish to crates.io ------------------------------------------
if (( SKIP_PUBLISH )); then
    say "--skip-publish set — stopping before crates.io"
    exit 0
fi

say "publishing ${#CRATES[@]} crates to crates.io (dependency order)"
for crate in "${CRATES[@]}"; do
    if curl -sf -H 'User-Agent: mousehop-release' \
         "https://crates.io/api/v1/crates/$crate/$NEW_VER" >/dev/null 2>&1; then
        say "  $crate@$NEW_VER already published — skipping"
        continue
    fi
    run cargo publish -p "$crate"
done

say "release $TAG complete"
printf '   %s\n' "https://github.com/jondkinney/mousehop/releases/tag/$TAG"
printf '   %s\n' "https://crates.io/crates/mousehop"
