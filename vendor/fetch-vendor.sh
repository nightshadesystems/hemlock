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
accton-as4610-54)
    # This board has no SAI: no libsaibcm is published for armhf (SONiC's
    # sai.mk builds _amd64.deb only, and the package server 404s on
    # _armhf/_arm64). Its datapath is Hemlock's own shim over the
    # source-available OpenBCM SDK — so what gets fetched here is an SDK
    # tree, not a vendor blob. See docs/as4610-54-port.md.
    OPENBCM_DIR="$ROOT/vendor/openbcm"
    # sdk-6.5.16 is a DIRECTORY on master, not a branch or a tag. Pin the
    # commit so a rebuild is reproducible; bcm56340_a0 (Helix4) and the
    # iproc-4_4 target are both present at this revision.
    OPENBCM_COMMIT="${OPENBCM_COMMIT:-master}"
    OPENBCM_SDK="sdk-6.5.16"

    if [ -d "$OPENBCM_DIR/$OPENBCM_SDK" ]; then
        echo "have    $OPENBCM_SDK/"
    else
        echo "clone   OpenBCM/$OPENBCM_SDK (sparse; this is a large tree)"
        tmp="$(mktemp -d)"
        git clone -q --filter=blob:none --sparse \
            https://github.com/Broadcom-Network-Switching-Software/OpenBCM.git "$tmp"
        [ "$OPENBCM_COMMIT" = "master" ] || git -C "$tmp" checkout -q "$OPENBCM_COMMIT"
        git -C "$tmp" sparse-checkout set "$OPENBCM_SDK"
        mkdir -p "$OPENBCM_DIR"
        cp -r "$tmp/$OPENBCM_SDK" "$OPENBCM_DIR/$OPENBCM_SDK"
        git -C "$tmp" rev-parse HEAD > "$OPENBCM_DIR/.openbcm-commit"
        rm -rf "$tmp"
        echo "pinned  $(cat "$OPENBCM_DIR/.openbcm-commit")"
    fi

    # Verify the chip is actually in this tree before anyone spends an
    # hour building it. The SDK's README advertises only the TD/TH
    # families, but that is a *support* statement: make/Make.local.template
    # says the default build includes every chip in the release.
    if [ -f "$OPENBCM_DIR/$OPENBCM_SDK/src/soc/mcm/bcm56340_a0.c" ]; then
        echo "ok      bcm56340_a0 (Helix4) present in $OPENBCM_SDK"
    else
        echo "error: bcm56340_a0 not found in $OPENBCM_SDK — wrong SDK revision?" >&2
        exit 1
    fi
    if [ -d "$OPENBCM_DIR/$OPENBCM_SDK/systems/linux/user/iproc-4_4" ]; then
        echo "ok      iproc-4_4 build target present"
    else
        echo "error: iproc-4_4 target missing from $OPENBCM_SDK" >&2
        exit 1
    fi

    if [ "$KMOD_ONLY" = 0 ]; then
        # ASIC init config, dumped from this board's stock ICOS NOS and
        # carried by the edgenos reference.
        fetch "https://raw.githubusercontent.com/wrightca1/edgenos/master/platform/accton-as4610-54/config/config.bcm" \
              "$PDIR/as4610-54.config.bcm"

        # 10G SFP+ PHY microcode: the BCM84758 pulls it through
        # request_firmware at PHY init, so it belongs in /lib/firmware in
        # the image rather than in the platform overlay.
        FWDIR="$ROOT/vendor/firmware"
        mkdir -p "$FWDIR"
        if [ -f "$FWDIR/bcm84758_ucode.bin" ]; then
            echo "have    bcm84758_ucode.bin"
        else
            cat >&2 <<'EOF'
warning: bcm84758_ucode.bin is not fetched automatically.

  The BCM84758 microcode is redistributed under Broadcom's firmware terms
  and is not on a stable public URL. Take it from the board's own ONL or
  ICOS image (/lib/firmware/), or from the OpenBCM tree's Firmware/
  directory if your revision carries it, and drop it at:

      vendor/firmware/bcm84758_ucode.bin

  Without it the four SFP+ ports stay down; the 48 copper ports do not
  need it.
EOF
        fi
    fi

    echo
    echo "All vendor artifacts for $PLATFORM are in place:"
    echo "  OpenBCM SDK:   vendor/openbcm/$OPENBCM_SDK ($(cat "$OPENBCM_DIR/.openbcm-commit" 2>/dev/null || echo pinned))"
    echo "  ASIC config:   platforms/$PLATFORM/as4610-54.config.bcm"
    echo "  SFP+ ucode:    vendor/firmware/bcm84758_ucode.bin (manual, see above)"
    echo
    echo "Then build the datapath shim in the ARM cross container:"
    echo "  vendor/openbcm-shim/build-shim.sh"
    echo "(the SDK's own libraries for iproc-4_4 must be built first)"
    ;;
*)
    echo "error: no fetch recipe for $PLATFORM (add one to this script)" >&2
    exit 1
    ;;
esac
