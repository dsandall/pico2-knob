#!/usr/bin/env bash
# Render schematic + PCB to images so the agent can SEE its work and iterate.
# Outputs SVG (vector, crisp) + a PNG of the board for quick visual inspection.
set -euo pipefail
HW="$(cd "$(dirname "$0")/../hardware" && pwd)"
OUT="$(cd "$(dirname "$0")/../build" && pwd)"
SCH="$HW/pico2-knob.kicad_sch"
PCB="$HW/pico2-knob.kicad_pcb"

if [ -f "$SCH" ]; then
  kicad-cli sch export svg --no-background-color -o "$OUT/sch" "$SCH"
  echo "schematic -> build/sch/"
fi
if [ -f "$PCB" ]; then
  kicad-cli pcb export svg --page-size-mode 2 --exclude-drawing-sheet \
    -l F.Cu,B.Cu,F.SilkS,Edge.Cuts -o "$OUT/pcb.svg" "$PCB"
  echo "pcb -> build/pcb.svg"
fi
