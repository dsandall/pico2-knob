#!/usr/bin/env python3
"""Import the freerouting .ses back onto the board, re-fill GND pours, save.
Run AFTER gen_pcb.py (which writes the board + exports nothing) and after freerouting:
  python3 scripts/gen_pcb.py
  pcbnew.ExportSpecctraDSN -> build/pico2-knob.dsn   (done inside the route pipeline)
  xvfb-run java -jar tools/freerouting.jar -de build/pico2-knob.dsn -do build/pico2-knob.ses
  python3 scripts/apply_routes.py
"""
import pcbnew, os
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BRD  = os.path.join(ROOT, "hardware", "pico2-knob.kicad_pcb")
SES  = os.path.join(ROOT, "build", "pico2-knob.ses")

b = pcbnew.LoadBoard(BRD)
ok = False
for call in (lambda: pcbnew.ImportSpecctraSES(b, SES), lambda: pcbnew.ImportSpecctraSES(SES)):
    try: call(); ok = True; break
    except Exception as e: last = e
assert ok, f"SES import failed: {last}"
pcbnew.ZONE_FILLER(b).Fill(b.Zones())     # re-pour GND around the new tracks
b.Save(BRD)
print("routes imported, GND re-filled, saved")
