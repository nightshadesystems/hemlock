# Edgecore AS4610-54T (Accton AS4610-54)

48x1G copper + 4x10G SFP+ + 2x20G QSFP+ stacking. Broadcom BCM56340
(Helix4) with an **on-die iProc dual-core Cortex-A9** host CPU — no PCIe,
the CMIC is CMICd on the SoC bus. 2 GB DDR3, 8 MB SPI-NOR (U-Boot +
ONIE) and USB-attached NOS storage. ONIE machine string
`arm-accton_as4610_54-r0` — note the underscores; the hyphenated form in
edgenos's switchdb does not match this box.

This is Hemlock's first non-x86 board and its first non-SAI datapath.
The reasoning, the schema additions and the phase plan are in
[docs/as4610-54-port.md](../../docs/as4610-54-port.md); the kernel work
is in [docs/as4610-kernel-port.md](../../docs/as4610-kernel-port.md).

## Why this board does not use SAI

There is no `libsaibcm` for armhf. SONiC's `platform/broadcom/sai.mk`
publishes `libsaibcm_<ver>_amd64.deb` only — no architecture variable —
the SONiC package server returns 404 for `_armhf` and `_arm64`, and
`device/accton/` in sonic-buildimage has no `as4610` directory at all.
So the datapath is Hemlock's own C shim (`libhemlockbcm.so`) over the
source-available OpenBCM SDK, behind the same `SaiBackend` trait every
other platform uses. Nothing above `hemlock-sai` can tell the difference.

## Vendor files (not committed)

Run `vendor/fetch-vendor.sh accton-as4610-54` (added in phase 3):

| Artifact | Source |
|---|---|
| `as4610-54.config.bcm` | edgenos `platform/accton-as4610-54/config/config.bcm` — dumped from the stock Edgecore ICOS/FASTPATH NOS with `bcmsh config show`, so it is the board's authoritative bring-up property set |
| OpenBCM SDK | `Broadcom-Network-Switching-Software/OpenBCM`, the `sdk-6.5.16/` **directory** on `master` (there is no such branch or tag), sparse-checked out at a pinned commit |
| `bcm84758_ucode` | 10G SFP+ PHY microcode, a `request_firmware` blob |
| Helix4 LED program | OpenMDK `board/xgsled/` GE-family reference program |
| iProc kernel patches | ONL `packages/base/any/kernels/4.14-lts/patches/brcm-iproc-4.14.patch` + the `armhf-iproc-all` config |

## Faceplate → SDK port map

**This is the one input that cannot be re-fetched from a public repo.**
The edgenos tree references a `PORTMAP.md` / `as4610-54.portmap` that is
not published; the table below is the Cumulus Linux `porttab` for
`arm-accton_as4610_54-r0`, read off the hardware. The map is scrambled
and pair-swapped — copied verbatim into `platform.toml`, never derived.

```
# linux_intf  sdk_intf  unit  is_fabric  logical_port  is_loopback
swp1   ge25  0 0 -1 0    swp13  ge37  0 0 -1 0    swp25  ge1   0 0 -1 0    swp37  ge13  0 0 -1 0
swp2   ge24  0 0 -1 0    swp14  ge36  0 0 -1 0    swp26  ge0   0 0 -1 0    swp38  ge12  0 0 -1 0
swp3   ge26  0 0 -1 0    swp15  ge38  0 0 -1 0    swp27  ge2   0 0 -1 0    swp39  ge14  0 0 -1 0
swp4   ge27  0 0 -1 0    swp16  ge39  0 0 -1 0    swp28  ge3   0 0 -1 0    swp40  ge15  0 0 -1 0
swp5   ge29  0 0 -1 0    swp17  ge41  0 0 -1 0    swp29  ge5   0 0 -1 0    swp41  ge17  0 0 -1 0
swp6   ge28  0 0 -1 0    swp18  ge40  0 0 -1 0    swp30  ge4   0 0 -1 0    swp42  ge16  0 0 -1 0
swp7   ge30  0 0 -1 0    swp19  ge42  0 0 -1 0    swp31  ge6   0 0 -1 0    swp43  ge18  0 0 -1 0
swp8   ge31  0 0 -1 0    swp20  ge43  0 0 -1 0    swp32  ge7   0 0 -1 0    swp44  ge19  0 0 -1 0
swp9   ge33  0 0 -1 0    swp21  ge45  0 0 -1 0    swp33  ge9   0 0 -1 0    swp45  ge21  0 0 -1 0
swp10  ge32  0 0 -1 0    swp22  ge44  0 0 -1 0    swp34  ge8   0 0 -1 0    swp46  ge20  0 0 -1 0
swp11  ge34  0 0 -1 0    swp23  ge46  0 0 -1 0    swp35  ge10  0 0 -1 0    swp47  ge22  0 0 -1 0
swp12  ge35  0 0 -1 0    swp24  ge47  0 0 -1 0    swp36  ge11  0 0 -1 0    swp48  ge23  0 0 -1 0
swp49  xe0   0 0 -1 0    swp50  xe1   0 0 -1 0    swp51  xe2   0 0 -1 0    swp52  xe3   0 0 -1 0
```

`swpN` is Cumulus's interface naming; Hemlock calls the same port
`EthernetN` with alias `etpN`. Structure, for sanity checks only —
**never for deriving the table**: faceplate 1-24 sit on `ge24-47` and
25-48 on `ge0-23`, and within each block of four the first pair is
swapped, `(b+1, b, b+2, b+3)`.

`platform.toml`'s `lanes` carries the SDK **logical port number** and
`sdk_names` this table's right-hand column. From `config.bcm`'s bitmaps
and edgenos's `bcmd.c`: `pbmp_xport_ge = 0x3fffffffffffe` puts `geN` at
logical N+1, `pbmp_xport_xe = 0xfc000000000000` puts `xeN` at logical
50+N. syncd asserts each shim-reported name against `sdk_names` at
startup, so the +1 is documented but never relied on, and
`src/hemlock-platform/tests/shipped_platforms.rs` checks the whole table
against both facts on every build.

Not modeled: `ge48` (logical 49) is internal — `config.bcm` gives it
`port_phy_addr_ge48=0` — and the two QSFP+ stacking ports (`xe4`/`xe5`,
logical 54/55), which Cumulus does not expose either. syncd logs and
ignores ASIC ports the manifest does not claim.

## Hardware

I2C topology from edgenos's device tree
(`platform/accton-as4610-54/dts/arm-accton-as4610.dts`):

```
i2c-0 (SoC)  cpld @0x30                          (accton,as4610_54_cpld)
i2c-1 (SoC)  pca9548 @0x70 -> buses 2-9
               ch0-3  optoe2 @0x50   SFP+  Ethernet49-52
               ch4-5  optoe1 @0x50   QSFP+ (not modeled)
               ch6    psu1/2 eeprom @0x50/0x51, pmbus ym1921 @0x58/0x59
               ch7    lm77 @0x48, board eeprom 24c04 @0x50, RTC @0x68
```

Both roots are SoC controllers that register as `iproc-smbus`
(`xgs_iproc_smbus.c`), so the manifest tells them apart by `instance`
rather than by adapter name.

CPLD register map, from ONL's
`accton_as4610_{cpld,fan,psu}.c` and edgenos's `platform.py`:

| Register | Meaning |
|---|---|
| `0x11` | PSU status: present bit `i*2`, power-good bit `i*2+1` |
| `0x2b` | Fan PWM, low nibble; duty % = `(n*125 + 5) / 10` |
| `0x2c`, `0x2d` | Fan 2 / fan 1 tach; rpm = `raw * 379 * 60 / 2 / 100` |
| `0x07`, `0x08`, `0x0d`, `0x19`, `0x1b` | External-PHY reset deassert (see below) |

## Bring-up

The gate-by-gate runbook for taking this board from "builds" to
"forwards" is [docs/as4610-bringup.md](../../docs/as4610-bringup.md).
Start with its gate 1: a handful of read-only commands in ONIE that
close four of the port's open questions.

## Known quirks

- **The external PHYs come up held in reset.** Until the CPLD at i2c-0
  0x30 is written `0x07=0x02, 0x08=0x02, 0x0d=0x01, 0x19=0x00,
  0x1b=0x00`, the 54282 copper PHYs and the 84758 SFP+ PHYs stay down and
  the SDK's PHY probe finds nothing. The CMICd platform driver must also
  be unbound (`48000000.iproc_cmicd` from `iproc_cmic`) before the BDE
  can claim the device. Both live in the `as4610` quirks driver's
  `pre_asic_init`.
- **Fans and PSUs are not modeled yet.** Both are read through the CPLD,
  and pmon reads fans and PSU state from sysfs — so which attributes
  exist depends on an unresolved question in the port spec: port ONL's
  three kernel modules into `kmod/` (the `cel-e1031` `smc.c` precedent)
  or teach pmon to talk to the CPLD directly. Until that is settled the
  manifest declares the `lm77` sensor and no fans, which is true, rather
  than inventing attribute names. Note ONL's fan driver registers hwmon
  on a *platform* device with non-standard attribute names
  (`fan1_speed_rpm`, `fan_duty_cycle_percentage`), which the manifest's
  `hwmon = "<bus>-<addr>"` form cannot address.
- **The RTC is deliberately absent from the topology.** The M41T11 at
  i2c-1 mux ch7 0x68 has a dead battery, which makes the 4.19+ RTC core
  re-arm an already-expired alarm through a muxed i2c read on every pass,
  holding `i2c1`'s transfer lock until userspace wedges before
  sshd/getty. NTP sets the clock, so nothing is lost. (edgenos ships a
  `-rtcdis` device tree for the same reason.)

## Notes

- `dmasize=8M` is edgenos's value for this board, a quarter of the
  E1031's. Phase 5 validates the KNET MTU against it: the manifest asks
  for 9100 (E1031 parity) and 52 netdevs of 9238-byte rx buffers is the
  load. edgenos runs `default_mtu=1600`, but their own comment says that
  was to match their peer network, not a hardware limit.
- No `psample` or `linux-knet-cb` in `required_modules`: OpenBCM 6.5.16's
  `knet-cb.c` includes only `gmodule.h`, `kcom.h` and `bcm-knet.h`. The
  psample sFlow callback is a SONiC `saibcm-modules` addition, so the
  E1031's module list does not carry over — and listing `psample` would
  fail the image build's loadability gate for nothing.
- **Console:** unconfirmed. The device tree aliases `serial0 = &uart1`
  and its commented-out bootargs say `console=ttyS0,115200n8`, which is
  Hemlock's default — so there is no `boot.env` here yet. Confirm against
  ONIE's `installer.conf` in phase 5 and add one if it differs.
- **Management netdev:** the manifest says `eth0`. `ma1` is produced by
  ONL's udev rules, and Hemlock does not ship ONL userspace. Confirm in
  phase 5; a udev rename is the fallback if the kernel names it something
  else.
