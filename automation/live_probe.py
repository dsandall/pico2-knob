#!/usr/bin/env python
"""Probe the KiCad IPC API on the running GUI instance.

Requires: Preferences -> Plugins -> "Enable KiCad API" in the running KiCad,
then run with the project venv: .venv/bin/python automation/live_probe.py
"""
import sys

from kipy import KiCad


def main() -> int:
    kicad = KiCad()
    try:
        version = kicad.get_version()
    except BaseException as e:
        print(f"cannot connect ({type(e).__name__}: {e})")
        print("-> enable Preferences > Plugins > 'Enable KiCad API' and retry")
        return 1

    print("connected:", version)

    try:
        board = kicad.get_board()
    except BaseException as e:
        print(f"no board open ({e})")
        return 0

    print("board:", board.name)
    print("  footprints:", len(board.get_footprints()))
    print("  tracks:", len(board.get_tracks()))
    print("  vias:", len(board.get_vias()))
    print("  zones:", len(board.get_zones()))
    nets = board.get_nets()
    print("  nets:", len(nets))
    for n in nets[:10]:
        print("   ", n.name)
    sel = board.get_selection()
    print("  current GUI selection:", len(sel), "items")
    return 0


if __name__ == "__main__":
    sys.exit(main())
