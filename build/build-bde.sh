#!/bin/bash
# build-bde.sh — build the Broadcom BDE/KNET kernel modules from the GPL
# saibcm-modules source (fetched by vendor/fetch-vendor.sh) against a
# Debian kernel.
#
# Usage: build-bde.sh <saibcm-modules-dir> <kernel-version> [<dest-dir>]
#
#   kernel-version:  e.g. "6.12.5-amd64" (a directory under /lib/modules)
#   dest-dir:        where the .ko files land (default: <src>/out)
#
# Mirrors the invocation from saibcm-modules' own debian/rules: Debian
# splits kernel headers into linux-headers-<v>-common (KERNDIR) and
# linux-headers-<v>-<arch> (KERNEL_SRC), and the source tree's build wants
# a handful of generated dirs linked into -common. Run on Debian (or in
# the image-build chroot) with linux-headers for the target kernel,
# build-essential, and bc installed. Idempotent.
set -euo pipefail

die() { echo "build-bde: error: $*" >&2; exit 1; }
log() { echo "build-bde: $*"; }

SRC="${1:?usage: build-bde.sh <saibcm-modules-dir> <kernel-version> [<dest-dir>]}"
KVER="${2:?kernel version required (see /lib/modules)}"
DEST="${3:-$SRC/out}"

[ -d "$SRC/systems/linux/user/x86-smp_generic_64-2_6" ] \
    || die "$SRC does not look like a saibcm-modules tree"

# Debian header split: 6.12.5-amd64 -> common=6.12.5-common, arch dir as-is.
KERNVERSION="${KVER%-amd64}"
COMMON="/usr/src/linux-headers-${KERNVERSION}-common"
ARCH_HDRS="/usr/src/linux-headers-${KVER}"
[ -d "$COMMON" ] || die "$COMMON missing (apt-get install linux-headers-${KVER})"
[ -d "$ARCH_HDRS" ] || die "$ARCH_HDRS missing (apt-get install linux-headers-${KVER})"

# The saibcm-modules build expects arch-generated artifacts visible from
# the -common tree (same links saibcm-modules' debian/rules creates).
link_into_common() {
    target="$1"; linkpath="$2"
    [ -e "$linkpath" ] || ln -s "$target" "$linkpath"
}
link_into_common "$ARCH_HDRS/include/generated"          "$COMMON/include/generated"
link_into_common "$ARCH_HDRS/arch/x86/include/generated" "$COMMON/arch/x86/include/generated"
link_into_common "$ARCH_HDRS/include/config"             "$COMMON/include/config"
[ -e "$ARCH_HDRS/arch/x86/module.lds" ] \
    && link_into_common "$ARCH_HDRS/arch/x86/module.lds" "$COMMON/arch/x86/module.lds"
[ -f "$COMMON/Module.symvers" ] || cp "$ARCH_HDRS/Module.symvers" "$COMMON/Module.symvers"

# --- Kernel API compat shims ------------------------------------------------
# Kernel 6.8 renamed MAX_ORDER to MAX_PAGE_ORDER (and 6.4 had already
# turned the old exclusive bound into an inclusive one, hence the +1).
# The 202305 saibcm-modules tree predates both and derives its DMA
# allocation cap from MAX_ORDER in linux_dma.c. Shim only when the target
# kernel's headers no longer define MAX_ORDER, so builds against older
# kernels stay byte-identical; the #ifndef keeps the edit idempotent and
# harmless either way.
if ! grep -qs "define MAX_ORDER" "$COMMON/include/linux/mmzone.h"; then
    DMA_C="$SRC/systems/bde/linux/kernel/linux_dma.c"
    if [ -f "$DMA_C" ] && ! grep -q "MAX_PAGE_ORDER + 1" "$DMA_C"; then
        log "shimming MAX_ORDER -> MAX_PAGE_ORDER + 1 in linux_dma.c (kernel >= 6.8)"
        sed -i '0,/#define DMA_MAX_ALLOC_SIZE/s//#ifndef MAX_ORDER\n#define MAX_ORDER (MAX_PAGE_ORDER + 1)\n#endif\n#define DMA_MAX_ALLOC_SIZE/' "$DMA_C"
        grep -q "MAX_PAGE_ORDER + 1" "$DMA_C" \
            || die "MAX_ORDER shim did not apply — linux_dma.c layout changed upstream"
    fi
fi

log "building BDE modules for $KVER"
SDK="$(realpath "$SRC")" LINUX_UAPI_SPLIT=1 DEBIAN_LINUX_HEADER=1 BUILD_KNET_CB=1 \
    KERNDIR="$COMMON" \
    KERNEL_SRC="$ARCH_HDRS" \
    make -C "$SRC/systems/linux/user/x86-smp_generic_64-2_6"

mkdir -p "$DEST"
FOUND=0
for ko in linux-kernel-bde.ko linux-user-bde.ko linux-bcm-knet.ko linux-knet-cb.ko; do
    built="$(find "$SRC/systems/linux/user/x86-smp_generic_64-2_6" -name "$ko" | head -1)"
    if [ -n "$built" ]; then
        cp "$built" "$DEST/"
        FOUND=$((FOUND + 1))
        log "built $ko"
    else
        log "WARNING: $ko not produced"
    fi
done
[ "$FOUND" -ge 2 ] || die "BDE pair not built; inspect the make output above"
log "modules in $DEST"
