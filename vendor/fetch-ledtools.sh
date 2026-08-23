#!/bin/sh
# fetch-ledtools.sh — fetch Broadcom's SM-Lite LED processor toolchain.
#
# Usage: vendor/fetch-ledtools.sh
#
# Downloads the LED microcode assembler (ledasm), disassembler (leddasm),
# and simulator (ledsim) from Broadcom's OpenBCM repository — the same
# SDK lineage (6.5.27) as the pinned libsaibcm — plus a few reference
# example programs. Used to (re)assemble the committed LED programs under
# platforms/*/led/ (the generated .hex files ARE committed, so the tools
# are only needed when editing .asm sources). Nothing fetched here is
# ever committed to git.
#
# Build (any C compiler):
#   cc -w vendor/ledtools/ledasm.c -o vendor/ledtools/ledasm
#   cc -w vendor/ledtools/leddasm.c vendor/ledtools/leddasmcore.c \
#       -o vendor/ledtools/leddasm
#   cc -w vendor/ledtools/ledsim.c vendor/ledtools/leddasmcore.c \
#       -o vendor/ledtools/ledsim
#
# Assemble (ledasm reads/writes <name>.asm/<name>.hex in the cwd):
#   cd platforms/cel-e1031/led && ../../../vendor/ledtools/ledasm e1031-sfp-link
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/vendor/ledtools"
BASE="https://raw.githubusercontent.com/Broadcom-Network-Switching-Software/OpenBCM/master/sdk-6.5.27/tools/led"

mkdir -p "$DEST"

fetch() {
    url="$1"; dest="$2"
    if [ -f "$dest" ]; then
        echo "have    $(basename "$dest")"
    else
        echo "fetch   $(basename "$dest")"
        curl -fsSL "$url" -o "$dest"
    fi
}

for f in ledasm.c ledasm.h leddasm.c leddasmcore.c ledsim.c README; do
    fetch "$BASE/tools/$f" "$DEST/$f"
done
# Reference programs: the simplest example and the closest board shape
# (BCM56334: 24GE+4XE, same PORTDATA/linkscan idioms as the E1031 code).
for f in ex1.asm sdk56334.asm; do
    fetch "$BASE/example/$f" "$DEST/$f"
done

echo "done — see header comments for build/assemble instructions"
