# pico2joy bring-up firmware

Bring-up firmware for the assembled puck: **no battery, no BLE**. It exists to
answer "did I solder it right?" — it blinks the nice!nano's LED, drives the
SH1107 ribbon OLED, and streams every encoder detent and button edge over USB
serial while mirroring the same state on the screen.

Rust + [Embassy] on the nice!nano v2 (nRF52840), flashed as a UF2 over the stock
bootloader — no debugger or SWD pads needed.

[Embassy]: https://embassy.dev

## Build and flash

```sh
cargo run --release              # build -> UF2 -> reboot the board -> flash it
MONITOR=1 cargo run --release    # ...and attach picocom when it comes back
```

`cargo run` goes through `flash.sh` (wired up as the cargo `runner`). It packages
the ELF as a UF2, asks the running firmware to reboot into UF2 mode — via its `b`
command, falling back to the 1200-baud touch — waits for the drive, copies, and
waits for the app's serial port to reappear. `udisksctl` is used to mount the
drive if your desktop doesn't automount it.

A board that has never been flashed can't reboot itself, so the first time
(or any time you see "board is in the bootloader's serial-only mode"):

1. Plug in USB-C.
2. **Double-tap** the reset button on the nice!nano. A drive named `NICENANO` (or
   `FTHR840BOOT`) appears — a single tap only gets you the bootloader's serial
   port, no drive.
3. `cargo run --release`, or drag `out/pico2joy-bringup.uf2` onto the drive.

The script waits 20 s for the drive, so you can start it and then double-tap;
`WAIT=40` if you want longer. Build without flashing:

```sh
./build.sh                       # -> out/pico2joy-bringup.uf2
./build.sh --offset-1000         # the alternate flash layout, see below
```

The first boot after flashing writes `UICR.NFCPINS` (to free P0.09/P0.10 from the
NFC block, where encoder A/B live) and resets itself once. If that reset lands in
the bootloader's double-tap window, the board comes up in the bootloader instead —
just tap reset once and it runs. This happens at most once per chip.

## Use

Open the app's serial port — a *second* `/dev/ttyACM*` distinct from the
bootloader's, `1209:0001 softek pico2joy bring-up`:

```sh
picocom -b 115200 /dev/ttyACM1     # baud is ignored, it's USB CDC
```

Opening the port prints a banner; then events stream as you use the puck:

```
[   12.345] BTN1 down
[   12.501] BTN1 up
[   13.002] ENC cw  detents=1
[   13.410] ENC_SW down
```

Single-key commands:

| key   | does                                                                |
|-------|---------------------------------------------------------------------|
| `?`   | help                                                                |
| `p`   | print current levels of every input, plus detents and 12V_EN        |
| `d`   | toggle the orientation test pattern                                 |
| `i`   | re-init the display (reset + full init sequence)                    |
| `f`   | flip the panel 180 degrees (COM/segment remap)                      |
| `+`/`-` | contrast, in steps of 0x10 from the vendor default 0x4F           |
| `e`   | toggle the 12 V panel rail — panel off first on the way down        |
| `v`   | verbose: log every encoder quadrature transition, not just detents  |
| `l`   | hold the LED on (heartbeat otherwise: 60 ms every second)           |
| `b`   | reboot into the UF2 bootloader                                      |

## What this actually tests

- nice!nano seated in its sockets, USB and the on-module LED alive.
- All three buttons, the encoder push, and both encoder phases. A cold joint on
  one encoder phase shows up as `step=0` noise under `v` instead of clean detents.
- The switch pull-ups at boot: `p` flags anything already reading low (a held
  button, or a short to GND on that net).
- **The SH1107 panel** on the J3 ribbon: the boot splash is an orientation test
  pattern (border, both diagonals, a solid block and `TL` in the top-left), then
  it switches to a live view — encoder ring with the detent count in the middle,
  a pip per button, and a `12V` flag.
- **U2, the 12 V boost**, which the panel needs as external VPP (11.5–12.5 V, the
  panel's own DC-DC stays off via `0xAD 0x8A`). Brought up after the controller
  is initialised and dropped before it, and `e` toggles it.

Untested: the LiPo path (J2, `VBAT`). No BLE.

### If the screen looks wrong

- **Mirrored or upside down** — press `f`. That flips COM scan and segment remap
  (`0xC8 0xA1` vs `0xC0 0xA0`), which is the assembly-orientation case.
- **Rotated 90 degrees** — already handled. This panel's RAM sits 90 degrees to
  the glass (an SH1107 page runs along the horizontal), and the controller only
  offers 180, so `Display::set_pixel` rotates on the way into the framebuffer:
  logical `(x, y)` lands at panel column `y`, row `127 - x`. If a future panel
  wants the plain mapping, that one line is the only thing to change — and note
  a bare transpose (column `y`, row `x`) rotates *and* mirrors.
- **Nothing at all, or faint** — check TP4 for ~12 V (`e`), then try `+` a few
  times. VPP below ~11.5 V reads as a dim or blank panel rather than an error.
- **Garbled but alive** — drop `spim::Frequency::M4` in `main.rs` to `M2`.

## Pinout

| Signal    | nRF52840 | pad | Notes                                       |
|-----------|----------|-----|---------------------------------------------|
| LED       | P0.15    |  -  | on-module blue LED, active high             |
| VCC_EN    | P0.13    |  -  | nice!nano load switch: high = 3V3 pad live  |
| ENC_A     | P0.09    | 24  | NFC pin, freed via `nfc-pins-as-gpio`       |
| ENC_B     | P0.10    | 23  | likewise                                    |
| ENC_SW    | P1.11    | 22  |                                             |
| BTN1      | P0.31    | 17  |                                             |
| BTN2      | P0.02    | 19  |                                             |
| BTN3      | P1.15    | 20  |                                             |
| OLED_DC   | P0.06    |  1  | SH1107, 4-wire SPI over the J3 ribbon       |
| OLED_SCLK | P0.08    |  2  |                                             |
| OLED_SDI  | P0.17    |  5  |                                             |
| OLED_RES  | P0.22    |  7  | 10k pull-up R2: boots out of reset          |
| OLED_CS   | P0.24    |  8  | 10k pull-up R6: boots deselected            |
| 12V_EN    | P1.06    | 12  | Q1 gate, **active low** — see below         |

Taken from `board_pico2knob/pico2-knob.kicad_sch` (the checked-in
`bom_netlist.xml` is older than the ribbon-OLED rework — don't use it).

### 12V_EN is active low, and on this revision "off" isn't off

R8 pulls Q1's gate up to +BATT, so the rail is off with the gate high and on with
it pulled down. 3V3 is *not* a clean off — it leaves Vgs near -0.9 V, inside the
DMG3415U's threshold band — so the firmware drives the pin **low to enable** and
leaves it **disconnected (high-Z) to disable**, never high.

That said, Q1 looks flipped, so the rail can't actually be gated on this build:

- the schematic has Q1's **drain** on +BATT and its **source** on U2's Vin;
- the PCB footprint renames the SOT-23 pads `G`/`S`/`D` onto pads 1/2/3, which is
  exactly the DMG3415U's real pinout — so the physical drain is the +BATT one.

A P-channel high-side switch needs its *source* on the supply. Wired this way the
body diode (drain → source) conducts +BATT into U2's Vin whenever a cell or USB is
present, so U2 free-runs and the 12 V rail is always live, burning its quiescent
current on battery. `e` then only removes the diode drop rather than switching
anything.

To confirm on the bench: meter TP4 right after boot, before touching `e`. ~12 V
means the diode path is feeding U2. The rev-2 fix is swapping Q1's source and
drain (or dropping the FET and using U2's own shutdown).

## Flash layout / the two UF2s

The stock bootloader starts the app at **0x26000**, above the (unused) SoftDevice
slot, and asks the MBR to forward interrupts there — the same slot ZMK uses. That
is `out/pico2joy-bringup.uf2`, and it's the one to use.

If the bootloader on your board was built without a SoftDevice it will expect the
app at 0x1000 instead, and copying the default UF2 will appear to do nothing (the
drive just remounts). `out/pico2joy-bringup-0x1000.uf2` (`./build.sh
--offset-1000`) is linked for that case. `INFO_UF2.TXT` on the bootloader drive
names the bootloader build if you want to check first.

Neither variant touches the bootloader itself (0xF4000+), so double-tap reset
always gets you back.

## Next

- Battery: read VDDH via SAADC for a rough LiPo percentage (the nice!nano v2
  senses the cell through VDDH, not a divider pin).
- Sleep: the panel and the 12 V rail dominate the power budget, so idle wants
  display-off plus VPP down — which needs the Q1 fix above to actually work.
- BLE HID over `nrf-softdevice` or `trouble`, which is where the app at 0x26000
  starts mattering: a SoftDevice build has to move up to 0x27000.
