# Celestica E1031 (Haliburton)

48x1G + 4x10G access switch. Intel Atom C2000 (Rangeley) CPU, Broadcom
Helix4 (BCM56340 family) ASIC. ONIE machine string `x86_64-cel_e1031-r0`.

## Vendor files (not committed)

Everything a real image build needs is publicly downloadable — run
`vendor/fetch-vendor.sh cel-e1031`:

| Artifact | Source |
|---|---|
| `helix4-e1031-48x1G+4x10G.config.bcm`, `sai_postinit_cmd.soc` | sonic-buildimage `202305`, `device/celestica/x86_64-cel_e1031-r0/Celestica-E1031-T48S4/` |
| `libsaibcm_8.4.50.0_amd64.deb` (+`-dev`) | SONiC public package server, `SAI_8.4.0_GA/8.4.50.0/xgs` |
| `saibcm-modules/` kmod source | sonic-buildimage `202305`, `platform/broadcom/saibcm-modules` |

## SAI pin

`libsaibcm 8.4.50.0` — the SONiC 202305 Broadcom XGS build, SDK 6.5.27,
SAI API v1.11.0 (headers vendored at `vendor/sai-headers/v1.11.0/`).
Inspected and confirmed to retain the full Helix4 family
(BCM56340/42/44/45/46) and to ship the E1031's own config
(`etc/bcm/hx4-cel-hbtn-48x1G+4x10G.config.bcm`), so no legacy-era pin is
needed. mock-sai development and CI never need any of this.

## Known quirks

- **Per-port SFP+ LEDs (Ethernet49-52) are not achievable on this
  hardware.** Bench-proven (`docs/e1031-led-bringup.md`): the Helix4
  LED-processor scan chains are unwired, and the SMC CPLD (fw v5) can
  only force all four green, blink all four (lamp test), or leave them
  off — no per-port source exists, which is why SONiC never shipped LED
  support either. Hemlock runs the CPLD in normal mode (`haliburton`
  quirks driver): SFP+ LEDs dark, system + fan LEDs driven truthfully.
  Copper-port LEDs are PHY-driven and work.

## Notes

- The kernel BDE modules (`linux-kernel-bde`, `linux-user-bde`) are built
  from the fetched `saibcm-modules` source (same SDK lineage as the pinned
  SAI) against the image's own kernel — `build/mkimage.sh` does this in
  the chroot via `build/build-bde.sh`.
- The 1G bank's serdes lanes are pair-swapped in hardware; the port table in
  `platform.toml` encodes this and must not be "simplified".
- Console: ttyS1 @ 9600 (ONIE `installer.conf`: `CONSOLE_PORT=0x2f8`).
