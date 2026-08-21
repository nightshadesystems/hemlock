#!/bin/bash
# mkimage.sh — assemble an ONIE-installable Hemlock image for one platform.
#
# Usage: build/mkimage.sh <platform-id> [--dummy-rootfs] [--version <v>]
#
#   --dummy-rootfs   Skip debootstrap; build a tiny placeholder rootfs.
#                    Produces a structurally valid .bin for CI/layout tests.
#
# Real builds run on Debian with: debootstrap, mksquashfs (squashfs-tools),
# a Rust toolchain, and the platform's vendor SAI .deb staged in vendor/sai/
# (see vendor/sai/README.md). Vendor *data* files (config.bcm) must sit in
# the platform directory (vendor/fetch-vendor.sh).
set -euo pipefail

die() { echo "mkimage: error: $*" >&2; exit 1; }
log() { echo "mkimage: $*"; }

PLATFORM="${1:-}"
[ -n "$PLATFORM" ] || die "usage: mkimage.sh <platform-id> [--dummy-rootfs] [--version <v>]"
shift

DUMMY=0
VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
    --dummy-rootfs) DUMMY=1 ;;
    --version) VERSION="$2"; shift ;;
    *) die "unknown option $1" ;;
    esac
    shift
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# Single source of truth for versioning: the top-level VERSION file.
[ -n "$VERSION" ] || VERSION="$(tr -d '[:space:]' < "$ROOT/VERSION")"
[ -n "$VERSION" ] || die "empty VERSION file at $ROOT/VERSION"
PDIR="$ROOT/platforms/$PLATFORM"
[ -f "$PDIR/platform.toml" ] || die "no platform.toml in $PDIR"

# Pull the identity fields out of the manifest (validated separately by
# `hemlockctl platform lint`).
manifest_value() {
    sed -n "s/^$1[[:space:]]*=[[:space:]]*\"\(.*\)\".*/\1/p" "$PDIR/platform.toml" | head -1
}
ONIE_MACHINE="$(manifest_value onie_machine)"
SAI_PIN="$(manifest_value version_pin)"
SAI_HEADERS="$(manifest_value api_headers)"
CONFIG_BCM="$(manifest_value config_bcm)"
[ -n "$ONIE_MACHINE" ] || die "cannot read onie_machine from $PDIR/platform.toml"

OUT="$ROOT/build/out"
WORK="$OUT/work-$PLATFORM"
PAYLOAD="$WORK/payload"
ROOTFS="$WORK/rootfs"
rm -rf "$WORK"
mkdir -p "$PAYLOAD" "$ROOTFS" "$OUT"

# --- 1. Root filesystem -----------------------------------------------------
if [ "$DUMMY" = 1 ]; then
    log "building DUMMY rootfs (layout test only)"
    mkdir -p "$ROOTFS/etc" "$ROOTFS/usr/lib/hemlock" "$ROOTFS/boot"
    cat > "$ROOTFS/etc/os-release" <<EOF
NAME="Hemlock"
VERSION="$VERSION (dummy)"
ID=hemlock
EOF
    echo "dummy-kernel" > "$ROOTFS/boot/vmlinuz"
    echo "dummy-initrd" > "$ROOTFS/boot/initrd.img"
else
    command -v debootstrap >/dev/null || die "debootstrap not installed"
    command -v mksquashfs >/dev/null || die "squashfs-tools not installed"

    # The vendor SAI blob must exist for a real image.
    SAI_DEB="$(ls "$ROOT"/vendor/sai/libsaibcm*"$SAI_PIN"*.deb 2>/dev/null | head -1 || true)"
    [ -n "$SAI_DEB" ] || die \
        "no libsaibcm .deb matching pin '$SAI_PIN' in vendor/sai/ — see vendor/sai/README.md
 (CI and development never need this: use --dummy-rootfs or mock-sai)"
    [ -f "$PDIR/$CONFIG_BCM" ] || die \
        "$CONFIG_BCM missing from $PDIR — run vendor/fetch-vendor.sh $PLATFORM"

    log "debootstrap Debian trixie"
    debootstrap --variant=minbase \
        --include="$(grep -v '^#' "$ROOT/build/rootfs/packages.list" | grep -v '^$' | paste -sd, -)" \
        trixie "$ROOTFS" https://deb.debian.org/debian

    log "installing Hemlock daemons"
    (cd "$ROOT" && cargo build --release --workspace --features hemlock-syncd/real-sai)
    install -D -t "$ROOTFS/usr/sbin" \
        "$ROOT"/target/release/hemlock-syncd \
        "$ROOT"/target/release/hemlock-pmon \
        "$ROOT"/target/release/hemlock-mgmtd \
        "$ROOT"/target/release/hemlock-orch
    install -D -t "$ROOTFS/usr/bin" "$ROOT"/target/release/hemlockctl
    install -D -m 644 -t "$ROOTFS/etc/systemd/system" "$ROOT"/build/rootfs/systemd/*.service "$ROOT"/build/rootfs/systemd/*.target
    for unit in "$ROOT"/build/rootfs/systemd/*.service; do
        chroot "$ROOTFS" systemctl enable "$(basename "$unit")" || true
    done

    log "installing vendor SAI ($SAI_DEB)"
    cp "$SAI_DEB" "$ROOTFS/tmp/"
    chroot "$ROOTFS" dpkg -i "/tmp/$(basename "$SAI_DEB")" || die "vendor SAI install failed"
    rm -f "$ROOTFS/tmp/$(basename "$SAI_DEB")"
fi

# Identity defaults: hostname "hemlock" (the CLI prompt is user@hostname).
echo hemlock > "$ROOTFS/etc/hostname"
grep -q "hemlock" "$ROOTFS/etc/hosts" 2>/dev/null     || echo "127.0.1.1 hemlock" >> "$ROOTFS/etc/hosts"

# --- 2. Squash it -----------------------------------------------------------
if command -v mksquashfs >/dev/null; then
    mksquashfs "$ROOTFS" "$PAYLOAD/rootfs.squashfs" -comp xz -noappend -quiet
else
    [ "$DUMMY" = 1 ] || die "squashfs-tools not installed"
    log "WARNING: mksquashfs unavailable; dummy payload uses a tar instead"
    tar -C "$ROOTFS" -czf "$PAYLOAD/rootfs.squashfs" .
fi

# --- 3. Platform overlay ----------------------------------------------------
mkdir -p "$PAYLOAD/platform"
cp "$PDIR/platform.toml" "$PAYLOAD/platform/"
echo "$ONIE_MACHINE" > "$PAYLOAD/platform/onie-machine"
echo "$PLATFORM" > "$PAYLOAD/platform/platform-id"
# Vendor data files ride along when present (real builds require them above).
for f in "$PDIR"/*; do
    case "$(basename "$f")" in
    platform.toml|README.md) ;;
    *) [ -f "$f" ] && cp "$f" "$PAYLOAD/platform/" ;;
    esac
done

# --- 4. Boot assets ---------------------------------------------------------
mkdir -p "$PAYLOAD/boot"
CONSOLE_DEV=0; CONSOLE_SPEED=115200
[ -f "$PDIR/boot.env" ] && . "$PDIR/boot.env"
sed -e "s/@CONSOLE_DEV@/$CONSOLE_DEV/g" \
    -e "s/@CONSOLE_SPEED@/$CONSOLE_SPEED/g" \
    -e "s/@VERSION@/$VERSION/g" \
    "$ROOT/build/rootfs/grub.cfg.in" > "$PAYLOAD/boot/grub.cfg"
cp "$ROOTFS/boot/vmlinuz"* "$PAYLOAD/boot/vmlinuz" 2>/dev/null \
    || echo dummy > "$PAYLOAD/boot/vmlinuz"
cp "$ROOTFS/boot/initrd.img"* "$PAYLOAD/boot/initrd.img" 2>/dev/null \
    || echo dummy > "$PAYLOAD/boot/initrd.img"

# --- 5. Installer binary ----------------------------------------------------
if [ "$DUMMY" = 1 ] && ! command -v cargo >/dev/null; then
    log "WARNING: cargo unavailable; dummy payload gets a stub installer"
    printf '#!/bin/sh\necho hemlock-installer stub\n' > "$PAYLOAD/hemlock-installer"
else
    (cd "$ROOT" && cargo build --release -p hemlock-installer)
    cp "$ROOT/target/release/hemlock-installer" "$PAYLOAD/hemlock-installer"
fi
chmod +x "$PAYLOAD/hemlock-installer"

# --- 6. Self-extracting ONIE .bin ------------------------------------------
BIN="$OUT/hemlock-$VERSION-$PLATFORM.bin"
TARBALL="$WORK/payload.tar.gz"
tar -C "$PAYLOAD" -czf "$TARBALL" .

cat > "$BIN" <<EOF
#!/bin/sh
# Hemlock ONIE installer image
# platform: $PLATFORM ($ONIE_MACHINE)   version: $VERSION
hemlock_image_platform=$ONIE_MACHINE
hemlock_image_version=$VERSION
set -e
EXTRACT_DIR="\${HEMLOCK_EXTRACT_DIR:-/tmp/hemlock-image.\$\$}"
mkdir -p "\$EXTRACT_DIR"
ARCHIVE_LINE=\$(awk '/^__HEMLOCK_PAYLOAD__/ { print NR + 1; exit 0 }' "\$0")
tail -n +\$ARCHIVE_LINE "\$0" | gzip -dc | ( cd "\$EXTRACT_DIR" && tar -xf - )
if [ -n "\${HEMLOCK_EXTRACT_ONLY:-}" ]; then
    echo "payload extracted to \$EXTRACT_DIR"
    exit 0
fi
cd "\$EXTRACT_DIR"
exec ./hemlock-installer --payload "\$EXTRACT_DIR" "\$@"
__HEMLOCK_PAYLOAD__
EOF
cat "$TARBALL" >> "$BIN"
chmod +x "$BIN"

log "built $BIN ($(du -h "$BIN" | cut -f1))"
