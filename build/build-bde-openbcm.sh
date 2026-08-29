#!/bin/bash
# build-bde-openbcm.sh — build the Broadcom BDE/KNET kernel modules from
# the OpenBCM tree's GPL sources against the platform kernel, natively
# inside the image-build chroot.
#
# Usage: build-bde-openbcm.sh <openbcm-sdk-dir> <kernel-version> [<dest-dir>]
#
#   openbcm-sdk-dir: the SDK root (holds src/gpl-modules) — mkimage copies
#                    vendor/openbcm/sdk-* here, so the shims below edit a
#                    throwaway copy, never the fetched vendor tree
#   kernel-version:  e.g. "6.1.186-hemlock-iproc" (under /lib/modules)
#   dest-dir:        where the .ko files land (default: <src>/bde-out)
#
# The counterpart of build-bde.sh (saibcm-modules, for vendor-SAI
# platforms): an openbcm platform's modules must come from the same SDK
# lineage its userland shim was linked from — the BDE ioctl surface and
# the KNET message ABI are kernel<->user contracts. The GPL tree wraps
# each module through a real kbuild pass (Makefile.linux-kmodule runs
# `make -C KERNDIR M=... modules`), so the .ko gets genuine vermagic and
# modpost treatment. bindeb-pkg headers are one tree (no Debian split).
#
# The source predates kernel 6.1 by four majors; the shims below rewrite
# the affected call sites, each only when the target kernel's headers
# show the new API, so builds against older kernels stay byte-identical.
# Kernel-module C is the one place the port rules allow this.
set -euo pipefail

die() { echo "build-bde-openbcm: error: $*" >&2; exit 1; }
log() { echo "build-bde-openbcm: $*"; }

SRC="${1:?usage: build-bde-openbcm.sh <openbcm-sdk-dir> <kernel-version> [<dest-dir>]}"
KVER="${2:?kernel version required (see /lib/modules)}"
DEST="${3:-$SRC/bde-out}"

GPL="$SRC/src/gpl-modules"
[ -f "$GPL/RELEASE" ] || die "$SRC has no src/gpl-modules tree (OpenBCM SDK expected)"

KERNDIR="/usr/src/linux-headers-$KVER"
[ -d "$KERNDIR" ] || KERNDIR="/lib/modules/$KVER/build"
[ -e "$KERNDIR/include/generated/autoconf.h" ] \
    || die "no usable kernel headers for $KVER (install the linux-headers deb)"

# bindeb-pkg cross-built these headers on x86_64, so the packaged host
# tools (fixdep, modpost) are x86_64 binaries this chroot cannot run.
# Their sources ship in the deb; rebuild them natively. Idempotent and
# cheap when the tools are already the right architecture.
#
# First, keep kbuild away from syncconfig — which would both fail (the
# headers ship no Kconfig files) and delete the generated headers on
# the way down. Two triggers to disarm: auto.conf.cmd lists every
# Kconfig in the source tree as a prerequisite of auto.conf, and those
# files' absence reads as "always newer" no matter the timestamps — the
# fragment is hard-included (so it must exist) but only to supply those
# prerequisites — truncating it pins the configuration; and the
# generated files must postdate .config.
: > "$KERNDIR/include/config/auto.conf.cmd"
touch "$KERNDIR/include/config/auto.conf" 2>/dev/null || true
for f in "$KERNDIR/include/generated/autoconf.h" \
    "$KERNDIR/include/generated/rustc_cfg"; do
    [ -f "$f" ] && touch "$f"
done

log "rebuilding kernel host tools natively (fixdep, modpost)"
# fixdep via the top-level target (self-contained, no prepare chain).
make -C "$KERNDIR" scripts_basic > /tmp/hemlock-kscripts.log 2>&1 \
    || { tail -20 /tmp/hemlock-kscripts.log >&2; die "rebuilding fixdep failed"; }
# modpost by hand: any make route to scripts/mod/ drags in kbuild's
# prepare chain, which wants source files the deb does not ship. The
# deb DOES ship modpost's three sources and both generated headers it
# needs — elfconfig.h is derived from a target-arch object, so it is
# correct here even though the shipped modpost binary is not.
[ -f "$KERNDIR/scripts/mod/devicetable-offsets.h" ] \
    || die "scripts/mod/devicetable-offsets.h missing from the headers deb"
cc -O2 -I"$KERNDIR/scripts/mod" -o "$KERNDIR/scripts/mod/modpost" \
    "$KERNDIR/scripts/mod/modpost.c" \
    "$KERNDIR/scripts/mod/file2alias.c" \
    "$KERNDIR/scripts/mod/sumversion.c" \
    || die "rebuilding modpost failed"
# The generated module linker script ships in the deb; nothing to build.

# --- Kernel API compat shims ------------------------------------------------
# ioremap_nocache was removed in 5.6; ioremap has been non-cached on ARM
# forever, and these are the IOREMAP() convenience defines.
if ! grep -rqs "ioremap_nocache" "$KERNDIR/include/asm-generic/io.h"; then
    for f in "$GPL/systems/bde/linux/include/linux_dma.h" \
        "$GPL/systems/bde/linux/user/kernel/linux-user-bde.c"; do
        if [ -f "$f" ] && grep -q "ioremap_nocache" "$f"; then
            log "shimming $(basename "$f"): ioremap_nocache -> ioremap"
            sed -i 's/ioremap_nocache(/ioremap(/g' "$f"
        fi
    done
fi

# Both BDE modules include <soc/drv.h>, but the standalone GPL tree
# ships no such header (its include/soc/ has only cmic.h and devids.h,
# which they also include directly — SONiC's descendant of this tree
# dropped the include entirely). Remove it when it cannot resolve;
# anything genuinely missing would name itself at compile time.
KBDE_C="$GPL/systems/bde/linux/kernel/linux-kernel-bde.c"
if [ ! -f "$GPL/include/soc/drv.h" ]; then
    for f in "$KBDE_C" "$GPL/systems/bde/linux/user/kernel/linux-user-bde.c"; do
        if [ -f "$f" ] && grep -q "#include <soc/drv.h>" "$f"; then
            log "shimming $(basename "$f"): dropping unresolvable <soc/drv.h> include"
            sed -i 's|#include <soc/drv.h>|/* hemlock shim: soc/drv.h is not in the standalone GPL tree */|' "$f"
        fi
    done
fi

# The iproc_platform_* externs are wrappers only Broadcom's vendor XLDK
# kernel exported; on mainline they resolve to nothing at modpost. They
# take the standard platform types, so alias the call sites to the real
# API. The #defines go AFTER the extern block (the externs then sit
# unreferenced): rewriting the externs themselves would trip on
# platform_driver_register being a macro in mainline.
if ! grep -rqs "iproc_platform_driver_register" "$KERNDIR/include" 2>/dev/null; then
    if grep -q "iproc_platform_driver_register" "$KBDE_C" \
        && ! grep -q "hemlock shim: iproc_platform" "$KBDE_C"; then
        log "shimming linux-kernel-bde.c: iproc_platform_* -> platform_*"
        perl -i -0pe 's{(\n#define IPROC_CHIPCOMMONA_BASE)}{
/* hemlock shim: iproc_platform_* wrappers exist only in the vendor
 * XLDK kernel; on mainline they are the standard platform API. */
#define iproc_platform_driver_register(d)    platform_driver_register(d)
#define iproc_platform_driver_unregister(d)  platform_driver_unregister(d)
#define iproc_platform_device_register(d)    platform_device_register(d)
#define iproc_platform_device_unregister(d)  platform_device_unregister(d)
#define iproc_platform_get_resource(d,t,n)   platform_get_resource(d,t,n)
$1}' "$KBDE_C"
        grep -q "hemlock shim: iproc_platform" "$KBDE_C" \
            || die "iproc_platform shim anchor not found in linux-kernel-bde.c — source drifted, extend the shim"
    fi
fi

# proc_create takes a struct proc_ops since 5.6; these sources hand it
# file_operations. Convert exactly the named proc fops structs (the
# chardev fops must stay file_operations) — the member sets here are
# the uniform seq_file quintet, in both GNU-label and dotted styles.
if grep -q "struct proc_ops" "$KERNDIR/include/linux/proc_fs.h" 2>/dev/null; then
    proc_ops_shim() {
        local file="$1"; shift
        local names="$*"
        log "shimming $(basename "$file"): file_operations -> proc_ops ($names)"
        PROC_NAMES="$names" perl -i -0pe '
            for my $n (split / /, $ENV{PROC_NAMES}) {
                s{((?:static\s+)?)struct file_operations (\Q$n\E) = \{(.*?)\};}{
                    my ($pre, $name, $body) = ($1, $2, $3);
                    $body =~ s/\n\s*\.?owner\s*[:=]\s*THIS_MODULE,//;
                    $body =~ s/(\n\s*)\.?llseek\s*[:=]\s*/$1.proc_lseek = /g;
                    $body =~ s/(\n\s*)\.?(open|read|write|release)\s*[:=]\s*/$1.proc_$2 = /g;
                    "${pre}struct proc_ops $name = {$body};"
                }se;
            }
        ' "$file"
        for n in $names; do
            grep -q "struct proc_ops $n" "$file" \
                || die "$n not converted to proc_ops in $(basename "$file") — struct shape drifted, extend the shim"
        done
    }
    grep -q "struct proc_ops _gmodule_proc_fops" \
        "$GPL/systems/linux/kernel/modules/shared/gmodule.c" || \
        proc_ops_shim "$GPL/systems/linux/kernel/modules/shared/gmodule.c" \
            _gmodule_proc_fops
    grep -q "struct proc_ops bkn_proc_link_file_ops" \
        "$GPL/systems/linux/kernel/modules/bcm-knet/bcm-knet.c" || \
        proc_ops_shim "$GPL/systems/linux/kernel/modules/bcm-knet/bcm-knet.c" \
            bkn_proc_link_file_ops bkn_proc_rate_file_ops bkn_seq_dma_file_ops \
            bkn_proc_debug_file_ops bkn_proc_stats_file_ops bkn_proc_dstats_file_ops
fi

KSAL_C="$GPL/systems/linux/kernel/modules/shared/ksal.c"

# ksal.c picks its SAL_YIELD implementation with "#ifdef
# MAX_USER_RT_PRIO", a macro kernels dropped in 5.13 — sending the build
# down a 2.4-era branch. Every kernel this script supports has yield().
if [ -f "$KSAL_C" ] && grep -q '#ifdef MAX_USER_RT_PRIO' "$KSAL_C"; then
    log "shimming SAL_YIELD to yield() in ksal.c"
    sed -i 's|#ifdef MAX_USER_RT_PRIO|#if 1 /* hemlock shim: MAX_USER_RT_PRIO removed in 5.13; yield() always exists */|' "$KSAL_C"
fi

# do_gettimeofday (and kernel-side struct timeval) went away in 5.6;
# sal_time_usecs is the one caller. ktime_get_real_ts64 is the
# documented replacement.
if [ -f "$KSAL_C" ] && ! grep -rqs "do_gettimeofday" "$KERNDIR/include/linux/timekeeping32.h" 2>/dev/null; then
    if grep -q "do_gettimeofday(&ltv);" "$KSAL_C"; then
        log "shimming sal_time_usecs to ktime_get_real_ts64 in ksal.c"
        perl -i -0pe 's/\Q    struct timeval ltv;\E\n\Q    do_gettimeofday(&ltv);\E\n\Q    return (ltv.tv_sec * SECOND_USEC + ltv.tv_usec);\E/    struct timespec64 lts;\n    ktime_get_real_ts64(&lts);\n    return (lts.tv_sec * SECOND_USEC + lts.tv_nsec \/ 1000);/' "$KSAL_C"
        ! grep -q "do_gettimeofday" "$KSAL_C" \
            || die "do_gettimeofday left in ksal.c after shimming — call-site text drifted, extend the shim"
    fi
fi

KNET_C="$GPL/systems/linux/kernel/modules/bcm-knet/bcm-knet.c"
if [ -f "$KNET_C" ]; then
    # strlcpy was removed in 6.8; strscpy (4.3+) is a drop-in at these
    # sites (return values unused).
    if ! grep -q "strlcpy" "$KERNDIR/include/linux/string.h"; then
        log "shimming bcm-knet.c: strlcpy -> strscpy"
        perl -i -pe 's/\bstrlcpy\(/strscpy(/g' "$KNET_C"
        ! grep -q "strlcpy" "$KNET_C" || die "strlcpy left in bcm-knet.c after shimming"
    fi
    # The legacy pci_set_dma_mask wrapper left with the pci-dma-compat
    # API in 5.18; dma_set_mask is the direct equivalent.
    if ! grep -q "pci_set_dma_mask" "$KERNDIR/include/linux/pci.h"; then
        if grep -q "pci_set_dma_mask" "$KNET_C"; then
            log "shimming bcm-knet.c: pci_set_dma_mask -> dma_set_mask"
            perl -i -pe 's/\bpci_set_dma_mask\(([a-zA-Z_>.-]+),/dma_set_mask(&($1)->dev,/g' "$KNET_C"
            grep -q "linux/dma-mapping.h" "$KNET_C" || perl -i -pe \
                's{\Q#include <linux/etherdevice.h>\E}{#include <linux/etherdevice.h>\n#include <linux/dma-mapping.h>}' \
                "$KNET_C"
            ! grep -q "pci_set_dma_mask" "$KNET_C" || die "pci_set_dma_mask left in bcm-knet.c after shimming"
        fi
    fi
    # netif_napi_add lost its weight parameter in 6.1; the explicit-weight
    # variant exists since 5.19.
    if grep -q "netif_napi_add_weight" "$KERNDIR/include/linux/netdevice.h"; then
        if grep -Eq "netif_napi_add\([^)]+,[^)]+,[^)]+,[^)]+\)" "$KNET_C"; then
            log "shimming bcm-knet.c: 4-arg netif_napi_add -> netif_napi_add_weight"
            perl -i -pe 's/\bnetif_napi_add\(([^;]+,[^;]+,[^;]+,[^;)]+)\)/netif_napi_add_weight($1)/g' "$KNET_C"
        fi
    fi
    # dev->dev_addr is const since 5.17 and mirrored in a lookup tree —
    # direct memcpy writes are wrong even where they only warn.
    if grep -q "eth_hw_addr_set" "$KERNDIR/include/linux/etherdevice.h"; then
        if grep -q "memcpy(dev->dev_addr" "$KNET_C"; then
            log "shimming bcm-knet.c: dev_addr writes -> eth_hw_addr_set"
            perl -i -pe 's/\Qmemcpy(dev->dev_addr, ((struct sockaddr *)addr)->sa_data, dev->addr_len);\E/eth_hw_addr_set(dev, (const u8 *)((struct sockaddr *)addr)->sa_data);/' "$KNET_C"
            perl -i -pe 's/\bmemcpy\(dev->dev_addr, ([a-zA-Z_>.-]+), 6\);/eth_hw_addr_set(dev, $1);/g' "$KNET_C"
            ! grep -q "memcpy(dev->dev_addr" "$KNET_C" \
                || die "dev_addr memcpy left in bcm-knet.c after shimming — new write site, extend the shim"
        fi
    fi
fi

# --- Build -------------------------------------------------------------------
# Native build inside the armhf chroot: CROSS_COMPILE pinned empty on
# the command line (the makefile only defaults it to Broadcom's uclibc
# prefix when unset), the internal toolchain paths pointed at nothing,
# and KFLAG_INCLD at the real compiler's own headers. The kernel module
# wrap runs kbuild against KERNDIR, which supplies the actual flags.
# -j1: the tree's `all` lists kernel_modules and the artifact-copy
# rules as siblings with no ordering between them — parallel make
# evaluates the copies before the modules exist. A handful of modules;
# serial costs minutes.
log "building BDE/KNET modules for $KVER (this runs under qemu in CI — slow)"
BUILD_LOG="$SRC/hemlock-bde-build.log"
# linux-user-bde and bcm-knet import lkbde_* from linux-kernel-bde, but
# the GPL tree runs each module through its own kbuild invocation with
# no Module.symvers chaining — modpost's extern check then cannot see
# the exporter. Chain them through an accumulator OUTSIDE build/ (the
# module dirs get wiped): each module's recipe appends its symvers
# after building, and every kbuild invocation reads the accumulator.
# The modules build in dependency order (kernel-bde first), so the
# exports are there when the importers reach modpost.
EXTRA_SYMVERS="$GPL/hemlock-extra.symvers"
: > "$EXTRA_SYMVERS"
export KBUILD_EXTRA_SYMBOLS="$EXTRA_SYMVERS"
KMOD_MK="$GPL/make/Makefile.linux-kmodule"
if ! grep -q "hemlock-extra.symvers" "$KMOD_MK"; then
    sed -i 's|^\tcp -f \$(KMODULE) \$(LIBDIR)|\tcat Module.symvers >> $(SDK)/hemlock-extra.symvers\n\tcp -f $(KMODULE) $(LIBDIR)|' "$KMOD_MK"
    grep -q "hemlock-extra.symvers" "$KMOD_MK" \
        || die "symvers-chain anchor not found in Makefile.linux-kmodule — recipe drifted, extend the shim"
fi

# BCM_CFLAGS: the tree's default is -Wall -Werror; modern GCC's grown
# warnings make -Werror hopeless against 2019 code (real errors stay
# fatal). HAVE_UNLOCKED_IOCTL: the kernel definition of that macro left
# in 5.9, and without it gmodule.c falls back to a .ioctl field that
# has not existed since 2.6.36.
if ! make -C "$GPL/systems/linux/user/iproc" \
    SDK="$GPL" \
    CROSS_COMPILE= \
    TOOLCHAIN_BASE_DIR=/nonexistent \
    BCM_CFLAGS="-Wall -DHAVE_UNLOCKED_IOCTL" \
    KFLAG_INCLD="$(gcc -print-file-name=include)" \
    KERNDIR="$KERNDIR" \
    -j1 > "$BUILD_LOG" 2>&1
then
    echo "build-bde-openbcm: compile FAILED — diagnostics:" >&2
    grep -E -B3 -A8 " error:" "$BUILD_LOG" >&2 || tail -n 60 "$BUILD_LOG" >&2
    die "kernel module build failed for $KVER (full log: $BUILD_LOG)"
fi

mkdir -p "$DEST"
FOUND=0
for ko in linux-kernel-bde.ko linux-user-bde.ko linux-bcm-knet.ko; do
    built="$(find "$GPL" -name "$ko" | head -1)"
    if [ -n "$built" ]; then
        cp "$built" "$DEST/"
        FOUND=$((FOUND + 1))
        log "built $ko"
    else
        log "WARNING: $ko not produced"
    fi
done
[ "$FOUND" -eq 3 ] || die "expected all three modules (bde pair + knet); inspect $BUILD_LOG"
log "modules in $DEST"
