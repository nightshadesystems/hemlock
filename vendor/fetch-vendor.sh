#!/bin/sh
# fetch-vendor.sh — place all vendor artifacts for a platform.
#
# Usage: vendor/fetch-vendor.sh <platform-id>
#
# Everything Hemlock needs for a real image is publicly downloadable:
#   1. libsaibcm .deb        — SONiC public package server (per-platform pin)
#   2. config.bcm/.soc data  — sonic-buildimage device tree
#   3. saibcm-modules source — GPL kernel-module source from sonic-buildimage
#                              (built for the image kernel by build/build-bde.sh)
# Nothing fetched here is ever committed to git.
set -eu

PLATFORM="${1:?usage: fetch-vendor.sh <platform-id> [--kmod-only]}"
# --kmod-only: fetch only the kernel-module sources (BDE + platform
# drivers), skipping the SAI blobs and ASIC data files. Enough for
# build/kmod-smoke.sh to compile-test modules without vendor binaries.
KMOD_ONLY=0
[ "${2:-}" = "--kmod-only" ] && KMOD_ONLY=1
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PDIR="$ROOT/platforms/$PLATFORM"
SAIDIR="$ROOT/vendor/sai"

[ -d "$PDIR" ] || { echo "error: unknown platform $PLATFORM" >&2; exit 1; }

manifest_value() {
    sed -n "s/^$1[[:space:]]*=[[:space:]]*\"\(.*\)\".*/\1/p" "$PDIR/platform.toml" | head -1
}

fetch() {
    url="$1"; dest="$2"
    if [ -f "$dest" ]; then
        echo "have    $(basename "$dest")"
    else
        echo "fetch   $(basename "$dest")"
        curl -fsSL "$url" -o "$dest"
    fi
}

# Sparse-clone one subdirectory of sonic-buildimage at a branch.
fetch_sonic_subdir() {
    branch="$1"; subdir="$2"; dest="$3"
    if [ -d "$dest" ]; then
        echo "have    $(basename "$dest")/"
        return
    fi
    echo "clone   sonic-buildimage[$branch]/$subdir"
    tmp="$(mktemp -d)"
    git clone -q --depth 1 --branch "$branch" --filter=blob:none --sparse \
        https://github.com/sonic-net/sonic-buildimage.git "$tmp"
    git -C "$tmp" sparse-checkout set "$subdir"
    mkdir -p "$(dirname "$dest")"
    cp -r "$tmp/$subdir" "$dest"
    git -C "$tmp" rev-parse HEAD > "$dest/.sonic-buildimage-commit"
    rm -rf "$tmp"
}

# SONiC public package server layout for Broadcom XGS SAI:
#   .../sai-broadcom/SAI_<maj>.<min>.0_GA/<pin>/xgs/libsaibcm_<pin>_amd64.deb
sai_deb_url() {
    pin="$1"
    branch="SAI_$(echo "$pin" | cut -d. -f1-2).0_GA"
    echo "https://packages.trafficmanager.net/public/sai/sai-broadcom/$branch/$pin/xgs"
}

case "$PLATFORM" in
cel-e1031)
    PIN="$(manifest_value version_pin)"
    SONIC_BRANCH="202305"   # the branch that ships this SAI pin

    if [ "$KMOD_ONLY" = 0 ]; then
        # 1. Vendor SAI blob (+ dev headers for reference/debug).
        URL="$(sai_deb_url "$PIN")"
        fetch "$URL/libsaibcm_${PIN}_amd64.deb"     "$SAIDIR/libsaibcm_${PIN}_amd64.deb"
        fetch "$URL/libsaibcm-dev_${PIN}_amd64.deb" "$SAIDIR/libsaibcm-dev_${PIN}_amd64.deb"

        # 2. Platform data files.
        BASE="https://raw.githubusercontent.com/sonic-net/sonic-buildimage/$SONIC_BRANCH/device/celestica/x86_64-cel_e1031-r0/Celestica-E1031-T48S4"
        fetch "$BASE/helix4-e1031-48x1G%2B4x10G.config.bcm" \
              "$PDIR/helix4-e1031-48x1G+4x10G.config.bcm"
        fetch "$BASE/sai_postinit_cmd.soc" "$PDIR/sai_postinit_cmd.soc"
    fi

    # 3. Kernel module source (GPL), matched to the SAI's SDK lineage.
    fetch_sonic_subdir "$SONIC_BRANCH" "platform/broadcom/saibcm-modules" \
        "$SAIDIR/saibcm-modules"

    # Platform driver sources are NOT fetched: they are committed, ported
    # to the image kernel, under platforms/$PLATFORM/kmod/ (see the
    # README there for upstream provenance).

    echo
    echo "All vendor artifacts for $PLATFORM are in place:"
    echo "  SAI blob:      vendor/sai/libsaibcm_${PIN}_amd64.deb"
    echo "  data files:    platforms/$PLATFORM/"
    echo "  BDE kmod src:  vendor/sai/saibcm-modules/"
    echo "  platform kmod: platforms/$PLATFORM/kmod/ (committed, not fetched)"
    echo "  (mkimage.sh builds both module sets into the image and fails"
    echo "   if any [kernel] required_modules would not be loadable)"
    ;;
*)
    echo "error: no fetch recipe for $PLATFORM (add one to this script)" >&2
    exit 1
    ;;
esac
