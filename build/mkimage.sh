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
SAI_BACKEND="$(manifest_value backend)"
[ -n "$SAI_BACKEND" ] || SAI_BACKEND="sai"
[ -n "$ONIE_MACHINE" ] || die "cannot read onie_machine from $PDIR/platform.toml"

# --- Architecture -----------------------------------------------------------
# Everything below that differs between an x86 and an ARM board hangs off
# this one manifest field. Boards are not all x86: the AS4610's host CPU
# is an on-die ARM Cortex-A9, which changes the rootfs architecture, the
# boot artifacts (a FIT image instead of GRUB), the installer's target
# triple and how the BDE kernel modules are built.
CPU_ARCH="$(manifest_value cpu_arch)"
[ -n "$CPU_ARCH" ] || CPU_ARCH="amd64"
case "$CPU_ARCH" in
amd64)
    DEB_ARCH="amd64"
    KERNEL_PKG="linux-image-amd64"
    RUST_TARGET=""                              # host build
    INSTALLER_TARGET="x86_64-unknown-linux-musl"
    BOOT_STYLE="grub"
    ;;
armhf)
    DEB_ARCH="armhf"
    KERNEL_PKG=""                               # platform kernel; see below
    RUST_TARGET="armv7-unknown-linux-gnueabihf"
    INSTALLER_TARGET="armv7-unknown-linux-musleabihf"
    BOOT_STYLE="fit"
    ;;
*)
    die "unknown cpu_arch $CPU_ARCH in $PDIR/platform.toml (known: amd64, armhf)"
    ;;
esac
# Cross-building at all? The host is assumed x86_64 (what CI and every
# development box is); a native armhf builder would set this to 0 and
# skip debootstrap's second stage.
CROSS=0
[ "$DEB_ARCH" = "amd64" ] || CROSS=1
log "platform $PLATFORM: cpu_arch=$CPU_ARCH backend=$SAI_BACKEND boot=$BOOT_STYLE"

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
    # An ARM board boots a FIT, not a kernel+initrd pair; the dummy one
    # is a placeholder so verify-image.sh exercises the same layout CI
    # would see for a real armhf build.
    [ "$BOOT_STYLE" = "fit" ] && echo "dummy-fit" > "$ROOTFS/boot/hemlock.itb"
else
    command -v debootstrap >/dev/null || die "debootstrap not installed"
    command -v mksquashfs >/dev/null || die "squashfs-tools not installed"

    # --- Preflight ---------------------------------------------------------
    # Everything a real build needs, checked before any of it is done.
    # debootstrap alone takes minutes; discovering a missing kernel or a
    # missing cross toolchain afterwards wastes all of it, and the whole
    # list at once beats finding the prerequisites one failure at a time.
    MISSING=""
    want() { # want <description> <test...>
        local what="$1"; shift
        "$@" >/dev/null 2>&1 || MISSING="$MISSING
  - $what"
    }

    if [ "$CROSS" = 1 ]; then
        # Foreign-architecture rootfs: debootstrap unpacks the first
        # stage, then the second stage runs the packages' maintainer
        # scripts *inside* the chroot under qemu-user via binfmt.
        want "qemu-arm-static (apt: qemu-user-static) — the $DEB_ARCH second stage" \
            test -x /usr/bin/qemu-arm-static
        want "binfmt qemu-arm registered (apt: binfmt-support)" \
            test -e /proc/sys/fs/binfmt_misc/qemu-arm
        want "arm-linux-gnueabihf-gcc (apt: gcc-arm-linux-gnueabihf) — links the daemons" \
            command -v arm-linux-gnueabihf-gcc
    fi
    if [ "$BOOT_STYLE" = "fit" ]; then
        want "mkimage (apt: u-boot-tools) — builds the FIT" command -v mkimage
        want "dtc (apt: device-tree-compiler) — compiles the board device tree" \
            command -v dtc
        want "a device tree in $PDIR/dts/" \
            sh -c "ls '$PDIR'/dts/*.dts >/dev/null 2>&1"
    fi

    # The datapath library. A SAI platform needs its pinned vendor blob;
    # an openbcm platform needs the shim built from the SDK (which is a
    # separate, cross-container step — see vendor/openbcm-shim/).
    SAI_DEB=""
    SHIM_SO=""
    if [ "$SAI_BACKEND" = "openbcm" ]; then
        SHIM_SO="$(ls "$ROOT"/vendor/openbcm/out/libhemlockbcm.so* 2>/dev/null | head -1 || true)"
        [ -n "$SHIM_SO" ] || MISSING="$MISSING
  - vendor/openbcm/out/libhemlockbcm.so — build it with
    vendor/fetch-vendor.sh $PLATFORM && vendor/openbcm-shim/build-shim.sh"
    else
        # Exact-name glob: libsaibcm-dev_* must never match here.
        SAI_DEB="$(ls "$ROOT"/vendor/sai/libsaibcm_"$SAI_PIN"_*.deb 2>/dev/null | head -1 || true)"
        [ -n "$SAI_DEB" ] || MISSING="$MISSING
  - vendor/sai/libsaibcm_${SAI_PIN}_*.deb — see vendor/sai/README.md"
    fi
    [ -f "$PDIR/$CONFIG_BCM" ] || MISSING="$MISSING
  - $PDIR/$CONFIG_BCM — run vendor/fetch-vendor.sh $PLATFORM"

    # The kernel. x86 rides Debian's own; armhf cannot, because upstream
    # Linux has no Helix4 support and Debian's armmp kernel will not boot
    # this board. Resolve it here so its absence is reported with all the
    # rest rather than after a full debootstrap.
    KERNEL_DEB=""
    if [ -z "$KERNEL_PKG" ]; then
        KERNEL_DEB="$(ls "$ROOT"/vendor/kernel/linux-image-*-hemlock-iproc*_"$DEB_ARCH".deb 2>/dev/null | head -1 || true)"
        [ -n "$KERNEL_DEB" ] || MISSING="$MISSING
  - vendor/kernel/linux-image-*-hemlock-iproc*_$DEB_ARCH.deb — the iProc
    kernel port is tracked in docs/as4610-kernel-port.md and is NOT DONE
    YET, so a full $CPU_ARCH image cannot be built at all right now"
    fi

    [ -z "$MISSING" ] || die "a real $CPU_ARCH image needs:$MISSING

 None of this is needed for a structural check: build/mkimage.sh $PLATFORM --dummy-rootfs"

    log "debootstrap Debian trixie ($DEB_ARCH)"
    DEBOOTSTRAP_ARGS=(--variant=minbase --arch="$DEB_ARCH"
        "--include=$(grep -v '^#' "$ROOT/build/rootfs/packages.list" | grep -v '^$' | paste -sd, -)")
    if [ "$CROSS" = 1 ]; then
        # First stage unpacks only; the maintainer scripts run in the
        # second stage, under qemu, from inside the chroot.
        debootstrap "${DEBOOTSTRAP_ARGS[@]}" --foreign \
            trixie "$ROOTFS" https://deb.debian.org/debian
        install -D /usr/bin/qemu-arm-static "$ROOTFS/usr/bin/qemu-arm-static"
        log "debootstrap second stage (qemu-user)"
        chroot "$ROOTFS" /debootstrap/debootstrap --second-stage \
            || die "debootstrap second stage failed (is binfmt qemu-arm registered?)"
    else
        debootstrap "${DEBOOTSTRAP_ARGS[@]}" trixie "$ROOTFS" https://deb.debian.org/debian
    fi

    # --- Platform kernel ---------------------------------------------------
    # amd64 rides Debian's own (pulled in by packages.list); armhf gets
    # the Hemlock iProc kernel resolved during preflight.
    if [ -n "$KERNEL_DEB" ]; then
        log "installing platform kernel $(basename "$KERNEL_DEB")"
        cp "$KERNEL_DEB" "$ROOTFS/tmp/"
        chroot "$ROOTFS" dpkg -i "/tmp/$(basename "$KERNEL_DEB")" \
            || die "platform kernel install failed"
        rm -f "$ROOTFS/tmp/$(basename "$KERNEL_DEB")"
    fi

    log "installing Hemlock daemons"
    CARGO_ARGS=(build --release --workspace)
    if [ "$SAI_BACKEND" = "openbcm" ]; then
        CARGO_ARGS+=(--features hemlock-syncd/openbcm)
    else
        CARGO_ARGS+=(--features hemlock-syncd/real-sai)
    fi
    BIN_DIR="$ROOT/target/release"
    if [ -n "$RUST_TARGET" ]; then
        command -v rustup >/dev/null && rustup target add "$RUST_TARGET" >/dev/null
        CARGO_ARGS+=(--target "$RUST_TARGET")
        BIN_DIR="$ROOT/target/$RUST_TARGET/release"
        # cargo needs to be told which linker drives the cross target.
        export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER="arm-linux-gnueabihf-gcc"
    fi
    (cd "$ROOT" && cargo "${CARGO_ARGS[@]}")
    install -D -t "$ROOTFS/usr/sbin" \
        "$BIN_DIR"/hemlock-syncd \
        "$BIN_DIR"/hemlock-pmon \
        "$BIN_DIR"/hemlock-mgmtd \
        "$BIN_DIR"/hemlock-orch \
        "$BIN_DIR"/hemlock-webd
    install -D -t "$ROOTFS/usr/bin" "$BIN_DIR"/hemlockctl
    install -D -m 644 -t "$ROOTFS/etc/systemd/system" "$ROOT"/build/rootfs/systemd/*.service "$ROOT"/build/rootfs/systemd/*.target
    for unit in "$ROOT"/build/rootfs/systemd/*.service; do
        chroot "$ROOTFS" systemctl enable "$(basename "$unit")" || true
    done
    # The web console is config-driven like sshd: mgmtd enables the unit
    # on `set system http|https` + commit and replays that at boot. An
    # unconfigured switch must not listen on 80/443.
    chroot "$ROOTFS" systemctl disable hemlock-webd >/dev/null 2>&1 || true

    # Web console UI: the exported Next.js build, served by hemlock-webd.
    # Built here when missing so a release build stays one command; CI
    # builds it in its own job.
    if [ ! -f "$ROOT/web/out/index.html" ]; then
        command -v npm >/dev/null || die \
            "web/out is missing and npm is not installed — build the web UI first: (cd web && npm ci && npm run build)"
        log "building web console UI (npm)"
        (cd "$ROOT/web" && npm ci --no-audit --no-fund && npm run build) \
            || die "web UI build failed"
    fi
    mkdir -p "$ROOTFS/usr/share/hemlock"
    cp -r "$ROOT/web/out" "$ROOTFS/usr/share/hemlock/web"

    if [ "$SAI_BACKEND" = "openbcm" ]; then
        # Hemlock's own shim, at exactly the path the manifest pins so
        # syncd's dlopen finds it.
        SHIM_PATH="$(manifest_value shim_path)"
        [ -n "$SHIM_PATH" ] || die "no [sai] shim_path in $PDIR/platform.toml"
        log "installing OpenBCM shim ($SHIM_SO -> $SHIM_PATH)"
        install -D -m 755 "$SHIM_SO" "$ROOTFS$SHIM_PATH"
        # No PHY firmware to stage: the BCM84758 microcode is a
        # compiled-in C array in the SDK (src/soc/phy/phy84758_ucode.c),
        # so it is already inside the shim rather than loaded from
        # /lib/firmware.
    else
        log "installing vendor SAI ($SAI_DEB)"
        cp "$SAI_DEB" "$ROOTFS/tmp/"
        chroot "$ROOTFS" dpkg -i "/tmp/$(basename "$SAI_DEB")" || die "vendor SAI install failed"
        rm -f "$ROOTFS/tmp/$(basename "$SAI_DEB")"
    fi

    # --- Kernel modules (BDE + platform drivers) ---------------------------
    # The manifest's [kernel] required_modules must be loadable in the
    # image or syncd/pmon fail on real hardware (deliberately: no mock
    # fallback when the ASIC is present — mock data must never look like
    # a healthy switch). Build the staged GPL sources inside the chroot so
    # headers match the image kernel exactly, and refuse to ship an image
    # where any required module would not resolve.
    KVER="$(ls "$ROOTFS/lib/modules" | head -1)"
    [ -n "$KVER" ] || die "no kernel in rootfs ($KERNEL_PKG missing?)"

    log "building kernel modules for $KVER (BDE + platform drivers)"
    export DEBIAN_FRONTEND=noninteractive
    chroot "$ROOTFS" apt-get -qq update
    chroot "$ROOTFS" apt-get -qq install --no-install-recommends -y \
        "linux-headers-$KVER" build-essential bc \
        || die "installing kernel build deps in the chroot failed"

    KMOD_TMP="$ROOTFS/tmp/kmod"
    MODDEST="$ROOTFS/lib/modules/$KVER/updates/hemlock"
    mkdir -p "$KMOD_TMP" "$MODDEST"

    # Where the BDE/KNET sources come from depends on the backend, not on
    # the architecture: a SAI platform's modules must match the pinned
    # SAI's SDK lineage (SONiC's saibcm-modules), while an openbcm
    # platform's come from the same OpenBCM tree the shim was built from.
    if [ "$SAI_BACKEND" = "openbcm" ]; then
        BDE_SRC="$(ls -d "$ROOT"/vendor/openbcm/sdk-* 2>/dev/null | head -1 || true)"
        [ -n "$BDE_SRC" ] || die \
            "no OpenBCM tree in vendor/openbcm/ — run vendor/fetch-vendor.sh $PLATFORM"
        log "building BDE/KNET from $(basename "$BDE_SRC") for $KVER"
        install -m 755 "$ROOT/build/build-bde-openbcm.sh" "$KMOD_TMP/build-bde-openbcm.sh"
        cp -r "$BDE_SRC" "$KMOD_TMP/openbcm"
        chroot "$ROOTFS" /tmp/kmod/build-bde-openbcm.sh /tmp/kmod/openbcm "$KVER" \
            /tmp/kmod/bde-out || die "OpenBCM BDE/KNET module build failed"
    else
        BDE_SRC="$ROOT/vendor/sai/saibcm-modules"
        [ -d "$BDE_SRC" ] || die \
            "vendor/sai/saibcm-modules missing — run vendor/fetch-vendor.sh $PLATFORM"
        cp -r "$BDE_SRC" "$KMOD_TMP/saibcm-modules"
        install -m 755 "$ROOT/build/build-bde.sh" "$KMOD_TMP/build-bde.sh"
        chroot "$ROOTFS" /tmp/kmod/build-bde.sh /tmp/kmod/saibcm-modules "$KVER" \
            /tmp/kmod/bde-out || die "BDE module build failed"
    fi
    cp "$KMOD_TMP/bde-out/"*.ko "$MODDEST/"

    # Platform driver kbuild dirs: the ones every board needs, under
    # platforms/_common/kmod/, then this board's own under
    # <platform>/kmod/ (upstream GPL sources ported to the image kernel;
    # see the README in each for provenance).
    for src in "$ROOT/platforms/_common/kmod"/*/ "$PDIR/kmod"/*/; do
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
 (sources: vendor/sai/saibcm-modules via fetch-vendor.sh, platforms/_common/kmod/, platforms/$PLATFORM/kmod/)"

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

    # ping needs cap_net_raw; the capability xattr does not always
    # survive debootstrap's postinst in a chroot, leaving operators
    # unable to ping without sudo. mksquashfs preserves xattrs, so
    # setting it here sticks in the image.
    chroot "$ROOTFS" setcap cap_net_raw+ep /usr/bin/ping \
        || log "WARNING: setcap on ping failed (ping will need sudo)"
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
# SSH is config-driven: `set system ssh` + commit turns it on, and mgmtd
# replays the persisted running config at every boot. The Debian package
# enables sshd unconditionally, so disable it here — an unconfigured
# switch (console only) then matches its empty running config.
chroot "$ROOTFS" systemctl disable ssh >/dev/null 2>&1 || true

# --- 2. Boot assets ---------------------------------------------------------
# Copied out of the rootfs BEFORE squashing: GRUB loads kernel + initrd
# from the flash partition (payload/boot), so keeping copies inside the
# squashfs would ship them twice — and pre-compressed artifacts gain
# nothing from squashfs xz, so it is a full-size waste.
mkdir -p "$PAYLOAD/boot"
CONSOLE_DEV=0; CONSOLE_SPEED=115200
[ -f "$PDIR/boot.env" ] && . "$PDIR/boot.env"

if [ "$BOOT_STYLE" = "fit" ]; then
    # U-Boot boots a FIT: one container holding the kernel, the board
    # device tree and the initramfs. There is no GRUB and no bootloader
    # config — the kernel command line is baked into the FIT's own
    # `bootargs`, which is why it is rendered here rather than in a
    # separate file the installer would have to place.
    KCMDLINE="console=ttyS${CONSOLE_DEV},${CONSOLE_SPEED}n8 root=/dev/ram0 hemlock.rootfs=/hemlock/rootfs.squashfs rw net.ifnames=0"
    if [ "$DUMMY" = 1 ]; then
        # CI has no kernel, no dtb and no mkimage. Ship the placeholder
        # the dummy rootfs made, so the payload layout is still the real
        # one and verify-image.sh checks the same thing it would for a
        # real build.
        cp "$ROOTFS/boot/hemlock.itb" "$PAYLOAD/boot/hemlock.itb"
    else
        command -v mkimage >/dev/null || die "mkimage not installed (u-boot-tools)"
        command -v dtc >/dev/null || die "dtc not installed (device-tree-compiler)"
        DTS="$(ls "$PDIR"/dts/*.dts 2>/dev/null | head -1 || true)"
        [ -n "$DTS" ] || die "no device tree in $PDIR/dts/ (needed for the FIT)"

        FITDIR="$WORK/fit"
        mkdir -p "$FITDIR"
        KIMAGE="$(ls "$ROOTFS"/boot/vmlinuz* "$ROOTFS"/boot/Image* 2>/dev/null | head -1 || true)"
        [ -n "$KIMAGE" ] || die "no kernel image in the rootfs"
        gzip -nc "$KIMAGE" > "$FITDIR/kernel.gz"
        INITRD="$(ls "$ROOTFS"/boot/initrd.img* 2>/dev/null | head -1 || true)"
        [ -n "$INITRD" ] || die "no initramfs in the rootfs"
        cp "$INITRD" "$FITDIR/initrd.img"
        dtc -I dts -O dtb "$DTS" -o "$FITDIR/board.dtb" 2>/dev/null \
            || die "compiling $DTS failed"

        # Load/entry 0x61008000: where U-Boot on this board expects the
        # decompressed kernel, per the board memory map in its device tree
        # (memory starts at 0x61000000).
        cat > "$FITDIR/fit.its" <<ITS
/dts-v1/;
/ {
    description = "Hemlock $VERSION for $PLATFORM";
    #address-cells = <1>;
    images {
        kernel {
            description = "Linux";
            data = /incbin/("kernel.gz");
            type = "kernel";
            arch = "arm";
            os = "linux";
            compression = "gzip";
            load = <0x61008000>;
            entry = <0x61008000>;
            hash-1 { algo = "crc32"; };
        };
        fdt {
            description = "$(basename "$DTS" .dts)";
            data = /incbin/("board.dtb");
            type = "flat_dt";
            arch = "arm";
            compression = "none";
            hash-1 { algo = "crc32"; };
        };
        ramdisk {
            description = "initramfs";
            data = /incbin/("initrd.img");
            type = "ramdisk";
            arch = "arm";
            os = "linux";
            compression = "none";
            hash-1 { algo = "crc32"; };
        };
    };
    configurations {
        default = "conf";
        conf {
            description = "$PLATFORM";
            kernel = "kernel";
            fdt = "fdt";
            ramdisk = "ramdisk";
        };
    };
};
ITS
        # SOURCE_DATE_EPOCH keeps the FIT byte-reproducible across builds.
        (cd "$FITDIR" && SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-0}" \
            mkimage -f fit.its "$PAYLOAD/boot/hemlock.itb" >/dev/null) \
            || die "building the FIT failed"
        log "built FIT $(du -h "$PAYLOAD/boot/hemlock.itb" | cut -f1) (cmdline: $KCMDLINE)"
    fi
    # The command line the installer stamps into the U-Boot environment.
    printf '%s\n' "$KCMDLINE" > "$PAYLOAD/boot/cmdline"
else
    sed -e "s/@CONSOLE_DEV@/$CONSOLE_DEV/g" \
        -e "s/@CONSOLE_SPEED@/$CONSOLE_SPEED/g" \
        -e "s/@VERSION@/$VERSION/g" \
        "$ROOT/build/rootfs/grub.cfg.in" > "$PAYLOAD/boot/grub.cfg"
    cp "$ROOTFS/boot/vmlinuz"* "$PAYLOAD/boot/vmlinuz" 2>/dev/null \
        || echo dummy > "$PAYLOAD/boot/vmlinuz"
    cp "$ROOTFS/boot/initrd.img"* "$PAYLOAD/boot/initrd.img" 2>/dev/null \
        || echo dummy > "$PAYLOAD/boot/initrd.img"
fi
rm -f "$ROOTFS"/boot/vmlinuz* "$ROOTFS"/boot/Image* "$ROOTFS"/boot/initrd.img* \
      "$ROOTFS"/boot/hemlock.itb \
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
# CPU architecture, so the installer and verify-image.sh know which boot
# layout this payload carries without re-parsing the manifest.
echo "$CPU_ARCH" > "$PAYLOAD/platform/cpu-arch"
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
# (the triple came from the manifest's cpu_arch, above)
if [ "$DUMMY" = 1 ] && ! command -v cargo >/dev/null; then
    log "WARNING: cargo unavailable; dummy payload gets a stub installer"
    printf '#!/bin/sh\necho hemlock-installer stub\n' > "$PAYLOAD/hemlock-installer"
else
    if command -v rustup >/dev/null; then
        rustup target add "$INSTALLER_TARGET" >/dev/null
    fi
    # A cross target's default linker is `cc`, which on an x86 host is
    # the host compiler and cannot link ARM. rust-lld ships with the
    # toolchain and links a fully static musl binary on its own, so
    # building the ARM installer needs no C cross-toolchain — which is
    # what keeps this step runnable in CI.
    if [ "$INSTALLER_TARGET" != "x86_64-unknown-linux-musl" ]; then
        export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABIHF_LINKER=rust-lld
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
# The mode should already be right, but it depends on the build host's
# filesystem and on tar preserving it. This runs once, as root, in ONIE,
# and a missing execute bit here is a failed install on a switch that has
# just been wiped — cheap insurance.
chmod +x ./hemlock-installer 2>/dev/null || true
exec ./hemlock-installer --payload "\$EXTRACT_DIR" "\$@"
__HEMLOCK_PAYLOAD__
EOF
cat "$TARBALL" >> "$BIN"
chmod +x "$BIN"

log "built $BIN ($(du -h "$BIN" | cut -f1))"
