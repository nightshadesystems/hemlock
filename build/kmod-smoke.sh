#!/bin/bash
# kmod-smoke.sh — compile-test every kernel module the image needs, in a
# Debian trixie container, without building a full image.
#
# Usage: build/kmod-smoke.sh [<platform-id>]        (default: cel-e1031)
#
# Runs the same builds mkimage.sh runs in its chroot — build-bde.sh over
# the staged saibcm-modules tree, then every kbuild dir under
# platforms/<platform>/kmod/ — against the trixie kernel headers. Catches
# kernel API drift in minutes on any machine with Docker, instead of a
# full image-build round in CI. Sources are fetched (kmod-only) if not
# already staged.
#
# Exit 0: every module compiled. Nonzero: compiler diagnostics on stderr.
set -euo pipefail

die() { echo "kmod-smoke: error: $*" >&2; exit 1; }
log() { echo "kmod-smoke: $*"; }

PLATFORM="${1:-cel-e1031}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
[ -f "$ROOT/platforms/$PLATFORM/platform.toml" ] || die "unknown platform $PLATFORM"
command -v docker >/dev/null || die "docker not installed (or not on PATH)"
docker info >/dev/null 2>&1 || die "docker daemon not running"

log "compile-testing kernel modules for $PLATFORM against Debian trixie headers"
MSYS_NO_PATHCONV=1 docker run --rm -v "$ROOT:/hemlock" -w /hemlock debian:trixie bash -euo pipefail -c '
    export DEBIAN_FRONTEND=noninteractive
    echo "kmod-smoke: installing toolchain + trixie kernel headers"
    apt-get -qq update
    apt-get -qq install --no-install-recommends -y \
        build-essential bc git ca-certificates curl perl-modules \
        linux-headers-amd64 >/dev/null
    KVER="$(ls /lib/modules | head -1)"
    echo "kmod-smoke: kernel $KVER"

    # Stage the BDE source if the host repo does not have it yet
    # (kmod-only: no SAI blobs needed to compile modules). Platform
    # drivers are committed under platforms/<id>/kmod/.
    [ -d vendor/sai/saibcm-modules ] \
        || sh vendor/fetch-vendor.sh '"$PLATFORM"' --kmod-only

    # Work on a throwaway copy: build-bde.sh patches and builds in-tree,
    # and the mounted repo copy must stay pristine.
    cp -r vendor/sai/saibcm-modules /tmp/saibcm-modules
    bash build/build-bde.sh /tmp/saibcm-modules "$KVER" /tmp/bde-out

    fail=0
    for src in "platforms/'"$PLATFORM"'/kmod"/*/; do
        [ -f "$src/Makefile" ] || continue
        name="$(basename "$src")"
        cp -r "$src" "/tmp/$name"
        echo "kmod-smoke: building platform module dir: $name"
        if ! make -C "/lib/modules/$KVER/build" "M=/tmp/$name" modules \
                > "/tmp/$name.log" 2>&1; then
            echo "kmod-smoke: FAIL $name — diagnostics:" >&2
            grep -E -B3 -A8 " error:" "/tmp/$name.log" >&2 || tail -n 60 "/tmp/$name.log" >&2
            fail=1
        fi
    done
    [ "$fail" = 0 ] || exit 1

    echo "kmod-smoke: all modules compiled:"
    ls -1 /tmp/bde-out/*.ko /tmp/*/[a-z]*.ko 2>/dev/null | sort -u | sed "s/^/  /"
'
log "PASS — module set compiles against trixie"
