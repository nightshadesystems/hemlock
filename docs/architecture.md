# Hemlock architecture

Hemlock is a Rust network operating system for whitebox switches. It drives
Broadcom XGS ASICs exclusively through the vendor's SAI library — never the
raw Broadcom SDK, OpenNSL, or switchdev — on a Debian 13 base with systemd,
installed via per-platform ONIE images.

This document describes the system: platform layer, SAI layer, the
daemons, the configuration model, and the image/installer pipeline.

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
      ┌───────────┐ ┌──────────┐  ┌─────────────┐      ┌─────────────┐
      │hemlock-   │ │ hemlock- │  │ hemlock-    │◄─────┤ hemlock-orch│
      │mgmtd      │ │ pmon     │  │ syncd       │      │ LACP / STP /│
      │commit     │ │ fans/    │  │ owns the    │      │ IGMP-MLD    │
      │engine     │ │ thermal/ │  │ ASIC via    │      │ snooping    │
      └─────┬─────┘ │ SFP/PSU  │  │ SaiBackend  │      │ engines     │
            │       └────┬─────┘  └──────┬──────┘      └──────▲──────┘
            │ gRPC       │ sysfs/i2c     │ dlopen             │
            └───────────►│               ▼        mgmtd pushes protocol
              (apply     │        ┌─────────────┐ config to orch; orch
               intents)  ▼        │ libsai.so   │ programs LAG gates /
                     hardware     │ (or MockSai)│ STP states / L2MC
                                  └─────────────┘ into syncd
```

- All IPC is tonic gRPC. Production endpoints are unix domain sockets under
  `/run/hemlock/`; every daemon also accepts `tcp:host:port` endpoints,
  which is what non-unix development hosts and portable integration tests
  use (`hemlock_common::ipc::IpcEndpoint`).
- Proto definitions live in `src/hemlock-common/proto/hemlock/v1/` — one
  service per daemon (`Syncd`, `Pmon`, `Mgmt`, `Orch`) plus shared enums.
  Generated types are re-exported as `hemlock_common::proto::v1`.
- `hemlockctl` talks to whichever daemon owns the state it needs: syncd for
  interfaces/VLANs/FDB, orch for protocol state (LACP, spanning tree,
  snooping), pmon for environment, mgmtd for the config lifecycle.

## Crate map

| Crate | Kind | Purpose |
|---|---|---|
| `hemlock-common` | lib | Error conventions, tracing init, IPC endpoints, generated gRPC types |
| `hemlock-platform` | lib | Manifest schema, loader, port-table expansion, lint, `PlatformQuirks` |
| `hemlock-sai` | lib | `SaiBackend` trait; `mock-sai` (default) and `real-sai` (bindgen + dlopen) backends |
| `hemlock-syncd` | bin | The only ASIC owner: switch create, port bring-up, port state gRPC |
| `hemlock-pmon` | bin | Manifest-driven environment monitoring + fan control |
| `hemlock-mgmtd` | bin | Candidate/running, commit, commit-confirm, rollback ring |
| `hemlock-orch` | bin | Protocol engines: LACP, spanning tree, IGMP/MLD snooping; packet I/O; the kernel-RIB → syncd FIB pipeline; FRR state queries |
| `hemlock-webd` | bin | Web console: serves the exported Next.js UI (`web/`) + JSON API over HTTP/HTTPS; config-driven via `set system http`/`https` |
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

Phase 1 exposed only switch create, port enumeration, admin state, and
oper-status notifications. The switching suite grew the trait — always
mock and vendor in lockstep — with VLAN/switchport programming, the FDB
(aging, static entries incl. drop, flush, and `SaiEvent::Fdb`
learn/age/move notifications), storm-control policers, mirror sessions,
LAGs (a LAG is created *as a `PortId`*, so VLAN membership, PVID, FDB and
storm calls take LAG ids transparently), STP instances + per-port STP
state, and L2MC groups for snooping-constrained multicast.

The vendor backend also answers a **capability probe**
(`SaiBackend::capabilities`, via `sai_query_attribute_capability`): which
of LAG, STP, FDB flush/aging, L2MC, storm policers, mirroring, and
outer-TPID rewrite this platform's SAI actually implements. syncd runs
the probe once at startup and every RPC that needs a missing capability
fails cleanly with `% <feature> is not supported by this platform's SAI`
— configuration is never silently dropped on the floor.

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
5. Run the SAI **capability probe** and cache the answers for the L2 RPC
   surface.
6. Run `quirks.post_asic_init`; serve gRPC.

At runtime the async side sends commands to the actor over a channel;
oper-status events flow out through a broadcast channel that updates the
shared port table and feeds `WatchPortEvents` streams. FDB notifications
flow the same way and maintain a **software FDB mirror** (dynamic entries
with move counters plus configured statics) — `show mac address-table`
and the paged `DumpFdb` RPC serve from the mirror rather than dumping the
ASIC. syncd also re-derives storm-control policer rates on link-speed
changes, since levels are configured as a percentage of link speed.

## hemlock-pmon

A poll/control loop at the fan-curve interval reads every manifest sensor,
drives all fans to the PWM given by linear interpolation over the curve
(below the first point: its PWM; above the last: 100%), and refreshes fan
tach + PSU status. A slower loop scans transceiver EEPROMs (SFF-8472
identity fields). All hardware access goes through the `HwBackend` trait:
`sysfs` for real hwmon/i2c paths, `mock` for CI and development.

## hemlock-orch

orch hosts the L2 protocol engines — the state machines that need packet
I/O and timers, which neither mgmtd (config lifecycle) nor syncd (ASIC
owner) should carry:

- **LACP** (`lacp.rs`) — per-member actor/partner mux machines, LACPDU
  tx/rx at the partner's rate, 3× timeout expiry, static (`mode on`) and
  LACP fallback, min-links. Its output is per-member **gate** decisions.
- **Spanning tree** (`stp.rs`) — a single CIST state machine driving all
  MST instances (the config surface has no per-instance priorities, so
  one machine is sufficient), RST BPDUs, root/designated/alternate roles,
  portfast, and BPDU guard → errdisable.
- **IGMP/MLD snooping** (`snoop.rs`) — one generic engine instantiated
  per family: group membership with timers, fast-leave, querier election,
  dynamic mrouter learning from queries, and optional local querier.

Each engine is a pure state machine spawned with mpsc channels (links and
packets in; frames and state updates out), so engine tests wire two
engines back-to-back with no sockets involved. mgmtd pushes the desired
protocol config to orch at commit (`SetLagConfigs`, `SetStpConfig`,
`SetSnoopingConfig` — a commit fails if orch is down); orch alone writes
the results into syncd: LAG membership + collect/distribute gates
(`SetLagMembers`), per-port STP states, errdisable, L2MC groups and
unknown-multicast flood restriction. Keeping orch the *only* writer of
LAG membership avoids a dual-writer race with mgmtd, which only creates
and removes the LAG object itself.

**The RIB pipeline (routing suite).** Routing state flows **kernel
netlink → orch → syncd**: mgmtd installs static routes and static
ARP/ND entries into the kernel (the OS applier), FRR's zebra installs
protocol routes the same way, and orch's RIB manager (`rib.rs`) mirrors
the kernel FIB and neighbor table into the ASIC over syncd's FIB RPCs
(`EnsureNeighbor`/`EnsureRoute`/`RemoveRoute`/...). One pipeline, no
side doors: mgmtd never programs routes into syncd directly, and orch
never renders FRR config. Details:

- The kernel feed runs `ip monitor route neigh` and re-dumps
  `ip -j route show` / `ip -j -s neigh show` on change (iproute2 is
  netlink under the hood and already the workspace's kernel access
  path). Full-dump snapshots make the engine pure state — tests inject
  synthetic snapshots — and double as the resync protocol: the pusher
  reconciles per-op against a local mirror and re-anchors on syncd's
  `DumpFib` every 30 s, so a restart on either side converges (add
  missing, delete stale).
- Only routes out ASIC L3 interfaces (front-panel hostifs, SVI bridges)
  are programmed; Management-only routes stay kernel-only.
  Connected/local routes ride syncd's RIF path and are skipped. ECMP
  kernel routes become next-hop sets; syncd deduplicates next hops and
  next-hop groups by member set (refcounted).
- **Resolve-via-punt**: a route whose next hops have no
  REACHABLE/PERMANENT neighbor is programmed to punt to the CPU, so the
  kernel resolves ARP/ND; the neighbor event reprograms it onto the
  resolved hops.
- `show ip route` / `show arp` render from orch's `GetRib` /
  `GetNeighbors` snapshots — statics, connected, and FRR routes appear
  uniformly, with FIB (hardware) state — never from vtysh.

**FRR (OSPFv2, BGP IPv4 unicast, VRRP).** FRR is the protocol stack
(pinned Debian package in the image). mgmtd's FRR applier
(`frrapply.rs`) renders `/etc/frr/frr.conf` + `daemons` from the
intents — deterministically, golden-tested — and reloads via
`frr-reload.py` (fallback `systemctl reload frr`; a daemons-file change
restarts instead). The OS applier creates the per-group VRRP macvlans
(virtual MAC `00:00:5e:00:01:<group>`) before the reload, and mgmtd
pushes the virtual MACs into the ASIC's My-MAC table through syncd
(capability-gated: a SAI without My-MAC fails the commit with the
platform error). Protocol *detail* state (OSPF neighbors, BGP
summaries, VRRP status) is queried live from `vtysh -c '... json'` by
orch's `frrshow` module — orch owns vtysh access; hemlockctl and webd
ask orch, and a dead FRR degrades to `% ospf is not running`.

**Packet I/O decision**: engines exchange PDUs over Linux `AF_PACKET`
sockets (`transport.rs`, via the `nix` crate — bound with `getifaddrs`
link addresses so no `unsafe` sockaddr construction is needed, keeping
the workspace-wide `unsafe_code = deny` intact). One reader dispatches by
frame type: EtherType 0x8809 → LACP, the STP multicast DA → STP, and
IGMP/ICMPv6-MLD → snooping, with VLAN classification from the port's
PVID. The transport is `cfg(target_os = "linux")`; on other hosts (and
in CI) the engines run against injected frames only. This rides the
kernel netdevs that the ASIC's CPU port exposes — no SAI hostif plumbing
is required for the current feature set.

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
  the changes to syncd (and, for protocol families, orch), then rotate the
  ring and promote the candidate. Each intent family is a pure function
  from config tree to typed intents plus an apply step, slotting into
  `intents.rs` without touching the lifecycle machinery. The families so
  far: interface admin-state/description/addresses, VLANs (including
  `state suspend`), switchport modes (access/trunk/dot1q-tunnel), static
  routes, system (hostname, users, http/https), and the switching suite —
  LAGs + LACP, spanning tree, MAC table (aging + statics), IGMP/MLD
  snooping, storm control, and mirror sessions. Cross-object validation
  (e.g. "a LAG member's L2 config lives on the Port-Channel", mirror
  destination exclusivity) runs at load/commit before anything is pushed.
  An interface removed from the config reverts to defaults (admin up, no
  description).
- **Commit-confirm** — `commit --confirm N` arms a timer holding the
  pre-commit running text; `hemlockctl confirm` disarms it, expiry
  re-applies the old config automatically.
- **Rollback** — `rollback N` loads ring entry N into the candidate and
  commits it (the underlying RPCs are separate, so a future interactive
  mode can load-then-inspect).

## The operator CLI

`hemlockctl` with no arguments is the interactive CLI (and, on a switch,
the operator's login shell). Arista EOS-style syntax with Nightshade
prompts: `user@hostname>` in operational mode, `user@hostname#` in
configuration mode (`admin@hemlock` by default — the image sets hostname
`hemlock` and creates the `admin` operator account; root stays locked). `configure`/`conf` enters config mode, `bash` drops to Linux,
unique command prefixes are accepted (`sh int status`).

Config-mode commands (`interface <name>` → `description`, `shutdown`,
`no shutdown`) edit the mgmtd *candidate* via the config tree; nothing
touches the ASIC until `commit` (or `commit confirmed <secs>` for
auto-rollback). `show interfaces status` renders the EOS-style summary
(Status = connected/notconnect/disabled; Type comes from the manifest's
per-port `media` field). Subcommand form (`hemlockctl show interfaces
status`, `hemlockctl commit`, ...) drives the same daemons for scripting.

At startup mgmtd replays the persisted running config onto syncd (with
retry), so a restart of either daemon — or the whole box — converges the
ASIC to the running config.

## Image and installer

`build/mkimage.sh <platform>` produces `hemlock-<version>-<platform>.bin`:

1. debootstrap a Debian 13 rootfs (`build/rootfs/packages.list`), install
   the Hemlock daemons + systemd units, and the platform's pinned vendor
   SAI `.deb` from `vendor/sai/` (hard failure with pointers if absent).
   The manifest's `[kernel] required_modules` are compiled in the chroot
   against the image kernel (BDE pair from `vendor/sai/saibcm-modules`
   via `build/build-bde.sh`; platform drivers committed and ported under
   `platforms/<id>/kmod/`), and
   the build refuses to ship an image where any required module is not
   loadable. syncd/pmon run with `--auto-mock`: mock backends only when
   no Broadcom ASIC is on PCI (QEMU); with the ASIC present, bring-up
   failures are fatal so mock data never impersonates real hardware.
   The rootfs is branded as Hemlock (os-release, issue — the banner also
   renders into `/etc/issue` so it shows at the console before login)
   with the default operator account `admin` / `Hemlock123!` (sudo; root
   locked; login shell `hemlockctl`, so a login lands straight in the
   CLI's operational mode), and gets
   the dynamic MOTD: `/etc/update-motd.d/00-hemlock-banner` (static art)
   and `10-hemlock-status` (a wrapper over `hemlockctl motd`, which polls
   syncd/pmon with short timeouts and degrades field-by-field). The stock
   Debian motd content is removed; `hemlock-motd` previews the result
   without logging in;
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

Boot hand-off: GRUB passes `hemlock.rootfs=/hemlock/rootfs.squashfs`; the
hemlock initramfs script (`build/rootfs/initramfs/`, run at local-bottom)
loop-mounts that squashfs read-only, overlays `/hemlock/persist` from the
flash partition as the writable upper layer, and leaves the flash mounted
at `/host` in the running system. The rootfs carries a `/hemlock ->
host/hemlock` symlink, so units and tools address the platform overlay and
persist dir by the stable `/hemlock/...` paths regardless of where the
flash lands. Wiping `/hemlock/persist` is a factory
reset; the squashfs is never modified in place.

In-band upgrades reuse the same image format. The engine lives in
`hemlock_common::image` and executes inside mgmtd (root, serialized with
commits) behind the `InstallImage` RPC: it verifies the
`hemlock_image_platform` header against the installed
`/hemlock/platform/onie-machine`, unpacks the `.bin` with the
self-extractor's `HEMLOCK_EXTRACT_ONLY` hook, and replaces
`rootfs.squashfs`, the boot assets and the platform overlay on `/host`
(each file via copy-to-.new + fsync + rename). Two front ends drive it:
the web console (System → Maintenance; webd streams the upload to its
flash-backed state dir, then calls the RPC) and the CLI —
`upgrade <image.bin> [force] [reboot]` in operational mode, or
`hemlockctl upgrade <image.bin> [--force] [--reboot]` from a shell.
There is no A/B slot — recovery from a bad image is a reinstall from
ONIE.

## Boundaries and seams

Still out of scope: EVPN, OSPFv3, the BGP IPv6 address family, VRFs,
multicast routing (PIM), policy routing, licensing, telemetry
streaming. The seams they will land on already exist:

- New routing families extend the `routing { ... }` intent extractors,
  the FRR render, and — where the ASIC is involved — the RIB pipeline's
  translation rules.
- New SAI object families extend `SaiBackend` + the mock in lockstep,
  and gate on the startup capability probe when support varies by
  platform (the routing suite added neighbors, next hops, ECMP groups,
  and My-MAC exactly this way).
- New config families add an intent extractor + apply step in mgmtd.
- Telemetry can subscribe to syncd's event broadcasts (port and FDB)
  and pmon's state.
