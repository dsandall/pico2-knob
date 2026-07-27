"""Complete the J3/DS1 20-pin screen wiring following the established format:
per pin, one horizontal wire ties label/power (left, x=241.3) -> J3 pin (junction, x=251.46)
-> DS1 pin (x=254.0). Fixes: J3 not connected (junctions), missing labels, IM1/IM2->GND,
WR#/E-RD#->+3.3V, D2-D7 -> NC."""
import sys
sys.path.insert(0, ".")
from konnect_client import Konnect, KonnectError

SCH = "/home/thebu/newhome/projects/pico2-knob/board_pico2knob/pico2-knob.kicad_sch"
X_LBL, X_J3, X_DS1 = 241.3, 251.46, 254.0
def y(n): return round(109.22 + (n - 1) * 2.54, 2)

def tryjson(label, fn):
    try:
        print(label, fn())
    except KonnectError as e:
        print("WARN", label, "->", e)

with Konnect() as k:
    k.load("sch_wiring", "sch_batch", "sch_components")

    # 1. remove NC flags on pins 4,5 (both J3 and DS1 sides)
    for n in (4, 5):
        for x in (X_J3, X_DS1):
            tryjson(f"del NC {n} {x}", lambda x=x, n=n: k.call("delete_no_connect", schematic=SCH, x=x, y=y(n)))

    # 2. remove the D2-D7 wires (pins 14-19)
    for n in range(14, 20):
        tryjson(f"del wire {n}", lambda n=n: k.call("delete_schematic_wire", schematic=SCH,
                                     x1=X_LBL, y1=y(n), x2=X_DS1, y2=y(n)))

    # 3. add wires for pins 4,5 (IM1/IM2 -> GND chain)
    print("add wires 4,5", k.call("batch_add_wire", schematic=SCH, wires=[
        {"x1": X_LBL, "y1": y(4), "x2": X_DS1, "y2": y(4)},
        {"x1": X_LBL, "y1": y(5), "x2": X_DS1, "y2": y(5)}]))

    # 4. junctions at J3 pins to actually bond the connector (pins 1-13 and 20)
    js = [{"x": X_J3, "y": y(n)} for n in list(range(1, 14)) + [20]]
    print("junctions", k.call("batch_add_junction", schematic=SCH, positions=js))

    # 5. signal labels (local, rot 180) for the un-named signal pins
    for n, name in [(9, "OLED_DC"), (12, "OLED_SCLK"), (13, "OLED_SDI")]:
        print("label", name, k.call("add_schematic_net_label", schematic=SCH,
                                     net=name, x=X_LBL, y=y(n), rotation=180,
                                     label_type="net_label"))

    # 6. power symbols: +3.3V on WR#(10)/E-RD#(11), GND on IM1(4)/IM2(5)
    for n in (10, 11):
        print("pwr +3.3V", n, k.call("add_power_symbol", schematic=SCH,
                                      power_net="+3.3V", x=X_LBL, y=y(n)))
    for n in (4, 5):
        print("pwr GND", n, k.call("add_power_symbol", schematic=SCH,
                                    power_net="GND", x=X_LBL, y=y(n)))

    # 7. NC flags on unused D2-D7 (both J3 and DS1 sides)
    for n in range(14, 20):
        for x in (X_J3, X_DS1):
            print("NC", n, x, k.call("add_no_connect", schematic=SCH, x=x, y=y(n)))
