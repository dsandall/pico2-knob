#!/usr/bin/env bash
# Cargo runner: ELF -> UF2 -> board, without touching the reset button.
#
#   cargo run --release            # via `runner` in .cargo/config.toml
#   ./flash.sh <path-to-elf>       # the same thing by hand
#   MONITOR=1 cargo run --release  # attach picocom once it's back
#   WAIT=40 cargo run --release    # seconds to wait for the drive (default 20)
#
# If the app is running we ask it to reboot into UF2 mode; if the board is
# already in the bootloader we just copy. Only a board sitting in the
# bootloader's serial-only mode (single tap, no drive) needs a double-tap.
set -euo pipefail

ELF=$(realpath "${1:?usage: flash.sh <elf>}")
cd "$(dirname "$(realpath "$0")")"
OUT=out
WAIT=${WAIT:-20}
mkdir -p "$OUT"

# Name the artifact after where it links, so a --features app-offset-1000 build
# can't be mistaken for the default one.
ORIGIN=$(rust-objdump -h "$ELF" | awk '$2==".vector_table" {print $4}')
case "$ORIGIN" in
  00026000) NAME=pico2joy-bringup ;;
  00001000) NAME=pico2joy-bringup-0x1000 ;;
  *)        NAME=pico2joy-bringup-0x$ORIGIN ;;
esac

# Ask the ELF which radio it has rather than trusting a flag, so the two builds
# can never overwrite each other's UF2.
if rust-nm "$ELF" 2>/dev/null | grep -q "MPSL_IRQ\|mpsl_init\|sdc_init"; then
  NAME="$NAME-ble"
fi

rust-objcopy -O ihex "$ELF" "$OUT/$NAME.hex"
rust-objcopy -O binary "$ELF" "$OUT/$NAME.bin"
cargo-hex-to-uf2 hex-to-uf2 -i "$OUT/$NAME.hex" -o "$OUT/$NAME.uf2" -f nrf52840
echo "built $OUT/$NAME.uf2 - app at 0x$ORIGIN, $(stat -c%s "$OUT/$NAME.bin") bytes of flash"

app_port() { ls /dev/serial/by-id/*pico2joy* 2>/dev/null | head -1; }

# The bootloader's own CDC port, present after a *single* tap. It speaks nRF
# serial DFU, not UF2, so the drive still needs a double-tap.
bootloader_port() {
  ls /dev/serial/by-id/*nRF52840* /dev/serial/by-id/*nice*nano* 2>/dev/null | head -1
}

uf2_drive() {
  local c
  for c in /run/media/"$USER"/* /media/"$USER"/* /media/* /mnt/*; do
    [[ -f "$c/INFO_UF2.TXT" ]] && { echo "$c"; return 0; }
  done
  return 1
}

# udisks mounts removable media without root, for desktops that don't automount.
mount_uf2_drive() {
  command -v udisksctl >/dev/null || return 1
  local dev
  for dev in $(lsblk -rno NAME,LABEL,RM |
      awk '$3==1 && ($2=="NICENANO" || $2=="FTHR840BOOT" || $2=="NRF52BOOT") {print "/dev/"$1}'); do
    udisksctl mount -b "$dev" >/dev/null 2>&1 && return 0
  done
  return 1
}

reboot_into_bootloader() {
  local port=$1
  echo "asking $port to reboot into the bootloader"
  # 'b' is this firmware's own command; the 1200-baud touch is the universal UF2
  # convention, and covers a build that predates the command.
  timeout 2 sh -c "printf b > '$port'" 2>/dev/null || true
  sleep 1
  [[ -e "$port" ]] || return 0
  timeout 2 stty -F "$port" 1200 </dev/null >/dev/null 2>&1 || true
  sleep 1
}

DRIVE=$(uf2_drive || true)
if [[ -z "$DRIVE" ]]; then
  PORT=$(app_port || true)
  if [[ -n "$PORT" ]]; then
    reboot_into_bootloader "$PORT"
  elif [[ -n "$(bootloader_port || true)" ]]; then
    echo "board is in the bootloader's serial-only mode ($(bootloader_port))"
    echo "-> double-tap the reset button now to get the UF2 drive"
  else
    echo "no pico2joy port found - plug in USB-C, or double-tap reset"
  fi
  printf 'waiting for the UF2 drive'
  for _ in $(seq $((WAIT * 2))); do
    DRIVE=$(uf2_drive || true)
    [[ -n "$DRIVE" ]] && break
    mount_uf2_drive || true
    printf '.'
    sleep 0.5
  done
  echo
fi

if [[ -z "$DRIVE" ]]; then
  echo "no UF2 drive appeared. Double-tap reset on the nice!nano and re-run;" >&2
  echo "the drive is labelled NICENANO or FTHR840BOOT." >&2
  exit 1
fi

echo "--- $DRIVE/INFO_UF2.TXT ---"
sed -n '1,6p' "$DRIVE/INFO_UF2.TXT" || true
echo '---'

# The bootloader reboots the instant it has the last block, so the copy and the
# sync that follows it often report an error on a drive that has already gone.
cp "$OUT/$NAME.uf2" "$DRIVE/" 2>/dev/null || true
sync 2>/dev/null || true
echo "copied $NAME.uf2"

printf 'waiting for the app to come up'
PORT=""
for _ in $(seq 30); do
  PORT=$(app_port || true)
  [[ -n "$PORT" ]] && break
  printf '.'
  sleep 0.5
done
echo

if [[ -z "$PORT" ]]; then
  cat >&2 <<'MSG'
the app's serial port didn't appear. Two usual reasons:
  - first flash on a fresh chip: the firmware writes UICR.NFCPINS and resets
    once, which can land back in the bootloader. Tap reset and check again.
  - your bootloader wants the app at 0x1000: build with --features app-offset-1000.
MSG
  exit 1
fi

# Leave the port at a sane speed. A recycled ttyACM node keeps the termios of
# its previous life, so a stale 1200 from the touch above would otherwise be
# re-applied to the *app* by whatever opens it next.
timeout 2 stty -F "$PORT" 115200 raw -echo </dev/null >/dev/null 2>&1 || true

echo "running: $PORT"
if [[ "${MONITOR:-0}" == 1 ]]; then
  exec picocom -b 115200 "$PORT"
fi
