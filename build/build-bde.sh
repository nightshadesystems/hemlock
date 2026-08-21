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
# turned the old exclusive bound into an inclusive one). The 202305
# saibcm-modules tree predates both: linux_dma.c sizes its DMA
# allocation cap as (MAX_ORDER - 1 + PAGE_SHIFT), which on 6.8+ is
# exactly (MAX_PAGE_ORDER + PAGE_SHIFT) — rewrite the expression rather
# than inserting a define, so preprocessor branches cannot hide it.
# Applied only when the target kernel's headers no longer define
# MAX_ORDER (exact-name match: 6.8+ still defines MAX_ORDER_NR_PAGES,
# which a substring check would mistake for MAX_ORDER itself), so builds
# against older kernels stay byte-identical.
if ! grep -Eqs "#define[[:space:]]+MAX_ORDER([[:space:]]|$)" "$COMMON/include/linux/mmzone.h"; then
    DMA_C="$SRC/systems/bde/linux/kernel/linux_dma.c"
    if [ -f "$DMA_C" ]; then
        if grep -q "MAX_ORDER - 1 + PAGE_SHIFT" "$DMA_C"; then
            log "shimming MAX_ORDER -> MAX_PAGE_ORDER in linux_dma.c (kernel >= 6.8)"
            sed -i 's/(MAX_ORDER - 1 + PAGE_SHIFT)/(MAX_PAGE_ORDER + PAGE_SHIFT)/g' "$DMA_C"
        fi
        # MAX_PAGE_ORDER does not contain "MAX_ORDER" as a substring, so
        # any hit here is a genuine leftover use the sed did not cover.
        ! grep -q "MAX_ORDER" "$DMA_C" \
            || die "linux_dma.c still references MAX_ORDER after shimming — new upstream use site, extend the shim"
    fi
fi

# ksal.c picks its SAL_YIELD implementation with "#ifdef
# MAX_USER_RT_PRIO", a macro kernels dropped in 5.13 — sending the build
# down a 2.4-era branch that uses the long-removed SCHED_YIELD. Every
# kernel this script supports has yield(); force that branch. (On old
# kernels that still defined MAX_USER_RT_PRIO the yield() branch was
# chosen anyway, so this is behavior-preserving.)
KSAL_C="$SRC/systems/linux/kernel/modules/shared/ksal.c"
if [ -f "$KSAL_C" ] && grep -q '#ifdef MAX_USER_RT_PRIO' "$KSAL_C"; then
    log "shimming SAL_YIELD to yield() in ksal.c"
    sed -i 's|#ifdef MAX_USER_RT_PRIO|#if 1 /* hemlock shim: MAX_USER_RT_PRIO removed in 5.13; yield() always exists */|' "$KSAL_C"
fi

# bcm-knet.c predates several 6.2..6.12 kernel API changes. Rewrite the
# affected call sites, each only when the target kernel's headers show
# the new API, so builds against older kernels stay byte-identical.
# Exact-string rewrites use perl \Q..\E (perl-base is Debian-essential,
# present even in a minbase chroot).
KNET_C="$SRC/systems/linux/kernel/modules/bcm-knet/bcm-knet.c"
if [ -f "$KNET_C" ]; then
    # strlcpy was removed in 6.8; strscpy (4.3+) is a drop-in at these
    # sites (return values unused).
    if ! grep -q "strlcpy" "$COMMON/include/linux/string.h"; then
        log "shimming bcm-knet.c: strlcpy -> strscpy"
        perl -i -pe 's/\bstrlcpy\(/strscpy(/g' "$KNET_C"
        ! grep -q "strlcpy" "$KNET_C" || die "strlcpy left in bcm-knet.c after shimming"
    fi
    # 6.11 changed ethtool_ops.get_ts_info to take kernel_ethtool_ts_info
    # (same member names, so the body needs no edits).
    if grep -q "struct kernel_ethtool_ts_info" "$COMMON/include/linux/ethtool.h"; then
        log "shimming bcm-knet.c: ethtool_ts_info -> kernel_ethtool_ts_info"
        perl -i -pe 's/\Qstruct ethtool_ts_info\E/struct kernel_ethtool_ts_info/g' "$KNET_C"
    fi
    # The legacy pci_set_dma_mask wrapper is gone; call dma_set_mask.
    # bcm-knet.c never includes dma-mapping.h itself (the old wrapper
    # arrived via an SDK header chain), so add the include too — implicit
    # declarations are hard errors.
    if ! grep -q "pci_set_dma_mask" "$COMMON/include/linux/pci.h"; then
        log "shimming bcm-knet.c: pci_set_dma_mask -> dma_set_mask"
        perl -i -pe 's/\Qpci_set_dma_mask(sinfo->pdev, 0xffffffff)\E/dma_set_mask(&sinfo->pdev->dev, 0xffffffff)/' "$KNET_C"
        grep -q "linux/dma-mapping.h" "$KNET_C" || perl -i -pe \
            's{\Q#include <linux/etherdevice.h>\E}{#include <linux/etherdevice.h>\n#include <linux/dma-mapping.h>}' \
            "$KNET_C"
        ! grep -q "pci_set_dma_mask" "$KNET_C" || die "pci_set_dma_mask left in bcm-knet.c after shimming"
    fi
    # netif_napi_add lost its weight parameter in 6.1; the explicit-weight
    # variant exists since 5.19. (The 4-arg macro at the top of the file
    # is inside a dead <2.6.24 branch and stays untouched.)
    if grep -q "netif_napi_add_weight" "$COMMON/include/linux/netdevice.h"; then
        log "shimming bcm-knet.c: netif_napi_add -> netif_napi_add_weight"
        perl -i -pe 's/\Qnetif_napi_add(dev, &sinfo->napi, bkn_poll, napi_weight);\E/netif_napi_add_weight(dev, &sinfo->napi, bkn_poll, napi_weight);/' "$KNET_C"
    fi
    # dev->dev_addr is const since 5.17 and mirrored in a lookup tree —
    # direct memcpy writes are wrong even where they only warn.
    if grep -q "eth_hw_addr_set" "$COMMON/include/linux/etherdevice.h"; then
        log "shimming bcm-knet.c: dev_addr writes -> eth_hw_addr_set"
        perl -i -pe 's/\Qmemcpy(dev->dev_addr, ((struct sockaddr *)addr)->sa_data, dev->addr_len);\E/eth_hw_addr_set(dev, (const u8 *)((struct sockaddr *)addr)->sa_data);/' "$KNET_C"
        perl -i -pe 's/\Qmemcpy(dev->dev_addr, mac, 6);\E/eth_hw_addr_set(dev, mac);/' "$KNET_C"
    fi
fi

log "building BDE modules for $KVER"
# The SDK build spews thousands of benign kernel-header warnings that bury
# the one diagnostic that matters, so capture everything and surface only
# the error context on failure.
BUILD_LOG="$SRC/hemlock-build.log"
if ! SDK="$(realpath "$SRC")" LINUX_UAPI_SPLIT=1 DEBIAN_LINUX_HEADER=1 BUILD_KNET_CB=1 \
    KERNDIR="$COMMON" \
    KERNEL_SRC="$ARCH_HDRS" \
    make -C "$SRC/systems/linux/user/x86-smp_generic_64-2_6" > "$BUILD_LOG" 2>&1
then
    echo "build-bde: compile FAILED — diagnostics:" >&2
    grep -E -B3 -A8 " error:" "$BUILD_LOG" >&2 || tail -n 60 "$BUILD_LOG" >&2
    die "kernel module build failed for $KVER (full log: $BUILD_LOG)"
fi

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
