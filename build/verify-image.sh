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

check "rootfs.squashfs present"            "[ -s '$WORK/rootfs.squashfs' ]"
check "installer binary present"           "[ -s '$WORK/hemlock-installer' ]"
check "installer is executable"            "[ -x '$WORK/hemlock-installer' ]"
# ONIE has no glibc dynamic loader: an ELF installer must be static.
# (Dummy images may carry a shell stub instead, which is exempt.)
check "installer needs no dynamic loader" \
    "! head -c4 '$WORK/hemlock-installer' | grep -aq 'ELF' || ! grep -aq 'ld-linux' '$WORK/hemlock-installer'"
check "platform manifest present"          "[ -s '$WORK/platform/platform.toml' ]"
check "platform identity markers present" \
    "[ -s '$WORK/platform/onie-machine' ] && [ -s '$WORK/platform/platform-id' ]"
check "boot assets present" \
    "[ -s '$WORK/boot/grub.cfg' ] && [ -s '$WORK/boot/vmlinuz' ] && [ -s '$WORK/boot/initrd.img' ]"

MACHINE="$(cat "$WORK/platform/onie-machine" 2>/dev/null || true)"
check "onie-machine marker matches image header" \
    "head -20 '$IMAGE' | grep -q \"^hemlock_image_platform=$MACHINE\$\""

if [ "$fail" = 0 ]; then
    echo "verify-image: $IMAGE OK (platform $MACHINE)"
else
    echo "verify-image: $IMAGE FAILED" >&2
    exit 1
fi
