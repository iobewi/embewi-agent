#!/usr/bin/env bash
# Flash dist/usb-radio-poc-esp32s3.bin (merged image, offset 0x0) and open the monitor.
# The serial port is auto-detected by espflash; pass --port <dev> to force one.
# Use the board's USB-UART port (not the native OTG port): it carries flashing, logs
# and Improv Serial provisioning.
set -euo pipefail

POC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="$POC_DIR/dist/usb-radio-poc-esp32s3.bin"
if [[ ! -f "$IMAGE" ]]; then
    echo "missing flash image: $IMAGE" >&2
    exit 1
fi

echo "flashing: $IMAGE"
espflash write-bin --chip esp32s3 "$@" 0x0 "$IMAGE"
espflash monitor --chip esp32s3 "$@"
