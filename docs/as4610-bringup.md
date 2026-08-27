# AS4610-54T bring-up runbook

Phase 5 of [`as4610-54-port.md`](as4610-54-port.md): taking the port from
"builds" to "forwards". Written to be worked through at the console in
order — each gate has the command, what a pass looks like, and what to do
when it does not.

**Work the gates in order and do not skip.** Every one of them depends on
the one before, and a failure three gates later is usually the earlier
gate having half-passed. When something fails, the diagnostic under it is
the next thing to run, not a guess.

**Record as you go.** Anything surprising goes in
`platforms/accton-as4610-54/README.md` under "Known quirks", in the style
of [`e1031-led-bringup.md`](e1031-led-bringup.md) — the point of that
section is that the next person does not rediscover it.

---

## Before you start

Gate 0 needs nothing but the box. **Gates 2 onward need an installable
image, which does not exist yet** — it is blocked on:

| Blocker | Where |
|---|---|
| The 6.1 iProc kernel package | [`as4610-kernel-port.md`](as4610-kernel-port.md) |
| `libhemlockbcm.so` for `iproc-4_4` | `vendor/openbcm-shim/build-shim.sh`, never yet compiled |
| The board device tree | `platforms/accton-as4610-54/dts/`, part of the kernel work |

`build/mkimage.sh accton-as4610-54` lists all of them and refuses to
start until they are there.

**Gate 1 is the exception and should be done first, today.** It needs
only ONIE, answers four open questions, and two of its answers change
code that is already written.

---

## Gate 0 — Console

Serial console, DB9 or RJ45 depending on the unit. The device tree
aliases `serial0 = &uart1` and its commented-out bootargs say
`console=ttyS0,115200n8`, so **115200 8N1** is the expectation.

**Pass:** ONIE's boot output and a prompt.

**If nothing:** try 9600 — the E1031 runs 9600 and Edgecore is not
consistent. If the speed differs from 115200, the manifest needs a
`boot.env` (`CONSOLE_DEV` / `CONSOLE_SPEED`), the way `cel-e1031` has one.

---

## Gate 1 — Capture the ONIE facts (do this first)

Boot to ONIE (`ONIE: Rescue` or `ONIE: Install OS`) and capture all of
this. It is quick, it is safe, and it closes open questions 1–4.

```sh
onie-sysinfo -p                 # expect: arm-accton_as4610_54-r0
cat /etc/machine.conf           # onie_arch=arm, onie_machine, onie_machine_rev
cat /proc/mtd                   # the NAND partition table
cat /proc/cmdline
fw_printenv                     # the whole U-Boot environment
ip link                         # is the management port ma1, eth0, or something else?
which ubiformat ubiattach ubimkvol fw_setenv onie-nos-mode
ubinfo -a 2>/dev/null           # is a UBI device already attached?
cat /etc/onie/installer.conf 2>/dev/null || true   # console dev/speed
```

**Why each matters:**

- `/proc/mtd` + `ubinfo` → **open question 2**, the NAND layout. The ARM
  install strategy in `src/hemlock-installer/src/install.rs` is written
  against an assumed UBI layout and is explicitly marked unverified.
  This output is what corrects it.
- `fw_printenv` → **open question 3**. The installer sets `nos_bootcmd`;
  if this board's U-Boot calls it something else, the install produces a
  box that boots straight back into ONIE.
- `ip link` → **open question 4**. The manifest says `os_device = "eth0"`
  on the reasoning that `ma1` comes from ONL's udev rules, which Hemlock
  does not ship. ONIE's own naming is a strong hint.
- `machine.conf` → confirms the exact `onie_machine` string the installer
  matches on. A mismatch means every install refuses without `--force`.

**Before installing anything**, dry-run the installer against what you
found and read the commands:

```sh
./hemlock-installer --payload . --disk /dev/mtdX --dry-run --non-interactive
```

It prints every command without running one. Check the UBI sequence
against the real `/proc/mtd` **before** the first real install — the
first step erases the NAND, and a wrong partition there costs you ONIE.

---

## Gate 2 — The kernel boots

```sh
onie-nos-install http://<server>/hemlock-<version>-accton-as4610-54.bin
```

**Pass:** the installer runs, the box reboots, and Linux reaches a login
prompt on the console.

**If U-Boot does not load the FIT:** you are back in ONIE. Compare
`fw_printenv nos_bootcmd` with what the installer set; that is gate 1's
answer being wrong.

**If the kernel panics or hangs early:** kernel-port territory, not
Hemlock's. Two things worth trying before anything else, both recorded in
[`as4610-kernel-port.md`](as4610-kernel-port.md) as known risks:

- `maxcpus=1` on the command line — SMP bring-up (`platsmp.c`) is the
  highest-risk part of the machine glue.
- Check the console is still attached at all; a wrong `console=` in the
  FIT's bootargs looks exactly like a hang.

---

## Gate 3 — Management networking

```sh
ip link                          # what is the iProc GMAC called?
ip addr add <addr>/<len> dev <netdev> && ip link set <netdev> up
ping <gateway>
```

**Pass:** the box is reachable over the management port.

**If the netdev name is not `eth0`:** fix `[management] os_device` in the
manifest. **This is open question 4 closing** — record the answer.

**If there is no netdev at all:** the `bgmac`/`bgmac-platform` deltas for
`brcm,xgs-iproc-amac` did not make it into the kernel build. If it exists
but never links, that is the **bucket-E question** from the kernel doc:
rebuild with `MDIO_XGS_IPROC` and `XGS_IPROC_SERDES` enabled and see
whether the SGMII link comes up. **That answer decides ~2,000 lines of
the kernel port** — record it either way.

---

## Gate 4 — BDE and KNET

```sh
lsmod | grep -E 'linux-kernel-bde|linux-user-bde|linux-bcm-knet'
ls -l /dev/linux-*
dmesg | grep -i -E 'bde|knet|cmic'
```

syncd loads these itself (`sysinit::load_kernel_modules`) and creates the
device nodes, so if it started at all they should be present.

**Pass:** all three modules loaded, `/dev/linux-kernel-bde`,
`/dev/linux-user-bde` and `/dev/linux-bcm-knet` present.

**If the BDE finds no device:** the CMICd is still bound to the kernel's
`iproc_cmic` driver — that is gate 5's job, and syncd runs it before
this. Check `ls /sys/bus/platform/drivers/iproc_cmic/`.

**If `dmasize` complains:** the manifest asks for `dmasize=8M`. That is
edgenos's value for this board; do not raise it without reading why in
the manifest comment.

---

## Gate 5 — The board quirk

Runs automatically inside syncd's `pre_asic_init`. To watch it:

```sh
journalctl -u hemlock-syncd | grep -i -E 'cmicd|cpld|quirk'
```

**Pass:** `unbound CMICd from iproc_cmic` (or "already unbound"), then
five `CPLD PHY reset deasserted` lines for registers `0x07, 0x08, 0x0d,
0x19, 0x1b`.

**If it reports a read-back mismatch:** the write did not stick. That is
the failure this quirk exists to catch, and it is real — check the CPLD
is at `i2c-0 0x30` and that the SoC i2c driver bound:

```sh
i2cdetect -y 0
i2cget -y -f 0 0x30 0x07
```

**If it cannot find the i2c root:** the manifest names the root
`cpld-bus`, matched as the *first* adapter called `iproc-smbus`. If the
SoC controllers enumerate in the other order, swap the `instance` values
in `[[hardware.i2c.root]]`. Confirm with:

```sh
for a in /sys/bus/i2c/devices/i2c-*; do echo "$a: $(cat $a/name)"; done
```

---

## Gate 6 — The datapath, and the port map

```sh
hemlock-syncd --platform accton-as4610-54 --probe
```

`--probe` creates the switch, prints the port table and exits — no gRPC,
no daemon. It is the fastest loop for this gate.

**Pass:** 52 ports, `Ethernet1..52`, with the SDK column populated:

```
Port         Index    Speed Lanes          SDK    Admin Oper  SAI OID
Ethernet1        1    1000M 26             ge25      up   up  ...
Ethernet2        2    1000M 25             ge24      up   up  ...
```

**If it aborts with "the port map is wrong":** syncd compared the
manifest's `sdk_names` against what the shim reported and they disagree.
This is the check doing its job — the Cumulus porttab is the one input
that could not be verified from a public source. Capture what the shim
reported and fix the manifest.

**If `create_switch` fails:** the shim's own `printf`s go to syncd's
stdout and name the failing SDK call. Run syncd in the foreground.
`--diag-shell` is available on the vendor backend only; for OpenBCM the
shim's log lines are the equivalent.

### The porttab spot-check — do not skip this

The faceplate map is scrambled and was transcribed from a source that
cannot be re-fetched. **Cable faceplate port 1 and confirm the port that
comes up is `Ethernet1` / `ge25`. Then do the same for port 2 (`ge24`)
and one port from the far half — say faceplate 25 (`ge1`).**

```sh
hemlockctl show interfaces status | head -5
```

Three points confirm the map's shape: the pair-swap, and both halves. If
any of them is wrong, stop and re-derive the whole table — a partially
wrong port map mis-cables a rack silently.

---

## Gate 7 — Real link on copper

Cable a copper port to something that will link.

```sh
hemlockctl show interfaces status
hemlockctl show interfaces Ethernet1 counters
```

**Pass:** the cabled port reads `connected`, the right speed, and
counters that move when traffic does.

**If the port never links:** the PHYs are still in reset (gate 5
half-passed), or software linkscan is not running. The shim enables SW
linkscan on every `config.e` port at `create_switch` because the MAC
cannot see an external 54282's link state on its own — if link state
never changes at all, that is the thing to check first.

**If link works but counters stay zero:** the counter mapping is the
shim's `bcm_stat_multi_get` block. Note that the RMON size bins are
approximate at one boundary by design (1519–1522-byte frames land one bin
high); everything else should be exact.

---

## Gate 8 — VLANs and PVID

```text
configure
vlans { vlan 100 { name test; } }
interfaces { ethernet Ethernet1 { switchport { mode access; access-vlan 100; } } }
commit
```

**Expected today: this fails** with `% ... is not supported by this
platform's SAI`. VLANs are phase 6 — the shim has no VLAN slots yet, so
`capabilities()` reports them absent and the commit is refused rather
than silently dropped. **That refusal is a pass at this gate**: it proves
the capability gating works end to end on this backend.

Getting an actual VLAN program requires the phase-6 slots. If you want
this working before phase 6, that is a scope decision, not a bug.

---

## Gate 9 — L3 and static routing

Same story: `create_router_interface`, `create_route` and the neighbour
family are phase-6 slots. Expect the platform-unsupported error.

**Pass at this gate** = the error is the clean capability refusal, not a
crash or a silent no-op.

---

## Gate 10 — OSPF via FRR

Depends on gate 9. FRR itself is CPU-side and will run — `vtysh` will
show neighbours — but nothing reaches the ASIC until the L3 slots exist,
so traffic will not be forwarded in hardware.

**Worth doing anyway once gate 7 passes:** it exercises the punt path
(KNET netdevs, CoPP traps) independently of the FIB, and a working
control plane over a non-forwarding ASIC is a genuinely useful
intermediate state to have confirmed.

---

## What to do with the answers

| Answer | Goes to |
|---|---|
| Console speed ≠ 115200 | `platforms/accton-as4610-54/boot.env` |
| `/proc/mtd`, `ubinfo`, `fw_printenv` | `install.rs` `fit_steps()` — replace the assumed UBI layout |
| Management netdev name | `[management] os_device` in the manifest |
| Bucket-E (does `ma1` link without MDIO/SerDes?) | `as4610-kernel-port.md`, decides ~2,000 lines |
| Porttab corrections | the manifest's `lanes` + `sdk_names`, **and** the raw table in the platform README |
| Anything surprising | platform README, "Known quirks" |
