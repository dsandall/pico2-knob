import sexpdata, sys
def load(path):
    with open(path) as f: return sexpdata.loads(f.read())
def S(x): return x.value() if isinstance(x,sexpdata.Symbol) else x
def find_symbol(lib, name):
    for e in lib[1:]:
        if isinstance(e,list) and S(e[0])=="symbol" and S(e[1])==name:
            return e
def pins(sym):
    out=[]
    def walk(node):
        for e in node:
            if isinstance(e,list) and e and S(e[0])=="pin":
                at=name=num=None
                for sub in e[1:]:
                    if isinstance(sub,list):
                        h=S(sub[0])
                        if h=="at": at=(sub[1],sub[2],sub[3])
                        if h=="name": name=S(sub[1])
                        if h=="number": num=S(sub[1])
                out.append((num,name,at))
            elif isinstance(e,list) and e and S(e[0]) in ("symbol",):
                walk(e)
    walk(sym)
    return out

libs={
 "Pico":("lib/KiCad-RP-Pico/RP-Pico Libraries/MCU_RaspberryPi_and_Boards.kicad_sym","Pico"),
 "Enc":("/usr/share/kicad/symbols/Device.kicad_sym","RotaryEncoder_Switch"),
 "SW":("/usr/share/kicad/symbols/Switch.kicad_sym","SW_Push"),
}
for k,(p,n) in libs.items():
    sym=find_symbol(load(p),n)
    ps=pins(sym)
    print(f"=== {k} ({n}) : {len(ps)} pins ===")
    for num,name,at in ps:
        print(f"  {num:>3} {name:<12} at {at}")
