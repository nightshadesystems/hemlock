# Hemlock architecture

Hemlock is a Rust network operating system for whitebox switches. It drives
Broadcom XGS ASICs exclusively through the vendor's SAI library — never the
raw Broadcom SDK, OpenNSL, or switchdev — on a Debian 13 base with systemd,
installed via per-platform ONIE images.

This document describes the phase-1 system: platform layer, SAI layer, the
four daemons, the configuration model, and the image/installer pipeline.

## Design principles

1. **Platform = data, not code.** A switch model is one directory under
   `platforms/` holding a `platform.toml` manifest plus vendor data files.
   Boards built from existing driver primitives require zero Rust changes.
2. **`hemlock-syncd` is platform-agnostic.** It receives a `libsai.so` path
   and a `config.bcm` path resolved from the manifest and knows nothing else
   about the board.
3. **SAI version pinning is per-platform, never global.** Each manifest pins
   its vendor SAI package (`sai.version_pin`) and the matching API header
   set (`sai.api_headers`); the build pipeline bundles the right blob and
   compiles syncd against the right headers per platform image. Real
   fleets straddle SAI eras, so nothing about a SAI version is ever a
   workspace-wide decision. (The E1031 currently rides `libsaibcm
   8.4.50.0` / SAI API v1.11.0 — inspected to still carry Helix4.)
4. **Rust everywhere; unsafe nowhere but the FFI.** `unsafe` is denied
   workspace-wide and allowed only inside `hemlock-sai`'s FFI modules.
   Everything above the FFI boundary builds and tests against a pure-Rust
   mock — CI needs no hardware and no vendor blobs.

## Process topology

```
                                  ┌──────────────┐
             unix:/run/hemlock/   │  hemlockctl  │  operator CLI
            ┌─────────────────────┴──────┬───────┘
            │            │               │
            ▼            ▼               ▼
      ┌───────────┐ ┌──────────┐  ┌─────────────┐     ┌─────────────┐
      │hemlock-   │ │ hemlock- │  │ hemlock-    │     │ hemlock-orch│
      │mgmtd      │ │ pmon     │  │ syncd       │◄────┤ (phase-1    │
      │commit     │ │ fans/    │  │ owns the    │     │  stub)      │
      │engine     │ │ thermal/ │  │ ASIC via    │     └─────────────┘
      └─────┬─────┘ │ SFP/PSU  │  │ SaiBackend  │
            │       └────┬─────┘  └──────┬──────┘
            │ gRPC       │ sysfs/i2c     │ dlopen
            └───────────►│               ▼
              (apply     │        ┌─────────────┐
               intents)  ▼        │ libsai.so   │ vendor blob, pinned
                     hardware     │ (or MockSai)│ per platform
                                  └─────────────┘
```

- All IPC is tonic gRPC. Production endpoints are unix domain sockets under
  `/run/hemlock/`; every daemon also accepts `tcp:host:port` endpoints,
  which is what non-unix development hosts and portable integration tests
  use (`hemlock_common::ipc::IpcEndpoint`).
- Proto definitions live in `src/hemlock-common/proto/hemlock/v1/` — one
  service per daemon (`Syncd`, `Pmon`, `Mgmt`, `Orch`) plus shared enums.
  Generated types are re-exported as `hemlock_common::proto::v1`.
- `hemlockctl` talks to whichever daemon owns the state it needs: syncd for
  interfaces, pmon for environment, mgmtd for the config lifecycle.

## Crate map

| Crate | Kind | Purpose |
|---|---|---|
| `hemlock-common` | lib | Error conventions, tracing init, IPC endpoints, generated gRPC types |
| `hemlock-platform` | lib | Manifest schema, loader, port-table expansion, lint, `PlatformQuirks` |
| `hemlock-sai` | lib | `SaiBackend` trait; `mock-sai` (default) and `real-sai` (bindgen + dlopen) backends |
| `hemlock-syncd` | bin | The only ASIC owner: switch create, port bring-up, port state gRPC |
| `hemlock-pmon` | bin | Manifest-driven environment monitoring + fan control |
| `hemlock-mgmtd` | bin | Candidate/running, commit, commit-confirm, rollback ring |
| `hemlock-orch` | bin | Phase-1 stub; future netlink/FRR → SAI orchestration |
| `hemlock-config` | lib | Curly-brace config language: lexer, parser, tree |
| `hemlockctl` | bin | Operator CLI |
| `hemlock-installer` | bin | ONIE installer: machine verification, disk TUI, GRUB |

## The platform layer

`platform.toml` (schema v1) declares everything board-specific:

- **`[platform]`** — identity: `id`, `onie_machine` (matched against ONIE's
  `machine.conf` at install time), vendor/model/ASIC strings.
- **`[sai]`** — the vendor package name, the abstract `version_pin`, the
  in-image `libsai_path`, and the `config_bcm` + `extra_files` data files
  (relative to the platform dir; never committed).
- **`[kernel]`** — modules that must be loaded before syncd (the Broadcom
  BDE pair must match the pinned SAI's SDK ABI, which is a second reason
  images are assembled per platform).
- **`[ports]`** — the port table. Regular runs of ports are declared as
  `[[ports.group]]` (prefix, name/index start, speed, and an explicit flat
  lane list — explicit because boards really do swap lanes in hardware, as
  the E1031's pair-swapped 1G bank shows); oddballs use `[[ports.port]]`.
  Groups expand to a flat, index-sorted `Vec<PortDef>` at load time.
- **`[hardware]`** — i2c mux/device topology, thermal sensors, fans, the
  fan curve, PSUs, and per-port transceiver EEPROM buses. This is the
  entirety of pmon's board knowledge.
- **`[hardware.quirks]`** — names a registered `PlatformQuirks` impl.
  `generic` (the default) does nothing; boards with CPLD reset/LED behavior
  that cannot be data register a named impl in `hemlock_platform::quirks`
  and get hooks at syncd/pmon lifecycle points (`pre_asic_init`,
  `post_asic_init`, `post_hw_init`). If several boards need the same hook,
  promote it to manifest data instead.

`hemlockctl platform lint <dir>` loads and validates a manifest: structural
errors (duplicate lanes, bad fan curves, dangling references, unknown quirks
drivers) fail; absent vendor data files only warn, so a fresh checkout lints
clean by design.

## The SAI layer

Everything above `hemlock-sai` sees one safe trait:

```rust
pub trait SaiBackend: Send {
    fn name(&self) -> String;
    fn create_switch(&mut self) -> Result<SwitchInfo, SaiError>;
    fn ports(&mut self) -> Result<Vec<SaiPort>, SaiError>;
    fn set_port_admin_state(&mut self, port: PortId, up: bool) -> Result<(), SaiError>;
    fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<SaiEvent>>;
}
```

Phase 1 deliberately exposes only: switch create, port enumeration, admin
state, and oper-status notifications. New object families (VLAN, LAG, L3)
extend this trait in later phases.

**Mock backend** (`mock-sai`, default feature): pure Rust, constructed from
the platform port table. Links follow admin state and emit the same
notifications the vendor library would, so syncd/mgmtd behavior — including
commit-confirm auto-rollback — is exercised end-to-end without hardware.

**Vendor backend** (`real-sai` feature): bindgen over SAI headers vendored
under `vendor/sai-headers/<version>/` (extracted from the pinned build's
own `-dev` package, so they are ABI-exact), plus `libloading` to dlopen
the manifest's `libsai_path` at runtime. The header *API* version is a
build-time selection (`HEMLOCK_SAI_HEADERS`, default `v1.11.0`); platforms
pinning a different SAI add a new header directory rather than upgrading
an existing one. Bindings always generate for `x86_64-unknown-linux-gnu`
so output is identical on every build host. The SAI profile
(`SAI_INIT_CONFIG_FILE` → config.bcm) is served through the standard
profile callbacks; port oper-status callbacks are forwarded into a tokio
channel. Building `real-sai` needs libclang (CI compiles it on every push
to keep the FFI honest); running it needs Linux plus the vendor blob.

## hemlock-syncd

Startup sequence:

1. Load the platform manifest; construct the backend (`--mock` or vendor).
2. Run `quirks.pre_asic_init`.
3. On a dedicated **SAI actor thread** (SAI calls are blocking C calls;
   the vendor library gets the single-threaded access pattern it expects):
   `create_switch`, enumerate ASIC ports, and **correlate them with the
   manifest port table by lane set** — SAI creates ports from config.bcm in
   its own order, so lane sets are the only stable join key. Manifest ports
   with no ASIC match are a fatal error; ASIC ports not in the manifest
   (internal/backplane links) are logged and left untouched.
4. Bring every mapped port to its default admin state (phase 1: up).
5. Run `quirks.post_asic_init`; serve gRPC.

At runtime the async side sends commands to the actor over a channel;
oper-status events flow out through a broadcast channel that updates the
shared port table and feeds `WatchPortEvents` streams.

## hemlock-pmon

A poll/control loop at the fan-curve interval reads every manifest sensor,
drives all fans to the PWM given by linear interpolation over the curve
(below the first point: its PWM; above the last: 100%), and refreshes fan
tach + PSU status. A slower loop scans transceiver EEPROMs (SFF-8472
identity fields). All hardware access goes through the `HwBackend` trait:
`sysfs` for real hwmon/i2c paths, `mock` for CI and development.

## Configuration model

The config language is the Nightshade-style curly-brace format
(`hemlock-config`: hand-rolled lexer + recursive-descent parser, canonical
serializer, `parse(to_text(t)) == t`):

```text
interfaces {
    ethernet Ethernet0 {
        description "uplink to core-1";
        admin-state disabled;
    }
}
```

`hemlock-mgmtd` owns the lifecycle. On disk (default
`/var/lib/hemlock/config`): `running.conf`, `candidate.conf`, and a
50-deep rollback ring (`rollback/N.conf` + JSON metadata), rotated on every
commit.

- **Load/validate** — `hemlockctl load file` parses the text, extracts
  intents, and cross-checks interface names against syncd before accepting
  the candidate.
- **Commit** — diff the candidate's *intents* against running's, push only
  the changes to syncd, then rotate the ring and promote the candidate.
  Phase 1 has one intent family (interface admin-state + description);
  each future family is a pure function from config tree to typed intents
  plus an apply step, slotting into `intents.rs` without touching the
  lifecycle machinery. An interface removed from the config reverts to
  defaults (admin up, no description).
- **Commit-confirm** — `commit --confirm N` arms a timer holding the
  pre-commit running text; `hemlockctl confirm` disarms it, expiry
  re-applies the old config automatically.
- **Rollback** — `rollback N` loads ring entry N into the candidate and
  commits it (the underlying RPCs are separate, so a future interactive
  mode can load-then-inspect).

## Image and installer

`build/mkimage.sh <platform>` produces `hemlock-<version>-<platform>.bin`:

1. debootstrap a Debian 13 rootfs (`build/rootfs/packages.list`), install
   the Hemlock daemons + systemd units, and the platform's pinned vendor
   SAI `.deb` from `vendor/sai/` (hard failure with pointers if absent);
2. squashfs the rootfs;
3. assemble the payload: squashfs + platform overlay (`platform.toml`,
   config.bcm, identity markers) + boot assets (GRUB config rendered with
   the platform's serial console parameters from `boot.env`) + the
   `hemlock-installer` binary;
4. wrap it all in a `#!/bin/sh` self-extractor — the format ONIE executes.

`--dummy-rootfs` swaps step 1 for a stub so CI can build and structurally
verify the image (`build/verify-image.sh`) with no root, no blobs.

The installer (ratatui TUI, or `--non-interactive --disk /dev/sdX`) refuses
to install unless ONIE's `machine.conf` platform matches the image's
`onie_machine` (override: `--force`), then partitions (GPT: BIOS boot, ESP,
root), copies the image + platform overlay, and installs GRUB. `--dry-run`
prints every command instead of running it.

## Phase-1 boundaries and seams

Explicitly out of scope in phase 1: FRR integration, L3/routing
orchestration, VLANs/LAG, EVPN, licensing, telemetry streaming. The seams
they will land on already exist:

- `hemlock-orch` runs and serves a stub `Orch` service; routing state will
  flow netlink/FRR → orch → syncd gRPC.
- New SAI object families extend `SaiBackend` + the mock in lockstep.
- New config families add an intent extractor + apply step in mgmtd.
- Telemetry can subscribe to syncd's event broadcast and pmon's state.
