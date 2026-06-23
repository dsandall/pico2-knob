#!/usr/bin/env python3
"""Hand-route the 3 encoder nets freerouting couldn't thread (collinear pile-up under the
nano), and make the encoder GND pads solid-fill so S2 isn't a starved thermal.
Run AFTER apply_routes.py. Idempotent-ish: clears any existing tracks on these 3 nets first.
"""
import pcbnew, os
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BRD = os.path.join(ROOT, "hardware", "pico2-knob.kicad_pcb")
b = pcbnew.LoadBoard(BRD)
FCu, BCu = pcbnew.F_Cu, pcbnew.B_Cu

def pad(ref, num):
    for fp in b.GetFootprints():
        if fp.GetReference() == ref:
            for p in fp.Pads():
                if p.GetNumber() == num: return p
def xy(p): return (pcbnew.ToMM(p.GetPosition().x), pcbnew.ToMM(p.GetPosition().y))
def V(x, y): return pcbnew.VECTOR2I(pcbnew.FromMM(x), pcbnew.FromMM(y))

def run_track(netname, pts, layer, w=0.25):
    net = b.FindNet(netname)
    for i in range(len(pts) - 1):
        t = pcbnew.PCB_TRACK(b)
        t.SetStart(V(*pts[i])); t.SetEnd(V(*pts[i + 1]))
        t.SetWidth(pcbnew.FromMM(w)); t.SetLayer(layer); t.SetNet(net)
        b.Add(t)

# clear any stray track segments already on these nets (from the failed autoroute)
for tr in list(b.GetTracks()):
    if tr.GetNetname() in ("ENC_A", "ENC_B", "ENC_SW"):
        b.Remove(tr)

a = xy(pad("RE1", "A")); bb = xy(pad("RE1", "B")); s1 = xy(pad("RE1", "S1"))
n24 = xy(pad("U1", "24")); n23 = xy(pad("U1", "23")); n12 = xy(pad("U1", "12"))

# ENC_A: short straight drop on back copper
run_track("ENC_A", [n24, a], BCu)
# ENC_B: front copper, jog left around pads A & C (both at x~142.5)
run_track("ENC_B", [n23, (138.0, n23[1]), (138.0, bb[1]), bb], FCu)
# ENC_SW: back copper, jog right around GND pad S2
run_track("ENC_SW", [n12, (161.0, n12[1]), (161.0, s1[1]), s1], BCu)

# encoder GND pads (C, S2) -> solid zone connection (S2 is hemmed in; needs all the pour
# contact it can get). min_resolved_spokes is relaxed to 1 in the .kicad_pro so its single
# thermal spoke passes DRC.
for num in ("C", "S2"):
    pad("RE1", num).SetLocalZoneConnection(pcbnew.ZONE_CONNECTION_FULL)

pcbnew.ZONE_FILLER(b).Fill(b.Zones())
b.Save(BRD)
print("hand-routed ENC_A/ENC_B/ENC_SW; encoder GND pads set solid; re-filled")
