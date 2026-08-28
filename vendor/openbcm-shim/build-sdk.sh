#!/usr/bin/env bash
# build-sdk.sh — build OpenBCM's userland libraries for iproc-4_4.
#
# Usage: vendor/openbcm-shim/build-sdk.sh [--sdk <dir>]
#
# Produces the three archives build-shim.sh links against
# (libbcm.a, libsoc.a, libsal.a under build/unix-user/iproc-4_4/).
# Userland only: the BDE/KNET *kernel* modules are built separately, in
# the image chroot, by build/build-bde-openbcm.sh — so no kernel source
# is needed here.
#
# The SDK's own makefiles default to Broadcom's internal uclibc
# toolchain at /projects/ntsw-tools/...; CROSS_COMPILE overrides it and
# TOOLCHAIN_BASE_DIR is pointed at nothing so its PATH prepends are
# inert. A modern GNU cross compiler is noisier than the 4.9-era one
# the SDK grew up with, so warnings are expected; errors are not.
#
# Run by the image workflow's full armhf build (cached on the shim
# ABI + SDK pin) and by hand before build-shim.sh. This is a long
# build — an hour-plus on a laptop.
set -euo pipefail

die() { echo "build-sdk: error: $*" >&2; exit 1; }
log() { echo "build-sdk: $*"; }

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

SDK="${SDK:-$ROOT/vendor/openbcm/sdk-6.5.16}"
CROSS_COMPILE="${CROSS_COMPILE:-arm-linux-gnueabihf-}"
JOBS="${JOBS:-$(nproc)}"

while [ $# -gt 0 ]; do
    case "$1" in
    --sdk) SDK="$2"; shift ;;
    *) die "unknown option $1" ;;
    esac
    shift
done

[ -f "$SDK/RELEASE" ] || die "no OpenBCM tree at $SDK
 (run: vendor/fetch-vendor.sh accton-as4610-54)"
command -v "${CROSS_COMPILE}gcc" >/dev/null \
    || die "${CROSS_COMPILE}gcc not on PATH (apt: gcc-arm-linux-gnueabihf)"

log "building userland (iproc-4_4) with ${CROSS_COMPILE}gcc -j$JOBS"
# BCM_CFLAGS: the SDK's default is -Wall -Werror, set with ?= for exactly
# this override. Its code predates the warnings modern GCC grew since
# (first stop: -Wstringop-truncation inside Broadcom's own phymod), and
# we do not patch vendor code — so keep -Wall, drop -Werror. -fcommon:
# GCC 10 flipped the default to -fno-common, and the SDK relies on the
# old behaviour — the same tentative definitions appear in several chip
# files (trident3.c and helix5.c both define l2_entry_hash_control) and
# the final link counts on the linker merging them.
#
# The target is `bcm`, not the default: default `all` also wants the
# BDE/KNET *kernel* modules, which need a configured kernel tree
# (KERNDIR) that this build deliberately does not have — the image
# chroot builds those. `bcm` drives user_libs (every archive we link)
# plus the bcm.user link, which is itself the proof the set is complete.
# LIBS: the SDK's default adds -lnsl, which modern glibc no longer
# ships (and never as the static archive this link wants); nothing in
# the userland build actually needs it.
#
# The bcm target's link writes into build/linux/user/iproc-4_4 but has
# no mkdir of its own — the default `all` target creates it as a side
# effect of the kernel-module rules this build deliberately skips.
mkdir -p "$SDK/build/linux/user/iproc-4_4"

# The SDK whitelists make versions ("4.4.1" passes only because the
# check is a substring match against "4.1"; "4.3" genuinely fails).
# The build is proven on make 4.4, so defeat the check — but not via a
# command-line variable: src/Makefile does `override MAKEFLAGS += ...`,
# which drops command-line variables for every make below it. The
# environment survives that, and env origin outranks MAKE_VERSION's
# default origin at every recursion level. The SDK reads MAKE_VERSION
# in exactly two places: this whitelist, and a >=3.81 check that "4.1"
# also satisfies.
export MAKE_VERSION=4.1

sdk_make() {
    make -C "$SDK/systems/linux/user/iproc-4_4" \
        SDK="$SDK" \
        CROSS_COMPILE="$CROSS_COMPILE" \
        TOOLCHAIN_BASE_DIR=/nonexistent \
        BCM_CFLAGS="-Wall -fno-strict-aliasing -fcommon" \
        LIBS="-pthread -lm -lrt" \
        BUILD_KNET=1 \
        targetplat=user \
        "$@" \
        bcm
}
# The SDK's techsupport makefiles race under -j: an archive rule can
# fire before all of its member objects exist ("ar: X.o: No such file
# or directory"). The victims vary run to run. A serial second pass
# over the parallel build's objects closes exactly that gap; it is
# incremental, so it costs minutes, not the hours of the first pass.
if ! sdk_make -j"$JOBS"; then
    log "parallel make failed — one -j1 pass to close the archive race"
    sdk_make -j1 \
        || die "the SDK userland build failed — the log above is the finding"
fi

# The SDK splits its libraries finely (soc per chip family, sal into
# core/appl); build-shim.sh links every archive here, so just spot-check
# the roster is present.
LIBDIR="$SDK/build/unix-user/iproc-4_4"
for lib in libbcm.a libsal_core.a libsoc_esw.a; do
    [ -f "$LIBDIR/$lib" ] || die "$lib not produced at $LIBDIR"
done

# The phase 3 gate from the port doc: the chip this board carries must
# be in the built driver tables, or the libraries are for the wrong
# family and every later step chases ghosts.
# grep -c, not -q: -q exits at the first match, nm dies of SIGPIPE, and
# pipefail turns the hit into a miss.
hit=""
for a in "$LIBDIR"/lib*.a; do
    n="$("${CROSS_COMPILE}nm" "$a" 2>/dev/null | grep -c "bcm56340" || true)"
    if [ "${n:-0}" -gt 0 ]; then
        hit="$a"
        break
    fi
done
[ -n "$hit" ] || die "bcm56340 (Helix4) is absent from every built archive"
log "bcm56340 symbols found in $(basename "$hit")"

log "libraries ready under $LIBDIR"
log "next: vendor/openbcm-shim/build-shim.sh"
