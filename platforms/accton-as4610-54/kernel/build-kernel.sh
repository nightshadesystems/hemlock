#!/usr/bin/env bash
# build-kernel.sh — build the AS4610's mainline 6.1 kernel .deb.
#
# Usage: platforms/accton-as4610-54/kernel/build-kernel.sh [--src <dir>]
#
# Produces vendor/kernel/linux-image-<ver>-hemlock-iproc_..._armhf.deb —
# the artifact build/mkimage.sh stages into the image. Mainline 6.1 LTS
# plus exactly two out-of-tree inputs, both from this directory:
#
#   dts/            the Helix4 SoC + board device tree (copied in-tree;
#                   one dtb-y line is appended to the dts Makefile)
#   hemlock.config  merged over multi_v7_defconfig
#
# There is no patch. The board's compatible chain ends in "brcm,hr2",
# which mainline's ARCH_BCM_HR2 machine matches as-is; the whole story
# is in docs/as4610-kernel-port.md.
#
# NOT RUN BY CI (it fetches and builds a kernel tree). Requirements:
#   gcc-arm-linux-gnueabihf, plus the kernel's own build deps
#   (apt: build-essential bc bison flex libssl-dev libelf-dev kmod
#    debhelper rsync u-boot-tools; git for the fetch).
set -euo pipefail

die() { echo "build-kernel: error: $*" >&2; exit 1; }
log() { echo "build-kernel: $*"; }

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"

SRC="${SRC:-$ROOT/vendor/kernel/linux-6.1.y}"
BRANCH="${BRANCH:-linux-6.1.y}"
CROSS_COMPILE="${CROSS_COMPILE:-arm-linux-gnueabihf-}"
JOBS="${JOBS:-$(nproc)}"

while [ $# -gt 0 ]; do
    case "$1" in
    --src) SRC="$2"; shift ;;
    *) die "unknown option $1" ;;
    esac
    shift
done

command -v "${CROSS_COMPILE}gcc" >/dev/null \
    || die "${CROSS_COMPILE}gcc not on PATH (apt: gcc-arm-linux-gnueabihf)"

if [ ! -d "$SRC/.git" ]; then
    log "fetching $BRANCH into $SRC (shallow)"
    mkdir -p "$(dirname "$SRC")"
    git clone -q --depth 1 --branch "$BRANCH" \
        https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git "$SRC"
else
    log "using existing tree at $SRC"
fi

# The two Hemlock inputs, refreshed on every run so the committed files
# stay the source of truth. The Makefile line is appended idempotently;
# it is the one in-tree edit, and it is an addition, not a patch.
cp "$HERE"/dts/bcm-hx4.dtsi "$HERE"/dts/arm-accton-as4610-54.dts \
    "$SRC/arch/arm/boot/dts/"
grep -q "arm-accton-as4610-54" "$SRC/arch/arm/boot/dts/Makefile" || {
    printf 'dtb-$(CONFIG_ARCH_BCM_HR2) += arm-accton-as4610-54.dtb\n' \
        >> "$SRC/arch/arm/boot/dts/Makefile"
}

cd "$SRC"
export ARCH=arm CROSS_COMPILE

log "multi_v7_defconfig + hemlock.config"
make -s multi_v7_defconfig
./scripts/kconfig/merge_config.sh -m .config "$HERE/hemlock.config" >/dev/null
make -s olddefconfig

# Every =y/=m in the fragment must have survived olddefconfig: a symbol
# that quietly fell out (a typo, a missing dependency) is exactly the
# failure that otherwise appears as an unbootable box.
while IFS= read -r want; do
    case "$want" in
    CONFIG_*=n) continue ;;
    CONFIG_*=*)
        name="${want%%=*}"
        grep -q "^$want\$" .config \
            || die "$name did not survive olddefconfig (asked: $want, got: $(grep "^$name=\|^# $name " .config || echo absent))"
        ;;
    esac
done < <(grep -E '^CONFIG_' "$HERE/hemlock.config")
log "config fragment fully applied"

log "building bindeb-pkg with -j$JOBS (this is the long part)"
make -s -j"$JOBS" bindeb-pkg LOCALVERSION=-hemlock-iproc KDEB_PKGVERSION="$(make -s kernelversion)-1"

DEB="$(ls "$SRC"/../linux-image-*-hemlock-iproc*_armhf.deb 2>/dev/null | sort | tail -1)"
[ -n "$DEB" ] || die "bindeb-pkg produced no linux-image deb"
mkdir -p "$ROOT/vendor/kernel"
cp "$DEB" "$ROOT/vendor/kernel/"
log "staged $(basename "$DEB") into vendor/kernel/"
log "next: build/mkimage.sh accton-as4610-54"
