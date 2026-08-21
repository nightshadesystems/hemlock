# Hemlock image build

`mkimage.sh <platform-id>` assembles an ONIE-installable, self-extracting
`.bin` for one platform:

1. **rootfs** — debootstrap Debian 13 (trixie) with
   [rootfs/packages.list](rootfs/packages.list), install the Hemlock
   daemons + [systemd units](rootfs/systemd/), and the platform's pinned
   vendor SAI `.deb` from `vendor/sai/` (per-platform pin from the
   manifest — never global). The [initramfs scripts](rootfs/initramfs/)
   are installed and the initrd regenerated so boot can interpret
   `hemlock.rootfs=`: loop-mount the squashfs from the flash partition,
   overlay `/hemlock/persist` on top, and expose the flash at `/host`.
   The rootfs is branded as Hemlock (os-release/issue carry the Hemlock
   version, never "Debian GNU/Linux"), gets the default operator account
   `admin` / `Hemlock123!` (sudo; root stays locked) whose login shell
   is `hemlockctl` — logging in lands straight in the network CLI, and
   `bash` inside it drops to Linux. The banner is also rendered into
   `/etc/issue` (backslashes doubled for agetty) so it shows at the
   console before login, and the dynamic
   MOTD: [rootfs/update-motd.d/](rootfs/update-motd.d/) scripts rendered
   by pam_motd on every login (banner + `hemlockctl motd` live status),
   with the stock Debian motd removed. `hemlock-motd` previews it without
   logging in; `test-motd.sh` shellchecks the scripts, diffs the banner
   byte-for-byte against [tests/motd/banner.txt](tests/motd/banner.txt),
   and proves the status script exits 0 with every data source missing.
2. **squashfs** — the rootfs, compressed.
3. **payload** — squashfs + platform overlay (`platform.toml`, config.bcm,
   identity markers) + boot assets + the `hemlock-installer` binary.
4. **.bin** — a `#!/bin/sh` self-extractor ONIE executes; it unpacks the
   payload and hands control to `hemlock-installer` (machine.conf check,
   disk selection TUI, GRUB install).

`--dummy-rootfs` skips debootstrap and vendor blobs entirely, producing a
structurally valid image for CI; `verify-image.sh` checks the layout without
executing anything. `boot-test.sh <image.bin>` boots a full image's
kernel+initrd+squashfs in QEMU against an installer-shaped disk (no root
needed) and passes only if a login prompt appears — run it before flashing
hardware; CI runs it on every full image build. Per-platform console settings come from an optional
`platforms/<id>/boot.env` (`CONSOLE_DEV`, `CONSOLE_SPEED`).

Real builds need: Debian host, `debootstrap`, `squashfs-tools`, a Rust
toolchain with libclang (for `real-sai`) plus the
`x86_64-unknown-linux-musl` target (the installer must be statically
linked — ONIE's BusyBox runtime has no glibc dynamic loader), and the
vendor blobs staged per [vendor/sai/README.md](../vendor/sai/README.md).
