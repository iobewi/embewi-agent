#!/usr/bin/env bash
# Build the ESP32-S3-N16R8 factory image:
#   ota_0 = disposable embewi-init
#   ota_1 = preloaded embewi-agent
# ESP Web Tools flashes both parts on a clean device. embewi-init verifies
# ota_1 exact size/SHA-256 before staging/activating it through IOBEWI OTA.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=xtensa-esp32s3-none-elf
FLASH_ARGS=(--chip esp32s3 --flash-mode dio --flash-size 16mb --flash-freq 40mhz)
OUT_DIR=web/firmware/esp32s3
FACTORY="$OUT_DIR/firmware.bin"
AGENT_BIN="$OUT_DIR/agent.bin"
APP_BIN="$OUT_DIR/app.bin"

echo "== iobewi-esp ESP bootloader"
BOOT_ELF="$(bash scripts/build-iobewi-esp-bootloader.sh)"
BOOT_BIN="target/iobewi-esp-bootloader.bin"
espflash save-image "${FLASH_ARGS[@]}" --ignore-app-descriptor     "$BOOT_ELF" "$BOOT_BIN"

echo "== embewi-agent"
cargo build --release --bin embewi-agent-esp
mkdir -p "$OUT_DIR"
espflash save-image "${FLASH_ARGS[@]}"     "target/${TARGET}/release/embewi-agent-esp" "$AGENT_BIN" >/dev/null
cp "$AGENT_BIN" "$APP_BIN"

AGENT_SIZE="$(stat -c%s "$AGENT_BIN")"
AGENT_SHA="$(sha256sum "$AGENT_BIN" | cut -d' ' -f1)"
AGENT_DIGEST="sha256:${AGENT_SHA}"
AGENT_DEPLOYMENT="factory-${AGENT_SHA:0:16}"

echo "== embewi-init"
EMBEWI_FACTORY_AGENT_SIZE="$AGENT_SIZE" EMBEWI_FACTORY_AGENT_DIGEST="$AGENT_DIGEST" EMBEWI_FACTORY_AGENT_DEPLOYMENT="$AGENT_DEPLOYMENT" cargo build --release --bin embewi-init

echo "== factory base image (bootloader + partitions + embewi-init in ota_0)"
espflash save-image "${FLASH_ARGS[@]}" --merge --skip-padding     --bootloader "$BOOT_BIN"     --partition-table partitions.csv     "target/${TARGET}/release/embewi-init" "$FACTORY"

# otadata intentionally remains blank in the factory image. The iobewi-esp
# bootloader executes the ESP EWBT lifecycle semantics, owns runtime boot state,
# and bootstraps ota_0 as Valid(seq=1) on first boot.
# ESP Web Tools adds AGENT_BIN separately at ota_1 (0x1a0000).
rm -f "$OUT_DIR/otadata.bin"

echo "Factory image : $FACTORY ($(stat -c%s "$FACTORY") bytes)"
echo "Agent ota_1   : $AGENT_BIN ($AGENT_SIZE bytes)"
echo "Agent digest  : $AGENT_DIGEST"
echo "Deployment    : $AGENT_DEPLOYMENT"
