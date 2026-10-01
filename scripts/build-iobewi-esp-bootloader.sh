#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# The bootloader now lives in the iobewi monorepo (provisional subtree
# iobewi-esp/); same revision as the iobewi crates pinned in Cargo.toml.
IOBEWI_REPO="https://github.com/iobewi/iobewi"
IOBEWI_REV="7141b8b545b86fa1ddaa82179dacd86763f8a65f"
CHECKOUT="$ROOT/target/iobewi-esp-bootloader-src"
CHIP=esp32s3
TARGET=xtensa-esp32s3-none-elf

if [[ "$(git -C "$CHECKOUT" remote get-url origin 2>/dev/null || true)" != "$IOBEWI_REPO" ]]; then
  rm -rf "$CHECKOUT"
  git clone --filter=blob:none --no-checkout "$IOBEWI_REPO" "$CHECKOUT"
fi
git -C "$CHECKOUT" fetch --depth 1 origin "$IOBEWI_REV"
git -C "$CHECKOUT" checkout --detach --force "$IOBEWI_REV" >/dev/null

cd "$CHECKOUT/iobewi-esp/bootloader/esp"
cargo +esp build --release --locked --features "$CHIP" -Z build-std=core,alloc --target "$TARGET"
printf "%s\n" "$PWD/target/$TARGET/release/iobewi-esp-bootloader"
