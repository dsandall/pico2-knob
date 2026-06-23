#!/usr/bin/env python3
"""First-pass PCB placement for pico2-knob, driven by the FreeCAD mockup arrangement.
Places every footprint, assigns nets (pad number/name -> net, matching the schematic),
draws the Ø74 round Edge.Cuts, saves the board. Run: system python3 (uses pcbnew).

Coord map: mockup is Y-up with encoder at origin; KiCad PCB is Y-down. Board centered at
(CX,CY); pcb = (CX + mock_x, CY - mock_y). Nano is on the BACK copper (bottom-mounted).
"""
import pcbnew, os, re, math

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OUT  = os.path.join(ROOT, "hardware", "pico2-knob.kicad_pcb")
CX, CY, R = 150.0, 100.0, 37.0           # board center + radius (Ø74)
XCUT = 24.0                              # flat-trim the left/right edges at x = CX +/- XCUT
                                         # (empty space there) -> vertical-lens outline
KS = "/usr/share/kicad/footprints"

def fpdir(nick):
    if nick.startswith("marbastlib"):
        return os.path.join(ROOT, "lib/marbastlib/footprints", nick + ".pretty")
    return f"{KS}/{nick}.pretty"

# ref, lib:footprint, mock_x, mock_y, rot_deg, on_back, value, {pad: net}
PLACES = [
    # nano on BACK; mock_y 18 (down 2mm from 20) so top pads clear the shrunk top rim
    ("U1", "marbastlib-xp-promicroish:nice_nano_AH_Reversible", 0, 18, 0, True, "nice!nano_v2", {
        "16":"+3V3","3":"GND","4":"GND","14":"GND","28":"GND","13":"VBAT","29":"VBAT",
        "24":"ENC_A","23":"ENC_B","12":"ENC_SW","5":"SDA","6":"SCL","8":"BTN1","9":"BTN2","10":"BTN3"}),
    # encoder lowered 8mm (mock_y 2.5 -> -5.5): shaft now at board (150,108)
    ("RE1", "Rotary_Encoder:RotaryEncoder_Alps_EC11E-Switch_Vertical_H20mm", -7.5, -5.5, 0, False,
        "RotaryEncoder_Switch", {"A":"ENC_A","B":"ENC_B","C":"GND","S1":"ENC_SW","S2":"GND"}),
    # SW_PUSH_6mm origin is its top-left pad, +3.25mm in X from the body center, so shift each
    # origin left 3.25mm -> button BODIES (the visible arc) are symmetric about the centerline.
    ("SW1", "Button_Switch_THT:SW_PUSH_6mm", -20.25, -13, 0, False, "SW_Push", {"1":"BTN1","2":"GND"}),
    ("SW2", "Button_Switch_THT:SW_PUSH_6mm",  -3.25, -20, 0, False, "SW_Push", {"1":"BTN2","2":"GND"}),
    ("SW3", "Button_Switch_THT:SW_PUSH_6mm",  13.75, -13, 0, False, "SW_Push", {"1":"BTN3","2":"GND"}),
    # OLED socket = single column -> sits in the nano's central channel (x0)
    # J1 offset LEFT ~18mm: the OLED module's connector sits ~18mm left of its glass center,
    # so this lands the glass centered over the knob (per the module MCAD drawing).
    ("J1", "Connector_PinSocket_2.54mm:PinSocket_1x04_P2.54mm_Vertical", -18, 27, 0, False,
        "OLED_0.87_I2C", {"1":"GND","2":"+3V3","3":"SCL","4":"SDA"}),
    ("R1", "Resistor_SMD:R_0603_1608Metric", -15, -28, 0, False, "4k7", {"1":"+3V3","2":"SCL"}),
    ("R2", "Resistor_SMD:R_0603_1608Metric",  15, -28, 0, False, "4k7", {"1":"+3V3","2":"SDA"}),
    ("J2", "Connector_JST:JST_PH_S2B-PH-K_1x02_P2.00mm_Horizontal", 0, -29, 0, False,
        "LiPo_JST_PH", {"1":"VBAT","2":"GND"}),
]

board = pcbnew.NewBoard(OUT)
nets = {}
def net(name):
    if name not in nets:
        ni = pcbnew.NETINFO_ITEM(board, name); board.Add(ni); nets[name] = ni
    return nets[name]

for ref, fpstr, mx, my, rot, back, val, netmap in PLACES:
    nick, fpname = fpstr.split(":")
    fp = pcbnew.FootprintLoad(fpdir(nick), fpname)
    if fp is None:
        print("FAILED to load", fpstr); continue
    fp.SetReference(ref); fp.SetValue(val)
    fp.SetPosition(pcbnew.VECTOR2I(pcbnew.FromMM(CX + mx), pcbnew.FromMM(CY - my)))
    board.Add(fp)
    if rot: fp.SetOrientationDegrees(rot)
    if back: fp.Flip(fp.GetPosition(), False)
    for pad in fp.Pads():
        n = netmap.get(pad.GetNumber())
        if n: pad.SetNet(net(n))

# Board outline: circle radius R clipped to |x-CX| <= XCUT (flat left/right). Edge.Cuts =
# top arc + right line + bottom arc + left line.
def V(x, y): return pcbnew.VECTOR2I(pcbnew.FromMM(x), pcbnew.FromMM(y))
yc = (R * R - XCUT * XCUT) ** 0.5
UL, UR = (CX - XCUT, CY - yc), (CX + XCUT, CY - yc)
LR, LL = (CX + XCUT, CY + yc), (CX - XCUT, CY + yc)
def edge(shape, *pts):
    s = pcbnew.PCB_SHAPE(board); s.SetShape(shape); s.SetLayer(pcbnew.Edge_Cuts)
    if shape == pcbnew.SHAPE_T_ARC: s.SetArcGeometry(V(*pts[0]), V(*pts[1]), V(*pts[2]))
    else: s.SetStart(V(*pts[0])); s.SetEnd(V(*pts[1]))
    s.SetWidth(pcbnew.FromMM(0.15)); board.Add(s)
edge(pcbnew.SHAPE_T_ARC, UL, (CX, CY - R), UR)   # top arc
edge(pcbnew.SHAPE_T_SEGMENT, UR, LR)             # right flat
edge(pcbnew.SHAPE_T_ARC, LR, (CX, CY + R), LL)   # bottom arc
edge(pcbnew.SHAPE_T_SEGMENT, LL, UL)             # left flat

board.BuildListOfNets()

# GND pours on both layers, following the trimmed outline inset 0.6mm
gnd = nets["GND"]
Rp, Xp = R - 0.6, XCUT - 0.6
def trimmed_pts(n=44):
    pts = []
    for i in range(n + 1):                       # top arc L->R
        x = CX - Xp + 2 * Xp * i / n; pts.append((x, CY - (Rp * Rp - (x - CX) ** 2) ** 0.5))
    for i in range(n + 1):                       # bottom arc R->L
        x = CX + Xp - 2 * Xp * i / n; pts.append((x, CY + (Rp * Rp - (x - CX) ** 2) ** 0.5))
    return pts
for layer in (pcbnew.F_Cu, pcbnew.B_Cu):
    z = pcbnew.ZONE(board)
    z.SetLayer(layer); z.SetNet(gnd); z.SetAssignedPriority(0)
    o = z.Outline(); o.NewOutline()
    for x, y in trimmed_pts():
        o.Append(pcbnew.FromMM(x), pcbnew.FromMM(y))
    board.Add(z)
pcbnew.ZONE_FILLER(board).Fill(board.Zones())

board.Save(OUT)
print(f"placed {len(PLACES)} footprints, {len(nets)} nets, Ø{2*R:.0f} outline + GND pours -> {OUT}")
