"""Split the `mouse` body into a two-part clamshell — fully parametric.

Run from the FreeCAD Python console with the housing doc active:

    exec(open("/home/thebu/newhome/softek/pico2-knob/mechanical/split_clamshell.py").read())

ROOT CAUSE of the original "slice gives 1 body" failure: the SubtractiveLoft
profiles (Sketch007/Sketch008) are full circles with parameter origin
AngleXU=0.  The seam of the resulting closed loft B-spline surfaces lands
exactly on the triple corner where the loft meets the SubtractivePipe surface
(x=+-32.23, y~70.1, z~-7.16), producing two 0.0018 mm^2 sliver faces pinned to
the seam.  Every OCC boolean touching z > -25.25 then silently no-ops.

FIX: rotate the circles' parameterization to AngleXU=pi/2.  Geometry is
unchanged (only the seam moves); all booleans then work.  Idempotent — safe to
re-run (e.g. after editing Sketch007/008 in the Sketcher GUI, which could in
principle renormalize AngleXU).
"""
import math

import FreeCAD as App
import Part
import Sketcher

SRC_LABEL = "mouse"       # body to split
DATUM = "DatumPlane"      # datum whose Z gives the split height (PCB top, -9.6)
LOWER_LABEL = "shell_lower"
UPPER_LABEL = "shell_upper"


def heal_loft_seam(doc, body):
    """Move the loft profile circles' seam off the degenerate triple corner."""
    loft = [f for f in body.Group if f.TypeId == "PartDesign::SubtractiveLoft"][0]
    for sk in (loft.Profile[0], loft.Sections[0][0]):   # Sketch007, Sketch008
        geos = sk.Geometry
        for g in geos:
            if isinstance(g, Part.Circle):
                g.AngleXU = math.pi / 2.0               # absolute -> idempotent
        sk.Geometry = geos                              # constraints untouched
        sk.solve()
    doc.recompute()
    App.Console.PrintMessage("healed %s: vol=%.1f valid=%s\n" % (
        body.Label, body.Shape.Volume, body.Shape.isValid()))


def make_half(doc, src_body, label, z_cut, keep_lower):
    """New body: clone of src_body + ThroughAll pocket removing one side."""
    tag = "Lower" if keep_lower else "Upper"
    b = doc.addObject("PartDesign::Body", "Shell%s" % tag)
    b.Label = label
    clone = doc.addObject("PartDesign::FeatureBase", "ShellClone%s" % tag)
    b.addObject(clone)          # must precede BaseFeature: addObject resets it
    b.Tip = clone
    clone.BaseFeature = src_body
    clone.Placement = src_body.Placement
    doc.recompute()

    sk = doc.addObject("Sketcher::SketchObject", "SplitSketch%s" % tag)
    b.addObject(sk)
    sk.Placement = App.Placement(App.Vector(0, 0, z_cut), App.Rotation(0, 0, 0))
    pts = [App.Vector(-200, -200, 0), App.Vector(200, -200, 0),
           App.Vector(200, 200, 0), App.Vector(-200, 200, 0)]
    for i in range(4):
        sk.addGeometry(Part.LineSegment(pts[i], pts[(i + 1) % 4]), False)
    for i in range(4):
        sk.addConstraint(Sketcher.Constraint("Coincident", i, 2, (i + 1) % 4, 1))
    sk.Visibility = False

    pk = doc.addObject("PartDesign::Pocket", "SplitPocket%s" % tag)
    b.addObject(pk)
    pk.Profile = sk
    pk.Type = "ThroughAll"
    pk.Reversed = keep_lower    # cut upward to keep the lower half
    pk.BaseFeature = clone
    b.Tip = pk
    doc.recompute()

    s = b.Shape
    bb = s.BoundBox
    App.Console.PrintMessage("%-12s vol=%9.1f  Z[%.2f, %.2f]  solids=%d\n" % (
        b.Label, s.Volume, bb.ZMin, bb.ZMax, len(s.Solids)))
    return b


def split_clamshell(doc=None):
    doc = doc or App.ActiveDocument
    src = next(o for o in doc.Objects
               if o.Label == SRC_LABEL and o.TypeId == "PartDesign::Body")
    heal_loft_seam(doc, src)
    z_cut = doc.getObject(DATUM).Placement.Base.z
    lower = make_half(doc, src, LOWER_LABEL, z_cut, True)
    upper = make_half(doc, src, UPPER_LABEL, z_cut, False)
    src.Visibility = False
    return lower, upper


bodies = split_clamshell()
