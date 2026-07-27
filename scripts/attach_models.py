#!/usr/bin/env python3
"""Attach the nano + encoder STEP models to their footprints (KiCad ships no 3D model
for either: U1's marbastlib footprint has none, and RE1 inherits a stock Alps EC11E
path that isn't installed). Model paths are project-relative (${KIPRJMOD}/../mechanical/step)
so the repo stays portable. Offsets/rotations are footprint-local, so they hold regardless
of where the part sits on the board. Edit TRANSFORMS and re-run.

Usage (run with SYSTEM python3 that has `import pcbnew`):
    python3 scripts/attach_models.py [board.kicad_pcb]
Defaults to board_pico2knob/pico2-knob.kicad_pcb. The board must NOT be open in KiCad.
"""
import os
import sys

import pcbnew

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_BRD = os.path.join(REPO, "board_pico2knob", "pico2-knob.kicad_pcb")
MODEL_DIR = "${KIPRJMOD}/../mechanical/step/"

# ref: (filename, offset_mm (x,y,z), rotation_deg (x,y,z))
TRANSFORMS = {
    "RE1": ("encoder.step",  (7.5, -2.5, 0.0), (0, 0, 90)),   # shaft -> footprint origin
    "U1":  ("nicenano.step", (0.0,  0.0, 8.5), (0, 0, 90)),   # back side; +8.5mm socket standoff
    # OLED is now off-board (ER-OLED1.12-2 via the FPC J3/J4), so oled.step/J1 is dropped.
}


def main(brd_path):
    b = pcbnew.LoadBoard(brd_path)
    touched = 0
    for fp in b.GetFootprints():
        ref = fp.GetReference()
        if ref not in TRANSFORMS:
            continue
        fname, off, rot = TRANSFORMS[ref]
        m = pcbnew.FP_3DMODEL()
        m.m_Filename = MODEL_DIR + fname
        m.m_Offset = pcbnew.VECTOR3D(*off)
        m.m_Rotation = pcbnew.VECTOR3D(*rot)
        m.m_Scale = pcbnew.VECTOR3D(1, 1, 1)
        m.m_Show = True
        models = fp.Models()
        while len(models) > 0:
            models.pop()
        models.push_back(m)
        print(f"{ref}: {fname} off={off} rot={rot}")
        touched += 1
    b.Save(brd_path)
    print(f"saved {brd_path} ({touched} footprints updated)")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else DEFAULT_BRD)
