"""Top cover for pico2-knob (live FreeCAD bridge; App/Part/Gui preloaded). Cap that seats on
the lower housing wall (z=+1.5 split) with a snap skirt+ridge into the lower's groove. Top
face above the OLED/buttons; cutouts for encoder shaft, OLED window, 3 buttons.
Feature centers (board frame, y negated): encoder (150,-108) shaft Ø6 / bushing Ø7 to z14.9;
OLED active 21x5 @ (149,-78) z7.7; buttons (133,-115)(150,-122)(167,-115) z5.9."""
import FreeCAD as App, Part
doc = App.getDocument("pico2_housing")
V = App.Vector
CX, CY = 150.0, -100.0
R_out, R_in = 40.5, 38.0
z_split = 1.5            # seats on lower wall top
z_top   = 8.5           # underside of the top face (above OLED z7.8 / buttons z5.9)
top_t   = 2.0
for o in list(doc.Objects):
    if o.Label.startswith("Top_Cover"): doc.removeObject(o.Name)

cap = Part.makeCylinder(R_out, (z_top + top_t) - z_split, V(CX, CY, z_split))
cap = cap.cut(Part.makeCylinder(R_in, z_top - z_split, V(CX, CY, z_split)))   # hollow under the top
# snap skirt down into the lower rim + ridge that clicks into the lower's groove
skirt = Part.makeCylinder(37.7, z_split, V(CX, CY, 0)).cut(Part.makeCylinder(36.0, z_split, V(CX, CY, 0)))
ridge = Part.makeCylinder(38.4, 0.8, V(CX, CY, 0.4)).cut(Part.makeCylinder(37.7, 0.8, V(CX, CY, 0.4)))
cover = cap.fuse(skirt).fuse(ridge)

def hole(x, y, r): return Part.makeCylinder(r, 12, V(x, y, z_top - 1))
cover = cover.cut(hole(150, -108, 4.0))                         # encoder shaft/bushing Ø8
for bx, by in [(133, -115), (150, -122), (167, -115)]:
    cover = cover.cut(hole(bx, by, 3.0))                        # buttons Ø6
oled = Part.makeBox(23, 7, 12, V(149.0 - 11.5, -78.0 - 3.5, z_top - 1))  # OLED window 23x7
cover = cover.cut(oled)

o = doc.addObject("Part::Feature", "Top_Cover")
o.Shape = cover
o.ViewObject.ShapeColor = (0.55, 0.55, 0.60); o.ViewObject.Transparency = 35
doc.recompute()
print(f"Top_Cover: Ø{2*R_out:.0f}, top face z{z_top}-{z_top+top_t}; shaft/OLED/3 button cutouts")
