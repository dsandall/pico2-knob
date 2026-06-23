# pico2-knob

A small **wireless BLE control puck**: a knob (rotary encoder), an OLED screen, and three
buttons, on a round Ø80 mm 2-layer PCB, powered by a **nice!nano v2** (nRF52840) + LiPo.

> Name is legacy — it started as an RP2350/Pico 2 board, then moved to the nice!nano for
> wireless. Firmware is out of scope here; this repo is the hardware (schematic + layout) + a
> rough mechanical mockup.

## Open it
`hardware/pico2-knob.kicad_pro` in **KiCad 10**. The schematic embeds its symbols and the
board embeds its footprints, so it opens standalone; `lib/marbastlib` (nice!nano sym/fp) and
the project lib-tables are included so libraries resolve cleanly too.

## Board summary
- **MCU:** nice!nano v2 — mounted on the **back**, on female sockets, USB-C facing the top
  rim (charge/flash; the device is otherwise sealed and battery-powered over BLE).
- **Encoder:** Alps EC11 w/ pushbutton, shaft at the puck center.
- **Buttons:** 3 × 6 mm tactile (SW_PUSH_6mm), in an arc below the knob.
- **Display:** 0.87" 128×32 SSD1316 I²C OLED module on a 4-pin female socket (J1).
- **Power:** LiPo on a JST-PH (J2) → nice!nano on-board charger (BAT+/GND). No power switch
  (nRF52 sleeps at µA; add an inline slide switch in VBAT if you want true-off).
- **Outline:** Ø80 mm round (bumped up from Ø74 for fab edge-clearance on the nano pads).
- 2-layer, GND pours both sides, 55 tracks, 0 vias. **DRC: 0 errors.**

## Net / GPIO map (nRF52 pads)
| Signal | nice!nano pad |
|--------|---------------|
| Encoder A / B / SW | P0.09 / P0.10 / P1.06 (pads 24 / 23 / 12) |
| Buttons 1 / 2 / 3 | P0.24 / P1.00 / P0.11 (pads 8 / 9 / 10) |
| OLED SDA / SCL | P0.17 / P0.20 (pads 5 / 6), 4.7 kΩ pull-ups (R1/R2) |
| Power | 3V3 (pad 16), GND, BAT+ → VBAT |

## For the reviewer 👀
This layout was **script-generated** (see `scripts/`) and auto-routed (freerouting), then
hand-touched at the encoder. Please sanity-check freely — a few notes:
- **2 DRC items are intentionally downgraded to warnings:** `pth_inside_courtyard` between
  the back-side socketed nano and the front-side encoder courtyard. The nano sits ~8.5 mm
  below the board on sockets, so there's no real clash — but please confirm you agree.
- The encoder signal nets stack collinearly under the nano; they're hand-routed
  (`scripts/manual_route.py`) and S2's GND return is a manual trace into the south pour
  (`scripts/fix_s2.py`).
- **Not yet done:** mounting features (holes/standoffs) — coming after the enclosure.
- Pin assignment is flexible (nRF52 GPIOs are interchangeable) — happy to remap for cleaner
  routing if you see a better arrangement.

## Layout of this repo
- `hardware/` — KiCad 10 project (schematic, PCB, lib-tables)
- `lib/marbastlib/` — nice!nano symbol + footprint (trimmed to what's used)
- `scripts/` — generators: `gen_sch.py`, `gen_pcb.py`, routing helpers; `*.sh` render/ERC/DRC
- `cad/` — FreeCAD mechanical mockup (`mockup.py`) + component STEP models (`step/`)
- `build/pico2-knob.pdf` — quick-look: schematic + routed layout
