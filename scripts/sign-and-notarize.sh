#!/usr/bin/env bash
#
# Sign (and, for a .dmg, notarize + staple) a macOS artifact for Mousehop.
#
# Designed to run on a GitHub Actions macos runner. A throwaway keychain
# holds the Developer ID Application cert for the duration of the job
# and is torn down on exit (success or failure).
#
# Usage:
#   sign-and-notarize.sh --app target/release/bundle/osx/Mousehop.app
#       Code-sign the bundle and every nested dylib (Developer ID +
#       hardened runtime). No notarization here — the .app is notarized
#       as part of the DMG.
#   sign-and-notarize.sh --dmg target/release/bundle/osx/mousehop-macos-arm64.dmg
#       Sign, notarize (Apple inspects the nested .app too), and
#       staple the DMG. One notary round-trip covers everything.
#
# Required env vars (all set as GitHub Actions secrets):
#   MACOS_CERTIFICATE_P12_BASE64   base64 of the exported Developer ID
#                                  Application .p12 (cert + private key)
#   MACOS_CERTIFICATE_PASSWORD     password used when exporting the .p12
#   MACOS_NOTARY_APPLE_ID          Apple ID email
#   MACOS_NOTARY_TEAM_ID           10-char team ID
#   MOUSEHOP_SIGNING_PASSWORD      app-specific password from
#                                  appleid.apple.com, used by notarytool
#
# Local development: leave MACOS_CERTIFICATE_P12_BASE64 unset and this
# script no-ops with a notice — the ad-hoc signature from
# copy-macos-dylib.sh stays in place so the build still launches after
# the right-click → Open dance.

set -euo pipefail

# --- args ----------------------------------------------------------
TARGET=""
KIND=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --app) TARGET="$2"; KIND="app"; shift 2 ;;
        --dmg) TARGET="$2"; KIND="dmg"; shift 2 ;;
        -h|--help)
            sed -n '2,30p' "$0"
            exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ -z "$TARGET" || -z "$KIND" ]]; then
    echo "usage: $0 --app <path> | --dmg <path>" >&2
    exit 2
fi

if [[ ! -e "$TARGET" ]]; then
    echo "error: $TARGET not found" >&2
    exit 1
fi

# --- credential / no-op gate --------------------------------------
if [[ -z "${MACOS_CERTIFICATE_P12_BASE64:-}" ]]; then
    echo "==> MACOS_CERTIFICATE_P12_BASE64 unset; skipping notarized signing for $TARGET."
    echo "    (Local builds keep their ad-hoc signature; CI must set the secret.)"
    exit 0
fi

: "${MACOS_CERTIFICATE_PASSWORD:?required when MACOS_CERTIFICATE_P12_BASE64 is set}"
: "${MACOS_NOTARY_APPLE_ID:?required for notarytool submission}"
: "${MACOS_NOTARY_TEAM_ID:?required for notarytool submission}"
: "${MOUSEHOP_SIGNING_PASSWORD:?app-specific password required for notarytool}"

# RUNNER_TEMP is set on GitHub Actions; fall back to a mktemp dir so
# the script also runs cleanly if invoked outside CI for testing.
WORK_DIR="${RUNNER_TEMP:-$(mktemp -d)}"
KEYCHAIN_PATH="$WORK_DIR/mousehop-signing.keychain-db"
P12_PATH="$WORK_DIR/mousehop-signing.p12"
KEYCHAIN_PASSWORD="$(openssl rand -hex 32)"

# Snapshot the existing keychain search list so we can restore it on
# exit. Without restoring, subsequent steps in the same job inherit
# our temporary keychain even after we delete the file.
ORIGINAL_KEYCHAINS="$(security list-keychains -d user | tr -d '"' | xargs)"

# Capture the triggering exit status first, then `exit "$rc"` at the
# end. Without that, the EXIT trap's last command (`rm`, which
# succeeds) becomes the script's exit status on bash 3.2 — so a
# crashed run would falsely report success to the caller.
cleanup() {
    local rc=$?
    if [[ -f "$KEYCHAIN_PATH" ]]; then
        # shellcheck disable=SC2086
        security list-keychains -d user -s $ORIGINAL_KEYCHAINS 2>/dev/null || true
        security delete-keychain "$KEYCHAIN_PATH" 2>/dev/null || true
    fi
    rm -f "$P12_PATH"
    exit "$rc"
}
trap cleanup EXIT

# --- temp keychain + cert import ----------------------------------
echo "==> Setting up temporary signing keychain"
security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN_PATH"
# Auto-lock after 6h is plenty for one job; we delete it long before then.
security set-keychain-settings -lut 21600 "$KEYCHAIN_PATH"
security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN_PATH"

# Prepend the temp keychain so codesign finds our identity first, but
# keep the previous list so other tooling on the runner still works.
# shellcheck disable=SC2086
security list-keychains -d user -s "$KEYCHAIN_PATH" $ORIGINAL_KEYCHAINS

echo "==> Importing Developer ID Application cert"
echo "$MACOS_CERTIFICATE_P12_BASE64" | base64 --decode > "$P12_PATH"
security import "$P12_PATH" \
    -k "$KEYCHAIN_PATH" \
    -P "$MACOS_CERTIFICATE_PASSWORD" \
    -T /usr/bin/codesign \
    -T /usr/bin/security
# Without this, codesign hangs on a UI prompt asking to allow access
# to the private key. apple-tool: + apple: cover codesign / security.
security set-key-partition-list \
    -S apple-tool:,apple: \
    -s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN_PATH" >/dev/null

IDENTITY="$(security find-identity -v -p codesigning "$KEYCHAIN_PATH" \
    | awk -F'"' '/Developer ID Application/ { print $2; exit }')"
if [[ -z "$IDENTITY" ]]; then
    echo "error: no Developer ID Application identity found after import" >&2
    security find-identity -v -p codesigning "$KEYCHAIN_PATH" >&2
    exit 1
fi
echo "==> Signing identity: $IDENTITY"

# No --entitlements: Mousehop needs none (Accessibility / Input
# Monitoring go through runtime TCC prompts, not entitlements).
# Add the flag here if that ever changes.

# --- sign the .app, then stop ---------------------------------------
# The .app pass only code-signs. package the signed bundle into the
# DMG, and the --dmg pass notarizes the DMG as a whole — Apple inspects
# the nested .app within that single submission. One notary round-trip
# instead of two; the dragged-out app still clears Gatekeeper via
# Apple's online ticket lookup on first launch.
if [[ "$KIND" == "app" ]]; then
    # Inside-out: nested executables/frameworks/dylibs first, then the
    # outer wrapper. Mousehop's bundle carries the GTK / libadwaita
    # dylibs under Contents/Frameworks (copied in by
    # copy-macos-dylib.sh), so this loop signs each of them before the
    # wrapper seals the bundle.
    if [[ -d "$TARGET/Contents/Frameworks" || -d "$TARGET/Contents/PlugIns" || -d "$TARGET/Contents/Helpers" ]]; then
        while IFS= read -r -d '' nested; do
            echo "==> Signing nested: $nested"
            codesign --force --options runtime --timestamp \
                --keychain "$KEYCHAIN_PATH" \
                --sign "$IDENTITY" \
                "$nested"
        done < <(find \
            "$TARGET/Contents/Frameworks" \
            "$TARGET/Contents/PlugIns" \
            "$TARGET/Contents/Helpers" \
            -type f \( -perm -u+x -o -name "*.dylib" \) -print0 2>/dev/null || true)
    fi

    # Sign the main binary explicitly before the wrapper — inside-out
    # order, so the wrapper seals an already-signed executable.
    echo "==> Signing main executable"
    codesign --force --options runtime --timestamp \
        --keychain "$KEYCHAIN_PATH" \
        --sign "$IDENTITY" \
        "$TARGET/Contents/MacOS/mousehop"

    echo "==> Signing app bundle"
    codesign --force --options runtime --timestamp \
        --keychain "$KEYCHAIN_PATH" \
        --sign "$IDENTITY" \
        "$TARGET"

    codesign --verify --deep --strict --verbose=2 "$TARGET"
    echo "Done: $TARGET signed (notarization happens on the DMG)."
    exit 0
fi

# --- sign the DMG --------------------------------------------------
# DMGs aren't executable code, so no hardened runtime — but
# --timestamp is still required for notarization to accept it.
echo "==> Signing DMG"
codesign --force --timestamp \
    --keychain "$KEYCHAIN_PATH" \
    --sign "$IDENTITY" \
    "$TARGET"
codesign --verify --verbose=2 "$TARGET"

# --- notarize ------------------------------------------------------
# Submit once, then poll `notarytool info` in our own loop. We do NOT
# use `notarytool submit --wait`: that is a single long-lived poller
# that aborts the whole build on ANY transient network blip, and
# Apple's queue routinely keeps us polling for the better part of an
# hour. Polling ourselves means one failed status check just sleeps
# and retries — a runner network hiccup can't kill the run.
notary() {
    xcrun notarytool "$@" \
        --apple-id "$MACOS_NOTARY_APPLE_ID" \
        --team-id "$MACOS_NOTARY_TEAM_ID" \
        --password "$MOUSEHOP_SIGNING_PASSWORD"
}

echo "==> Submitting to Apple notary service: $TARGET"
SUBMIT_JSON=""
for attempt in 1 2 3; do
    if SUBMIT_JSON="$(notary submit "$TARGET" --output-format json 2>/dev/null)"; then
        break
    fi
    echo "    submit attempt $attempt/3 failed to reach Apple; retrying in 30s..." >&2
    SUBMIT_JSON=""
    sleep 30
done
if [[ -z "$SUBMIT_JSON" ]]; then
    echo "error: notarytool could not upload to Apple after 3 attempts." >&2
    echo "       Looks like a runner network problem — re-run the workflow." >&2
    exit 1
fi
SUBMIT_ID="$(printf '%s' "$SUBMIT_JSON" | grep -o '"id":[^,}]*' | head -1 | awk -F'"' '{print $4}' || true)"
if [[ -z "$SUBMIT_ID" ]]; then
    echo "error: could not parse a submission id from notarytool output:" >&2
    printf '%s\n' "$SUBMIT_JSON" >&2
    exit 1
fi
echo "    submission id: $SUBMIT_ID"

# Poll for a verdict. A failed `info` call (transient network) just
# logs and retries — it cannot kill the build. Apple's queue is the
# slow part; give it up to 45 minutes overall.
POLL_DEADLINE=$(( $(date +%s) + 45 * 60 ))
NOTARY_STATUS=""
while true; do
    if (( $(date +%s) > POLL_DEADLINE )); then
        echo "error: notarization did not finish within 45 minutes." >&2
        echo "       Submission $SUBMIT_ID is still valid at Apple; check it with" >&2
        echo "       'xcrun notarytool info $SUBMIT_ID ...' and re-run the workflow." >&2
        exit 1
    fi
    if INFO_JSON="$(notary info "$SUBMIT_ID" --output-format json 2>/dev/null)"; then
        NOTARY_STATUS="$(printf '%s' "$INFO_JSON" | grep -o '"status":[^,}]*' | head -1 | awk -F'"' '{print $4}' || true)"
        case "$NOTARY_STATUS" in
            Accepted)
                echo "    notarization Accepted"
                break ;;
            "In Progress"|"")
                echo "    waiting on Apple notary service (In Progress)..."
                sleep 30 ;;
            *)
                echo "error: Apple rejected notarization (status: ${NOTARY_STATUS})." >&2
                echo "==> notary log for $SUBMIT_ID:" >&2
                notary log "$SUBMIT_ID" >&2 || true
                exit 1 ;;
        esac
    else
        echo "    notary status poll failed to reach Apple; retrying in 30s..." >&2
        sleep 30
    fi
done

# --- staple --------------------------------------------------------
echo "==> Stapling notarization ticket"
xcrun stapler staple "$TARGET"
xcrun stapler validate "$TARGET"

echo "Done: $TARGET signed, notarized, and stapled."
