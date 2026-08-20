# Hemlock image build

`mkimage.sh <platform-id>` assembles an ONIE-installable, self-extracting
`.bin` for one platform:

1. **rootfs** — debootstrap Debian 13 (trixie) with
   [rootfs/packages.list](rootfs/packages.list), install the Hemlock
   daemons + [systemd units](rootfs/systemd/), and the platform's pinned
   vendor SAI `.deb` from `vendor/sai/` (per-platform pin from the
   manifest — never global).
2. **squashfs** — the rootfs, compressed.
3. **payload** — squashfs + platform overlay (`platform.toml`, config.bcm,
   identity markers) + boot assets + the `hemlock-installer` binary.
4. **.bin** — a `#!/bin/sh` self-extractor ONIE executes; it unpacks the
   payload and hands control to `hemlock-installer` (machine.conf check,
   disk selection TUI, GRUB install).

`--dummy-rootfs` skips debootstrap and vendor blobs entirely, producing a
structurally valid image for CI; `verify-image.sh` checks the layout without
executing anything. Per-platform console settings come from an optional
`platforms/<id>/boot.env` (`CONSOLE_DEV`, `CONSOLE_SPEED`).

Real builds need: Debian host, `debootstrap`, `squashfs-tools`, a Rust
toolchain with libclang (for `real-sai`), and the vendor blobs staged per
[vendor/sai/README.md](../vendor/sai/README.md).
