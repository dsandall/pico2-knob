#!/usr/bin/env python3
"""Plane-aware headless autoroute for KiCad 10.

Adds copper-pour planes (GND on the bottom, +3.3V on the top by default) BEFORE
export, so Freerouting sees them as planes and routes only the signal nets — the
power/ground pads connect through the pour instead of as traces. Then fills the
zones after importing the routed result.

    python3 automation/autoroute_planes.py board_pico2knob/pico2-knob.kicad_pcb

Same conventions as autoroute.py: SYSTEM python3 (needs `import pcbnew`), reads
the input and writes a separate <name>.planes.kicad_pcb (original untouched),
Freerouting via Docker.
"""
import argparse
import os
import subprocess
import sys

FREEROUTING_IMAGE = "ghcr.io/freerouting/freerouting:latest"


def add_plane(pcbnew, board, net_name, layer):
    """Add an unfilled copper zone for net_name across the board bounding box."""
    net = board.FindNet(net_name)
    if net is None:
        print(f"  WARNING: net {net_name!r} not found — skipping plane")
        return False
    bb = board.GetBoardEdgesBoundingBox()
    zone = pcbnew.ZONE(board)
    zone.SetLayer(layer)
    zone.SetNetCode(net.GetNetCode())
    zone.SetIsFilled(False)
    poly = zone.Outline()
    poly.NewOutline()
    for x, y in [(bb.GetLeft(), bb.GetTop()), (bb.GetRight(), bb.GetTop()),
                 (bb.GetRight(), bb.GetBottom()), (bb.GetLeft(), bb.GetBottom())]:
        poly.Append(x, y)
    board.Add(zone)
    return True


def run(pcb_path, out_path, passes, gnd_net, pwr_net, keep):
    try:
        import pcbnew
    except ImportError:
        print("ERROR: `import pcbnew` failed — run with system python3, not the venv.")
        return 2

    pcb_path = os.path.abspath(pcb_path)
    out_path = os.path.abspath(out_path)
    work = os.path.dirname(out_path)
    base = os.path.splitext(os.path.basename(pcb_path))[0]
    dsn = os.path.join(work, base + ".planes.dsn")
    ses = os.path.join(work, base + ".planes.ses")

    def load_with_planes():
        b = pcbnew.LoadBoard(pcb_path)
        for t in list(b.GetTracks()):      # rip up to route clean against the planes
            b.RemoveNative(t)
        for z in list(b.Zones()):          # drop any pre-existing pours
            b.RemoveNative(z)
        add_plane(pcbnew, b, gnd_net, pcbnew.B_Cu)
        add_plane(pcbnew, b, pwr_net, pcbnew.F_Cu)
        return b

    board = load_with_planes()
    print(f"[1/4] {base}: {len(board.GetFootprints())} footprints, planes added "
          f"({gnd_net} on B.Cu, {pwr_net} on F.Cu)")

    if not pcbnew.ExportSpecctraDSN(board, dsn):
        print("ERROR: ExportSpecctraDSN failed")
        return 1
    print(f"      DSN exported: {os.path.getsize(dsn)} bytes")

    print(f"[2/4] routing signals via Freerouting ({passes} passes)...")
    cmd = ["docker", "run", "--rm", "-v", f"{work}:/work", FREEROUTING_IMAGE,
           "java", "-jar", "/app/freerouting-executable.jar",
           "-de", f"/work/{os.path.basename(dsn)}",
           "-do", f"/work/{os.path.basename(ses)}", "-mp", str(passes)]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    for line in proc.stdout.splitlines():
        if "session completed" in line or "ERROR" in line:
            print("      " + line.split("INFO")[-1].strip())
    if not os.path.exists(ses):
        print("ERROR: Freerouting produced no SES")
        print(proc.stdout[-500:] or proc.stderr[-500:])
        return 1

    # Reload with planes, import routed signals, then fill the pours.
    board = load_with_planes()
    if not pcbnew.ImportSpecctraSES(board, ses):
        print("ERROR: ImportSpecctraSES failed")
        return 1
    tr = board.GetTracks()
    seg = sum(1 for t in tr if t.GetClass() == "PCB_TRACK")
    via = sum(1 for t in tr if t.GetClass() == "PCB_VIA")
    print(f"[3/4] imported SES -> {seg} signal segments, {via} vias")

    filler = pcbnew.ZONE_FILLER(board)
    filler.Fill(board.Zones())
    for z in board.Zones():
        print(f"      filled {z.GetNetname()} on {board.GetLayerName(z.GetLayer())}: "
              f"{round(z.GetFilledArea()/1e12, 1)} mm^2")
    pcbnew.SaveBoard(out_path, board)
    print(f"[4/4] wrote {out_path}")

    if not keep:
        for f in (dsn, ses):
            try:
                os.remove(f)
            except OSError:
                pass
    return 0


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("pcb")
    ap.add_argument("-o", "--output")
    ap.add_argument("-p", "--passes", type=int, default=30)
    ap.add_argument("--gnd", default="GND", help="ground net name (default GND)")
    ap.add_argument("--power", default="+3.3V", help="power net name (default +3.3V)")
    ap.add_argument("--keep", action="store_true")
    a = ap.parse_args()
    out = a.output or os.path.splitext(a.pcb)[0] + ".planes.kicad_pcb"
    sys.exit(run(a.pcb, out, a.passes, a.gnd, a.power, a.keep))
