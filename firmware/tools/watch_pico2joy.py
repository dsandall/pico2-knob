#!/usr/bin/env python3
"""Watch the puck's BLE advertisements. Needs root for the management socket.

    sudo tools/watch_pico2joy.py

Passive listening only: the puck advertises non-connectably, so this never
connects, never pairs, and writes nothing to /var/lib/bluetooth. Discovery is
stopped again on the way out. BlueZ may hold the address in its in-memory
device cache for a while; nothing persists across a bluetoothd restart.

Uses the BlueZ management socket rather than raw HCI, because bluetoothd owns
the adapter: raw LE Set Scan Parameters comes back "command disallowed" and no
advertising reports are delivered.
"""

import argparse
import socket
import struct
import sys
import time

HCI_DEV_NONE = 0xFFFF
HCI_CHANNEL_CONTROL = 3

MGMT_OP_START_DISCOVERY = 0x0023
MGMT_OP_STOP_DISCOVERY = 0x0024
MGMT_EV_CMD_COMPLETE = 0x0001
MGMT_EV_CMD_STATUS = 0x0002
MGMT_EV_DEVICE_FOUND = 0x0012
# LE public + LE random. Deliberately no BR/EDR: nothing here is classic.
ADDR_TYPE_LE = 0x06

AD_COMPLETE_LOCAL_NAME = 0x09
AD_MANUFACTURER_DATA = 0xFF
COMPANY_ID = 0xFFFF  # reserved-for-testing, matching the firmware
FORMAT = 1  # radio.rs FORMAT
BUTTONS = ["BTN1", "BTN2", "BTN3", "ENC_SW"]


def parse_eir(blob):
    """Split an EIR/AD payload into {type: bytes}; last occurrence wins."""
    fields, i = {}, 0
    while i < len(blob):
        length = blob[i]
        if length == 0 or i + 1 + length > len(blob):
            break
        fields[blob[i + 1]] = blob[i + 2 : i + 1 + length]
        i += 1 + length
    return fields


def decode_state(fields):
    mfg = fields.get(AD_MANUFACTURER_DATA)
    if mfg is None or len(mfg) < 10:
        return None
    company, fmt, seq, buttons, detents, uptime, flags = struct.unpack("<HBBBhHB", mfg[:10])
    if company != COMPANY_ID or fmt != FORMAT:
        return None
    return {
        "seq": seq,
        "down": [name for i, name in enumerate(BUTTONS) if buttons & (1 << i)],
        "detents": detents,
        "uptime_s": uptime,
        "vpp": bool(flags & 1),
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("-i", "--index", type=int, default=0, help="adapter index (hciN)")
    ap.add_argument("-n", "--name", default="pico2joy", help="local name to match")
    ap.add_argument("-t", "--seconds", type=float, help="stop after this long")
    ap.add_argument("-a", "--all", action="store_true", help="show every advertiser, not just the puck")
    args = ap.parse_args()

    try:
        sock = socket.socket(socket.AF_BLUETOOTH, socket.SOCK_RAW, socket.BTPROTO_HCI)
        sock.bind((HCI_DEV_NONE, HCI_CHANNEL_CONTROL))
    except PermissionError:
        sys.exit("need root for the management socket: try sudo")
    except OSError as err:
        sys.exit(f"can't open the management socket: {err}")

    def send(opcode, params=b""):
        sock.send(struct.pack("<HHH", opcode, args.index, len(params)) + params)

    send(MGMT_OP_START_DISCOVERY, bytes([ADDR_TYPE_LE]))
    print(f"listening for {args.name!r} on hci{args.index}; ctrl-c to stop", file=sys.stderr)

    deadline = time.monotonic() + args.seconds if args.seconds else None
    seen = 0
    try:
        while deadline is None or time.monotonic() < deadline:
            sock.settimeout(0.5)
            try:
                packet = sock.recv(2048)
            except socket.timeout:
                continue
            if len(packet) < 6:
                continue
            event, _index, length = struct.unpack("<HHH", packet[:6])
            params = packet[6 : 6 + length]

            if event in (MGMT_EV_CMD_COMPLETE, MGMT_EV_CMD_STATUS) and len(params) >= 3:
                opcode, status = struct.unpack("<HB", params[:3])
                if opcode == MGMT_OP_START_DISCOVERY and status != 0:
                    sys.exit(f"couldn't start discovery: mgmt status 0x{status:02x}")
                continue

            if event != MGMT_EV_DEVICE_FOUND or len(params) < 14:
                continue

            address = params[0:6][::-1]
            rssi = struct.unpack("b", params[7:8])[0]
            eir_len = struct.unpack("<H", params[12:14])[0]
            fields = parse_eir(params[14 : 14 + eir_len])
            name = fields.get(AD_COMPLETE_LOCAL_NAME, b"").decode("utf-8", "replace")
            mac = ":".join(f"{b:02X}" for b in address)

            if args.all and name != args.name:
                print(f"{mac}  {rssi:4d} dBm  {name!r}")
                continue
            if name != args.name:
                continue

            state = decode_state(fields)
            if state is None:
                print(f"{mac}  {rssi:4d} dBm  {name} - no recognisable payload", flush=True)
                continue

            seen += 1
            down = ",".join(state["down"]) or "-"
            print(
                f"{mac}  {rssi:4d} dBm  seq={state['seq']:3d}  detents={state['detents']:+5d}  "
                f"down={down:<20} up={state['uptime_s']:5d}s  vpp={'on' if state['vpp'] else 'off'}",
                flush=True,
            )
    except KeyboardInterrupt:
        pass
    finally:
        send(MGMT_OP_STOP_DISCOVERY, bytes([ADDR_TYPE_LE]))
        sock.close()

    if seen == 0:
        print("nothing from the puck - powered? advertising on ('w' toggles it)?", file=sys.stderr)


if __name__ == "__main__":
    main()
