#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IOBEWI_ESP_REV="a0602c1da227f02a6759ab1d643105e2eca5b9b0"
CHECKOUT="$ROOT/target/iobewi-esp-bootloader-src"
CHIP=esp32s3
TARGET=xtensa-esp32s3-none-elf

if [[ ! -d "$CHECKOUT/.git" ]]; then
  rm -rf "$CHECKOUT"
  git clone --filter=blob:none --no-checkout https://github.com/iobewi/iobewi-esp "$CHECKOUT"
fi
git -C "$CHECKOUT" fetch --depth 1 origin "$IOBEWI_ESP_REV"
git -C "$CHECKOUT" checkout --detach --force "$IOBEWI_ESP_REV" >/dev/null

cd "$CHECKOUT/bootloader/esp"
cargo +esp build --release --locked --features "$CHIP" -Z build-std=core,alloc --target "$TARGET"
printf "%s\n" "$PWD/target/$TARGET/release/iobewi-esp-bootloader"
