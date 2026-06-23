#!/usr/bin/env python3
"""Attach the nano + encoder STEP models to their footprints (KiCad has no 3D model for
either). Paths are project-relative (${KIPRJMOD}/../cad/step) so the repo stays portable.
Offsets/rotations tuned against `kicad-cli pcb render`. Edit the TRANSFORMS and re-run."""
import pcbnew, os
BRD = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                   "hardware", "pico2-knob.kicad_pcb")

# ref: (filename, offset_mm (x,y,z), rotation_deg (x,y,z))
TRANSFORMS = {
    "RE1": ("encoder.step",  (7.5, -2.5, 0.0), (0, 0, 90)),    # shaft -> footprint (7.5,2.5)
    "U1":  ("nicenano.step", (0.0,  0.0, 0.0), (0, 0, 90)),    # back-mounted; KiCad flips it
}

b = pcbnew.LoadBoard(BRD)
for fp in b.GetFootprints():
    ref = fp.GetReference()
    if ref not in TRANSFORMS:
        continue
    fname, off, rot = TRANSFORMS[ref]
    m = pcbnew.FP_3DMODEL()
    m.m_Filename = "${KIPRJMOD}/../cad/step/" + fname
    m.m_Offset = pcbnew.VECTOR3D(*off)
    m.m_Rotation = pcbnew.VECTOR3D(*rot)
    m.m_Scale = pcbnew.VECTOR3D(1, 1, 1)
    m.m_Show = True
    models = fp.Models()
    while len(models) > 0:
        models.pop()
    models.push_back(m)
    print(f"{ref}: {fname} off={off} rot={rot}")
b.Save(BRD)
print("saved")
