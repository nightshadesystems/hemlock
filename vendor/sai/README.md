# Vendor SAI blobs (never committed)

This directory stages proprietary vendor artifacts for image builds:

- `libsaibcm_<pin>_<platform>.deb` — the Broadcom SAI implementation
- kernel BDE module packages matching the same SDK lineage

Nothing here is required for development or CI: the workspace builds and
tests against `mock-sai` by default, and `--features real-sai` compiles
against the committed headers in `../sai-headers/`.

## Acquisition

Run `./fetch-vendor.sh <platform-id>` (documents itself). Platform *data*
files (`config.bcm`, `.soc`) come from public sonic-buildimage; the
`libsaibcm` .deb must come from your own SONiC build artifacts or vendor
NDA channel — for cel-e1031 it must be the SONiC 202205/202211-era Helix4
build (see `platforms/cel-e1031/README.md`).

`build/mkimage.sh` fails with a clear message if a required blob is absent.
