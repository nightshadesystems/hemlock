# accton-as4610-54 kernel pieces

The AS4610 boots a **mainline 6.1 LTS** kernel — not a forward-port of
ONL's 4.14 patch. `docs/as4610-kernel-port.md` ("Mainline 6.1 already
carries the SoC") is the verification: `ARCH_BCM_HR2` covers the
XGS-iProc family, and every peripheral this board needs has a mainline
driver. What mainline does not carry is *this SoC's device tree*, which
is what lives here.

## Contents

| File | What |
|---|---|
| `dts/bcm-hx4.dtsi` | The Helix4 SoC: A9 mpcore, UARTs, both SMBus controllers, QSPI (the 8 MB SPI-NOR), the AMAC pair, MDIO, EHCI, watchdog, and the CMICd node the BDE claims |
| `dts/arm-accton-as4610-54.dts` | The board: 2 GB at `0x60000000`, console `ttyS0`, the enabled subset, and the chain `"accton,as4610-54", "brcm,hx4", "brcm,hr2"` — the last entry is what lets a stock `ARCH_BCM_HR2` kernel boot it with **zero out-of-tree machine code** |
| `check-dts.sh` | cpp + dtc against mainline v6.1's dt-bindings; run after touching `dts/` |

## Provenance

Both DTS files are Hemlock's own, dual-licensed the kernel's usual way.
The *structure* follows mainline's `bcm-hr2.dtsi` (BSD); the Helix4
*facts* — addresses, interrupt numbers, clock rates — come from ONL's
`bcm-helix4.dtsi`/`bcm956340.dts` (read as data, per the port's rule)
and from the vendor kernel salvaged off this box (DRAM base, console).
Every node uses a mainline driver's compatible where one exists.

## Deliberate absences, each a recorded decision

- **No SMP enable-method** — whether `brcm,bcm-nsp-smp`'s boot register
  exists on Helix4 is a bench question; one core boots the box.
- **No USB PHY node** — U-Boot has already brought the port up to load
  the FIT, so the first experiment is bare `generic-ehci`. ONL's PHY
  trio (`brcm,usb-phy-hx4` + CCA GPIO + internal-MDIO tuning) gets
  ported kmod-style only if that fails. Boot-critical either way: the
  NOS storage is USB.
- **No SPI-NOR partitions** — the offsets must be copied from this
  box's `/proc/mtd`, because a guessed `uboot-env` offset means
  `fw_setenv` writing into U-Boot itself.
- **No i2c children** — pmon instantiates the topology from
  `platform.toml`; the RTC stays out (dead battery wedges the bus).
- **No NAND, no PCIe, no CCA GPIO** — absent hardware, unused, and
  USB-PHY-only respectively.

## Known follow-ups when this kernel first boots

1. `[[hardware.i2c.root]]` in `platform.toml`: mainline names the
   adapters `"Broadcom iProc (i2c@38000)"` / `"(i2c@3b000)"`, not
   ONIE's `iproc-smb0/1`.
2. Pin `phy-handle` on `gmac0` once `mdiobus_scan` reports the
   management PHY's address.
3. Confirm the two stand-in clock rates (uart 62.5 MHz, APB 100 MHz)
   and the full-2 GB memory node.
4. FIT packaging at load/entry `0x61008000` (`build/mkimage.sh`
   already expects `boot/hemlock.itb`).
