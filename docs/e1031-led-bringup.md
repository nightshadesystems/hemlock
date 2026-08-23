# E1031 port LED bring-up (bench procedure)

## Symptom and root cause

The four SFP+ port LEDs (Ethernet49-52) are lit solid green from power-on
regardless of link state. This is inherited from SONiC — the upstream
`x86_64-cel_e1031-r0` device tree ships **no** LED support at all: no
`led_proc_init.soc`, no LED microcode, no `led_control` plugin, and the
hwsku's `sai_postinit_cmd.soc` contains only an RTAG7 hash register write.
The Helix4's on-chip LED processor (LEDUP) is therefore never programmed
or started, and the LED scan-chain outputs sit at their reset defaults —
which on this board happens to drive those LEDs green.

The 48 copper ports behave correctly because their link lights are driven
by the BCM54282 PHYs' LED pins in hardware. Whether the copper LEDs are
*also* routed through the LEDUP scan chain (some boards mux both) is one
of the questions this procedure answers.

Related hardware the CPU can reach directly (SMC CPLD, `smc.c`):
`LED_OPMOD` (io 0x0208), `LED_TEST` (io 0x0209), and the system/master
LED register (`LED_FPS`, 0x020a). The CPLD register map exposes no
per-port LED control, which points at the ASIC scan chain for the SFP+
LEDs — but 0x0208/0x0209 may gate them; check both.

## Prerequisites

- Real E1031 with the vendor artifacts installed
  (`vendor/fetch-vendor.sh cel-e1031` / a full image).
- Root shell on the switch (`bash` from the CLI, then sudo). Stop the
  production daemon first:

```sh
sudo systemctl stop hemlock-syncd
```

- Start syncd manually in the foreground with the vendor diag shell.
  Mirror the systemd unit: the binary is in `/usr/sbin` (not in operator
  PATHs), the platform manifest directory on a switch is
  `/hemlock/platform` (passed directly as `--platform`), and Broadcom's
  SAI writes its SyncDB state relative to the working directory — run
  from the service's state dir so it finds/keeps the same files:

```sh
cd /var/lib/hemlock
sudo /usr/sbin/hemlock-syncd --platform /hemlock/platform --diag-shell
```

  After `create_switch` finishes, Broadcom's `BCM.0>` prompt appears on
  the terminal (the gRPC service keeps running alongside it). `exit`
  leaves the shell; Ctrl-C stops syncd. Restart the service afterwards.

Keep a fiber loop or a DAC plugged into at least one SFP+ cage so a
genuine link exists while probing.

## Bench findings so far (2026-08-23)

Established on real hardware; later experiments build on these, don't
re-derive them:

- **The SMC CPLD gates the port LEDs.** `LED_OPMOD` (io 0x0208) powers
  up as `0x00` = forced/default mode — this is the stuck-green. Writing
  `0x01` switches to normal operation: all forced LEDs drop, and the
  real sources take over.
- In mode 1: **copper (RJ45) LEDs work** (PHY-driven, as assumed);
  **SFP+ LEDs stay dark even with link up** → they are driven by the
  ASIC LED processor scan chain, which has no program.
- **LEDUP program RAM contains only power-on garbage** (`led dump`
  shows high-entropy noise); `led start` had set `LEDUP_EN=1`
  (`CMIC_LEDUP0_CTRL=0xa3`) but there is nothing real to execute. No
  layer of the stack (CPLD, SAI, SONiC heritage) ever loads ledcode.
- In mode 1 the **system LEDs follow `LED_FPS` (io 0x020a)**: writing
  `0x05` = status green + master green works. In mode 0 the register is
  ignored (forced). So flipping to mode 1 in production requires pmon
  to own `LED_FPS`, or the system LEDs go dark.

Consequences for the fix:

1. Platform init must set `LED_OPMOD=1` (a `haliburton` PlatformQuirks
   impl) **and** pmon must drive `LED_FPS` from then on.
2. The SFP+ LEDs need real LEDUP microcode (4 LEDs only). The complete
   toolchain is public in OpenBCM — matching our exact SDK:
   `sdk-6.5.27/tools/led/tools/` (`ledasm.c` assembler, `leddasm.c`
   disassembler, `ledsim.c` simulator) and ~150 annotated example
   programs under `sdk-6.5.27/tools/led/example/` at
   https://github.com/Broadcom-Network-Switching-Software/OpenBCM
   (no BCM56340/Helix4 example ships; write a minimal 4-bit program and
   map the chain with it).
3. The boot sweep could even run CPU-driven pre-ASIC if the CPLD's
   forced mode turns out to have per-port force registers — nothing in
   `smc.c` suggests so; park the idea unless probing finds more.

### Session 2 findings (CPLD probing on the bench)

- **The SFP+ cage LEDs are driven directly by the SMC CPLD, not by the
  ASIC.** Both LEDUP scan chains are unconnected on this board: full
  64-bit all-ones frames from LEDUP0 *and* LEDUP1 changed nothing, while
  the CPLD's `LED_TEST` register (io 0x0209, write 1) blinks the SFP+
  LEDs — a lamp test. All LEDUP microcode work is therefore moot for
  this board; the programs under `platforms/cel-e1031/led/` are kept
  only as a reference for boards whose chains are wired.
- `LED_OPMOD` values 2 and 3 behave no differently from 1.
- Undocumented bytes 0x20b/0x20d/0x20e take writes with no visible
  effect.
- **DANGER — never write io 0x20f** (powers up as 0x03; undocumented).
  Writing it caused a fatal, unrecoverable PCIe error on the Helix4
  (linux-kernel-bde fell off the bus; full power cycle required). It is
  presumably a reset/control register. Probing policy from here: reads
  anywhere, writes only inside register blocks smc.c documents.
- `LED_FPS[7:4]` ("RESERVED"): writes have no effect — not the SFP LEDs.
- Fan tray LEDs confirmed register-driven: writing 0 (green) to
  0x205-0x207 works; power-on state in normal mode is 4 (off). The
  haliburton quirk sets them green at pmon start.
- The CPLD has a second register block at io 0x100-0x1ff (version reg
  0x100 = 0x05, scratch 0x101, watchdog 0x110-0x11a, reboot cause
  0x112 — mapped from SONiC's watchdog.py/chassis.py). Full read dump
  shows nothing else alive and nothing sensitive to module presence.
  **The 0x1xx block contains the watchdog enable — no writes there.**

### Conclusion (2026-08-23): per-port SFP+ LEDs are not achievable

Exhaustively tested: both LEDUP processors with 16/64/255-bit frames in
both polarities (chains are unwired), every documented CPLD register,
both CPLD blocks, and all safely-probeable undocumented bytes. The SMC
CPLD firmware (v5) offers exactly three states for the four SFP+ cage
LEDs:

| State | How |
|---|---|
| Forced solid green (power-on default; a lie) | `LED_OPMOD=0` |
| Blinking (lamp test, all four) | `LED_TEST=1` |
| Off | `LED_OPMOD=1` (normal mode; no per-port source exists) |

Hemlock's policy: **normal mode, LEDs dark** — honest for the common
no-link case, and the quirk keeps system + fan LEDs truthful. This
matches why SONiC never had SFP LED support here: the hardware feature
does not exist in this CPLD build. A newer SMC firmware from Celestica
(SONiC's component.py shows CPLD update plumbing) is the only plausible
path to real per-port control.

Boot-time visual feedback IS achievable via CPLD writes alone, and is
**implemented**: `build/rootfs/bin/hemlock-boot-led` driven by
`hemlock-boot-led.service` (early boot: SFP+ bank lamp-test blink,
status LED blinking green, fans green) and
`hemlock-boot-led-done.service` (after the Hemlock daemons: everything
steady). The script gates on `/hemlock/platform/platform-id` and is a
silent no-op elsewhere. A per-port sweep is off the table on this
hardware; a copper-port sweep would require forcing BCM54282 PHY LED
registers over MDIO once the ASIC is up (untested; future curiosity).

### Fixes already landed in the repo

- `i2c-dev` added to the manifest's `required_modules` — without it,
  pmon's raw i2cset pre-writes have no `/dev/i2c-*` nodes and pmon
  crash-loops before instantiating anything.
- `[hardware.quirks] driver = "haliburton"`: pmon's post-hw-init now
  writes `LED_OPMOD=1` and `LED_FPS=0x05` (system LEDs green). **Deploy
  the manifest change together with matching binaries** — an older
  pmon/syncd rejects the unknown quirks name at startup.
- LED programs (probe + link-tracking) committed under
  `platforms/cel-e1031/led/`, assembled with Broadcom's own toolchain
  (`vendor/fetch-ledtools.sh`, OpenBCM sdk-6.5.27) and validated in its
  simulator. The link program's constants await Experiment 3/4 results;
  then its load sequence moves into `sai_postinit_cmd.soc`.

## Experiment 1 — does the SDK default LED program just work? (5 min)

SONiC's Helix4 Delta ET-6248BRB (48x1G + 2x10G, same ASIC family) fixes
its LEDs *without custom microcode*: its `led_proc_init.soc` only remaps
`CMIC_LEDUP0_PORT_ORDER_REMAP_*` and runs the stock program. Try that
first:

```text
BCM.0> led status
BCM.0> led auto on
BCM.0> led start
```

Watch the four SFP+ LEDs. Outcomes:

- **LEDs start tracking link** → done in principle; skip to
  "Productizing".
- **LEDs change but track the wrong ports** → port-order remap needed;
  go to Experiment 3.
- **No change** → either the default program does not match this board's
  scan chain (Experiment 3/4) or the CPLD gates the LEDs (Experiment 2).

## Experiment 2 — CPLD LED mode gates

From a second shell (the CPLD is port-I/O; needs root):

```sh
# read LED_OPMOD / LED_TEST / LED_FPS
for p in 0x208 0x209 0x20a; do
  printf '%s: ' $p
  dd if=/dev/port bs=1 count=1 skip=$(($p)) 2>/dev/null | od -An -tx1
done
```

Record the power-on values. If `LED_TEST` (0x209) reads non-zero or
`LED_OPMOD` (0x208) looks like a "test/forced" mode, try writing 0 and
re-check the faceplate:

```sh
printf '\x00' | dd of=/dev/port bs=1 count=1 seek=$((0x209)) 2>/dev/null
```

(One byte at a time; note every change so it can be reverted.)

## Experiment 3 — scan-chain mapping with the probe program

Bench-verified constraints: this drivshell's `led` command supports
`status | start | stop | load <file.hex> | auto | prog <hex> | dump`
(no `led data`); data RAM is poked via the register array
`CMIC_LEDUP0_DATA_RAM(i)`. Hemlock ships a purpose-built probe program
(`platforms/cel-e1031/led/e1031-led-probe.asm`, simulator-validated)
that emits 16 chain bits, bit *i* from bit 0 of data byte `0xA0+i`.

1. Set `LED_OPMOD=1` (Experiment 2), then load the program. Serial-only
   consoles need no file transfer — `led prog` takes the bytes inline
   (these are the assembled contents of `e1031-led-probe.hex`; execution
   ends at the final `send`, so unpasted program RAM is never reached):

   ```text
   drivshell> led stop
   drivshell> led prog 02 00 60 E0 12 A0 F8 15 1A 00 71 11 32 0E 87 77 14 32 0F 87 06 E0 80 D2 10 74 02 3A 10
   drivshell> led auto off
   drivshell> led start
   ```

   (With file access, `led load <path>/e1031-led-probe.hex` is
   equivalent.)

   All-zero data RAM should now hold the LEDs in the familiar all-green
   look if the chain is active-low as suspected (`ZERO` bit = LED on).

2. Walk the bits and watch the four cages:

   ```text
   drivshell> setreg CMIC_LEDUP0_DATA_RAM(0xa0) 1   ; chain bit 0
   drivshell> setreg CMIC_LEDUP0_DATA_RAM(0xa0) 0
   drivshell> setreg CMIC_LEDUP0_DATA_RAM(0xa1) 1   ; chain bit 1
   ...
   ```

3. Record: chain length, bit -> cage order, polarity, and whether any
   bit touches anything besides the four SFP+ cages.

   | Chain bit (data byte) | Faceplate LED | Notes |
   |---|---|---|
   | | | |

## Experiment 4 — the link-tracking program

`platforms/cel-e1031/led/e1031-sfp-link.asm` (also simulator-validated)
drives the four cages from linkscan-maintained link state: green up,
dark down. Four constants at the top are marked for bench verification
against the Experiment 3 results — physical port numbers (expected
50-53), emission order, chain length, polarity. After fixing constants,
reassemble (`vendor/fetch-ledtools.sh`, see `led/README.md`) and:

```text
drivshell> led stop
drivshell> led prog 02 32 67 12 02 33 67 12 02 34 67 12 02 35 67 12 3A 04 12 A0 F8 15 1A 00 71 1E 32 0F 87 57 32 0E 87 57
drivshell> led auto on
drivshell> led start
```

(The `led prog` bytes are the assembled `e1031-sfp-link.hex`; with file
access `led load` is equivalent. Re-derive the bytes after any constant
change by reassembling — see `led/README.md`.)

Then pull/insert the DAC and confirm the right cage tracks link.

## Productizing the result

`sai_postinit_cmd.soc` is executed by the vendor SAI at create_switch
(`sai_postinit_cmd_file` in config.bcm + the `/usr/share/sonic/hwsku`
symlink), so the fix is data, not code: append the working command
sequence (remaps + `led auto on` + `led start`, plus any `led prog`
payload) to that file. Note the file is currently *fetched* from SONiC by
`vendor/fetch-vendor.sh`; once we carry LED commands it becomes a
Hemlock-owned file — commit it under `platforms/cel-e1031/` and drop it
from the fetch list. If a CPLD write (Experiment 2) turns out to be
required, it belongs in a `PlatformQuirks` impl (`post_asic_init`), not
in the soc script.

## Boot-time LED sweep (stretch goal)

A "sweeping" link-light animation while the system finishes booting is
feasible *after* create_switch only — the LEDs are unreachable before the
ASIC exists, and syncd starts late in boot. The plan, gated on the
Experiment 3 mapping:

1. The LEDUP runs autonomously at its refresh rate, so the sweep is pure
   microcode (or `led auto off` + a small data-RAM animation driven from
   the postinit soc); no CPU involvement.
2. Load the sweep at create_switch via `sai_postinit_cmd.soc`.
3. When the system reaches ready (systemd target), swap to the normal
   link-tracking program — a oneshot unit driving a future
   `hemlock-syncd` "run soc script" hook, or simply `led auto on` once
   the mapping shows the stock program suffices.
4. Coverage: all 52 ports if the copper LEDs prove chain-driven,
   otherwise Ethernet49-52 only.

Until the ASIC is up, the only boot-time visual available is the CPLD
status/master LED (`smc` sysfs), which can blink from a systemd unit at
any point in boot.
