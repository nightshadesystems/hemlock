# cel-e1031 platform kernel modules

Out-of-tree GPL driver sources for the Celestica E1031 (haliburton),
committed here — unlike the vendor SAI blobs — because they needed real
porting to the image kernel and are now maintained with the platform.
`build/mkimage.sh` builds every subdirectory with a `Makefile` against the
image kernel and installs the results; `build/kmod-smoke.sh` compile-tests
the same set in a Debian trixie container.

## Provenance

| Dir / file | Upstream | Notes |
|---|---|---|
| `haliburton/smc.c`, `hlx_gpio_ich.c`, `emc2305.c`, `mc24lc64t.c` | sonic-buildimage `202305`, `platform/broadcom/sonic-platform-modules-cel/haliburton/modules` | E1031 CPLD (SFP presence, LEDs), Rangeley GPIO, fan controller, board EEPROM |
| `haliburton/dps200.c` | sonic-buildimage `201911` (dropped upstream after that branch) | DPS-200 PSU pmbus driver |
| `haliburton/pmbus.h` | Linux `v6.12`, `drivers/hwmon/pmbus/pmbus.h` | replaces the 2019 copy dps200.c shipped with |
| `optoe/optoe.c` | opencomputeproject/oom, `optoe/` | transceiver EEPROM access (built with `-DLATEST_KERNEL`) |
| `kernel-backports/i2c-mux-pca954x.c`, `max6697.c` | Linux `v6.12` (in-tree drivers) | Debian's amd64 kernel config leaves `CONFIG_I2C_MUX_PCA954x` and `CONFIG_SENSORS_MAX6697` unset, so the stock drivers the i2c topology needs are built out-of-tree, byte-identical to upstream |

## Local port (Debian 13 / kernel 6.12)

All files carry the same family of fixes for kernel API changes since
their upstream targets (≈4.9–6.1):

- i2c `probe` lost its `i2c_device_id` parameter; i2c and platform
  `remove` callbacks return `void`.
- `class_create()` lost its module parameter (6.4).
- `strlcpy` → `strscpy` (removed in 6.8).
- `i2c_new_dummy` → `*_device` variants, with `IS_ERR`-style checks
  (the `devm_` variants return error pointers, not NULL).
- dps200: `pmbus_do_probe(client, info)` (no id), no `pmbus_do_remove`
  (devres), `read_word_data` gained a `phase` parameter,
  `MODULE_IMPORT_NS(PMBUS)`.

When bumping the base kernel, re-run `build/kmod-smoke.sh` and extend the
ports here (and the BDE shims in `build/build-bde.sh`) as needed.
