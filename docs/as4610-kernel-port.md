# AS4610-54T kernel port

Companion to [`as4610-54-port.md`](as4610-54-port.md). The AS4610 needs a
kernel Hemlock builds itself, and that port is on **Phase 4's critical
path** — nothing about it can be deferred past the first bootable image.
This document says why, how big the job actually is, and how it is
sequenced so it does not block the datapath work.

## Why a custom kernel

Upstream Linux has no Helix4 support. It carries Hurricane 2
(`ARCH_BCM_HR2`, `arch/arm/boot/dts/broadcom/bcm-hr2.dtsi`) — a sibling
iProc Cortex-A9 part with a CMICd — but no `ARCH_XGS_IPROC` machine, no
`bcm-helix4.dtsi`, and no CMICd driver. Debian's `linux-image-armmp`
therefore cannot boot this board.

The only public patch set is Open Network Linux's
`packages/base/any/kernels/4.14-lts/patches/brcm-iproc-4.14.patch`
(1.1 MB, 138 files) with the `armhf-iproc-all` kernel config, plus three
small companions in the same `series` file
(`drivers-i2c-busses-xgs_iproc_smbus-clk-freq.patch`,
`drivers-usb-phy-phy-xgs-iproc-usb-phy-mode.patch`,
`0001-drivers-i2c-muxes-pca954x-deselect-on-exit.patch`).

**Verified: there is no newer ONL fallback.** ONL's `4.19-lts` and
`5.4-lts` patch directories contain only x86_64/Mellanox material — no
`brcm-iproc` patch — and their `configs/` directories hold only
`x86_64-all`. `packages/base/armhf/kernels/` contains exactly one entry,
`kernel-4.14-lts-armhf-iproc-all`. So 4.14 is the *only* known-good
iProc kernel in public existence; every other version is a forward-port
with no landing point, which is why there is no intermediate hop worth
taking.

edgenos forward-ported this set to 4.19 / 5.10 / 5.15 / 6.1.175. Those
trees are private. Asking for the 6.1 tree is worth doing (it would
collapse most of this document), but the plan below assumes we do not
get it.

## Why 4.14 cannot carry the Hemlock rootfs

systemd's `README` states a hard **minimum baseline of Linux 5.10** —
"kernel versions below 5.10 are not supported at all" — and a
*recommended* baseline of 5.14, below which systemd sets the
`old-kernel` taint. Debian 13 (trixie) ships systemd 257.13.

So a Hemlock trixie rootfs cannot boot on 4.14, and 4.19 is no better.
This is what makes 4.14 a **bench platform only**.

## Two tracks

**Track 1 — datapath bench (4.14, no Hemlock rootfs).** Build ONL's
4.14 armhf-iproc kernel as-is, boot it with a minimal init (ONL's own
userspace, or a static busybox initramfs), and use it to validate
everything below the OS: the BDE/KNET modules, the CPLD reset-deassert
sequence, OpenBCM's `iproc-4_4` build, and `libhemlockbcm.so` itself
driven by a small statically-linked C or Rust harness against the shim
ABI. **The Hemlock daemons never run here.** This exists so Phase 3 can
finish and be proven while Track 2 is still in progress.

**Track 2 — the real port (6.1 LTS).** Forward-port the triaged subset
to 6.1 and ship it as a `linux-image-<ver>-hemlock-iproc` .deb, built
out of tree and installed into the image chroot the way the vendor SAI
.deb is on the E1031. 6.1 is chosen because it clears the systemd
baseline with margin, it is an LTS with a long tail, and it is the
version edgenos already proved on this exact board — so if their tree
ever becomes available the work converges rather than diverges.

Track 2 gates the first bootable image. Track 1 does not gate anything
except its own findings.

## What the port actually is

The "1.1 MB patch" figure is misleading. Measured per file and bucketed
against what *this board's* device tree actually enables:

| Bucket | Files | Added lines | Notes |
|---|---:|---:|---|
| **A. SoC / machine glue** | 6 | 878 | `arch/arm/mach-iproc/`: `Kconfig`, `Makefile`, `board_bu.c`, `platsmp.c` (SMP bring-up), `shm.c` + `plat/shm.h` |
| **B. Device tree (Helix4 only)** | 4 | 773 | `bcm-helix4.dtsi` (483), `bcm956340.dts` (230, the reference board we replace with the AS4610 DTS), `helix4.its`, the `dts/Makefile` hunk |
| **C. CMICd** | 8 | 864 | `drivers/soc/bcm/xgs_iproc/`: `iproc-cmic.c`, `iproc-cmicd.c`, `xgs-iproc.c`, `xgs-iproc-idm.c`, `xgs-iproc-misc-setup.c`, `include/linux/soc/bcm/iproc-cmic.h` |
| **D. Drivers this board needs** | 14 | 2239 | see below |
| **E. MDIO / SerDes (investigate)** | 5 | 1987 | `mdio-xgs-iproc-cmicd.c`, `mdio-xgs-iproc-cc.c`, `mdio-xgs-iproc.h`, `xgs-iproc-serdes.c`, `xgs_iproc_serdes_def.h` |
| **F. Kconfig/Makefile wiring** | 33 | 185 | |
| **Z. Droppable** | 68 | 22201 | see below |
| | **138** | **29127** | |

**The real port is A+B+C+D+F ≈ 4,900 added lines, or ≈ 6,900 with E.**
Roughly **76% of the patch is dead weight for this board.**

### D — drivers this board needs

Every one is pinned to a `compatible` string that `bcm-helix4.dtsi` or
the AS4610 DTS actually instantiates:

| DT node | `compatible` | Source |
|---|---|---|
| `i2c0`, `i2c1` | `brcm,iproc-i2c` | `xgs_iproc_smbus.c` + `iproc_smbus_regs.h` (1362 lines) |
| `gmac0` (management) | `brcm,xgs-iproc-amac` | `bgmac.c` / `bgmac-platform.c` / `bgmac.h` deltas (327) |
| clocks | `brcm,xgs-iproc-armpll`, `brcm,xgs-iproc-axi-clk` | `clk-xgs-iproc.c`, `clk-iproc-armpll.c` delta (173) |
| `hwrng` | `brcm,iproc-rng100` | `xgs-iproc-rng100.c` (211) — optional |
| `iproc_wdt` | `arm,sp805` | `sp805_wdt.c` delta (32) |
| `qspi` | `brcm,spi-bcm-qspi`, `brcm,spi-xgs-iproc-qspi` | `spi-bcm-qspi.c` delta (94/-47) — upstream driver, extra compatible |
| `nand` | `brcm,nand-iproc`, `brcm,brcmnand-v6.1` | `brcmnand.c` delta (4) — effectively upstream |
| `uart1`/`uart2` | `ns16550a` | upstream 8250 |
| `dmac0` | `arm,pl330` | upstream |
| `gpio_cca` | `brcm,iproc-gpio-cca` | **upstream already** — see below |

> **Hazard, and it will bite silently.** Upstream's
> `drivers/i2c/busses/i2c-bcm-iproc.c` binds `.compatible =
> "brcm,iproc-i2c"` — the *same string* the XGS controller uses, for
> different hardware. On 6.1 with `I2C_BCM_IPROC` enabled, upstream's
> driver will claim `i2c0`/`i2c1` on the AS4610. Since the DTS is ours,
> the fix is to rename the XGS compatible (`brcm,xgs-iproc-smbus`) in
> both the dtsi and the ported driver, and leave upstream's alone.
> Relying on `# CONFIG_I2C_BCM_IPROC is not set` would work but is a
> landmine for anyone who later enables it.

Note also that the patch ships **two mutually exclusive** i2c drivers —
`I2C_XGS_IPROC` (`i2c-xgs-iproc.c`) and `SMBUS_XGS_IPROC`
(`xgs_iproc_smbus.c`, `depends on ... && !I2C_XGS_IPROC`). ONL's
`armhf-iproc-all.config` sets `# CONFIG_I2C_XGS_IPROC is not set` and
`CONFIG_SMBUS_XGS_IPROC=y`, and edgenos's DTS comment blames
`xgs_iproc_smbus`'s per-transfer `msleep(1)` for the RTC-induced i2c
lock starvation. **Port `xgs_iproc_smbus.c`; drop `i2c-xgs-iproc.c`.**

### E — MDIO / SerDes: needs one bench answer

The front-panel PHYs are driven by the SDK over the CMIC's MDIO, not by
the kernel, so `mdio-xgs-iproc-cmicd.c` looks droppable. But `gmac0` is
`phy-mode = "sgmii"`, and ONL builds `CONFIG_MDIO_XGS_IPROC=y` +
`CONFIG_XGS_IPROC_SERDES=y`. If the management port's SGMII link needs
the ChipCommon MDIO bus (`mdio-xgs-iproc-cc.c`) and the SerDes driver to
come up, this bucket is mandatory and the port grows by ~2,000 lines.

**Resolve this on Track 1**, by building the 4.14 kernel with
`MDIO_XGS_IPROC`/`XGS_IPROC_SERDES` disabled and seeing whether `ma1`
still links. That single boot decides ~40% of the remaining port size,
so do it early.

### Z — droppable, with reasons

| What | Lines | Why |
|---|---:|---|
| Other SoCs' DTS (Katana2, Greyhound/2, Hurricane3, Saber2, Wolfhound2, Helix5, their `.dts`/`.its`) | ~5075 | not this board |
| APM ethernet (`apm.c/h`, `apm_ethtool.c`, `pm4x10.c`, `pm.h`, `merlin16_ucode.h`) | ~5334 | CMICx-era SoCs; Helix4 uses bgmac |
| USB gadget UDC + DRD + OHCI/EHCI + `phy-xgs-iproc` | ~4290 | a switch needs no USB; DTS nodes stay `disabled` |
| arm64 material (incl. `sha256-core.S`) | ~2500 | 32-bit board |
| CMICx (`iproc-cmicx.c`, `mdio-xgs-iproc-cmicx.c`) | 812 | this board is CMICd |
| `gpio-xgs-iproc.c` | 739 | **already upstream**, and upstream's `of_match` is exactly `brcm,iproc-gpio-cca` |
| `i2c-xgs-iproc.c` | 584 | mutually exclusive with the SMBus driver ONL selects |
| `pcie-xgs-iproc.c` | 541 | no PCIe on this board |
| `sdhci-bcm-hr3.c` | 458 | Hurricane 3 |
| `mtd/maps/xgs-iproc-flash.c` | 184 | `m25p80` + `brcmnand` cover the flash |
| `arch/arm/boot/compressed/*.S` backports | ~500 | 4.14-era; present in 6.1 |

## The board's memory map, confirmed

Salvaged off `sda1` before the install wiped it: the previous NOS's
`uImage` — "Broadcom Linux" 3.6.5, built 2016-08-08 — with an
`IKCONFIG`-embedded kernel config. A kernel that demonstrably booted
this board, which makes its memory map authoritative rather than
inferred:

| Constant | Value |
|---|---|
| `CONFIG_BCM_RAM_BASE` | `0x60000000` |
| `CONFIG_BCM_RAM_START_RESERVED_SIZE` | `0x200000` (2 MB) |
| `CONFIG_BCM_PARAMS_PHYS` | `0x61000000` |
| `CONFIG_BCM_ZRELADDR` | **`0x61008000`** |
| `CONFIG_CMDLINE` | `console=ttyS0,9600n8 maxcpus=2 mem=2000M` |

The uImage header carries the same load and entry address,
`0x61008000`. That is **three independent confirmations** — the uImage
header, this config, and edgenos's `build-fit-4610.sh` — of the value
`build/mkimage.sh` bakes into the FIT. It is not a guess.

Two more things fall out of it:

- **`maxcpus=2 mem=2000M`** is the vendor's own command line, matching
  the commented-out `bootargs` in edgenos's device tree. If SMP bring-up
  misbehaves, `maxcpus=1` is a deviation from a known-good value, not a
  shot in the dark.
- **The built-in console default is 9600**, though
  `CONFIG_CMDLINE_FROM_BOOTLOADER=y` means U-Boot's `115200` wins in
  practice (and `/proc/cmdline` on the box confirms 115200). Edgecore
  evidently ships some units at 9600 — the same trap the E1031 sets — so
  if a future unit boots silently, try 9600 before suspecting the image.

What the config does **not** transfer is its driver selections. It is
`CONFIG_ARCH_IPROC` / `CONFIG_MACH_IPROC` — Broadcom's own 3.6.5 tree —
whereas the port targets ONL's `CONFIG_ARCH_XGS_IPROC` lineage on 6.1.
The symbol names differ and the two are not interchangeable; only the
memory map above is a durable, lineage-independent fact.

## Deliverables

- `platforms/accton-as4610-54/dts/` — the AS4610 board DTS (RTC node
  disabled) plus our trimmed `bcm-helix4.dtsi`, with the i2c compatible
  renamed per the hazard note. Committed; it is our data, not ONL's
  binary.
- `build/build-kernel-arm.sh` — fetches a 6.1 LTS tarball, applies the
  triaged series from `vendor/` (fetched, never committed), builds with
  the Hemlock config, and emits `linux-image-<ver>-hemlock-iproc*.deb`
  plus the matching headers package, which `build-bde-arm.sh` then
  builds BDE/KNET against.
- A Hemlock kernel config derived from ONL's `armhf-iproc-all` with the
  Z-bucket symbols off and everything trixie's systemd wants on
  (cgroup v2, `CONFIG_PSI`, `SECCOMP`, `FSCRYPT`, overlayfs, squashfs +
  xz — the last two are load-bearing for Hemlock's boot contract).
- The triaged patch series itself, as a `series` file + per-bucket
  patches under `vendor/kernel/as4610/`, generated by the fetch recipe.

## Sequencing against the phases

| Phase | Kernel work |
|---|---|
| 3 | Track 1: ONL 4.14 built as-is, minimal init, shim + BDE/KNET proven. Answer the bucket-E question. |
| 4 | Track 2: the 6.1 port, the kernel .deb, the DTS, `build-kernel-arm.sh`. **Critical path for the first bootable image.** |
| 5 | Regressions found on hardware; `dmasize=8M` and KNET MTU validation. |

## Risks

- **SMP bring-up** (`platsmp.c`) and the SoC's secondary-CPU protocol
  changed shape between 4.14 and 6.1 (`smp_operations`, CPU hotplug
  callbacks). This is the highest-risk 300 lines in bucket A. Falling
  back to `maxcpus=1` is an acceptable temporary unblock — the commented
  bootargs in edgenos's DTS already carry `maxcpus=2`, so the knob is
  understood.
- **`brcmnand` v6.1 controller revision** on a 6.1 kernel: upstream has
  moved substantially since 4.14. The 4-line delta suggests it will be
  fine, but NAND is where install failures are unrecoverable, so prove
  read/write from Track 1 before the installer depends on it.
- **The i2c compatible collision** above, if the rename is skipped.
- **`xgs_iproc_smbus`'s per-transfer `msleep(1)`** is the direct cause
  of the RTC/i2c-mux wedge edgenos documented. We disable the RTC in the
  DTS, which removes the trigger, but the sleep is still a latency floor
  on every muxed transceiver read. Measure it in Phase 5 before assuming
  pmon's scan interval is achievable.

## Open questions

1. Is the edgenos 6.1.175 iProc tree (or its patch series) available to
   reuse? It would collapse Track 2 into a rebase.
2. Bucket E: does `ma1` link with `MDIO_XGS_IPROC` and
   `XGS_IPROC_SERDES` disabled? (Answerable on Track 1.)
3. Does ONIE on this box expose the NAND as raw MTD, UBI volumes, or an
   `ubiblock`? This decides both the installer strategy and which NAND
   config symbols the kernel needs. (Shared with the main spec.)
