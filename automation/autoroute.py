#!/usr/bin/env python3
"""Headless autoroute for KiCad 10 via SWIG pcbnew + Freerouting (Docker).

Bypasses both the GUI and the IPC API (which crashes on board writes in 10.0.4):
  1. ExportSpecctraDSN  (SWIG pcbnew, headless)   .kicad_pcb -> .dsn
  2. Freerouting        (Docker, one-shot CLI)     .dsn       -> .ses
  3. ImportSpecctraSES  (SWIG pcbnew, headless)    .ses       -> tracks -> .kicad_pcb

MUST run with the SYSTEM python (the one with `import pcbnew`), not the venv:
    python3 automation/autoroute.py board_pico2knob/pico2-knob.kicad_pcb

The input board must NOT be open in KiCad with unsaved changes — this reads and
writes the file directly. By default it writes <name>.routed.kicad_pcb so your
original is never touched; review it, then open/replace in KiCad yourself.

SWIG pcbnew is deprecated and slated for removal in KiCad 11; on 11 the DSN/SES
round-trip should return to kicad-cli / the IPC API.
"""
import argparse
import os
import subprocess
import sys

FREEROUTING_IMAGE = "ghcr.io/freerouting/freerouting:latest"


def _load(pcbnew, pcb_path: str, ripup: bool):
    """Load a board, optionally stripping all existing tracks/vias first."""
    board = pcbnew.LoadBoard(pcb_path)
    if ripup:
        for t in list(board.GetTracks()):
            board.RemoveNative(t)
    return board


def run(pcb_path: str, out_path: str, passes: int, keep_intermediate: bool,
        ripup: bool) -> int:
    try:
        import pcbnew
    except ImportError:
        print("ERROR: `import pcbnew` failed — run with system python3, not the venv.")
        return 2

    pcb_path = os.path.abspath(pcb_path)
    out_path = os.path.abspath(out_path)
    # Must be absolute: Docker treats a relative -v source as a named volume,
    # not a bind mount, so the container would see an empty /work.
    work = os.path.dirname(out_path)
    base = os.path.splitext(os.path.basename(pcb_path))[0]
    dsn = os.path.join(work, base + ".dsn")
    ses = os.path.join(work, base + ".ses")

    raw = pcbnew.LoadBoard(pcb_path)
    n_before = len(raw.GetTracks())
    board = _load(pcbnew, pcb_path, ripup)
    print(f"[1/3] loaded {base}: {len(board.GetFootprints())} footprints, "
          f"{n_before} existing tracks{' (ripped up for clean-slate route)' if ripup else ''}, "
          f"{board.GetNetCount()} nets")

    if not pcbnew.ExportSpecctraDSN(board, dsn):
        print("ERROR: ExportSpecctraDSN failed")
        return 1
    print(f"      DSN exported: {os.path.getsize(dsn)} bytes")

    print(f"[2/3] routing via Freerouting (Docker, {passes} passes)...")
    cmd = [
        "docker", "run", "--rm", "-v", f"{work}:/work", FREEROUTING_IMAGE,
        "java", "-jar", "/app/freerouting-executable.jar",
        "-de", f"/work/{os.path.basename(dsn)}",
        "-do", f"/work/{os.path.basename(ses)}",
        "-mp", str(passes),
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    for line in proc.stdout.splitlines():
        if "unrouted" in line or "session completed" in line or "ERROR" in line:
            print("      " + line.split("INFO")[-1].strip())
    if not os.path.exists(ses):
        print("ERROR: Freerouting produced no SES file")
        print(proc.stdout[-500:] or proc.stderr[-500:])
        return 1

    # Reload (ripping up again if requested) so imported routing matches what
    # Freerouting actually saw — otherwise SES tracks stack on top of old ones.
    board = _load(pcbnew, pcb_path, ripup)
    if not pcbnew.ImportSpecctraSES(board, ses):
        print("ERROR: ImportSpecctraSES failed")
        return 1
    tracks = board.GetTracks()
    seg = sum(1 for t in tracks if t.GetClass() == "PCB_TRACK")
    via = sum(1 for t in tracks if t.GetClass() == "PCB_VIA")
    pcbnew.SaveBoard(out_path, board)
    print(f"[3/3] imported SES -> {seg} segments, {via} vias")
    print(f"      wrote {out_path}")

    if not keep_intermediate:
        for f in (dsn, ses):
            try:
                os.remove(f)
            except OSError:
                pass
    return 0


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("pcb", help="input .kicad_pcb")
    ap.add_argument("-o", "--output", help="output board (default <name>.routed.kicad_pcb)")
    ap.add_argument("-p", "--passes", type=int, default=10, help="Freerouting max passes")
    ap.add_argument("--keep", action="store_true", help="keep .dsn/.ses intermediates")
    ap.add_argument("--ripup", action="store_true",
                    help="strip all existing tracks first (route the whole board from scratch)")
    a = ap.parse_args()
    out = a.output or os.path.splitext(a.pcb)[0] + ".routed.kicad_pcb"
    sys.exit(run(a.pcb, out, a.passes, a.keep, a.ripup))
