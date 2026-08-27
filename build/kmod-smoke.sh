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
    echo "kmod-smoke: installing toolchain + trixie kernel (headers + image)"
    apt-get -qq update
    # linux-image too (not just headers): the loadability gate below needs
    # the real module set and modules.builtin, exactly like the image chroot.
    apt-get -qq install --no-install-recommends -y \
        build-essential bc git ca-certificates curl perl-modules kmod \
        linux-headers-amd64 linux-image-amd64 >/dev/null
    KVER="$(ls /lib/modules | head -1)"
    echo "kmod-smoke: kernel $KVER"

    # Stage the BDE source if the host repo does not have it yet
    # (kmod-only: no SAI blobs needed to compile modules). Platform
    # drivers are committed under platforms/_common/kmod/ (every board)
    # and platforms/<id>/kmod/ (this board).
    [ -d vendor/sai/saibcm-modules ] \
        || sh vendor/fetch-vendor.sh '"$PLATFORM"' --kmod-only

    # Work on a throwaway copy: build-bde.sh patches and builds in-tree,
    # and the mounted repo copy must stay pristine.
    cp -r vendor/sai/saibcm-modules /tmp/saibcm-modules
    bash build/build-bde.sh /tmp/saibcm-modules "$KVER" /tmp/bde-out

    fail=0
    for src in "platforms/_common/kmod"/*/ "platforms/'"$PLATFORM"'/kmod"/*/; do
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

    # Same loadability gate as mkimage.sh: install everything we built,
    # then prove every manifest required_modules entry resolves (builtins
    # count — hence modprobe --dry-run, not modinfo).
    install -D -t "/lib/modules/$KVER/updates/hemlock" \
        /tmp/bde-out/*.ko /tmp/*/[a-z]*.ko 2>/dev/null || true
    depmod "$KVER"
    missing=""
    for module in $(sed -n "/^required_modules[[:space:]]*=/,/]/p" \
                        "platforms/'"$PLATFORM"'/platform.toml" \
                    | sed -n "s/^[[:space:]]*\"\([^\"]*\)\".*/\1/p"); do
        modprobe -S "$KVER" --dry-run --quiet "$module" || missing="$missing $module"
    done
    if [ -n "$missing" ]; then
        echo "kmod-smoke: FAIL — required modules not loadable:$missing" >&2
        exit 1
    fi
    echo "kmod-smoke: every [kernel] required_modules entry is loadable"
'
log "PASS — module set compiles and loads against trixie"
