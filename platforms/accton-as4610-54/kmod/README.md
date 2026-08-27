# accton-as4610-54 platform kernel modules

Out-of-tree GPL driver sources for the Edgecore AS4610-54T, committed
here — unlike the vendor SAI/SDK blobs — because they needed real porting
to the image kernel and are now maintained with the platform. This is the
same arrangement as [`cel-e1031/kmod/`](../../cel-e1031/kmod/README.md);
`build/mkimage.sh` builds every subdirectory with a `Makefile` against the
image kernel and installs the results, and `build/kmod-smoke.sh
accton-as4610-54` compile-tests the same set in a container.

Porting *kernel-module C* is the one exception to this port's rule of
referencing vendor data and never carrying vendor code. Everything else
about the board — the port map, the CPLD register map, the i2c topology —
is data in `platform.toml`.

## Provenance

| Dir / file | Upstream | Notes |
|---|---|---|
| `accton/accton_as4610_cpld.c` | ONL, `packages/platforms/accton/armxx/arm-accton-as4610/onlp/builds/src/module/` | CPLD at i2c-0 0x30: SFP/QSFP presence, rx_los, tx_fault, product ID, CPLD version, a raw `access` poke. Exports the register accessors the other two call, and registers the `as4610_fan` / `as4610_led` platform devices |
| `accton/accton_as4610_fan.c` | same | 2 fans behind the CPLD: tach 0x2c/0x2d, one shared PWM nibble at 0x2b, fault bits in 0x11 |
| `accton/accton_as4610_psu.c` | same | 2 PSU bays: presence/power-good decoded from CPLD 0x11, model name and serial read from each bay's EEPROM |
| `accton/accton_as4610.h` | **new here** | prototypes for the four symbols the CPLD driver exports; see below |

ONL's `accton_as4610_leds.c` is deliberately not ported yet — nothing in
Hemlock drives front-panel LEDs from sysfs today. The CPLD driver still
registers the `as4610_led` platform device it would bind to; with no
driver present that device is inert, and keeping it means the LED driver
can be dropped in later without touching the CPLD port.

## Local port (Linux 6.1 and 6.12)

Two kernels matter and they differ in exactly the places these drivers
touch, so the compat is version-conditional rather than pinned:

- **6.1** is the AS4610 image kernel (`docs/as4610-kernel-port.md`).
- **6.12** is Debian trixie's, which is what `build/kmod-smoke.sh` has
  headers for.

Both are compile-tested warning-free. A `#error` asserts the 6.1 floor
rather than letting an older kernel fail somewhere less obvious.

- i2c `.probe` lost its `i2c_device_id` argument in 6.3, which also added
  `i2c_client_get_device_id()` to recover it, and `.probe_new` was renamed
  back to `.probe` in 6.6. Each driver keeps its real probe as
  `*_do_probe(client, id)`; the three-way `#if` picks how the core calls
  it. Both drivers need the id (the CPLD for its chip type, the PSU for
  its bay index), and both now tolerate a NULL one.
- i2c `.remove` returns `void` from 6.1 — unconditional, given the floor.
- platform `.remove` returns `void` from 6.11, so the fan driver's is
  split into a `void` body plus a thin `int`-returning wrapper below that.
- `-Wmissing-prototypes` fires on the CPLD driver's four exported
  symbols, which upstream declares `extern` in each consumer and defines
  with no prototype in scope. `accton_as4610.h` replaces those three
  copies with one declaration.
- The PSU driver's `.class = I2C_CLASS_HWMON` and `.address_list` are
  dropped. The i2c core reads both only from `i2c_detect()`, which needs
  a `.detect` callback this driver never had, so they were dead — and the
  address list disagreed with the board's device tree besides (0x50/0x53
  vs 0x50/0x51). Both clients are created from the manifest topology.

Not changed, and worth knowing when reading the diff: `sprintf` in sysfs
show functions (`sysfs_emit` is preferred now but the buffers are safe),
the module-global `fan_data` singleton, and the CPLD driver's client list
with its own mutex. Those are upstream's design, they work, and rewriting
them would make the port harder to diff against ONL.

## What the manifest expects of these

- The CPLD driver must load first: it exports `as4610_54_cpld_read` /
  `_write` and both other modules resolve against them. `required_modules`
  lists it first for that reason.
- `accton_as4610_psu` puts `psu_present` and `psu_power_good` on each PSU
  client's own sysfs dir, which is why `platform.toml` names them as
  *relative* attributes — pmon resolves them against the bus number the
  kernel actually assigned.
- `accton_as4610_fan` puts its attributes on the `as4610_fan` **platform**
  device (its hwmon node is empty), which no `<bus>-<addr>` identity can
  name. That is what the manifest's `hwmon = "platform:<name>"` form is
  for. It is not wired up yet: the CPLD driver registers that device only
  for product IDs 30P, 54P and 54T_B, so whether this board has fans at
  all is one register read away — gate 1 of `docs/as4610-bringup.md`.

When bumping either base kernel, re-run `build/kmod-smoke.sh
accton-as4610-54` and extend the compat above.
