#!/bin/bash
# verify-image.sh — structural validation of a Hemlock ONIE .bin.
#
# Usage: build/verify-image.sh <image.bin>
#
# Confirms the self-extractor layout ONIE relies on and that the payload
# carries everything the installer needs. Does not need root and never
# executes the payload's installer.
set -euo pipefail

IMAGE="${1:?usage: verify-image.sh <image.bin>}"
fail=0
check() {
    if eval "$2"; then
        echo "ok    $1"
    else
        echo "FAIL  $1"
        fail=1
    fi
}

[ -f "$IMAGE" ] || { echo "no such image: $IMAGE" >&2; exit 1; }

check "image starts with a shell interpreter line" \
    "head -c 9 '$IMAGE' | grep -q '^#!/bin/sh'"
check "image declares hemlock_image_platform" \
    "head -20 '$IMAGE' | grep -q '^hemlock_image_platform='"
check "payload marker present" \
    "grep -aqm1 '^__HEMLOCK_PAYLOAD__$' '$IMAGE'"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Extract exactly the way the self-extractor header does.
ARCHIVE_LINE=$(awk '/^__HEMLOCK_PAYLOAD__/ { print NR + 1; exit 0 }' "$IMAGE")
check "payload is a valid gzip tarball" \
    "tail -n +$ARCHIVE_LINE '$IMAGE' | gzip -dc | tar -tf - >/dev/null"
tail -n +"$ARCHIVE_LINE" "$IMAGE" | gzip -dc | tar -xf - -C "$WORK"

# Which boot layout this payload carries. Older payloads have no marker;
# they are all x86, which is what the default preserves.
CPU_ARCH="$(cat "$WORK/platform/cpu-arch" 2>/dev/null || echo amd64)"
case "$CPU_ARCH" in
armhf) BOOT_ARTIFACT="$WORK/boot/hemlock.itb" ;;
*)     BOOT_ARTIFACT="$WORK/boot/vmlinuz" ;;
esac

# Is this a --dummy-rootfs image? Several checks below only make sense
# against a real build, and asking the question needs the *right* boot
# artifact: an ARM payload has no vmlinuz, and testing a file that does
# not exist reads as "not a dummy" and runs the checks anyway.
is_dummy() {
    [ -e "$BOOT_ARTIFACT" ] && head -c5 "$BOOT_ARTIFACT" | grep -q dummy
}

check "rootfs.squashfs present"            "[ -s '$WORK/rootfs.squashfs' ]"
# The dynamic MOTD must land in the rootfs (skipped for the dummy tar
# fallback, which unsquashfs cannot list).
if command -v unsquashfs >/dev/null && unsquashfs -s "$WORK/rootfs.squashfs" >/dev/null 2>&1; then
    check "rootfs carries the hemlock MOTD scripts" \
        "unsquashfs -l '$WORK/rootfs.squashfs' 2>/dev/null | grep -q 'etc/update-motd.d/00-hemlock-banner' \
         && unsquashfs -l '$WORK/rootfs.squashfs' 2>/dev/null | grep -q 'etc/update-motd.d/10-hemlock-status' \
         && unsquashfs -l '$WORK/rootfs.squashfs' 2>/dev/null | grep -q 'usr/bin/hemlock-motd'"
    # Without the BDE pair syncd cannot drive the ASIC on real hardware
    # (and deliberately refuses to mock when one is present). Dummy images
    # (placeholder kernel) never build modules; skip those.
    if ! is_dummy; then
        check "rootfs carries the BDE kernel modules" \
            "unsquashfs -l '$WORK/rootfs.squashfs' 2>/dev/null | grep -q 'updates/hemlock/linux-kernel-bde.ko' \
             && unsquashfs -l '$WORK/rootfs.squashfs' 2>/dev/null | grep -q 'updates/hemlock/linux-user-bde.ko'"
    fi
fi
check "installer binary present"           "[ -s '$WORK/hemlock-installer' ]"
check "installer is executable"            "[ -x '$WORK/hemlock-installer' ]"
# ONIE has no glibc dynamic loader: an ELF installer must be static.
# (Dummy images may carry a shell stub instead, which is exempt.)
check "installer needs no dynamic loader" \
    "! head -c4 '$WORK/hemlock-installer' | grep -aq 'ELF' || ! grep -aq 'ld-linux' '$WORK/hemlock-installer'"
check "platform manifest present"          "[ -s '$WORK/platform/platform.toml' ]"
check "platform identity markers present" \
    "[ -s '$WORK/platform/onie-machine' ] && [ -s '$WORK/platform/platform-id' ]"

check "cpu-arch marker is known" \
    "case '$CPU_ARCH' in amd64|armhf) true ;; *) false ;; esac"

if [ "$CPU_ARCH" = "armhf" ]; then
    # U-Boot boots one FIT container (kernel + dtb + initramfs); there is
    # no GRUB, no separate vmlinuz and no separate initrd.
    check "FIT boot image present"  "[ -s '$WORK/boot/hemlock.itb' ]"
    check "kernel command line present" "[ -s '$WORK/boot/cmdline' ]"
    check "kernel command line names the squashfs" \
        "grep -q 'hemlock.rootfs=' '$WORK/boot/cmdline'"
    check "no GRUB artifacts in an ARM payload" \
        "[ ! -e '$WORK/boot/grub.cfg' ]"
    # A real FIT starts with the flattened-device-tree magic; the dummy
    # one is a placeholder, so only check when it is not.
    if ! is_dummy; then
        check "FIT has the flat device tree magic" \
            "[ \"\$(head -c4 '$WORK/boot/hemlock.itb' | od -An -tx1 | tr -d ' \n')\" = 'd00dfeed' ]"
        if command -v mkimage >/dev/null; then
            check "FIT lists a kernel, an fdt and a ramdisk" \
                "mkimage -l '$WORK/boot/hemlock.itb' 2>/dev/null | grep -qi kernel \
                 && mkimage -l '$WORK/boot/hemlock.itb' 2>/dev/null | grep -qi 'flat device tree' \
                 && mkimage -l '$WORK/boot/hemlock.itb' 2>/dev/null | grep -qi ramdisk"
        fi
    fi
else
    check "boot assets present" \
        "[ -s '$WORK/boot/grub.cfg' ] && [ -s '$WORK/boot/vmlinuz' ] && [ -s '$WORK/boot/initrd.img' ]"
    # A real initrd must carry the hemlock squashfs/overlay boot script, or
    # the system boots the raw flash partition and finds no /sbin/init.
    # Dummy images ship a placeholder initrd; skip the check for those.
    if command -v lsinitramfs >/dev/null && ! is_dummy; then
        check "initrd contains hemlock boot script" \
            "lsinitramfs '$WORK/boot/initrd.img' | grep -q 'scripts/local-bottom/hemlock'"
    fi
fi

MACHINE="$(cat "$WORK/platform/onie-machine" 2>/dev/null || true)"
check "onie-machine marker matches image header" \
    "head -20 '$IMAGE' | grep -q \"^hemlock_image_platform=$MACHINE\$\""

if [ "$fail" = 0 ]; then
    echo "verify-image: $IMAGE OK (platform $MACHINE)"
else
    echo "verify-image: $IMAGE FAILED" >&2
    exit 1
fi
