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
    echo "dummy-kernel" > "$ROOTFS/boot/vmlinuz"
    echo "dummy-initrd" > "$ROOTFS/boot/initrd.img"
else
    command -v debootstrap >/dev/null || die "debootstrap not installed"
    command -v mksquashfs >/dev/null || die "squashfs-tools not installed"

    # The vendor SAI blob must exist for a real image.
    # Exact-name glob: libsaibcm-dev_* must never match here.
    SAI_DEB="$(ls "$ROOT"/vendor/sai/libsaibcm_"$SAI_PIN"_*.deb 2>/dev/null | head -1 || true)"
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

    # --- Kernel modules (BDE + platform drivers) ---------------------------
    # The manifest's [kernel] required_modules must be loadable in the
    # image or syncd/pmon fail on real hardware (deliberately: no mock
    # fallback when the ASIC is present — mock data must never look like
    # a healthy switch). Build the staged GPL sources inside the chroot so
    # headers match the image kernel exactly, and refuse to ship an image
    # where any required module would not resolve.
    KVER="$(ls "$ROOTFS/lib/modules" | head -1)"
    [ -n "$KVER" ] || die "no kernel in rootfs (linux-image-amd64 missing?)"
    BDE_SRC="$ROOT/vendor/sai/saibcm-modules"
    [ -d "$BDE_SRC" ] || die \
        "vendor/sai/saibcm-modules missing — run vendor/fetch-vendor.sh $PLATFORM"

    log "building kernel modules for $KVER (BDE + platform drivers)"
    export DEBIAN_FRONTEND=noninteractive
    chroot "$ROOTFS" apt-get -qq update
    chroot "$ROOTFS" apt-get -qq install --no-install-recommends -y \
        "linux-headers-$KVER" build-essential bc \
        || die "installing kernel build deps in the chroot failed"

    KMOD_TMP="$ROOTFS/tmp/kmod"
    MODDEST="$ROOTFS/lib/modules/$KVER/updates/hemlock"
    mkdir -p "$KMOD_TMP" "$MODDEST"

    cp -r "$BDE_SRC" "$KMOD_TMP/saibcm-modules"
    install -m 755 "$ROOT/build/build-bde.sh" "$KMOD_TMP/build-bde.sh"
    chroot "$ROOTFS" /tmp/kmod/build-bde.sh /tmp/kmod/saibcm-modules "$KVER" /tmp/kmod/bde-out \
        || die "BDE module build failed"
    cp "$KMOD_TMP/bde-out/"*.ko "$MODDEST/"

    # Platform driver kbuild dirs committed under <platform>/kmod/
    # (upstream GPL sources ported to the image kernel; see the README
    # there for provenance).
    for src in "$PDIR/kmod"/*/; do
        [ -f "$src/Makefile" ] || continue
        name="$(basename "$src")"
        rm -rf "$KMOD_TMP/$name"
        cp -r "$src" "$KMOD_TMP/$name"
        chroot "$ROOTFS" make -C "/lib/modules/$KVER/build" "M=/tmp/kmod/$name" modules \
            || die "platform module build failed: $name"
        cp "$KMOD_TMP/$name/"*.ko "$MODDEST/"
    done

    # Kernel module trees a fixed-function switch can never use. The
    # image must stay small: ONIE stages the whole payload in tmpfs, so
    # every MB is RAM at install time on a 2GB box.
    KMODTREE="$ROOTFS/lib/modules/$KVER/kernel"
    rm -rf "$KMODTREE/sound" \
           "$KMODTREE/drivers/gpu" \
           "$KMODTREE/drivers/media" \
           "$KMODTREE/drivers/staging" \
           "$KMODTREE/drivers/infiniband" \
           "$KMODTREE/drivers/net/wireless" \
           "$KMODTREE/drivers/net/wwan" \
           "$KMODTREE/drivers/net/can" \
           "$KMODTREE/drivers/bluetooth" \
           "$KMODTREE/drivers/isdn" \
           "$KMODTREE/net/bluetooth" \
           "$KMODTREE/net/wireless" \
           "$KMODTREE/net/mac80211" \
           "$KMODTREE/net/can"

    chroot "$ROOTFS" depmod "$KVER"
    # Entry lines start with whitespace+quote; capturing the FIRST quoted
    # string keeps quoted words in trailing comments out of the list.
    # modprobe --dry-run (not modinfo) so drivers Debian builds into the
    # kernel image count as loadable too.
    missing=""
    for module in $(sed -n '/^required_modules[[:space:]]*=/,/\]/p' "$PDIR/platform.toml" \
                    | sed -n 's/^[[:space:]]*"\([^"]*\)".*/\1/p'); do
        chroot "$ROOTFS" modprobe -S "$KVER" --dry-run --quiet "$module" \
            || missing="$missing $module"
    done
    [ -z "$missing" ] || die \
        "required kernel modules not loadable in the image:$missing
 (sources: vendor/sai/saibcm-modules via fetch-vendor.sh, platforms/$PLATFORM/kmod/)"

    # Drop the toolchain again; it has no business on a switch. Then
    # scrub the apt caches the toolchain install left behind — the
    # downloaded .debs and package lists alone are worth hundreds of MB
    # of image.
    chroot "$ROOTFS" apt-get -qq purge -y "linux-headers-$KVER" build-essential bc || true
    chroot "$ROOTFS" apt-get -qq autoremove --purge -y || true
    chroot "$ROOTFS" apt-get clean
    rm -rf "$ROOTFS/var/lib/apt/lists"/* "$ROOTFS/var/cache/apt"/*
    rm -rf "$KMOD_TMP"

    # Docs, man pages, and translations an appliance never renders.
    # Debian copyright files stay (licensing).
    find "$ROOTFS/usr/share/doc" -type f ! -name copyright -delete 2>/dev/null || true
    find "$ROOTFS/usr/share/doc" -type d -empty -delete 2>/dev/null || true
    rm -rf "$ROOTFS/usr/share/man" "$ROOTFS/usr/share/info" \
           "$ROOTFS/usr/share/lintian" "$ROOTFS/usr/share/locale"/*

    # Boot hand-off: the stock initramfs cannot interpret hemlock.rootfs=.
    # Install the hemlock hook + local-bottom script and regenerate the
    # initrd so it loop-mounts the squashfs and overlays /hemlock/persist.
    log "installing hemlock initramfs scripts, regenerating initrd"
    install -D -m 755 "$ROOT/build/rootfs/initramfs/hemlock-hook" \
        "$ROOTFS/etc/initramfs-tools/hooks/hemlock"
    install -D -m 755 "$ROOT/build/rootfs/initramfs/hemlock-local-bottom" \
        "$ROOTFS/etc/initramfs-tools/scripts/local-bottom/hemlock"
    chroot "$ROOTFS" update-initramfs -u -k all || die "update-initramfs failed"

    # Default operator account. debootstrap leaves root locked (no
    # password at all), so without this the booted system is a brick at
    # the login prompt. Root stays locked; admin has full sudo.
    #
    # The login shell is hemlockctl: logging in lands straight in the
    # network CLI (operational mode), like EOS/JunOS. `bash` inside the
    # CLI drops to Linux; `hemlockctl -c` keeps ssh remote commands and
    # the sftp subsystem working.
    # The hemlock group grants connect access to the daemon sockets under
    # /run/hemlock (daemons chgrp+0660 them at bind time); every operator
    # account needs it or the CLI cannot reach the daemons.
    log "creating default operator account (admin)"
    chroot "$ROOTFS" groupadd --system hemlock
    echo /usr/bin/hemlockctl >> "$ROOTFS/etc/shells"
    chroot "$ROOTFS" useradd -m -s /usr/bin/hemlockctl -G sudo,hemlock admin
    echo 'admin:Hemlock123!' | chroot "$ROOTFS" chpasswd
fi

# Runtime path contract: the initramfs mounts the flash partition (which
# holds hemlock/{platform,persist,rootfs.squashfs}) at /host. Everything
# in the running system — units, hemlockctl, the MOTD — addresses it as
# /hemlock via this symlink.
ln -sfn host/hemlock "$ROOTFS/hemlock"

# Broadcom's SAI hardcodes SONiC's hwsku directory when it looks for
# optional rc scripts (sai_postinit_cmd.soc et al); point it at the
# platform overlay so those files are found without a SONiC layout.
install -d "$ROOTFS/usr/share/sonic"
ln -sfn /hemlock/platform "$ROOTFS/usr/share/sonic/hwsku"

# Identity defaults: hostname "hemlock" (the CLI prompt is user@hostname).
echo hemlock > "$ROOTFS/etc/hostname"
grep -q "hemlock" "$ROOTFS/etc/hosts" 2>/dev/null     || echo "127.0.1.1 hemlock" >> "$ROOTFS/etc/hosts"

# Hemlock branding: everything user-facing says Hemlock + version, not
# "Debian GNU/Linux". VERSION_CODENAME keeps the Debian base codename —
# tooling (and the MOTD) reads it from here.
cat > "$ROOTFS/etc/os-release" <<EOF
NAME="Hemlock"
VERSION="$VERSION (trixie)"
VERSION_ID="$VERSION"
VERSION_CODENAME=trixie
ID=hemlock
ID_LIKE=debian
PRETTY_NAME="Hemlock NOS v$VERSION"
HOME_URL="https://github.com/nightshadesystems/hemlock"
EOF
# Pre-login console banner: the MOTD art plus a version/hostname/tty
# line. agetty interprets backslash escapes in /etc/issue (\n = hostname,
# \l = tty, unknown ones are mangled), so the art's own backslashes must
# be doubled.
{
    HEMLOCK_MOTD_COLOR=0 sh "$ROOT/build/rootfs/update-motd.d/00-hemlock-banner" | sed 's/\\/\\\\/g'
    printf '\nHemlock NOS v%s \\n \\l\n\n' "$VERSION"
} > "$ROOTFS/etc/issue"
printf 'Hemlock NOS v%s\n' "$VERSION" > "$ROOTFS/etc/issue.net"

# --- Dynamic MOTD -----------------------------------------------------------
# Debian's update-motd.d mechanism: pam_motd runs these on every login and
# renders the output into /run/motd.dynamic. 00 is the static banner, 10
# the live status (a thin wrapper over `hemlockctl motd`); hemlock-motd
# previews the whole thing without logging in. The stock Debian pieces
# (10-uname, the /etc/motd license blurb) are removed so only Hemlock
# content shows.
install -D -m 755 "$ROOT/build/rootfs/update-motd.d/00-hemlock-banner" \
    "$ROOTFS/etc/update-motd.d/00-hemlock-banner"
install -D -m 755 "$ROOT/build/rootfs/update-motd.d/10-hemlock-status" \
    "$ROOTFS/etc/update-motd.d/10-hemlock-status"
install -D -m 755 "$ROOT/build/rootfs/bin/hemlock-motd" "$ROOTFS/usr/bin/hemlock-motd"
# Boot LED indication (CPLD-driven; no-op on platforms without support).
install -D -m 755 "$ROOT/build/rootfs/bin/hemlock-boot-led" "$ROOTFS/usr/bin/hemlock-boot-led"
rm -f "$ROOTFS/etc/update-motd.d/10-uname" "$ROOTFS/etc/motd"
# pam_motd renders update-motd.d; sshd must not print a motd of its own.
install -d "$ROOTFS/etc/ssh/sshd_config.d"
printf 'PrintMotd no\n' > "$ROOTFS/etc/ssh/sshd_config.d/10-hemlock-motd.conf"

# --- 2. Boot assets ---------------------------------------------------------
# Copied out of the rootfs BEFORE squashing: GRUB loads kernel + initrd
# from the flash partition (payload/boot), so keeping copies inside the
# squashfs would ship them twice — and pre-compressed artifacts gain
# nothing from squashfs xz, so it is a full-size waste.
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
rm -f "$ROOTFS"/boot/vmlinuz* "$ROOTFS"/boot/initrd.img* \
      "$ROOTFS"/boot/System.map* "$ROOTFS"/boot/config-*

# --- 3. Squash it -----------------------------------------------------------
if command -v mksquashfs >/dev/null; then
    mksquashfs "$ROOTFS" "$PAYLOAD/rootfs.squashfs" -comp xz -noappend -quiet
else
    [ "$DUMMY" = 1 ] || die "squashfs-tools not installed"
    log "WARNING: mksquashfs unavailable; dummy payload uses a tar instead"
    tar -C "$ROOTFS" -czf "$PAYLOAD/rootfs.squashfs" .
fi

# --- 4. Platform overlay ----------------------------------------------------
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

# --- 5. Installer binary ----------------------------------------------------
# ONIE's runtime is BusyBox with no glibc dynamic loader, so the installer
# must be statically linked: build it for the musl target.
INSTALLER_TARGET="x86_64-unknown-linux-musl"
if [ "$DUMMY" = 1 ] && ! command -v cargo >/dev/null; then
    log "WARNING: cargo unavailable; dummy payload gets a stub installer"
    printf '#!/bin/sh\necho hemlock-installer stub\n' > "$PAYLOAD/hemlock-installer"
else
    if command -v rustup >/dev/null; then
        rustup target add "$INSTALLER_TARGET" >/dev/null
    fi
    (cd "$ROOT" && cargo build --release -p hemlock-installer --target "$INSTALLER_TARGET")
    cp "$ROOT/target/$INSTALLER_TARGET/release/hemlock-installer" "$PAYLOAD/hemlock-installer"
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
