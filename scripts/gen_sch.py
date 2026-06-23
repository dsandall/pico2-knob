#!/usr/bin/env python3
"""Generate hardware/pico2-knob.kicad_sch from scratch.

Method: embed each symbol's real definition into lib_symbols, place instances,
and connect every used pin with a short stub wire + a net label placed at the
pin's exact transformed coordinate. Connectivity is by label NAME, so symbol
placement is free and no cross-sheet routing geometry is needed.
Pin transform (instance rot 0, unmirrored): sch = (X + px, Y - py).
"""
import sexpdata, uuid, os
from sexpdata import Symbol as Sym

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
OUT  = os.path.join(ROOT, "hardware", "pico2-knob.kicad_sch")

def S(x): return x.value() if isinstance(x, Sym) else x
def load(p): return sexpdata.loads(open(p).read())
def find_symbol(lib, name):
    for e in lib[1:]:
        if isinstance(e, list) and S(e[0]) == "symbol" and S(e[1]) == name:
            return e
def pin_list(sym):
    out = []
    def walk(node):
        for e in node:
            if isinstance(e, list) and e and S(e[0]) == "pin":
                at = num = None
                for s in e[1:]:
                    if isinstance(s, list) and S(s[0]) == "at": at = (s[1], s[2], s[3])
                    if isinstance(s, list) and S(s[0]) == "number": num = S(s[1])
                out.append((num, at))
            elif isinstance(e, list) and e and S(e[0]) == "symbol":
                walk(e)
    walk(sym)
    return out

LIBS = {
    "marbastlib-promicroish:nice_nano": ("lib/marbastlib/symbols/marbastlib-promicroish.kicad_sym", "nice_nano"),
    "Device:RotaryEncoder_Switch": ("/usr/share/kicad/symbols/Device.kicad_sym", "RotaryEncoder_Switch"),
    "Switch:SW_Push": ("/usr/share/kicad/symbols/Switch.kicad_sym", "SW_Push"),
    "power:GND": ("/usr/share/kicad/symbols/power.kicad_sym", "GND"),
    "power:+3V3": ("/usr/share/kicad/symbols/power.kicad_sym", "+3V3"),
    "Device:R": ("/usr/share/kicad/symbols/Device.kicad_sym", "R"),
    "Connector_Generic:Conn_01x04": ("/usr/share/kicad/symbols/Connector_Generic.kicad_sym", "Conn_01x04"),
    "Connector_Generic:Conn_01x02": ("/usr/share/kicad/symbols/Connector_Generic.kicad_sym", "Conn_01x02"),
}

# load + cache symbol defs and pin tables
defs, pins = {}, {}
for libid, (path, name) in LIBS.items():
    sym = find_symbol(load(path), name)
    sym = list(sym)
    sym[1] = libid                      # rename top-level to lib_id (sub-units keep base name)
    defs[libid] = sym
    pins[libid] = {n: at for n, at in pin_list(sym)}

def downgrade_redundant_power(symdef):
    """nice!nano marks every GND/BAT+ pad power_out; tying them trips power_out<->power_out
    ERC. Keep the first of each name as the driver, demote the rest to passive."""
    seen = set()
    def walk(node):
        for e in node:
            if isinstance(e, list) and e and S(e[0]) == "pin":
                nm = next((S(x[1]) for x in e[1:] if isinstance(x, list) and S(x[0]) == "name"), None)
                if nm in ("GND", "BAT+"):
                    if nm in seen: e[1] = Sym("passive")
                    else: seen.add(nm)
            elif isinstance(e, list) and e and S(e[0]) == "symbol":
                walk(e)
    walk(symdef)
downgrade_redundant_power(defs["marbastlib-promicroish:nice_nano"])

ROOT_UUID = str(uuid.uuid4())
def U(): return str(uuid.uuid4())

# ---- instances: (lib_id, ref, value, footprint, X, Y, {pin#: net}) ----
PLACES = [
    # nice!nano v2 (nRF52840 + BLE + on-board LiPo charger). Power pins are power_out
    # in this symbol, so they drive +3V3/GND/VBAT (no PWR_FLAG needed).
    ("marbastlib-promicroish:nice_nano", "U1", "nice!nano_v2",
        "marbastlib-xp-promicroish:nice_nano_AH_Reversible", 130, 105, {
        "16": "+3V3", "3": "GND", "4": "GND", "14": "GND", "28": "GND",
        "13": "VBAT", "29": "VBAT",                           # on-board charger BAT+
        "24": "ENC_A", "23": "ENC_B", "12": "ENC_SW",         # pads next to the encoder (short traces)
        "5": "SDA", "6": "SCL",                               # P0.17 / P0.20 (I2C -> OLED)
        "8": "BTN1", "9": "BTN2", "10": "BTN3"}),             # P0.24 / P1.00 / P0.11
    ("Device:RotaryEncoder_Switch", "RE1", "RotaryEncoder_Switch",
        "Rotary_Encoder:RotaryEncoder_Alps_EC11E-Switch_Vertical_H20mm", 215, 90, {
        "A": "ENC_A", "B": "ENC_B", "C": "GND", "S1": "ENC_SW", "S2": "GND"}),
    ("Switch:SW_Push", "SW1", "SW_Push", "Button_Switch_THT:SW_PUSH_6mm", 215, 118,
        {"1": "BTN1", "2": "GND"}),
    ("Switch:SW_Push", "SW2", "SW_Push", "Button_Switch_THT:SW_PUSH_6mm", 215, 130,
        {"1": "BTN2", "2": "GND"}),
    ("Switch:SW_Push", "SW3", "SW_Push", "Button_Switch_THT:SW_PUSH_6mm", 215, 142,
        {"1": "BTN3", "2": "GND"}),
    # --- 0.87" 128x32 SSD1316 I2C OLED on a 4-pin module header + I2C pull-ups ---
    ("Connector_Generic:Conn_01x04", "J1", "OLED_0.87_I2C",
        "Connector_PinSocket_2.54mm:PinSocket_1x04_P2.54mm_Vertical", 60, 150,
        {"1": "GND", "2": "+3V3", "3": "SCL", "4": "SDA"}),
    ("Device:R", "R1", "4k7", "Resistor_SMD:R_0603_1608Metric", 92, 148,
        {"1": "+3V3", "2": "SCL"}),
    ("Device:R", "R2", "4k7", "Resistor_SMD:R_0603_1608Metric", 104, 148,
        {"1": "+3V3", "2": "SDA"}),
    # --- LiPo battery on a 2-pin JST-PH (BAT+ to nano charger, BAT- to GND) ---
    ("Connector_Generic:Conn_01x02", "J2", "LiPo_JST_PH",
        "Connector_JST:JST_PH_S2B-PH-K_1x02_P2.00mm_Horizontal", 60, 100,
        {"1": "VBAT", "2": "GND"}),
    ("power:GND", "#PWR01", "GND", "", 135, 155, {"1": "GND"}),
    ("power:+3V3", "#PWR02", "+3V3", "", 155, 155, {"1": "+3V3"}),
]

def prop(name, value, x, y, hide=False):
    eff = [Sym("effects"), [Sym("font"), [Sym("size"), 1.27, 1.27]]]
    if hide: eff.append([Sym("hide"), Sym("yes")])
    return [Sym("property"), name, value, [Sym("at"), x, y, 0], eff]

def snap(v):  # nearest 1.27 mm (50 mil) grid so all pins land on-grid
    return round(round(v / 1.27) * 1.27, 4)

wires, labels, symbols = [], [], []
for libid, ref, value, fp, X, Y, netmap in PLACES:
    X, Y = snap(X), snap(Y)
    inst = [Sym("symbol"),
            [Sym("lib_id"), libid],
            [Sym("at"), X, Y, 0],
            [Sym("unit"), 1],
            [Sym("exclude_from_sim"), Sym("no")],
            [Sym("in_bom"), Sym("yes")],
            [Sym("on_board"), Sym("yes")],
            [Sym("dnp"), Sym("no")],
            [Sym("uuid"), U()],
            prop("Reference", ref, X + 2.54, Y - 12.7),
            prop("Value", value, X + 2.54, Y - 10.16),
            prop("Footprint", fp, X, Y, hide=True)]
    # pin uuids
    for num in pins[libid]:
        inst.append([Sym("pin"), num, [Sym("uuid"), U()]])
    inst.append([Sym("instances"),
                 [Sym("project"), "pico2-knob",
                  [Sym("path"), "/" + ROOT_UUID, [Sym("reference"), ref], [Sym("unit"), 1]]]])
    symbols.append(inst)

    # stub + label per connected pin
    for num, net in netmap.items():
        px, py, _ = pins[libid][num]
        ex, ey = X + px, Y - py                     # pin connection point in schematic
        L = 5.08                                    # stub length (clears symbol bodies)
        just = Sym("left")
        if px > 0:   lx, ly, ang, just = ex + L, ey, 0, Sym("left")
        elif px < 0: lx, ly, ang, just = ex - L, ey, 0, Sym("right")
        elif py < 0: lx, ly, ang, just = ex, ey + L, 90, Sym("right")  # bottom pin -> label below
        else:        lx, ly, ang, just = ex, ey - L, 90, Sym("left")   # top/power pin -> label above
        wires.append([Sym("wire"),
                      [Sym("pts"), [Sym("xy"), ex, ey], [Sym("xy"), lx, ly]],
                      [Sym("stroke"), [Sym("width"), 0], [Sym("type"), Sym("default")]],
                      [Sym("uuid"), U()]])
        labels.append([Sym("label"), net,
                       [Sym("at"), lx, ly, ang],
                       [Sym("effects"), [Sym("font"), [Sym("size"), 1.27, 1.27]],
                        [Sym("justify"), just]],
                       [Sym("uuid"), U()]])

sch = [Sym("kicad_sch"),
       [Sym("version"), 20231120],
       [Sym("generator"), "pico2-knob-gen"],
       [Sym("generator_version"), "8.0"],
       [Sym("uuid"), ROOT_UUID],
       [Sym("paper"), "A4"],
       [Sym("lib_symbols")] + [defs[k] for k in LIBS],
       ]
sch += wires + labels + symbols
sch.append([Sym("sheet_instances"),
            [Sym("path"), "/", [Sym("page"), "1"]]])

with open(OUT, "w") as f:
    f.write(sexpdata.dumps(sch))
print("wrote", OUT, "-", len(symbols), "symbols,", len(wires), "wires,", len(labels), "labels")
