#!/bin/bash
# test-motd.sh — MOTD tests: shellcheck, byte-exact banner rendering, and
# graceful degradation of the status script when data sources are missing.
#
# Usage: build/test-motd.sh [--hemlockctl <path>]
#
#   --hemlockctl   A built hemlockctl binary; enables the end-to-end status
#                  test (daemons down, platform dir mocked from the repo).
#                  Without it those checks are skipped.
#
# Needs no daemons, no root, no hardware: the platform "source" is the
# committed cel-e1031 manifest and everything else degrades by design.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MOTD_DIR="$ROOT/build/rootfs/update-motd.d"
PREVIEW="$ROOT/build/rootfs/bin/hemlock-motd"
CANON="$ROOT/build/tests/motd/banner.txt"

HEMLOCKCTL=""
while [ $# -gt 0 ]; do
    case "$1" in
    --hemlockctl) HEMLOCKCTL="$2"; shift ;;
    *) echo "test-motd: unknown option $1" >&2; exit 1 ;;
    esac
    shift
done

fail=0
check() {
    if eval "$2"; then
        echo "ok    $1"
    else
        echo "FAIL  $1"
        fail=1
    fi
}

# Referenced inside eval'd check strings, which shellcheck cannot see into.
# shellcheck disable=SC2034
esc=$(printf '\033')

# --- shellcheck -------------------------------------------------------------
if command -v shellcheck >/dev/null; then
    check "shellcheck: motd scripts" \
        "shellcheck '$MOTD_DIR/00-hemlock-banner' '$MOTD_DIR/10-hemlock-status' '$PREVIEW' '$ROOT/build/test-motd.sh'"
else
    echo "skip  shellcheck not installed"
fi

# --- banner -----------------------------------------------------------------
check "banner: plain output matches canonical art byte-for-byte" \
    "HEMLOCK_MOTD_COLOR=0 sh '$MOTD_DIR/00-hemlock-banner' | diff -u '$CANON' -"

# Piped stdout is not a tty, so auto mode must also produce plain text.
check "banner: non-tty output is plain (no escape sequences)" \
    "! sh '$MOTD_DIR/00-hemlock-banner' | grep -q \"\$esc\""

check "banner: colored output differs from plain" \
    "! HEMLOCK_MOTD_COLOR=1 sh '$MOTD_DIR/00-hemlock-banner' | diff -q '$CANON' - >/dev/null"

check "banner: colored output minus escapes matches canonical art" \
    "HEMLOCK_MOTD_COLOR=1 sh '$MOTD_DIR/00-hemlock-banner' | sed \"s/\${esc}\\[[0-9;]*m//g\" | diff -u '$CANON' -"

check "banner: no line exceeds 76 columns" \
    "[ \"\$(awk '{ if (length(\$0) > n) n = length(\$0) } END { print n }' '$CANON')\" -le 76 ]"

# --- status: missing data sources ------------------------------------------
# No hemlockctl on PATH at all: silence, exit 0.
check "status: exits 0 and stays silent without hemlockctl" \
    "out=\$(PATH=/nonexistent /bin/sh '$MOTD_DIR/10-hemlock-status') && [ -z \"\$out\" ]"

if [ -n "$HEMLOCKCTL" ]; then
    [ -x "$HEMLOCKCTL" ] || { echo "test-motd: $HEMLOCKCTL is not executable" >&2; exit 1; }
    ctl_dir="$(cd "$(dirname "$HEMLOCKCTL")" && pwd)"

    # Daemons down, platform dir mocked from the committed manifest: the
    # script must exit 0 and render the lines whose sources exist.
    run_status() {
        PATH="$ctl_dir:$PATH" HEMLOCK_PLATFORM_DIR="$ROOT/platforms/cel-e1031" \
            /bin/sh "$MOTD_DIR/10-hemlock-status"
    }
    # out is read inside the eval'd check strings below.
    # shellcheck disable=SC2034
    out="$(run_status)" || { echo "FAIL  status: nonzero exit with daemons down"; fail=1; out=""; }
    check "status: version line present"    "echo \"\$out\" | grep -q '^Hemlock NOS v'"
    check "status: platform line rendered from manifest" \
        "echo \"\$out\" | grep -q '^Platform : Celestica E1031 (Haliburton) (BCM Helix4)\$'"
    check "status: no daemon-backed fields leak when daemons are down" \
        "! echo \"\$out\" | grep -qE 'Temp:|PSU:'"
    check "status: no error text in output" \
        "! echo \"\$out\" | grep -qiE 'error|unavailable|panic'"

    # Login-latency budget. The wrapper hard-caps at 1s; warn (don't fail)
    # above the 200ms design budget so slow CI runners stay green.
    start=$(date +%s%N)
    run_status >/dev/null
    elapsed_ms=$(( ($(date +%s%N) - start) / 1000000 ))
    echo "info  status script took ${elapsed_ms}ms (budget 200ms)"
    check "status: completes within the 1s hard cap" "[ \"$elapsed_ms\" -lt 1000 ]"
    [ "$elapsed_ms" -lt 200 ] || echo "warn  status script exceeded the 200ms design budget"
else
    echo "skip  end-to-end status checks (pass --hemlockctl <path>)"
fi

# --- hemlock-motd preview ---------------------------------------------------
# run-parts needs +x bits; stage copies so the test never depends on git
# file modes (mkimage.sh installs with -m 755 regardless).
if command -v run-parts >/dev/null; then
    stage="$(mktemp -d)"
    trap 'rm -rf "$stage"' EXIT
    cp "$MOTD_DIR/00-hemlock-banner" "$MOTD_DIR/10-hemlock-status" "$stage/"
    chmod 755 "$stage"/*
    check "hemlock-motd: renders the banner via run-parts" \
        "sh '$PREVIEW' '$stage' | head -7 | diff -u '$CANON' -"
else
    echo "skip  run-parts not installed"
fi

if [ "$fail" = 0 ]; then
    echo "test-motd: OK"
else
    echo "test-motd: FAILED" >&2
    exit 1
fi
