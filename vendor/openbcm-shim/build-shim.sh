#!/usr/bin/env bash
# build-shim.sh — build libhemlockbcm.so inside an OpenBCM SDK tree.
#
# Usage: vendor/openbcm-shim/build-shim.sh [--sdk <dir>] [--out <file>]
#
# Produces the datapath library for an `[sai] backend = "openbcm"`
# platform: Hemlock's shim (hemlockbcm.c) compiled against the SDK's
# headers and linked against its libraries, for the iproc-4_4 target.
#
# NOT RUN BY CI, and it cannot be: it needs the OpenBCM tree (fetched, not
# committed), an ARM cross toolchain, and kernel headers for KNET. Run it
# in the cross-build container. CI's coverage of this boundary is the
# committed ABI header plus the stub shim hemlock-sai builds from source.
#
# Requirements:
#   - vendor/fetch-vendor.sh accton-as4610-54 has staged the SDK
#   - arm-linux-gnueabihf- toolchain on PATH (or CROSS_COMPILE set)
#   - KERNDIR pointing at the matching kernel source (KNET builds against it)
#
# The SDK tree is left as it was found: this appends nothing to the SDK's
# own sources and patches none of them, unlike the diag-shell splicing the
# edgenos reference does. The shim is a normal translation unit that
# happens to be compiled with the SDK's flags.
set -euo pipefail

die() { echo "build-shim: error: $*" >&2; exit 1; }
log() { echo "build-shim: $*"; }

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

SDK="${SDK:-$ROOT/vendor/openbcm/sdk-6.5.16}"
OUT="${OUT:-$ROOT/vendor/openbcm/out/libhemlockbcm.so.1}"
CROSS_COMPILE="${CROSS_COMPILE:-arm-linux-gnueabihf-}"
PLATFORM="${PLATFORM:-iproc-4_4}"
KERNDIR="${KERNDIR:-}"

while [ $# -gt 0 ]; do
    case "$1" in
    --sdk) SDK="$2"; shift ;;
    --out) OUT="$2"; shift ;;
    --kerndir) KERNDIR="$2"; shift ;;
    *) die "unknown option $1" ;;
    esac
    shift
done

[ -d "$SDK" ] || die "no OpenBCM tree at $SDK
 (run: vendor/fetch-vendor.sh accton-as4610-54)"
[ -f "$SDK/RELEASE" ] || die "$SDK does not look like an OpenBCM SDK tree"

CC="${CROSS_COMPILE}gcc"
command -v "$CC" >/dev/null || die "$CC not on PATH (set CROSS_COMPILE)"

# The SDK's libraries must already be built for this platform: building
# them is the SDK's own job and takes far longer than the shim.
LIBDIR="$SDK/build/unix-user/$PLATFORM"
[ -d "$LIBDIR" ] || die "SDK libraries not built at $LIBDIR
 (build the SDK's own 'bcm' target for $PLATFORM first; see
  docs/as4610-54-port.md)"
for lib in libbcm.a libsoc.a libsal.a; do
    [ -f "$LIBDIR/$lib" ] || die "$LIBDIR/$lib missing — SDK build incomplete"
done

HEADER_DIR="$ROOT/src/hemlock-sai/openbcm-shim"
[ -f "$HEADER_DIR/hemlockbcm.h" ] || die "ABI header missing at $HEADER_DIR"

mkdir -p "$(dirname "$OUT")"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Flags mirroring the SDK's own unix-user build for this platform. The
# -D set selects the chip family and the KNET packet path; without
# INCLUDE_KNET the SDK builds a datapath with no CPU punt at all.
CFLAGS=(
    -O2 -fPIC -Wall
    -DINCLUDE_KNET
    -DBCM_PLATFORM_STRING=\"$PLATFORM\"
    -I"$HEADER_DIR"
    -I"$SDK/include"
    -I"$SDK/systems/linux/kernel/modules/include"
    -I"$SDK/systems/bde/linux/include"
)
[ -n "$KERNDIR" ] && CFLAGS+=(-I"$KERNDIR/include")

log "compiling hemlockbcm.c for $PLATFORM"
"$CC" "${CFLAGS[@]}" -c "$HERE/hemlockbcm.c" -o "$WORK/hemlockbcm.o" \
    || die "compiling the shim failed
 (every SDK symbol was checked against sdk-6.5.16's headers, but this is
  the first actual compile — read the errors as review feedback)"

# --whole-archive around libbcm: the shim references a small part of the
# API directly, but the SDK's chip drivers register themselves through
# constructors that a normal archive link would discard.
log "linking $OUT"
"$CC" -shared -o "$OUT" \
    -Wl,-soname,libhemlockbcm.so.1 \
    "$WORK/hemlockbcm.o" \
    -Wl,--whole-archive "$LIBDIR/libbcm.a" -Wl,--no-whole-archive \
    "$LIBDIR/libsoc.a" "$LIBDIR/libsal.a" \
    -lpthread -lm -lrt \
    || die "linking the shim failed"

# The one symbol the ABI promises. A shim that loads but exports nothing
# fails at dlopen time on the switch, which is a much worse place to find
# out than here.
if command -v "${CROSS_COMPILE}nm" >/dev/null; then
    "${CROSS_COMPILE}nm" -D --defined-only "$OUT" | grep -q hemlockbcm_get_api \
        || die "$OUT does not export hemlockbcm_get_api"
fi

log "built $OUT"
command -v file >/dev/null && file "$OUT"
cat <<EOF

Next: stage it into the image at the path the manifest pins
($(sed -n 's/^shim_path = "\(.*\)"/\1/p' "$ROOT/platforms/accton-as4610-54/platform.toml")).
EOF
