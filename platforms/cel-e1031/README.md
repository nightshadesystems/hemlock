# Celestica E1031 (Haliburton)

48x1G + 4x10G access switch. Intel Atom C2000 (Rangeley) CPU, Broadcom
Helix4 (BCM56340 family) ASIC. ONIE machine string `x86_64-cel_e1031-r0`.

## Vendor files (not committed)

Two data files must sit in this directory for a real (non-mock) image build;
both exist in the public [sonic-buildimage](https://github.com/sonic-net/sonic-buildimage)
tree and are fetched by `vendor/fetch-vendor.sh cel-e1031`:

| File | Source (SONiC 202211 branch) |
|---|---|
| `helix4-e1031-48x1G+4x10G.config.bcm` | `device/celestica/x86_64-cel_e1031-r0/Celestica-E1031-T48S4/` |
| `sai_postinit_cmd.soc` | same directory |

## SAI blob

Helix4 support was dropped from newer Broadcom SAI releases; this platform
pins `libsaibcm 3.7.x-helix4` (SONiC 202205/202211-era, SDK 6.5.x lineage).
The `.deb` comes from the SONiC 202211 Broadcom build artifacts — see
[vendor/sai/README.md](../../vendor/sai/README.md) for acquisition options.
It is staged under `vendor/sai/` and bundled at image-build time; mock-sai
development and CI never need it.

## Notes

- The kernel BDE modules (`linux-kernel-bde`, `linux-user-bde`) must be
  built from the same SDK lineage as the pinned SAI.
- The 1G bank's serdes lanes are pair-swapped in hardware; the port table in
  `platform.toml` encodes this and must not be "simplified".
- Console: ttyS1 @ 9600 (ONIE `installer.conf`: `CONSOLE_PORT=0x2f8`).
