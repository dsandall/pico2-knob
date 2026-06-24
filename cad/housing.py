"""Lower housing for pico2-knob, built in the live FreeCAD bridge session (App/Part/Gui
preloaded). Round tray a bit bigger than the lens PCB; PCB rests on 4 posts at the M3
mounting holes. Frame = imported board: center (150,-100), board top z=0, PCB bottom z=-1.6,
nano reaches z=-11.1, USB-C front at y=-64.4 (z -11..-7.8), encoder shaft to z=+28.
Pipe to fc.py; re-run to regenerate (clears the previous Lower_Housing)."""
import FreeCAD as App, Part
doc = App.getDocument("pico2_housing")
V = App.Vector

# ---- parameters ----
CX, CY    = 150.0, -100.0
R_in      = 38.0
wall      = 2.5
R_out     = R_in + wall
pcb_bot   = -1.6
floor_top = -14.0
floor_t   = 1.6
z_bot     = floor_top - floor_t
wall_top  = 1.5
post_r, insert_r, insert_d = 3.5, 2.0, 5.0
HOLES = [(167, -80), (134, -87), (131, -126), (169, -126)]
# snap-fit groove on the inner rim (upper cover's ridge clicks in)
sn_z, sn_h, sn_depth = 0.4, 0.8, 0.7
# USB-C charging cutout (nano's USB-C, top edge, below PCB)
usb_x, usb_w = 150.0, 13.0
usb_zc, usb_h = -9.4, 7.0

for o in list(doc.Objects):
    if o.Label.startswith("Lower_Housing"): doc.removeObject(o.Name)

outer = Part.makeCylinder(R_out, wall_top - z_bot, V(CX, CY, z_bot))
cav   = Part.makeCylinder(R_in, (wall_top + 1) - floor_top, V(CX, CY, floor_top))
tray  = outer.cut(cav)
for px, py in HOLES:                                   # posts + insert pockets
    post = Part.makeCylinder(post_r, pcb_bot - floor_top, V(px, py, floor_top))
    ins  = Part.makeCylinder(insert_r, insert_d, V(px, py, pcb_bot - insert_d))
    tray = tray.fuse(post.cut(ins))
# battery corral: 40x30 LiPo on the floor, lower half. 30 across (X) to clear the bottom
# posts, 40 long (Y); wires exit top-left corner toward J2 -> leave that corner open.
bcx, bcy, bw, bl = 150.0, -113.0, 30.0, 40.0
rib_t, rib_h = 1.5, 4.0
x0, x1 = bcx - bw/2, bcx + bw/2
y0, y1 = bcy - bl/2, bcy + bl/2
for r in [Part.makeBox(bw + 2*rib_t, rib_t, rib_h, V(x0 - rib_t, y0 - rib_t, floor_top)),  # bottom
          Part.makeBox(rib_t, 16, rib_h, V(x0 - rib_t, bcy - 8, floor_top)),               # left mid
          Part.makeBox(rib_t, 16, rib_h, V(x1, bcy - 8, floor_top)),                       # right mid
          Part.makeBox(12, rib_t, rib_h, V(x1 - 12, y1, floor_top))]:                      # top-right
    tray = tray.fuse(r)

# snap groove: ring channel cut into the inner wall near the top
groove = Part.makeCylinder(R_in + sn_depth, sn_h, V(CX, CY, sn_z)).cut(
         Part.makeCylinder(R_in, sn_h, V(CX, CY, sn_z)))
tray = tray.cut(groove)
# USB-C cutout through the +Y (top) wall
usb = Part.makeBox(usb_w, 10, usb_h, V(usb_x - usb_w/2, -67, usb_zc - usb_h/2))
tray = tray.cut(usb)

o = doc.addObject("Part::Feature", "Lower_Housing")
o.Shape = tray
o.ViewObject.ShapeColor = (0.35, 0.35, 0.40); o.ViewObject.Transparency = 50
doc.recompute()
print(f"Lower_Housing: Ø{2*R_out:.0f} outer, snap groove + USB-C cutout")
