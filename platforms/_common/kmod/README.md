# Shared platform kernel modules

Out-of-tree GPL driver sources that **every** Hemlock platform needs, as
opposed to the board-specific ones under `platforms/<id>/kmod/`.
`build/mkimage.sh` and `build/kmod-smoke.sh` build both sets: this
directory first, then the platform's own.

The `_` prefix keeps this out of the platform enumeration — it has no
`platform.toml` and is not a board.

## Provenance

| Dir / file | Upstream | Notes |
|---|---|---|
| `optoe/optoe.c` | opencomputeproject/oom, `optoe/` | transceiver EEPROM access (built with `-DLATEST_KERNEL`) |

## Why optoe lives here

Any board with `[[hardware.transceiver]]` entries needs it, and every
board Hemlock will ever support has cages. It started under
`cel-e1031/kmod/` because that was the only platform; the AS4610 needed
the identical file, and a second copy of ~1000 lines of vendor source
that must be kept byte-identical is worse than one shared directory.

Add a driver here only when it is genuinely platform-independent. A
driver that happens to be shared by the two boards in the tree today,
but is really a vendor CPLD or a board's fan controller, belongs to the
platform.
