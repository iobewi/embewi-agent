#!/usr/bin/env bash
# Flash the local P2 image when present; otherwise fall back to the versioned P1 image.
# Use --p1 to force the credential-free reference image.
# The serial port is auto-detected by espflash; pass --port <dev> to force one.
set -euo pipefail

POC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST="$POC_DIR/dist-local"

if [[ "${1:-}" == "--p1" ]]; then
    DIST="$POC_DIR/dist"
    shift
elif [[ ! -f "$DIST/usb-radio-poc-esp32s3.bin" ]]; then
    DIST="$POC_DIR/dist"
fi

IMAGE="$DIST/usb-radio-poc-esp32s3.bin"
if [[ ! -f "$IMAGE" ]]; then
    echo "missing flash image: $IMAGE" >&2
    exit 1
fi

echo "flashing: $IMAGE"
espflash write-bin --chip esp32s3 "$@" 0x0 "$IMAGE"
espflash monitor --chip esp32s3 "$@"
