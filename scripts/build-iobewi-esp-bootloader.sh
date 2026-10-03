#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# The bootloader lives in the iobewi monorepo (bootloader/esp); same revision as the iobewi crates pinned in Cargo.toml.
IOBEWI_REPO="https://github.com/iobewi/iobewi"
IOBEWI_REV="e7d66f4b775c9a7be7588c490d61a62dfaee5da0"
CHECKOUT="$ROOT/target/iobewi-esp-bootloader-src"
CHIP=esp32s3
TARGET=xtensa-esp32s3-none-elf

if [[ "$(git -C "$CHECKOUT" remote get-url origin 2>/dev/null || true)" != "$IOBEWI_REPO" ]]; then
  rm -rf "$CHECKOUT"
  git clone --filter=blob:none --no-checkout "$IOBEWI_REPO" "$CHECKOUT"
fi
git -C "$CHECKOUT" fetch --depth 1 origin "$IOBEWI_REV"
git -C "$CHECKOUT" checkout --detach --force "$IOBEWI_REV" >/dev/null

cd "$CHECKOUT/bootloader/esp"
cargo +esp build --release --locked --features "$CHIP" -Z build-std=core,alloc --target "$TARGET"
printf "%s\n" "$PWD/target/$TARGET/release/iobewi-esp-bootloader"
