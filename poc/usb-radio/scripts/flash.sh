#!/usr/bin/env bash
# Flash dist/usb-radio-poc-esp32s3.bin (merged image, offset 0x0) and open the monitor.
# The serial port is auto-detected by espflash; pass --port <dev> to force one.
# Use the board's USB-UART port (not the native OTG port) for the monitor.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../dist"
espflash write-bin --chip esp32s3 "$@" 0x0 usb-radio-poc-esp32s3.bin
espflash monitor --chip esp32s3 "$@"
