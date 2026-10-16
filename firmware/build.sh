#!/usr/bin/env bash
# Build the bring-up firmware and package it as a UF2 for the nice!nano's
# bootloader. The default build has the connectable BLE link; --no-ble gives the
# broadcast-only one. Pass --flash to also copy it onto a mounted UF2 drive, and
# --offset-1000 to link over the SoftDevice slot (see README).
set -euo pipefail
cd "$(dirname "$0")"

FEATURES=()
FLASH=0
DFU=0
# --name BASE renames the outputs (BASE.uf2, BASE-noble.uf2, BASE-0x1000.uf2),
# so a candidate build can sit in out/ next to the one on the puck without
# overwriting it - the old UF2 is the rollback.
BASE=pico2joy-bringup
SUFFIX=
NEXT_IS_NAME=0
for arg in "$@"; do
  if [[ $NEXT_IS_NAME == 1 ]]; then BASE=$arg; NEXT_IS_NAME=0; continue; fi
  case "$arg" in
    --name) NEXT_IS_NAME=1 ;;
    --name=*) BASE=${arg#--name=} ;;
    --flash) FLASH=1 ;;
    --dfu) DFU=1 ;;
    --offset-1000) FEATURES+=(--features app-offset-1000); SUFFIX=-0x1000 ;;
    # The broadcast-only build wants the single-core critical section instead of
    # MPSL's, which means dropping the default features rather than adding to
    # them - the two implementations cannot both be linked.
    --no-ble) FEATURES+=(--no-default-features --features cs-single-core); SUFFIX=-noble ;;
    # What the BLE build used to be called, back when it was the exception.
    --ble) echo "note: BLE is the default now; --ble does nothing" >&2 ;;
    *) echo "usage: $0 [--flash] [--dfu] [--no-ble] [--offset-1000] [--name BASE]" >&2; exit 2 ;;
  esac
done
NAME=$BASE$SUFFIX

ELF=target/thumbv7em-none-eabihf/release/pico2-knob-fw
OUT=out
mkdir -p "$OUT"

cargo build --release "${FEATURES[@]+"${FEATURES[@]}"}"
rust-objcopy -O ihex "$ELF" "$OUT/$NAME.hex"
rust-objcopy -O binary "$ELF" "$OUT/$NAME.bin"
cargo-hex-to-uf2 hex-to-uf2 -i "$OUT/$NAME.hex" -o "$OUT/$NAME.uf2" -f nrf52840
rust-size "$ELF"
ls -l "$OUT/$NAME.uf2"

# A DFU package is what the bootloader's over-the-air update wants: the same
# image, plus the init packet naming the device type. 0x0052 is the nRF52840.
if [[ $DFU == 1 ]]; then
  adafruit-nrfutil dfu genpkg --dev-type 0x0052 \
    --application "$OUT/$NAME.hex" "$OUT/$NAME-dfu.zip"
  ls -l "$OUT/$NAME-dfu.zip"
fi

if [[ $FLASH == 1 ]]; then
  exec ./flash.sh "$ELF"
fi
