# Porting guide: bringing up a new platform

Adding a switch model to Hemlock means writing **data, not code**: one
directory under `platforms/` with a `platform.toml` manifest and (at build
time) the board's vendor data files. This guide walks the whole path using a
hypothetical board — the "ACME SW48", a 48x1G + 4x10G Trident-era box with
ONIE machine string `x86_64-acme_sw48-r0` — starting from
[`platforms/_template/`](../platforms/_template/platform.toml).

If your board needs a driver primitive Hemlock doesn't have yet (a new mux
chip, a CPLD-only reset dance), you'll additionally touch the small
extension points listed in [When data isn't enough](#when-data-isnt-enough)
— still no changes to the daemons themselves.

## 0. Gather the facts

Before editing anything, collect:

| Fact | Where it usually comes from |
|---|---|
| ONIE machine string | boot the box into ONIE: `cat /etc/machine.conf` (`onie_platform=...`) |
| Port map: lanes, speeds, aliases | vendor port map, or SONiC `device/<vendor>/<platform>/<HWSKU>/port_config.ini` |
| SAI package + era | which `libsaibcm` build supports this ASIC (check SONiC branch for the platform) |
| config.bcm + SOC scripts | SONiC hwsku directory (`*.config.bcm`, `sai_postinit_cmd.soc`, LED microcode) |
| i2c topology | SONiC `platform/broadcom/sonic-platform-modules-<vendor>/<board>/` init script — the sequence of `new_device` writes *is* the topology |
| Sensors, thresholds, fan curve | SONiC `sensors.conf`, `fancontrol-*`, `thermal_policy.json` |
| Serial console | ONIE `installer.conf` (`CONSOLE_DEV`, `CONSOLE_SPEED`) |

When SONiC is the reference, reference its **data**. Do not port its C++.

## 1. Create the platform directory

```console
$ cp -r platforms/_template platforms/acme-sw48
$ rm platforms/acme-sw48/platform.toml   # keep it, actually — edit in place
```

Keep `hemlockctl platform lint platforms/acme-sw48` running after every
section below; it is the fastest feedback loop you have. A fresh port lints
with *warnings* about absent vendor files — that's expected. *Errors* mean
the manifest is wrong.

## 2. Identity: `[platform]`

```toml
[platform]
id = "acme-sw48"                      # must equal the directory name
onie_machine = "x86_64-acme_sw48-r0"  # exactly what machine.conf reports
vendor = "ACME"
model = "SW48"
asic_family = "broadcom-xgs"
asic = "trident3"
```

`onie_machine` is load-bearing: the installer refuses to install this
platform's image onto a machine reporting anything else (unless the
operator passes `--force`).

## 3. SAI pinning: `[sai]` and `[kernel]`

```toml
[sai]
package = "libsaibcm"
version_pin = "8.x-td3"               # abstract pin, per platform
libsai_path = "/usr/lib/libsai.so.1"
config_bcm = "td3-acme-sw48.config.bcm"
extra_files = ["sai_postinit_cmd.soc"]

[kernel]
required_modules = ["linux-kernel-bde", "linux-user-bde"]
```

Rules that keep multi-platform fleets sane:

- **The pin is per platform.** Never "upgrade the SAI" globally; a new
  platform with a newer SAI simply pins it, and older boards keep theirs.
  (This is why the E1031 can stay on its 202211-era Helix4 build forever
  while a Questone 2A ships something current.)
- The BDE kernel modules in `required_modules` must come from the same SDK
  lineage as the pinned SAI — bundle them per image, never share.
- If the newer SAI's *API* differs enough that the vendored headers won't
  do, vendor a second header set under `vendor/sai-headers/<version>/` and
  build that platform's image with `HEMLOCK_SAI_HEADERS=<version>`. Do not
  upgrade an existing header directory in place.
- Vendor files (`config.bcm`, `.soc`) live next to `platform.toml` but are
  **gitignored**; add a fetch recipe for your platform to
  `vendor/fetch-vendor.sh` (public sonic-buildimage data can be downloaded;
  the `libsaibcm` .deb is documented in `vendor/sai/README.md`).

## 4. The port table: `[ports]`

Translate the port map into groups. From a SONiC `port_config.ini`:

```text
# name         lanes    index  speed   alias
Ethernet0      1        1      1000    etp1
Ethernet1      2        2      1000    etp2
...
Ethernet48     53       49     10000   etp49
```

becomes:

```toml
[[ports.group]]
prefix = "Ethernet"
name_start = 0        # Ethernet0, Ethernet1, ...
index_start = 1       # front-panel 1..48
speed_mbps = 1000
autoneg = true
alias_prefix = "etp"
lanes = [1, 2, 3, /* ... every lane, in port order ... */ 48]

[[ports.group]]
prefix = "Ethernet"
name_start = 48
index_start = 49
speed_mbps = 10000
alias_prefix = "etp"
lanes = [53, 54, 55, 56]
```

Hard-won rules:

- **Copy the lane list verbatim; never "simplify" it into a formula.**
  Boards swap serdes lanes in hardware — the E1031's entire 1G bank is
  pair-swapped (2, 1, 4, 3, ...). The lane set is also how syncd matches
  manifest ports to the ports SAI creates from config.bcm, so a wrong lane
  list is a startup failure, not a cosmetic bug.
- Multi-lane ports set `lanes_per_port` (e.g. 4 for 40G/100G) and list
  lanes flat; the loader chunks them. Declare breakout capability with
  `breakout = ["4x10G"]` (consumed in a later phase, but declare it now).
- Ports in config.bcm that are *not* front-panel (internal/backplane
  links) are simply omitted; syncd logs and ignores unmatched ASIC ports.
- One-off oddballs get an explicit `[[ports.port]]` entry.

Sanity-check the expansion:

```console
$ cargo run -p hemlockctl -- platform lint platforms/acme-sw48
acme-sw48: ok (52 ports, 2 warnings)
```

## 5. Hardware: `[hardware.*]`

Transcribe the platform init script's i2c setup into data, in
instantiation order. Each `echo <driver> <addr> > .../i2c-<N>/new_device`
line becomes a mux or device entry:

```toml
[hardware.i2c]
root_adapter = "SMBus iSMT adapter"   # matched by name; bus number varies

[[hardware.i2c.mux]]
name = "main-mux"
driver = "pca9548"
parent_bus = "root"
address = 0x70
child_bus_base = 2                    # first bus its channels claim
channels = 8

[[hardware.i2c.device]]
driver = "24lc64t"
bus = 2
address = 0x50
purpose = "syseeprom"
```

Then thermal (names are yours; `hwmon` is the `<bus>-<addr>` sysfs
identity, `input` the channel), fans (note tach/PWM channels are often not
in chassis-label order), the fan curve (from the SONiC `fancontrol` file:
`MINTEMP`→`MINPWM`, `MAXTEMP`→`MAXPWM`, PWM converted from 0–255 to
percent), PSUs, and one `[[hardware.transceiver]]` per pluggable cage
mapping port name → EEPROM bus (`optoe2` for SFP, `optoe1` for QSFP).

Lint cross-checks all of it: curve sensors must exist, transceiver ports
must be real ports, mux child-bus ranges must not overlap.

## 6. Quirks: only if the manifest can't say it

```toml
[hardware.quirks]
driver = "generic"
```

`generic` is correct for most boards. Reach for a named quirk only when
behavior genuinely can't be data — e.g. a CPLD register poke to release the
ASIC from reset before SAI init. Then:

1. Add an impl in `src/hemlock-platform/src/quirks.rs` overriding the
   hooks you need (`pre_asic_init`, `post_asic_init`, `post_hw_init`);
   hooks must be idempotent.
2. Register its name in `by_name()` / `known_names()`.
3. Set `driver = "acme-sw48"` in the manifest.

If you find yourself writing the same quirk for a second board, stop and
promote it to manifest data instead.

## 7. Bring it up against the mock

No hardware needed for any of this:

```console
$ cargo run -p hemlock-syncd -- --platform acme-sw48 --mock \
      --listen tcp:127.0.0.1:60071
$ cargo run -p hemlockctl -- --syncd tcp:127.0.0.1:60071 show interfaces
```

All 52 ports should enumerate with the right names, indexes, and speeds.
Then run mgmtd against it and exercise a config commit + rollback — this
validates the port names you'll be typing in configs forever, so look at
them carefully now.

## 8. Console and image

If the box's serial console isn't ttyS0 @ 115200, add
`platforms/acme-sw48/boot.env` (from ONIE's `installer.conf`):

```sh
CONSOLE_DEV=1
CONSOLE_SPEED=9600
```

Structural check first (no root, no blobs):

```console
$ build/mkimage.sh acme-sw48 --dummy-rootfs
$ build/verify-image.sh build/out/hemlock-*-acme-sw48.bin
```

Real image, on a Debian host with debootstrap/squashfs-tools and the
vendor blobs staged:

```console
$ vendor/fetch-vendor.sh acme-sw48          # platform data files
$ cp .../libsaibcm_8.x-td3_amd64.deb vendor/sai/
$ build/mkimage.sh acme-sw48 --version 0.1.0
```

## 9. Install on the box

From ONIE's install environment:

```console
ONIE:/ # onie-nos-install http://server/hemlock-0.1.0-acme-sw48.bin
```

The installer verifies `machine.conf` (a mismatched image refuses without
`--force`), walks you through disk selection on the serial console, and
installs GRUB + the platform overlay. First boot: check `hemlockctl show
switch`, `show interfaces`, `show environment` — if the port table or i2c
topology is wrong, this is where it shows.

## Checklist

- [ ] `platforms/<id>/platform.toml` — every section filled from real data
- [ ] `hemlockctl platform lint platforms/<id>` passes (warnings about
      vendor files only)
- [ ] Lane lists copied verbatim from the vendor/SONiC port map
- [ ] SAI pin chosen per platform; header set selected if the API era differs
- [ ] Fetch recipe added to `vendor/fetch-vendor.sh`
- [ ] `boot.env` if the console isn't ttyS0/115200
- [ ] Mock bring-up: 52 (or N) ports listed correctly; commit + rollback work
- [ ] `mkimage.sh <id> --dummy-rootfs` + `verify-image.sh` pass
- [ ] README.md in the platform dir: where the blobs come from
- [ ] Real image installs on hardware; `show environment` readings sane

## When data isn't enough

| You need | Touch this | Scope |
|---|---|---|
| Board-specific CPLD/LED behavior | `hemlock-platform::quirks` | one impl + registry entry |
| A newer SAI API era | `vendor/sai-headers/<ver>/` + `HEMLOCK_SAI_HEADERS` | build-time only |
| A new manifest field | `hemlock-platform::schema` + lint + this guide | schema review |
| A new SAI object family | `SaiBackend` trait + mock + vendor impl | design review |
