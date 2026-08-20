# Hemlock

Hemlock is a Rust-based network operating system for whitebox Ethernet
switches, built by [Nightshade Systems](https://github.com/nightshadesystems).
It is a sibling project to Nightshade (a Debian-based firewall OS) and shares
its operational model: a curly-brace hierarchical configuration with
candidate/running separation, `commit`, `commit confirm`, and `rollback`.

Hemlock drives Broadcom XGS switch ASICs exclusively through the vendor's
**SAI** (Switch Abstraction Interface) library — never the raw Broadcom SDK,
OpenNSL, or switchdev. Base OS is Debian 13 (trixie) with systemd, installed
via ONIE self-extracting images built per platform.

## Design principles

- **Platform = data, not code.** A switch model is a directory under
  [platforms/](platforms/) with a `platform.toml` manifest and vendor data
  files. Boards using existing driver primitives need zero Rust changes.
- **`hemlock-syncd` is platform-agnostic.** It receives a `libsai.so` path
  and a `config.bcm` path resolved from the manifest and knows nothing else
  about the board.
- **SAI version pinning is per-platform, never global.** Each manifest pins
  its SAI package; the build pipeline bundles the right vendor blob per image.
  (The first target, Celestica E1031 / Helix4, needs an older pinned SAI —
  newer releases dropped Helix4.)
- **Rust everywhere** except unavoidable vendor C libraries, which sit behind
  bindgen FFI and a safe wrapper in `hemlock-sai`. Everything above the FFI
  boundary builds and tests with a pure-Rust mock — CI needs no hardware and
  no vendor blobs.

## Repository layout

| Path | Contents |
|---|---|
| [src/](src/) | The Rust workspace: daemons (`syncd`, `orch`, `pmon`, `mgmtd`), libraries (`common`, `platform`, `sai`, `config`), the `hemlockctl` CLI, and the ONIE installer |
| [platforms/](platforms/) | Per-platform manifests + data; start at [platforms/_template/](platforms/_template/) |
| [vendor/](vendor/) | Pinned OCP SAI headers (committed) and vendor blob staging (never committed) |
| [build/](build/) | Rootfs package set, systemd units, ONIE image assembly |
| [docs/](docs/) | [Architecture](docs/architecture.md) and the [porting guide](docs/porting-guide.md) |

## Building

```console
$ cargo build --workspace          # mock-sai by default; no blobs needed
$ cargo test --workspace
$ cargo run -p hemlockctl -- platform lint platforms/cel-e1031
```

## Hardware targets

| Platform | ASIC | Status |
|---|---|---|
| Celestica E1031 (Seastone) | Broadcom Helix4, 48x1G + 4x10G | phase 1 bring-up |
| Celestica Questone 2A | Broadcom Trident3 | planned |

## License

MIT — see [LICENSE](LICENSE). Vendor SAI libraries and ASIC data files are
proprietary and are never distributed with this repository.
