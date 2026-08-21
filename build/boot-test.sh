#!/bin/bash
# boot-test.sh — boot a Hemlock image's kernel+initrd+squashfs in QEMU and
# verify the initramfs hand-off reaches a login prompt.
#
# Usage: build/boot-test.sh <image.bin | payload-dir> [--timeout <sec>]
#
# Tests the part of the boot GRUB cannot break: the initramfs local-bottom
# hand-off (losetup, squashfs, overlay) and systemd bring-up. Boots the
# payload's real kernel/initrd with the same cmdline grub.cfg uses (console
# retargeted to QEMU's ttyS0) against a disk laid out like the installer's
# root partition. Needs no root: the disk is built with mkfs.ext4 -d.
#
# Exit 0: reached a login prompt. Exit 1: hemlock panic, kernel panic,
# initramfs shell, or timeout. Serial log is printed on failure.
set -euo pipefail

die() { echo "boot-test: error: $*" >&2; exit 1; }
log() { echo "boot-test: $*"; }

TARGET="${1:-}"
[ -n "$TARGET" ] || die "usage: boot-test.sh <image.bin | payload-dir> [--timeout <sec>]"
shift
TIMEOUT=600
while [ $# -gt 0 ]; do
    case "$1" in
    --timeout) TIMEOUT="$2"; shift ;;
    *) die "unknown option $1" ;;
    esac
    shift
done

command -v qemu-system-x86_64 >/dev/null || die "qemu-system-x86_64 not installed"
command -v mkfs.ext4 >/dev/null || die "mkfs.ext4 (e2fsprogs) not installed"

WORK="$(mktemp -d)"
QEMU_PID=""
cleanup() {
    [ -n "$QEMU_PID" ] && kill "$QEMU_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT

# --- Payload ---------------------------------------------------------------
if [ -d "$TARGET" ]; then
    PAYLOAD="$TARGET"
else
    [ -f "$TARGET" ] || die "no such image or directory: $TARGET"
    log "extracting payload from $TARGET"
    HEMLOCK_EXTRACT_DIR="$WORK/payload" HEMLOCK_EXTRACT_ONLY=1 sh "$TARGET" >/dev/null
    PAYLOAD="$WORK/payload"
fi
for f in boot/vmlinuz boot/initrd.img rootfs.squashfs; do
    [ -s "$PAYLOAD/$f" ] || die "payload is missing $f"
done
head -c5 "$PAYLOAD/boot/vmlinuz" | grep -q dummy \
    && die "dummy image (placeholder kernel) — boot test needs a full image"

# --- Disk: what the installer's root partition looks like ------------------
log "building test disk"
TREE="$WORK/tree"
mkdir -p "$TREE/hemlock/persist"
cp "$PAYLOAD/rootfs.squashfs" "$TREE/hemlock/rootfs.squashfs"
[ -d "$PAYLOAD/platform" ] && cp -r "$PAYLOAD/platform" "$TREE/hemlock/platform"
# Squashfs + headroom for the overlay upper layer.
SIZE_MB=$(( $(du -m "$PAYLOAD/rootfs.squashfs" | cut -f1) + 512 ))
DISK="$WORK/disk.img"
truncate -s "${SIZE_MB}M" "$DISK"
mkfs.ext4 -q -F -L HEMLOCK -d "$TREE" "$DISK"

# --- Boot ------------------------------------------------------------------
SERIAL="$WORK/serial.log"
ACCEL="tcg"
[ -w /dev/kvm ] && ACCEL="kvm"
log "booting under QEMU ($ACCEL, timeout ${TIMEOUT}s)"
qemu-system-x86_64 \
    -machine pc,accel="$ACCEL" -cpu max -smp 2 -m 2048 \
    -kernel "$PAYLOAD/boot/vmlinuz" \
    -initrd "$PAYLOAD/boot/initrd.img" \
    -append "console=ttyS0,115200n8 root=LABEL=HEMLOCK hemlock.rootfs=/hemlock/rootfs.squashfs rw" \
    -drive file="$DISK",format=raw,if=virtio \
    -nographic -no-reboot \
    </dev/null >"$SERIAL" 2>&1 &
QEMU_PID=$!

fail() {
    echo "boot-test: FAIL — $*" >&2
    KEEP="${TMPDIR:-/tmp}/hemlock-boot-test-serial.log"
    cp "$SERIAL" "$KEEP" 2>/dev/null || true
    echo "boot-test: notable errors:" >&2
    grep -aE "Initramfs unpacking failed|hemlock: |Kernel panic|ALERT!|No init found|Failed to mount" "$SERIAL" >&2 || true
    echo "boot-test: last serial output (full log: $KEEP):" >&2
    tail -40 "$SERIAL" >&2
    exit 1
}

verdict() {
    # Success: any getty prompt from the booted system.
    grep -aq "login:" "$SERIAL" && return 0
    # Failure: our own panics, kernel panic, or the initramfs giving up.
    grep -aqE "hemlock: |Kernel panic|ALERT!.*does not exist|No init found" "$SERIAL" && return 1
    return 2 # still booting
}

ELAPSED=0
while [ "$ELAPSED" -lt "$TIMEOUT" ]; do
    if ! kill -0 "$QEMU_PID" 2>/dev/null; then break; fi
    set +e; verdict; V=$?; set -e
    if [ "$V" = 0 ]; then
        log "PASS — login prompt reached after ~${ELAPSED}s"
        exit 0
    elif [ "$V" = 1 ]; then
        fail "boot did not reach a login prompt"
    fi
    sleep 5
    ELAPSED=$((ELAPSED + 5))
done

fail "no verdict within ${TIMEOUT}s"
