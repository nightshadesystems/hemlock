# Vendor SAI artifacts (never committed)

This directory stages per-platform vendor artifacts for image builds:

- `libsaibcm_<pin>_amd64.deb` — the Broadcom SAI implementation
- `libsaibcm-dev_<pin>_amd64.deb` — its headers (reference/debug)
- `saibcm-modules/` — GPL kernel-module source (BDE/KNET), built for the
  image's kernel by `build/build-bde.sh`

**Everything is publicly downloadable** — run `vendor/fetch-vendor.sh
<platform-id>`. SAI .debs come from the SONiC public package server
(`packages.trafficmanager.net/public/sai/sai-broadcom/...`), platform data
files and the kmod source from `sonic-buildimage`.

Nothing here is required for development or CI: the workspace builds and
tests against `mock-sai` by default, and `--features real-sai` compiles
against the committed headers in `../sai-headers/`.

Pinning is per platform (`[sai] version_pin` in the manifest), never
global. For cel-e1031 the pin is `8.4.50.0` (SONiC 202305 XGS build,
SDK 6.5.27) — verified to retain the full Helix4 family.

`build/mkimage.sh` fails with a clear message if a required artifact is
absent.
