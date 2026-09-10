#!/usr/bin/env bash
# Runtime oracle for the update path, against a locally running server.
#
# Publishes a locally signed manifest for a fake 9.9.9 release into the
# development releases directory and drives `vorcall-probe check-update` /
# `apply-update` through it: an honest release is accepted and hash-checked, a
# tampered manifest and a tampered asset are refused, a min_version above the
# running version is reported as required, and the Linux swap-and-relaunch
# replaces a throwaway copy of the probe.
#
# Usage:
#   VORCALL_PROBE_USER=alice VORCALL_PROBE_PASSWORD=... scripts/update-oracle.sh
#
#   --http URL   server to talk to (default http://localhost:5000)
#   --keep       leave the published release and the work directory in place
#
#   VORCALL_RELEASES_DIR   overrides <repo>/releases
#
# Exit codes: 0 every case passed, 1 a case failed, 2 a precondition failed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

HTTP="http://localhost:5000"
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --http)
            [ $# -ge 2 ] || { echo "ERROR: --http wants a URL" >&2; exit 2; }
            HTTP="$2"
            shift 2
            ;;
        --keep)
            KEEP=1
            shift
            ;;
        -h|--help)
            sed -n '2,19p' "${BASH_SOURCE[0]}"
            exit 0
            ;;
        *)
            echo "ERROR: unknown argument $1" >&2
            exit 2
            ;;
    esac
done
HTTP="${HTTP%/}"

REL="${VORCALL_RELEASES_DIR:-$ROOT/releases}"
VERSION="9.9.9"
PLATFORM="linux-x86_64"
ASSET_NAME="vorcall-$PLATFORM"

# ---------------------------------------------------------------- preconditions

if [ -z "${VORCALL_PROBE_USER:-}" ] || [ -z "${VORCALL_PROBE_PASSWORD:-}" ]; then
    echo "ERROR: VORCALL_PROBE_USER and VORCALL_PROBE_PASSWORD must be set to an account on $HTTP" >&2
    exit 2
fi

command -v jq >/dev/null 2>&1 || { echo "ERROR: jq not found on PATH" >&2; exit 2; }

CARGO=cargo
command -v cargo >/dev/null 2>&1 || CARGO="$HOME/.cargo/bin/cargo"
if ! command -v "$CARGO" >/dev/null 2>&1; then
    echo "ERROR: cargo not found on PATH nor at $HOME/.cargo/bin/cargo" >&2
    exit 2
fi

if ! curl -fsS "$HTTP/health" >/dev/null 2>&1; then
    echo "ERROR: no server at $HTTP; run ~/.dotnet/dotnet run --project server/Vorcall.Server.csproj" >&2
    exit 2
fi

# ---------------------------------------------------------------------- build

echo "building vorcall-release and vorcall-probe (debug)"
( cd "$ROOT/client" && "$CARGO" build -p vorcall-release -p vorcall-probe )

RELEASE="$ROOT/client/target/debug/vorcall-release"
PROBE="$ROOT/client/target/debug/vorcall-probe"
for binary in "$RELEASE" "$PROBE"; do
    [ -x "$binary" ] || { echo "ERROR: $binary was not built" >&2; exit 2; }
done

# ------------------------------------------------------------------- work dir

TMP="$(mktemp -d)"
RESTORE=0

cleanup() {
    local status=$?
    if [ "$KEEP" -eq 1 ]; then
        echo "--keep: left $TMP and $REL/$VERSION in place"
        return $status
    fi

    rm -f "$REL/manifest.json" "$REL/manifest.json.tmp" "$REL/manifest.sig" "$REL/manifest.sig.tmp"
    rm -rf "${REL:?}/$VERSION"
    if [ "$RESTORE" -eq 1 ]; then
        cp "$TMP/backup/manifest.json" "$REL/manifest.json"
        if [ -f "$TMP/backup/manifest.sig" ]; then
            cp "$TMP/backup/manifest.sig" "$REL/manifest.sig"
        fi
        echo "restored the manifest that was in $REL before this run"
    fi
    rm -rf "$TMP"
    return $status
}
trap cleanup EXIT

mkdir -p "$REL"
if [ -f "$REL/manifest.json" ]; then
    mkdir -p "$TMP/backup"
    cp "$REL/manifest.json" "$TMP/backup/manifest.json"
    if [ -f "$REL/manifest.sig" ]; then
        cp "$REL/manifest.sig" "$TMP/backup/manifest.sig"
    fi
    RESTORE=1
    echo "backed up the existing $REL/manifest.json"
fi

# A run killed halfway leaves a 9.9.9 release behind; start from a clean slate.
rm -rf "${REL:?}/$VERSION"

# ------------------------------------------------------------------- helpers

PASSED=0
FAILED=0
SKIPPED=0

pass() {
    echo "PASS $1"
    PASSED=$((PASSED + 1))
}

fail() {
    echo "FAIL $1: $2"
    FAILED=$((FAILED + 1))
}

skip() {
    echo "SKIP $1: $2"
    SKIPPED=$((SKIPPED + 1))
}

sha_of() {
    sha256sum "$1" | cut -c1-64
}

# Runs check-update for one case. Stdout lands in $TMP/<case>.json, stderr in
# $TMP/<case>.log, and the exit code in $STATUS — never aborting the script.
STATUS=0
LOG=/dev/null
run_probe() {
    local case_name="$1"
    shift
    STATUS=0
    LOG="$TMP/$case_name.log"
    VORCALL_SERVER_URL="$HTTP" VORCALL_PROBE_PASSWORD="$VORCALL_PROBE_PASSWORD" \
        "$PROBE" check-update \
        --username "$VORCALL_PROBE_USER" \
        --platform "$PLATFORM" \
        --pubkey "$PUB" \
        "$@" \
        >"$TMP/$case_name.json" 2>"$TMP/$case_name.log" || STATUS=$?
}

# assert_json FILE FILTER EXPECTED — compares a jq filter's output; a mismatch
# is reported through the caller's case name in $CASE.
assert_json() {
    local file="$1" filter="$2" expected="$3" actual
    actual="$(jq -r "$filter" <"$file" 2>/dev/null)" || actual="<not JSON>"
    if [ "$actual" = "$expected" ]; then
        return 0
    fi
    fail "$CASE" "$filter is $actual, expected $expected"
    return 1
}

assert_status() {
    local expected="$1"
    if [ "$STATUS" = "$expected" ]; then
        return 0
    fi
    fail "$CASE" "exit $STATUS, expected $expected ($(tail -n 1 "$LOG" 2>/dev/null))"
    return 1
}

# Builds, signs, verifies and installs a manifest for the fake release. The
# signature is moved into place before the manifest so a reader never sees a new
# manifest paired with the previous signature.
publish_manifest() {
    local min="$1"
    "$RELEASE" manifest \
        --version "$VERSION" \
        --min-version "$min" \
        --notes-file "$TMP/notes.txt" \
        --asset "$PLATFORM=$REL/$VERSION/$ASSET_NAME" \
        --out "$TMP/manifest.json" >/dev/null
    "$RELEASE" sign --key-file "$TMP/key.hex" "$TMP/manifest.json" --out "$TMP/manifest.sig" >/dev/null
    "$RELEASE" verify --key "$PUB" "$TMP/manifest.json" "$TMP/manifest.sig" >/dev/null

    cp "$TMP/manifest.sig" "$REL/manifest.sig.tmp"
    cp "$TMP/manifest.json" "$REL/manifest.json.tmp"
    mv -f "$REL/manifest.sig.tmp" "$REL/manifest.sig"
    mv -f "$REL/manifest.json.tmp" "$REL/manifest.json"
}

# ----------------------------------------------------------- key and fake asset

PUB="$("$RELEASE" gen-key --out "$TMP/key.hex")"
printf 'oracle release\n' >"$TMP/notes.txt"

# The asset is the probe itself with a trailer appended: the loader ignores
# bytes past the end of an ELF image, so the fake release still runs (case E
# relaunches it) while hashing differently from the binary it was copied from.
mkdir -p "$REL/$VERSION"
cp "$PROBE" "$REL/$VERSION/$ASSET_NAME"
printf '\nVORCALL-ORACLE-9.9.9\n' >>"$REL/$VERSION/$ASSET_NAME"
chmod +x "$REL/$VERSION/$ASSET_NAME"
ASSET_SHA="$(sha_of "$REL/$VERSION/$ASSET_NAME")"

echo "publishing $VERSION into $REL (asset sha256 ${ASSET_SHA:0:12}...)"
publish_manifest "0.1.0"

# ---------------------------------------------------------------------- cases

CASE="A accepted"
run_probe A --out "$TMP/A.bin"
if assert_status 0 \
    && assert_json "$TMP/A.json" '.update_available' true \
    && assert_json "$TMP/A.json" '.required' false \
    && assert_json "$TMP/A.json" '.verified' true \
    && assert_json "$TMP/A.json" '.manifest_version' "$VERSION" \
    && assert_json "$TMP/A.json" '.sha256' "$ASSET_SHA" \
    && assert_json "$TMP/A.json" '.asset' "$ASSET_NAME"; then
    if [ ! -f "$TMP/A.bin" ]; then
        fail "$CASE" "no file at $TMP/A.bin"
    elif [ "$(sha_of "$TMP/A.bin")" != "$ASSET_SHA" ]; then
        fail "$CASE" "the downloaded file does not hash to the manifest's sha256"
    else
        pass "$CASE"
    fi
fi

CASE="B tampered manifest"
sed 's/oracle release/oracle re1ease/' "$REL/manifest.json" >"$REL/manifest.json.tmp"
mv -f "$REL/manifest.json.tmp" "$REL/manifest.json"
run_probe B --out "$TMP/B.bin"
if assert_status 1 && assert_json "$TMP/B.json" '.stage' signature; then
    if [ -e "$TMP/B.bin" ]; then
        fail "$CASE" "a refused check left $TMP/B.bin behind"
    else
        pass "$CASE"
    fi
fi
# The edited bytes cannot be un-edited byte for byte; re-publishing is the way
# back to a manifest that matches its signature.
publish_manifest "0.1.0"

CASE="C tampered asset"
printf 'x' >>"$REL/$VERSION/$ASSET_NAME"
run_probe C --out "$TMP/C.bin"
if assert_status 1; then
    stage="$(jq -r '.stage' <"$TMP/C.json" 2>/dev/null || echo '<not JSON>')"
    if [ "$stage" != "size" ] && [ "$stage" != "hash" ]; then
        fail "$CASE" ".stage is $stage, expected size or hash"
    elif [ -e "$TMP/C.bin" ]; then
        fail "$CASE" "a refused download left $TMP/C.bin behind"
    else
        pass "$CASE"
    fi
fi
truncate -s -1 "$REL/$VERSION/$ASSET_NAME"
if [ "$(sha_of "$REL/$VERSION/$ASSET_NAME")" != "$ASSET_SHA" ]; then
    echo "ERROR: could not restore the asset bytes after case C" >&2
    exit 2
fi

CASE="D required"
publish_manifest "$VERSION"
run_probe D --out "$TMP/D.bin"
if assert_status 0 \
    && assert_json "$TMP/D.json" '.required' true \
    && assert_json "$TMP/D.json" '.update_available' true; then
    pass "$CASE"
fi
publish_manifest "0.1.0"

CASE="E swap-and-relaunch"
if [ "$(uname -s)" != "Linux" ]; then
    skip "$CASE" "not Linux"
elif [ ! -f "$TMP/A.bin" ]; then
    fail "$CASE" "case A left no verified download to apply"
else
    mkdir -p "$TMP/swap"
    cp "$PROBE" "$TMP/swap/vorcall-probe"
    cp "$TMP/A.bin" "$TMP/swap/.vorcall-update-$VERSION"
    chmod +x "$TMP/swap/vorcall-probe"
    SWAP_EXE="$(readlink -f "$TMP/swap/vorcall-probe")"
    STATUS=0
    LOG="$TMP/E.log"
    "$TMP/swap/vorcall-probe" apply-update --file "$TMP/swap/.vorcall-update-$VERSION" \
        >"$TMP/E.json" 2>"$TMP/E.log" || STATUS=$?
    if assert_status 0 \
        && assert_json "$TMP/E.json" '.relaunched' true \
        && assert_json "$TMP/E.json" '.exe' "$SWAP_EXE"; then
        if [ -e "$TMP/swap/.vorcall-update-$VERSION" ]; then
            fail "$CASE" "the pending file survived the swap"
        elif [ "$(sha_of "$TMP/swap/vorcall-probe")" != "$ASSET_SHA" ]; then
            fail "$CASE" "the swapped binary is not the downloaded asset"
        else
            pass "$CASE"
        fi
    fi
fi

CASE="F no-download"
run_probe F --no-download
if assert_status 0 \
    && assert_json "$TMP/F.json" '.downloaded' null \
    && assert_json "$TMP/F.json" '.verified' false; then
    pass "$CASE"
fi

# --------------------------------------------------------------------- summary

echo "passed $PASSED, failed $FAILED, skipped $SKIPPED"
if [ "$FAILED" -gt 0 ]; then
    exit 1
fi
echo "ALL PASS"
