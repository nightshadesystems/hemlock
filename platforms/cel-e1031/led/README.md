# E1031 LED processor programs

Microcode for the Helix4's on-chip LED processor (LEDUP0), which drives
the four SFP+ cage LEDs (Ethernet49-52). The copper ports' LEDs are
PHY-driven and are not on this scan chain. Background, bench findings,
and the full bring-up procedure: `docs/e1031-led-bringup.md`.

| File | Purpose |
|---|---|
| `e1031-led-probe.asm` / `.hex` | Bench-only: emits 16 chain bits from hand-poked data RAM bytes, to map chain-bit -> faceplate LED and confirm polarity |
| `e1031-sfp-link.asm` / `.hex` | The real program: link up = green, down = dark, from linkscan-maintained PORTDATA (`led auto on`) |

The `.hex` files are committed (generated output, needed on-switch);
regenerate after editing an `.asm` with Broadcom's assembler:

```sh
vendor/fetch-ledtools.sh                   # once; see its header for build
cd platforms/cel-e1031/led
../../../vendor/ledtools/ledasm e1031-sfp-link
```

Status: **pending bench verification** of four constants marked in
`e1031-sfp-link.asm` (physical port numbers, emission order, chain
length, polarity). Once verified, the load sequence
(`led load .../e1031-sfp-link.hex`, `led auto on`, `led start`) moves
into `sai_postinit_cmd.soc` and this note gets updated.
