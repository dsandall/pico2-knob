#!/usr/bin/env bash
# Build the bring-up firmware and package it as a UF2 for the nice!nano's
# bootloader. Pass --flash to also copy it onto a mounted UF2 drive, and
# --offset-1000 to link over the SoftDevice slot (see README).
set -euo pipefail
cd "$(dirname "$0")"

FEATURES=()
FLASH=0
NAME=pico2joy-bringup
for arg in "$@"; do
  case "$arg" in
    --flash) FLASH=1 ;;
    --offset-1000) FEATURES+=(--features app-offset-1000); NAME=pico2joy-bringup-0x1000 ;;
    # The BLE build needs MPSL's critical-section implementation instead of the
    # single-core one, which means dropping the default features.
    --ble) FEATURES+=(--no-default-features --features ble); NAME=pico2joy-bringup-ble ;;
    *) echo "usage: $0 [--flash] [--ble] [--offset-1000]" >&2; exit 2 ;;
  esac
done

ELF=target/thumbv7em-none-eabihf/release/pico2-knob-fw
OUT=out
mkdir -p "$OUT"

cargo build --release "${FEATURES[@]+"${FEATURES[@]}"}"
rust-objcopy -O ihex "$ELF" "$OUT/$NAME.hex"
rust-objcopy -O binary "$ELF" "$OUT/$NAME.bin"
cargo-hex-to-uf2 hex-to-uf2 -i "$OUT/$NAME.hex" -o "$OUT/$NAME.uf2" -f nrf52840
rust-size "$ELF"
ls -l "$OUT/$NAME.uf2"

if [[ $FLASH == 1 ]]; then
  exec ./flash.sh "$ELF"
fi
