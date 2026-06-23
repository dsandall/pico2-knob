#!/usr/bin/env bash
# ERC + DRC verify loop. Run after every schematic/PCB edit; read the reports.
set -euo pipefail
HW="$(cd "$(dirname "$0")/../hardware" && pwd)"
OUT="$(cd "$(dirname "$0")/../build" && pwd)"
SCH="$HW/pico2-knob.kicad_sch"
PCB="$HW/pico2-knob.kicad_pcb"

[ -f "$SCH" ] && kicad-cli sch erc --severity-error --severity-warning \
  --exit-code-violations -o "$OUT/erc.rpt" "$SCH" || echo "no schematic yet"
[ -f "$PCB" ] && kicad-cli pcb drc --severity-error --severity-warning \
  --exit-code-violations -o "$OUT/drc.rpt" "$PCB" || echo "no pcb yet"
echo "--- ERC ---"; [ -f "$OUT/erc.rpt" ] && cat "$OUT/erc.rpt" || true
echo "--- DRC ---"; [ -f "$OUT/drc.rpt" ] && cat "$OUT/drc.rpt" || true
