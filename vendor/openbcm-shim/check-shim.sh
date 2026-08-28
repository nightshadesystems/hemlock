#!/usr/bin/env bash
# check-shim.sh — header-true compile check of hemlockbcm.c, no SDK build.
#
# Usage: vendor/openbcm-shim/check-shim.sh
#
# build-shim.sh needs the whole OpenBCM tree, its libraries built for
# iproc-4_4, and an ARM cross toolchain — so the shim spent phase 6 as
# ~2500 lines of C that had only ever been *read* against the SDK
# headers. This script closes most of that gap without any of those
# prerequisites: it sparse-fetches just the SDK's header directories
# (a few MB, cached) and runs `gcc -fsyntax-only -Wall -Wextra` with
# the same feature defines build-shim.sh uses. Every signature, struct
# field, enum member and macro the shim touches is checked by a real
# compiler against the real headers.
#
# What it does NOT prove: linking (the SDK libraries are absent) and
# ARM-specific type widths (the host compiler is x86_64; int and the
# SDK's fixed-width types match, but long differs — the shim avoids
# bare long for exactly that reason).
#
# NOT RUN BY CI, deliberately: CI must never require the OpenBCM tree,
# and this fetches one (headers only). Run it locally after touching
# the shim, the way build/kmod-smoke.sh is run after touching a kernel
# module.
set -euo pipefail

die() { echo "check-shim: error: $*" >&2; exit 1; }
log() { echo "check-shim: $*"; }

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
SDK_VER="sdk-6.5.16"

command -v gcc >/dev/null || die "gcc not on PATH"
command -v git >/dev/null || die "git not on PATH"

# A full staged tree wins; otherwise a cached headers-only sparse clone.
if [ -d "$ROOT/vendor/openbcm/$SDK_VER/include/bcm" ]; then
    SDK="$ROOT/vendor/openbcm/$SDK_VER"
    log "using the staged SDK tree at $SDK"
else
    CACHE="${CHECK_SHIM_CACHE:-$ROOT/vendor/openbcm/.header-check}"
    SDK="$CACHE/$SDK_VER"
    if [ ! -d "$SDK/include/bcm" ]; then
        log "sparse-fetching the SDK headers into $CACHE"
        rm -rf "$CACHE"
        git clone -q --filter=blob:none --sparse --depth 1 \
            https://github.com/Broadcom-Network-Switching-Software/OpenBCM.git \
            "$CACHE"
        git -C "$CACHE" sparse-checkout set \
            "$SDK_VER/include" \
            "$SDK_VER/systems/linux/kernel/modules/include" \
            "$SDK_VER/systems/bde/linux/include"
    else
        log "using cached headers at $SDK"
    fi
fi

# The same feature defines build-shim.sh compiles with. INCLUDE_L3 is
# load-bearing: without it the SDK's own headers compile the entire L3
# API away, which is how the first run of this script caught it missing
# from the build script. BCM_ESW_SUPPORT pulls the generated chip enums
# (soc/mcm) that soc/drv.h needs; the chip-model defines come from the
# SDK itself under it.
log "gcc -fsyntax-only -Wall -Wextra over hemlockbcm.c"
gcc -fsyntax-only -Wall -Wextra \
    -DINCLUDE_KNET \
    -DINCLUDE_L3 \
    -DLE_HOST=1 \
    -DLINUX \
    -DBCM_ESW_SUPPORT \
    -I"$ROOT/src/hemlock-sai/openbcm-shim" \
    -I"$SDK/include" \
    -I"$SDK/systems/linux/kernel/modules/include" \
    -I"$SDK/systems/bde/linux/include" \
    "$HERE/hemlockbcm.c" \
    || die "the shim does not compile against the $SDK_VER headers"

log "hemlockbcm.c is header-clean against $SDK_VER"
