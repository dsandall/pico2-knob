#!/usr/bin/env python3
"""Connect encoder S2 (GND) to the pour. The encoder's no-fill island isolates S2 (0 pour
contact), and it's boxed in by ENC_SW pads/jog above & right and S1 below. Clear exit is
down-LEFT on F.Cu (ENC_SW jog is B.Cu) into the open pour south of the encoder.
No-Remove (pcbnew segfaults on track removal); run once."""
import pcbnew, os
BRD = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                   "hardware", "pico2-knob.kicad_pcb")
b = pcbnew.LoadBoard(BRD)
s2 = next(p for fp in b.GetFootprints() if fp.GetReference() == "RE1"
          for p in fp.Pads() if p.GetNumber() == "S2")
pts = [(pcbnew.ToMM(s2.GetPosition().x), pcbnew.ToMM(s2.GetPosition().y)),
       (153.0, 100.0), (153.0, 108.0)]   # down-left, then south into open GND pour
gnd = b.FindNet("GND")
for i in range(len(pts) - 1):
    t = pcbnew.PCB_TRACK(b)
    t.SetStart(pcbnew.VECTOR2I(pcbnew.FromMM(pts[i][0]), pcbnew.FromMM(pts[i][1])))
    t.SetEnd(pcbnew.VECTOR2I(pcbnew.FromMM(pts[i + 1][0]), pcbnew.FromMM(pts[i + 1][1])))
    t.SetWidth(pcbnew.FromMM(0.3)); t.SetLayer(pcbnew.F_Cu); t.SetNet(gnd)
    b.Add(t)
pcbnew.ZONE_FILLER(b).Fill(b.Zones())
b.Save(BRD)
print("S2 GND route added (F.Cu, down-left into south pour) + re-filled")
