#!/usr/bin/env bash
# check-dts.sh — compile the AS4610 device tree without a kernel build.
#
# Usage: platforms/accton-as4610-54/kernel/check-dts.sh
#
# cpp resolves the dt-bindings includes (sparse-fetched from mainline
# v6.1, a few hundred KB, cached), then dtc compiles the result to a
# throwaway .dtb. Catches syntax errors, bad phandles, duplicate unit
# addresses and include drift at the desk instead of in a kernel build
# on the bench.
#
# NOT RUN BY CI (it fetches from the kernel repo); run it after touching
# anything under dts/, the way check-shim.sh is run for the shim.
set -euo pipefail

die() { echo "check-dts: error: $*" >&2; exit 1; }
log() { echo "check-dts: $*"; }

HERE="$(cd "$(dirname "$0")" && pwd)"
KVER="v6.1"

command -v cpp >/dev/null || die "cpp not on PATH"
command -v dtc >/dev/null || die "dtc not on PATH (apt: device-tree-compiler)"
command -v git >/dev/null || die "git not on PATH"

CACHE="${CHECK_DTS_CACHE:-$HERE/.dt-bindings}"
if [ ! -d "$CACHE/include/dt-bindings" ]; then
    log "sparse-fetching $KVER dt-bindings into $CACHE"
    rm -rf "$CACHE"
    git clone -q --filter=blob:none --sparse --no-checkout --depth 1 \
        --branch "$KVER" https://github.com/torvalds/linux.git "$CACHE"
    git -C "$CACHE" sparse-checkout set include/dt-bindings
    git -C "$CACHE" checkout -q HEAD
fi

OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT

for dts in "$HERE"/dts/*.dts; do
    name="$(basename "$dts")"
    log "cpp + dtc: $name"
    cpp -nostdinc -undef -x assembler-with-cpp \
        -I "$HERE/dts" -I "$CACHE/include" \
        "$dts" -o "$OUT/$name.pre" \
        || die "$name does not preprocess"
    dtc -I dts -O dtb -o "$OUT/$name.dtb" "$OUT/$name.pre" \
        || die "$name does not compile"
    log "  $(stat -c '%s bytes' "$OUT/$name.dtb")"
done

log "all device trees compile against $KVER dt-bindings"
