"""Rough mechanical mockup of pico2-knob, built in a live FreeCAD session via the
bridge (App/Gui are preloaded). Approximate bounding solids — enough to arrange the
parts and derive the PCB outline. Mount plane = top of carrier PCB = z0; parts grow +z.

Dims (mm), sources:
  Pico 2 (RP2350) board 51.0 x 21.0 x 1.0, micro-USB-B at one short end.
  OLED ER-OLEDM0.87-1W-I2C: PCB 38.0 x 12.0 x 1.2; glass 29.0 x 8.7, +2.8 proud,
    inset 6.0 from header end; active 21.356 x 5.324; 4-pin 2.54 header, pins down 6.0.
  Tactile SW_PUSH 6mm: 6 x 6 x 3.5 body, D3.5 plunger to ~5.0.
  EC11 encoder (vertical, H20 shaft): body 12 x 12 x 6.5; M7 bushing D7 x5; D6 shaft.
"""
import FreeCAD as App, FreeCADGui as Gui, Part, os

STEP_DIR = "/home/thebu/newhome/softek/pico2-knob/cad/step"
DOC = "pico2_knob_mockup"
# fresh doc each run (avoids stale-object iteration when groups cascade-delete children)
if DOC in App.listDocuments():
    App.closeDocument(DOC)
doc = App.newDocument(DOC)

def box(L, W, H, x=0, y=0, z=0):
    return Part.makeBox(L, W, H, App.Vector(x, y, z))
def cyl(d, h, x=0, y=0, z=0):
    return Part.makeCylinder(d / 2.0, h, App.Vector(x, y, z))

def component(name, parts, base):
    """parts: list of (shape, (r,g,b)[, transparency]).  base: (x,y,z) placement."""
    grp = doc.addObject("App::Part", name)
    grp.Label = name
    for i, p in enumerate(parts):
        shp, col = p[0], p[1]
        tr = p[2] if len(p) > 2 else 0
        f = doc.addObject("Part::Feature", f"{name}_{i}")
        f.Shape = shp
        f.ViewObject.ShapeColor = col
        if tr:
            f.ViewObject.Transparency = tr
        grp.addObject(f)
    grp.Placement.Base = App.Vector(*base)
    return grp

def step_part(name, filename, base, rot=None):
    """Import a STEP, group its bodies under one App::Part, place at base.
    rot = ((ax,ay,az), deg) optional rotation applied before base translation."""
    before = set(doc.Objects)
    Part.insert(os.path.join(STEP_DIR, filename), doc.Name)
    new = [o for o in doc.Objects if o not in before]          # imported bodies only
    newset = set(new)
    roots = [o for o in new if not any(p in newset for p in o.InList)]  # top-level only
    grp = doc.addObject("App::Part", name)
    for o in roots:
        grp.addObject(o)
    pl = App.Placement()
    if rot:
        pl.Rotation = App.Rotation(App.Vector(*rot[0]), rot[1])
    pl.Base = App.Vector(*base)
    grp.Placement = pl
    return grp

GREEN=(0.10,0.45,0.20); SILVER=(0.75,0.75,0.78); BLUE=(0.10,0.15,0.55)
BLACK=(0.06,0.06,0.07); SCREEN=(0.55,0.75,0.85); GREY=(0.25,0.25,0.27)
RED=(0.55,0.12,0.12); METAL=(0.62,0.62,0.65); CABLE=(0.85,0.80,0.30)

# --- MCU dev board: nice!nano v2 (real STEP; USB at -X, board bottom lifted to z0) ---
mcu = step_part("MCU_niceNano_v2", "nicenano.step", base=(0, 0, 0))
# flipped over (bottom-mount) + USB facing +Y top wall, tucked under the PCB
mcu.Placement = App.Placement(
    App.Vector(0, 16.4, -3.4),
    App.Rotation(App.Vector(0,0,1), -90).multiply(App.Rotation(App.Vector(1,0,0), 180)))

# ===== Arrangement: knob-centered round puck, control face on z=0, MCU+battery below =====
# --- Battery (PROVISIONAL ~300 mAh pouch) under PCB, opposite the nano ---
bat = component("Battery_LiPo_PROVISIONAL", [
    (box(25, 20, 5.0),                         (0.20,0.20,0.22)),
], base=(-7.5, -24, -9))

# --- OLED module STEP: above the knob, lifted on its socket standoff ---
oled = step_part("OLED_0.87", "oled.step", base=(-19, 16, 6))

# --- EC11 encoder STEP: dead center ---
enc = step_part("Encoder_EC11", "encoder.step", base=(0, 0, 0))

# --- 3 tactile buttons: arc below the knob ---
for i, (bx, by) in enumerate([(-17, -12), (0, -20), (17, -12)], start=1):
    component(f"Button_SW{i}", [
        (box(6, 6, 3.5, -3, -3, 0),            GREY),           # body
        (cyl(3.5, 1.5, 0, 0, 3.5),             RED),            # plunger
    ], base=(bx, by, 0))

# --- placeholder knob cap (Ø26) on the encoder shaft ---
kn = doc.addObject("Part::Feature", "Knob_cap_ref")
kn.Shape = Part.makeCylinder(13, 16, App.Vector(0, 0, 8))
kn.ViewObject.ShapeColor = (0.15, 0.15, 0.17)

# --- candidate PCB outline: Ø74 round, top at z0 (locked from arrangement) ---
disc = doc.addObject("Part::Feature", "PCB_outline_ref")
disc.Shape = Part.makeCylinder(37, 1.6, App.Vector(0, 0, -1.6))
disc.ViewObject.ShapeColor = (0.10, 0.35, 0.15); disc.ViewObject.Transparency = 60

doc.recompute()
try:
    Gui.activeDocument().activeView().viewIsometric()
    Gui.SendMsgToActiveView("ViewFit")
except Exception:
    pass
print("built", [o.Label for o in doc.Objects if o.TypeId == "App::Part"])
