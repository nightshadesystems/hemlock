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

## Experiment 3 — scan-chain mapping

Goal: which scan-chain bit position drives which faceplate LED, and
whether the copper ports appear on the chain at all.

1. With the LED processor running (`led start` from Experiment 1), stop
   the auto-updater so the data RAM is yours to poke:

   ```text
   BCM.0> led auto off
   ```

2. The program reads per-port link state from LEDUP data RAM (one byte
   per port for the stock program, `0x01` = link up). Walk it:

   ```text
   BCM.0> led data 0x00 01
   ; watch the faceplate; then clear and advance
   BCM.0> led data 0x00 00
   BCM.0> led data 0x01 01
   ...
   ```

   `led data` with no value dumps the RAM. If `led data` is unavailable
   in this SAI's shell, the raw register path is:

   ```text
   BCM.0> getreg CMIC_LEDUP0_CTRL
   BCM.0> mod CMIC_LEDUP0_DATA_RAM 0 1 DATA=1
   ```

3. Record the mapping:

   | Data-RAM offset / chain bit | Faceplate LED | Notes |
   |---|---|---|
   | | | |

4. If the stock program produces nothing sensible, load the Delta-style
   port remap first (`m CMIC_LEDUP0_PORT_ORDER_REMAP_0_3 REMAP_PORT_0=…`,
   groups of four — see sonic-buildimage
   `device/delta/x86_64-delta_et-6248brb-r0/led_proc_init.soc` for the
   idiom), then repeat.

Key question to answer here: do any of offsets corresponding to the
copper ports move a copper LED? If yes, the boot sweep can cover all 52
ports; if no, the copper LEDs are PHY-hardwired and the sweep is limited
to Ethernet49-52.

## Experiment 4 — custom microcode (only if the stock program is wrong)

If the chain layout doesn't match anything the stock program can drive,
the LEDUP needs a small custom program (classic 256-byte LEDUP ISA,
program RAM `CMIC_LEDUP0_PROGRAM_RAM`, loaded via `led prog <hex bytes>`
or `led load <file>`). Start from a published Helix4-era example
(`led_proc_init.soc` files under sonic-buildimage `device/*/`) and adapt
the bit order found in Experiment 3.

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
