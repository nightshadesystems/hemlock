#!/bin/sh
# fetch-vendor.sh — place vendor data files for a platform.
#
# Usage: vendor/fetch-vendor.sh <platform-id>
#
# Public platform data (config.bcm, .soc files) is downloaded from
# sonic-buildimage. Proprietary blobs (libsaibcm .deb) cannot be downloaded
# here; drop them into vendor/sai/ yourself (see vendor/sai/README.md).
set -eu

PLATFORM="${1:?usage: fetch-vendor.sh <platform-id>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PDIR="$ROOT/platforms/$PLATFORM"

[ -d "$PDIR" ] || { echo "error: unknown platform $PLATFORM" >&2; exit 1; }

fetch() {
    url="$1"; dest="$2"
    if [ -f "$dest" ]; then
        echo "have    $(basename "$dest")"
    else
        echo "fetch   $(basename "$dest")"
        curl -fsSL "$url" -o "$dest"
    fi
}

case "$PLATFORM" in
cel-e1031)
    BASE="https://raw.githubusercontent.com/sonic-net/sonic-buildimage/202211/device/celestica/x86_64-cel_e1031-r0/Celestica-E1031-T48S4"
    fetch "$BASE/helix4-e1031-48x1G%2B4x10G.config.bcm" \
          "$PDIR/helix4-e1031-48x1G+4x10G.config.bcm"
    fetch "$BASE/sai_postinit_cmd.soc" "$PDIR/sai_postinit_cmd.soc"
    echo
    echo "Platform data placed. Reminder: the libsaibcm .deb (pin: 3.7.x-helix4)"
    echo "must be provided manually in vendor/sai/ — see vendor/sai/README.md."
    ;;
*)
    echo "error: no fetch recipe for $PLATFORM (add one to this script)" >&2
    exit 1
    ;;
esac
