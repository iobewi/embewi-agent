#!/usr/bin/env bash
# Build the USB-radio firmware and produce a flashable image.
# The firmware carries no Wi-Fi credentials (provisioned at runtime via Improv Serial),
# so the output in dist/ is safe to version and share.
# Re-runnable; run from anywhere. Requires: rustup toolchains 1.95.0 + esp, espflash.
set -euo pipefail

POC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$POC_DIR"

TARGET=xtensa-esp32s3-none-elf
PKG=usb-radio-firmware
NAME=usb-radio-poc-esp32s3
DIST="$POC_DIR/dist"
BUILD_KIND="credential-free (Wi-Fi provisioned at runtime via Improv Serial)"
if [[ -n "${WIFI_SSID:-}${WIFI_PASSWORD:-}" ]]; then
    echo "note: WIFI_SSID/WIFI_PASSWORD are ignored; the firmware is provisioned via Improv Serial." >&2
fi
ELF_SRC="target/$TARGET/release/$PKG"

# Deterministic build: strip absolute paths of the checkout, cargo registry and
# toolchain sysroot from the ELF (debug info / panic locations).
SYSROOT="$(rustc +esp --print sysroot)"
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="-C force-frame-pointers \
 --remap-path-prefix=$POC_DIR=/src \
 --remap-path-prefix=$CARGO_HOME_DIR=/cargo \
 --remap-path-prefix=$SYSROOT=/sysroot"

# esp_app_desc embeds a build date/time; esp-bootloader-esp-idf honours
# SOURCE_DATE_EPOCH. Tie it to the last commit touching the POC sources
# (generated delivery directories excluded, so rebuilding artefacts does not change it).
SOURCE_DATE_EPOCH="$(git log -1 --format=%ct -- . ':(exclude)dist' ':(exclude)dist-local')"
export SOURCE_DATE_EPOCH

CORE_CMD="cargo +1.95.0 test -p usb-radio-core --target x86_64-unknown-linux-gnu"
BUILD_CMD="cargo +esp build -p $PKG --release -Z build-std=core,alloc --target $TARGET"
IMAGE_CMD="espflash save-image --chip esp32s3 --merge --skip-padding $ELF_SRC $DIST/$NAME.bin"

echo "== usb-radio-core tests"
$CORE_CMD
echo "== firmware release build"
$BUILD_CMD

mkdir -p "$DIST"
cp "$ELF_SRC" "$DIST/$NAME.elf"
echo "== flashable image"
$IMAGE_CMD

( cd "$DIST" && sha256sum "$NAME.bin" "$NAME.elf" > SHA256SUMS )

COMMIT="$(git rev-parse HEAD)"
DIRTY=""
[ -n "$(git status --porcelain --untracked-files=no -- "$POC_DIR" ':(exclude)dist' ':(exclude)dist-local')" ] && DIRTY=" (+ uncommitted source changes under poc/usb-radio)"
{
  echo "repository:   $(git remote get-url origin 2>/dev/null || echo unknown)"
  echo "branch:       $(git rev-parse --abbrev-ref HEAD)"
  echo "source:       $COMMIT$DIRTY"
  echo "target:       $TARGET (ESP32-S3)"
  echo "profile:      release"
  echo "delivery:     $BUILD_KIND"
  echo "rustc (esp):  $(rustc +esp --version)"
  echo "cargo (esp):  $(cargo +esp --version)"
  echo "rustc (core): $(rustc +1.95.0 --version)"
  echo "espflash:     $(espflash --version)"
  echo
  echo "RUSTFLAGS:    $RUSTFLAGS"
  echo "SOURCE_DATE_EPOCH: $SOURCE_DATE_EPOCH"
  echo "build (cwd poc/usb-radio):"
  echo "  $CORE_CMD"
  echo "  $BUILD_CMD"
  echo "image (cwd poc/usb-radio):"
  echo "  $IMAGE_CMD"
  echo
  echo "flash layout: single merged image (bootloader + partition table + app) at 0x0"
  echo "  0x0 -> $NAME.bin"
  echo "  (espflash default bootloader and partition table; no custom partitions)"
  echo
  echo "sha256:"
  sed 's/^/  /' "$DIST/SHA256SUMS"
} > "$DIST/BUILD.txt"

echo "== done: $BUILD_KIND"
echo "output: $DIST"
cat "$DIST/SHA256SUMS"
