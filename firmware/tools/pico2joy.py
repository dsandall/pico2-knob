#!/usr/bin/env python3
# /// script
# requires-python = ">=3.9"
# dependencies = ["bleak>=0.22", "pillow>=10"]
# ///
"""One program for talking to the pico2joy puck, over USB or Bluetooth.

    tools/pico2joy.py scan                    # what can see the puck right now
    tools/pico2joy.py monitor                 # console passthrough
    tools/pico2joy.py relay                   # every app, following the screen
    tools/pico2joy.py gantry                  # drive a Klipper gantry
    tools/pico2joy.py xcarve                  # drive the X-Carve, through CNCJS
    tools/pico2joy.py spotify                 # transport + album art for the player
    tools/pico2joy.py quota                   # Claude/Codex rate-limit windows
    tools/pico2joy.py flash out/…uf2          # reflash over USB, no reset button
    tools/pico2joy.py ota out/…-dfu.zip       # reflash over BLE, no cable at all

Every subcommand takes `--link usb|ble|auto`, because the puck speaks the same
line protocol either way (see `src/gantry.rs`): `#`-prefixed lines, integer
micrometres, one line per message. USB carries them on the CDC console; BLE
carries them on two characteristics of the puck service. Nothing above the
transport knows which one it got.

BLE needs `bleak`, which this script declares inline - run it with `uv run
tools/pico2joy.py …` and it is fetched for you. Plain `python3` still works for
everything over USB, which is what the printer host has.

  Who is the server?  Nobody here. This program is a *client* of everything: it
  opens the puck's port or connects to it as a BLE central, and it makes HTTP
  requests to Moonraker. The puck never initiates and never listens; Moonraker
  is the only real server in the picture. That means this runs anywhere - your
  workstation over the LAN, or the printer host itself - with no listening
  socket, no port to open, and no discovery. The one hard rule is that a serial
  port and a BLE connection each have exactly one owner, so exactly one copy of
  this program owns the puck at a time.
"""

import argparse
import base64
import errno
import glob
import hashlib
import hmac
import json
import os
import queue
import select
import shutil
import subprocess
import sys
import termios
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile

AXES = ("x", "y", "z")

# Klipper's names for what it is doing, mapped onto the small enum the puck
# displays. Anything unlisted reads as "idle", which is the honest answer.
STATES = {"standby": 1, "complete": 1, "cancelled": 1, "printing": 2, "paused": 3, "error": 4}

# Jog feedrates, mm/min. Z is slower because a Z jog usually means the nozzle is
# near something.
FEED = {"x": 6000.0, "y": 6000.0, "z": 900.0}

# GRBL's activeState onto the same enum: 1 ready, 2 mid-job, 3 held, 4 alarm.
# Jog and Check are "ready" because a jog is exactly what the knob is doing.
GRBL_STATES = {"Idle": 1, "Jog": 1, "Check": 1, "Run": 2, "Home": 2,
               "Hold": 3, "Door": 3, "Alarm": 4, "Sleep": 0}

# X-Carve jog feedrates, mm/min. GRBL's own ceilings there are $110/$111 = 8000
# and $112 = 500; a knob wants to sit well inside them, Z most of all.
CARVE_FEED = {"x": 3000.0, "y": 3000.0, "z": 400.0}

USB_GLOB = "/dev/serial/by-id/*pico2joy*"
BLE_NAME = "pico2joy"

# The puck's own service - see src/ble.rs.
PUCK_RX = "9f4a0002-1d2b-4c65-9c31-7f9a2b0d5e01"   # host writes lines here
PUCK_TX = "9f4a0003-1d2b-4c65-9c31-7f9a2b0d5e01"   # puck notifies lines here

# Nordic legacy DFU, which is what the Adafruit bootloader on the nice!nano
# speaks over the air.
DFU_SERVICE = "00001530-1212-efde-1523-785feabcd123"
DFU_CONTROL = "00001531-1212-efde-1523-785feabcd123"
DFU_PACKET = "00001532-1212-efde-1523-785feabcd123"


def log(message):
    print("%s %s" % (time.strftime("%H:%M:%S"), message), flush=True)


# --------------------------------------------------------------------------
# transports
# --------------------------------------------------------------------------

class UsbLink:
    """The CDC-ACM console, raw, without pyserial - the printer host has none."""

    kind = "usb"

    def __init__(self, path):
        self.path = path
        self.fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        iflag, oflag, cflag, lflag, _, _, cc = termios.tcgetattr(self.fd)
        iflag &= ~(termios.IGNBRK | termios.BRKINT | termios.PARMRK | termios.ISTRIP |
                   termios.INLCR | termios.IGNCR | termios.ICRNL | termios.IXON)
        oflag &= ~termios.OPOST
        lflag &= ~(termios.ECHO | termios.ECHONL | termios.ICANON | termios.ISIG | termios.IEXTEN)
        cflag &= ~(termios.CSIZE | termios.PARENB | termios.CSTOPB)
        cflag |= termios.CS8 | termios.CREAD | termios.CLOCAL
        cc[termios.VMIN] = 0
        cc[termios.VTIME] = 0
        termios.tcsetattr(self.fd, termios.TCSANOW,
                          [iflag, oflag, cflag, lflag, termios.B115200, termios.B115200, cc])
        self.buffer = b""

    #: A cable either works or raises; there is nothing for it to heal from.
    self_healing = False
    #: Counts connections, as [`BleLink.generation`] does. A cable that is open
    #: has connected exactly once, so this never moves - `reopen` builds a whole
    #: new link, and the counter goes with it.
    generation = 1

    @property
    def name(self):
        return self.path

    @property
    def connected(self):
        return True

    def close(self):
        try:
            os.close(self.fd)
        except OSError:
            pass

    def send(self, line):
        # Leading newline: if the puck rebooted mid-line, this ends whatever it
        # was collecting before the next '#' opens a fresh one.
        data = ("\n" + line + "\n").encode()
        while data:
            try:
                data = data[os.write(self.fd, data):]
            except OSError as error:
                if error.errno in (errno.EAGAIN, errno.EWOULDBLOCK):
                    select.select([], [self.fd], [], 1.0)
                    continue
                raise

    def lines(self, timeout):
        ready, _, _ = select.select([self.fd], [], [], timeout)
        if not ready:
            return []
        try:
            chunk = os.read(self.fd, 4096)
        except OSError as error:
            if error.errno in (errno.EAGAIN, errno.EWOULDBLOCK):
                return []
            raise
        if not chunk:
            return []
        self.buffer += chunk
        out = []
        while b"\n" in self.buffer:
            line, self.buffer = self.buffer.split(b"\n", 1)
            out.append(line.decode("utf-8", "replace").strip())
        if len(self.buffer) > 4096:
            self.buffer = b""
        return out


def forget_ble_device(address):
    """Drop BlueZ's cached record of `address`, GATT database and all.

    BlueZ caches a service list per address for LE devices it has seen, bonded
    or not. The puck keeps *one* address across its bootloader and its
    application - which is the right call for flashing, and a trap here: connect
    while BlueZ still holds the bootloader's `AdaDFU` services and the puck
    service simply isn't there, with no error to say why. Removing the device
    makes the next connection rediscover.

    Done through `busctl` rather than a D-Bus binding: it ships with systemd, it
    needs no dependency, and this is a rare repair rather than a hot path. Any
    failure is ignored - the caller is already handling "couldn't find it".
    """
    node = "dev_" + address.upper().replace(":", "_")
    for adapter in sorted(glob.glob("/sys/class/bluetooth/hci*")):
        hci = os.path.basename(adapter)
        try:
            subprocess.run(
                ["busctl", "call", "--system", "org.bluez", "/org/bluez/%s" % hci,
                 "org.bluez.Adapter1", "RemoveDevice", "o",
                 "/org/bluez/%s/%s" % (hci, node)],
                capture_output=True, timeout=10)
        except (OSError, subprocess.SubprocessError):
            pass


class BleLink:
    """The same lines over GATT, on a link that puts itself back together.

    bleak is asyncio and everything above here is a plain loop, so the event loop
    lives in a thread of its own and the two sides meet at queues.

    The thread's job is not "connect" but "stay connected": it scans, connects,
    serves until the link drops, and starts over - so a puck that walks out of
    range, or a host that suspends, costs a gap rather than the session. Nothing
    above needs to know. `generation` counts the connections, which is how the
    relay notices it is talking to a puck that has forgotten everything it was
    told and needs it all again.
    """

    kind = "ble"
    #: The relay's `reopen` is for links that need rebuilding from outside. This
    #: one heals itself, so tearing it down and scanning again would only add a
    #: minute to something already in progress.
    self_healing = True

    def __init__(self, address=None, name=BLE_NAME, timeout=20.0):
        import asyncio
        from bleak import BleakClient, BleakScanner

        self.address = address
        self.want_name = name
        # Bounded, both of them. A queue that grows without limit while the link
        # is down is a queue that delivers a minute of stale jogs the moment it
        # comes back.
        self.rx = queue.Queue(maxsize=4096)
        self.outgoing = queue.Queue(maxsize=256)
        self.up = threading.Event()
        self.first = threading.Event()
        self.generation = 0
        self.error = None
        self.buffer = b""
        self.name = address or name
        self._stop = threading.Event()

        def on_notify(_handle, data):
            self.buffer += bytes(data)
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                try:
                    self.rx.put_nowait(line.decode("utf-8", "replace").strip())
                except queue.Full:
                    pass
            if len(self.buffer) > 4096:
                self.buffer = b""

        async def find():
            if self.address:
                return self.address
            device = await BleakScanner.find_device_by_filter(
                lambda d, ad: (ad.local_name or d.name or "") == self.want_name,
                timeout=timeout)
            if device is None:
                raise RuntimeError("no BLE puck advertising as %r" % self.want_name)
            return device.address

        async def serve(client, target):
            # A connection that came up against a stale service list looks
            # exactly like a working one until the first write fails, so check
            # for the channel before announcing the link.
            if client.services.get_characteristic(PUCK_TX) is None:
                forget_ble_device(target)
                raise RuntimeError("no puck service at %s - dropped the BlueZ "
                                   "cache, retrying" % target)
            self.buffer = b""
            await client.start_notify(PUCK_TX, on_notify)
            self.name = target
            self.generation += 1
            self.up.set()
            self.first.set()
            log("ble: connected to %s" % target)
            while not self._stop.is_set() and client.is_connected:
                try:
                    line = self.outgoing.get_nowait()
                except queue.Empty:
                    # Polled rather than awaited: the producer is a plain thread
                    # and 20 ms is under the puck's frame time either way.
                    await asyncio.sleep(0.02)
                    continue
                await client.write_gatt_char(PUCK_RX, (line + "\n").encode(),
                                             response=False)

        async def worker():
            backoff = 1.0
            while not self._stop.is_set():
                target = None
                try:
                    target = await find()
                    async with BleakClient(target, timeout=timeout) as client:
                        await serve(client, target)
                except Exception as failure:      # noqa: BLE001 - reported below
                    self.error = str(failure)
                    if self.first.is_set():
                        log("ble: %s" % failure)
                finally:
                    if self.up.is_set():
                        log("ble: link down")
                    self.up.clear()
                    # Whatever was queued for a puck that isn't listening is
                    # stale by the time one is.
                    while True:
                        try:
                            self.outgoing.get_nowait()
                        except queue.Empty:
                            break
                    self.first.set()
                if self._stop.is_set():
                    break
                await asyncio.sleep(backoff)
                backoff = 1.0 if self.up.is_set() else min(backoff * 2, 30.0)

        self._thread = threading.Thread(target=lambda: asyncio.run(worker()), daemon=True)
        self._thread.start()
        # Wait for the first attempt to settle, so a puck that simply isn't there
        # is reported as such rather than becoming a relay that logs nothing.
        # After this the thread keeps the link up on its own.
        if not self.first.wait(timeout + 15):
            self._stop.set()
            raise RuntimeError("BLE connect timed out")
        if not self.up.is_set():
            self._stop.set()
            raise RuntimeError(self.error or "could not connect")

    @property
    def connected(self):
        return self.up.is_set()

    def close(self):
        self._stop.set()

    def send(self, line):
        if not self.up.is_set():
            return                          # dropped on purpose: see `outgoing`
        try:
            self.outgoing.put_nowait(line)
        except queue.Full:
            pass

    def lines(self, timeout):
        out = []
        deadline = time.time() + timeout
        while True:
            try:
                out.append(self.rx.get(timeout=max(0.0, deadline - time.time())))
            except queue.Empty:
                return out
            if time.time() >= deadline:
                return out


def find_usb(pattern=USB_GLOB):
    matches = sorted(glob.glob(pattern))
    return matches[0] if matches else None


def open_link(args):
    """Whichever transport the puck is actually on, preferring the cable."""
    want = args.link
    if want in ("auto", "usb"):
        path = args.port or find_usb(args.port_glob)
        if path:
            link = UsbLink(path)
            log("puck on %s (usb)" % path)
            return link
        if want == "usb":
            raise SystemExit("no puck on USB (looked for %s)" % args.port_glob)
    if want in ("auto", "ble"):
        try:
            link = BleLink(args.address)
            log("puck on %s (ble)" % link.name)
            return link
        except ImportError:
            raise SystemExit("BLE needs bleak: run this with `uv run tools/pico2joy.py …`")
        except RuntimeError as error:
            raise SystemExit("no puck over BLE: %s" % error)
    raise SystemExit("no puck found on USB or BLE")


# --------------------------------------------------------------------------
# the printer
# --------------------------------------------------------------------------

class Moonraker:
    """Klipper, over HTTP. Small enough not to want a websocket.

    Moonraker only answers hosts in its `trusted_clients`, so from a machine
    that isn't on that list, pass `--api-key` or let `--tunnel` forward the port
    from localhost.
    """

    def __init__(self, base, api_key=None, timeout=4.0):
        self.base = base.rstrip("/")
        self.api_key = api_key
        self.timeout = timeout

    def _request(self, path, payload=None):
        headers = {}
        if self.api_key:
            headers["X-Api-Key"] = self.api_key
        data = None
        if payload is not None:
            data = json.dumps(payload).encode()
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(self.base + path, data=data, headers=headers)
        with urllib.request.urlopen(request, timeout=self.timeout) as response:
            return json.loads(response.read().decode())

    def status(self):
        query = ("toolhead=position,axis_minimum,axis_maximum,homed_axes"
                 "&gcode_move=gcode_position&print_stats=state")
        return self._request("/printer/objects/query?" + query)["result"]["status"]

    def gcode(self, script):
        return self._request("/printer/gcode/script", {"script": script})


def start_tunnel(spec, remote_port=7125):
    """`ssh -N -L` so requests reach Moonraker as if from localhost.

    A tunnel someone already opened is reused rather than fought with - two of
    these running at once is a normal thing to want, and the cheapest way to
    tell a working tunnel from a stale listener is to ask Moonraker through it.
    """
    import socket

    for candidate in range(17125, 17136):
        base = "http://127.0.0.1:%d" % candidate
        try:
            Moonraker(base, timeout=2.0).status()
        except Exception:      # noqa: BLE001 - not a tunnel of ours; try the next
            continue
        log("tunnel: reusing the one already on :%d" % candidate)
        return None, base

    local = None
    for candidate in range(17125, 17136):
        probe = socket.socket()
        try:
            probe.bind(("127.0.0.1", candidate))
        except OSError:
            continue
        finally:
            probe.close()
        local = candidate
        break
    if local is None:
        raise SystemExit("no free local port for the ssh tunnel")

    proc = subprocess.Popen(
        ["ssh", "-N", "-o", "BatchMode=yes", "-o", "ExitOnForwardFailure=yes",
         "-L", "%d:127.0.0.1:%d" % (local, remote_port), spec],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    base = "http://127.0.0.1:%d" % local
    # Give it as long as an ssh handshake to a small printer host takes, and
    # judge it by whether Moonraker answers rather than by a fixed sleep.
    for _ in range(20):
        time.sleep(0.5)
        if proc.poll() is not None:
            raise SystemExit("ssh tunnel to %s exited" % spec)
        try:
            Moonraker(base, timeout=2.0).status()
        except Exception:      # noqa: BLE001 - still coming up
            continue
        log("tunnel: %s:%d -> 127.0.0.1:%d" % (spec, remote_port, local))
        return proc, base
    proc.terminate()
    raise SystemExit("ssh tunnel to %s never started serving" % spec)


def um(value_mm):
    return int(round(value_mm * 1000.0))


class Gantry:
    """The relay: Moonraker's truth down to the puck, jog requests back up."""

    def __init__(self, link, printer, args):
        self.link = link
        self.printer = printer
        self.args = args
        self.limits = None
        self.limits_sent = 0.0
        self.homed = ""
        self.state = 0
        self.position = [0.0, 0.0, 0.0]
        # Jogs waiting to go out, in micrometres per axis. The puck already
        # coalesces a frame's worth of detents into one request; this coalesces
        # the requests themselves, because each one costs an HTTP round trip and
        # a G-code script that Moonraker holds open until the move finishes.
        # Spin the knob fast without it and the relay spends its time waiting
        # for 0.1 mm moves while the queue - and the lag - grows.
        self.queued = [0, 0, 0]
        self.queued_since = 0.0
        self.queued_count = 0

    def poll(self):
        status = self.printer.status()
        toolhead = status.get("toolhead", {})
        move = status.get("gcode_move", {})
        stats = status.get("print_stats", {})
        # gcode_position is what every other UI shows, so it is what the puck shows.
        self.position = (move.get("gcode_position") or toolhead.get("position") or [0, 0, 0])[:3]
        self.homed = toolhead.get("homed_axes", "") or ""
        self.state = STATES.get(stats.get("state", ""), 1 if self.homed else 0)
        minimum, maximum = toolhead.get("axis_minimum"), toolhead.get("axis_maximum")
        if minimum and maximum:
            self.limits = [(um(minimum[i]), um(maximum[i])) for i in range(3)]

    def push(self, now):
        bits = 0
        for index, axis in enumerate(AXES):
            if axis in self.homed:
                bits |= 1 << index
        self.link.send("#s %d %d %d %d %d" % (
            um(self.position[0]), um(self.position[1]), um(self.position[2]), bits, self.state))
        if self.limits and now - self.limits_sent > 5.0:
            self.limits_sent = now
            flat = []
            for low, high in self.limits:
                flat += [low, high]
            self.link.send("#l " + " ".join(str(v) for v in flat))

    def handle(self, line):
        if not line.startswith("#"):
            if self.args.verbose and line:
                log("puck: %s" % line)
            return
        fields = line[1:].split()
        if not fields:
            return
        kind, rest = fields[0], fields[1:]
        if kind == "j":
            self.jog(rest)
        elif kind == "c":
            self.command(rest)
        elif kind == "v":
            log("puck: protocol v%s" % (rest[0] if rest else "?"))
        elif kind == "b":
            log("puck: rebooting into bootloader mode %s" % (rest[0] if rest else "?"))
        elif self.args.verbose:
            log("puck: unknown %s" % line)

    def jog(self, fields):
        """Take a jog request. It goes out on the next flush, not now."""
        try:
            axis_index, delta_um = int(fields[0]), int(fields[1])
        except (IndexError, ValueError):
            log("bad jog: %r" % (fields,))
            return
        if axis_index not in (0, 1, 2) or delta_um == 0:
            return
        axis = AXES[axis_index]
        if self.state == 2:
            log("refused jog %s: printing" % axis.upper())
            return
        if axis not in self.homed:
            log("refused jog %s: not homed" % axis.upper())
            return
        if not self.queued_count:
            self.queued_since = time.time()
        self.queued[axis_index] += delta_um
        self.queued_count += 1

    def flush_jogs(self, now):
        """Send everything queued as one move, once it has had time to gather.

        Waiting a beat is the whole point: a knob spun quickly arrives as a
        stream of small deltas, and one move of their sum reaches the same place
        far sooner than fifty moves in a row. Axes travelling together go in one
        G1, so a diagonal is a diagonal rather than a staircase.
        """
        if not self.queued_count or now - self.queued_since < self.args.jog_interval:
            return

        pending, count = self.queued, self.queued_count
        self.queued, self.queued_count = [0, 0, 0], 0

        moves = []
        for axis_index, delta_um in enumerate(pending):
            if delta_um == 0:
                continue
            axis = AXES[axis_index]
            if self.limits:
                low, high = self.limits[axis_index]
                target = um(self.position[axis_index]) + delta_um
                if target < low or target > high:
                    log("refused jog %s: %.2f outside %.2f..%.2f"
                        % (axis.upper(), target / 1000.0, low / 1000.0, high / 1000.0))
                    continue
            moves.append((axis, delta_um / 1000.0))
        if not moves:
            return

        # The slowest axis in the move sets the feedrate; Z is the slow one, and
        # a diagonal that includes it should travel at Z's pace.
        feed = min(FEED[axis] for axis, _ in moves)
        travel = " ".join("%s%.3f" % (axis.upper(), delta) for axis, delta in moves)
        script = ("SAVE_GCODE_STATE NAME=pico2joy_jog\n"
                  "G91\n"
                  "G1 %s F%.0f\n"
                  "RESTORE_GCODE_STATE NAME=pico2joy_jog" % (travel, feed))
        try:
            self.printer.gcode(script)
            log("jog %s%s" % (travel, "" if count == 1 else " (%d requests)" % count))
        except urllib.error.HTTPError as error:
            log("jog %s rejected: %s" % (travel, error.read().decode()[:120]))
        except OSError as error:
            log("jog %s failed: %s" % (travel, error))

    def command(self, fields):
        if not fields:
            return
        name = fields[0]
        scripts = {"home": "G28", "home_z": "G28 Z", "stop": "M112"}
        script = scripts.get(name)
        if script is None:
            log("unknown command %r" % name)
            return
        if self.state == 2 and name != "stop":
            log("refused %s: printing" % name)
            return
        log("command %s -> %s" % (name, script))
        try:
            self.printer.gcode(script)
        except (urllib.error.HTTPError, OSError) as error:
            log("%s failed: %s" % (name, error))


# --------------------------------------------------------------------------
# the X-Carve (GRBL, through CNCJS)
# --------------------------------------------------------------------------

def utf16_len(text):
    """String length the way JavaScript counts it, which is how engine.io does."""
    return sum(2 if ord(ch) > 0xFFFF else 1 for ch in text)


class Cncjs:
    """CNCJS, as a socket.io 2 client over engine.io 3's long-polling transport.

    CNCJS owns the X-Controller's serial port, so this joins the connection it
    already holds - the way a second browser tab does - instead of opening the
    port itself. socket.io would normally upgrade to a websocket; nothing here
    needs one, and plain HTTP keeps this standard-library only like the rest of
    the USB path (the Pi next to the machine has no pip to reach).

    Engine.io 3 frames a polling payload as `<length>:<packet>` repeated, the
    length in UTF-16 units. A packet is a type digit and a body: 0 open, 1 close,
    2 ping, 3 pong, 4 message, 6 noop. A message carries a socket.io packet with
    its own type digit - 0 connect, 2 event (`["name", args...]`), 4 error. The
    client pings every `pingInterval`, or the server drops the session.

    State arrives as events and is cached; nothing polls GRBL from here, because
    CNCJS already asks it for a status report several times a second.

    If someone closes the port in CNCJS, this does not reopen it - that was on
    purpose, whoever did it. It asks for the port list every few seconds and
    rejoins once somebody opens it again. Only a fresh connection (the bridge
    starting, or CNCJS restarting) opens a port nobody has open.
    """

    def __init__(self, base, port, rcfile, name="pico2joy"):
        self.base = base.rstrip("/")
        self.port = port
        self.rcfile = os.path.expanduser(rcfile)
        self.name = name
        self.lock = threading.Lock()          # the cache, between the reader and the relay
        self.post_lock = threading.Lock()     # one POST at a time per session
        self.sid = None
        self.token = None
        self.ping_interval = 25.0
        self.connected = False                # socket.io session up and authorised
        self.joined = False                   # in the port's room, getting its events
        self.rejoin_at = 0.0
        self.status = {}
        self.settings = {}
        self.workflow = "idle"
        self.error = None
        self.stopping = False
        threading.Thread(target=self._run, name="cncjs", daemon=True).start()
        threading.Thread(target=self._keepalive, name="cncjs-ping", daemon=True).start()

    def _make_token(self):
        """An HS256 JWT signed with CNCJS's own secret: what its web UI gets by
        signing in, minted here instead because this runs as the user CNCJS
        runs as, and a CNCJS with no users configured accepts any signed token."""
        with open(self.rcfile) as handle:
            secret = json.load(handle)["secret"]

        def part(data):
            raw = json.dumps(data, separators=(",", ":")).encode()
            return base64.urlsafe_b64encode(raw).rstrip(b"=")

        now = int(time.time())
        signing = part({"alg": "HS256", "typ": "JWT"}) + b"." + part(
            {"id": "", "name": self.name, "iat": now, "exp": now + 86400})
        signature = hmac.new(secret.encode(), signing, hashlib.sha256).digest()
        return (signing + b"." + base64.urlsafe_b64encode(signature).rstrip(b"=")).decode()

    def _url(self):
        query = {"EIO": "3", "transport": "polling", "b64": "1",
                 "t": "%d" % (time.time() * 1000)}
        if self.sid:
            query["sid"] = self.sid
        else:
            query["token"] = self.token
        return "%s/socket.io/?%s" % (self.base, urllib.parse.urlencode(query))

    @staticmethod
    def decode(payload):
        """Split an engine.io 3 polling payload into its packets."""
        packets, index = [], 0
        while index < len(payload):
            colon = payload.index(":", index)
            want, index = int(payload[index:colon]), colon + 1
            start, units = index, 0
            while units < want and index < len(payload):
                units += 2 if ord(payload[index]) > 0xFFFF else 1
                index += 1
            packets.append(payload[start:index])
        return packets

    def _post(self, packet):
        if not self.sid:
            raise OSError("cncjs is not connected")
        data = ("%d:%s" % (utf16_len(packet), packet)).encode()
        request = urllib.request.Request(
            self._url(), data=data, headers={"Content-Type": "text/plain;charset=UTF-8"})
        with self.post_lock, urllib.request.urlopen(request, timeout=5.0) as response:
            response.read()

    def emit(self, name, *args):
        self._post("42" + json.dumps([name] + list(args), separators=(",", ":")))

    def command(self, cmd, *args):
        """A CNCJS controller command: `gcode`, `homing`, `feedhold`, ..."""
        self.emit("command", self.port, cmd, *args)

    def snapshot(self):
        with self.lock:
            return dict(self.status), dict(self.settings), self.workflow, self.joined

    def close(self):
        """Leave, politely. Never `close` the port: that would close it for the
        browser and everyone else too."""
        self.stopping = True
        try:
            self._post("1")
        except OSError:
            pass

    def _run(self):
        backoff = 1.0
        while not self.stopping:
            try:
                self._session()
            except (OSError, ValueError, KeyError, TypeError, AttributeError) as error:
                # urllib's errors, timeouts included, are all OSError; the rest
                # are CNCJS saying something shaped unlike what this expects,
                # which must cost a reconnect rather than the reader thread.
                if str(error) != self.error:
                    self.error = str(error)
                    log("cncjs: %s" % error)
            else:
                backoff = 1.0
            with self.lock:
                self.sid = None
                self.connected = self.joined = False
                self.status = {}
            if self.stopping:
                return
            time.sleep(backoff)
            backoff = min(backoff * 2.0, 30.0)

    def _session(self):
        self.sid = None
        self.token = self._make_token()
        while not self.stopping:
            # Long-polls: CNCJS answers as soon as it has something, or sends a
            # noop within the ping interval.
            url = self._url()
            with urllib.request.urlopen(url, timeout=self.ping_interval + 20.0) as response:
                payload = response.read().decode("utf-8")
            for packet in self.decode(payload):
                self._packet(packet)
            if not self.sid:
                raise ValueError("no engine.io handshake from %s" % self.base)

    def _keepalive(self):
        last_ping = 0.0
        while not self.stopping:
            time.sleep(1.0)
            if not self.sid:
                last_ping = 0.0
                continue
            now = time.time()
            try:
                if now - last_ping >= self.ping_interval * 0.8:
                    last_ping = now
                    self._post("2")
                if self.connected and not self.joined and now >= self.rejoin_at:
                    self.rejoin_at = now + 5.0
                    self.emit("list")
            except OSError:
                pass          # a dead session is the reader's to notice and rebuild

    def _packet(self, packet):
        kind, body = packet[:1], packet[1:]
        if kind == "0":
            handshake = json.loads(body)
            self.sid = handshake["sid"]
            self.ping_interval = handshake.get("pingInterval", 25000) / 1000.0
        elif kind == "1":
            raise ValueError("cncjs closed the session")
        elif kind == "4":
            self._message(body)

    def _message(self, body):
        kind, rest = body[:1], body[1:]
        if kind == "0":
            with self.lock:
                self.connected = True
            self.error = None
            log("cncjs: connected to %s, joining %s" % (self.base, self.port))
            self.rejoin_at = time.time() + 5.0
            self.emit("open", self.port, {"controllerType": "Grbl", "baudrate": 115200})
        elif kind == "4":
            # socketio-jwt turning the token down lands here.
            raise ValueError("cncjs refused the connection: %s" % rest)
        elif kind == "2":
            event = json.loads(rest.lstrip("0123456789"))     # skip an ack id
            if event:
                self._event(event[0], event[1:])

    def _event(self, name, args):
        if name == "Grbl:state" and args:
            with self.lock:
                self.status = args[0].get("status") or {}
        elif name == "Grbl:settings" and args:
            with self.lock:
                self.settings = args[0].get("settings") or {}
        elif name == "workflow:state" and args:
            with self.lock:
                self.workflow = args[0]
        elif name == "serialport:open":
            with self.lock:
                self.joined = True
            log("cncjs: on %s" % self.port)
        elif name == "serialport:close":
            with self.lock:
                self.joined = False
                self.status = {}
            self.rejoin_at = time.time() + 5.0
            log("cncjs: %s was closed; waiting for someone to open it again" % self.port)
        elif name == "serialport:list" and args:
            if any(p.get("port") == self.port and p.get("inuse") for p in args[0]):
                self.emit("open", self.port, {"controllerType": "Grbl", "baudrate": 115200})
        elif name == "serialport:error" and args:
            log("cncjs: port error %s" % (args[0],))


class Carve:
    """The relay for the X-Carve: GRBL's truth down to the puck, jogs back up.

    The same bargain and the same coalescing as [`Gantry`], in GRBL's vocabulary.
    What differs:

    - Positions go down in *work* coordinates, the ones CNCJS shows and you zero,
      and so do the limits: machine travel `[-$13x, 0]` shifted by the work
      offset. The puck only needs position and limits in one frame, and this
      way its numbers match the CNCJS screen.
    - GRBL has no per-axis homed flag. With homing enabled it boots into Alarm
      and stays there until `$H`, so "not in Alarm" is the nearest thing it has,
      and all three axes share it. A machine unlocked with `$X` and never homed
      passes that test while its limits mean nothing - and soft limits are off
      on this machine, so nothing below the puck would catch it either. Home it.
    - Jogs are GRBL 1.1 `$J=` moves, which leave the modal state alone: no
      save-and-restore around them, and a job's G90 is never at risk.
    """

    def __init__(self, link, cnc, args):
        self.link = link
        self.cnc = cnc
        self.args = args
        self.limits = None
        self.limits_sent = 0.0
        self.sent_limits = None
        self.active = ""
        self.state = 0
        self.homed = False
        self.workflow = "idle"
        self.position = [0.0, 0.0, 0.0]
        self.queued = [0, 0, 0]
        self.queued_since = 0.0
        self.queued_count = 0

    def poll(self):
        """Read CNCJS's latest word. False while there is no machine to describe."""
        status, settings, self.workflow, joined = self.cnc.snapshot()
        wpos, wco = status.get("wpos") or {}, status.get("wco") or {}
        self.active = status.get("activeState", "")
        if not joined or not self.active or not wpos:
            return False
        # $13=1 has GRBL report in inches; its travel settings stay millimetres.
        scale = 25.4 if settings.get("$13") == "1" else 1.0
        self.position = [float(wpos.get(axis, 0)) * scale for axis in AXES]
        offset = [float(wco.get(axis, 0)) * scale for axis in AXES]
        self.state = GRBL_STATES.get(self.active, 0)
        # A job paused between lines can leave GRBL itself Idle.
        if self.workflow == "running":
            self.state = 2
        elif self.workflow == "paused" and self.state != 4:
            self.state = 3
        self.homed = self.active != "Alarm"
        try:
            travel = [float(settings["$13%d" % index]) for index in range(3)]
        except (KeyError, ValueError):
            travel = None
        if travel:
            self.limits = [(um(-travel[i] - offset[i]), um(-offset[i])) for i in range(3)]
        return True

    def push(self, now):
        # Limits first: after a re-zero the puck should clamp in the new frame
        # before it sees a position in it.
        if self.limits and (self.limits != self.sent_limits or now - self.limits_sent > 5.0):
            self.limits_sent, self.sent_limits = now, self.limits
            flat = []
            for low, high in self.limits:
                flat += [low, high]
            self.link.send("#xl " + " ".join(str(v) for v in flat))
        self.link.send("#xs %d %d %d %d %d" % (
            um(self.position[0]), um(self.position[1]), um(self.position[2]),
            0b111 if self.homed else 0, self.state))

    def describe(self):
        return "%s, work X%.3f Y%.3f Z%.3f" % (self.active, *self.position)

    def handle(self, line):
        fields = line[1:].split()
        if not fields:
            return
        if fields[0] == "xj":
            self.jog(fields[1:])
        elif fields[0] == "xc":
            self.command(fields[1:])

    def jog(self, fields):
        """Take a jog request. It goes out on the next flush, not now."""
        try:
            axis_index, delta_um = int(fields[0]), int(fields[1])
        except (IndexError, ValueError):
            log("bad jog: %r" % (fields,))
            return
        if axis_index not in (0, 1, 2) or delta_um == 0:
            return
        axis = AXES[axis_index]
        # GRBL only takes a jog from Idle or mid-jog, and a job that is paused
        # expects to find the head where it left it.
        if self.workflow != "idle" or self.state != 1:
            log("refused jog %s: %s" % (
                axis.upper(), self.active if self.workflow == "idle" else "job " + self.workflow))
            return
        if not self.queued_count:
            self.queued_since = time.time()
        self.queued[axis_index] += delta_um
        self.queued_count += 1

    def flush_jogs(self, now):
        """Send everything queued as one `$J=` move, once it has had time to gather."""
        if not self.queued_count or now - self.queued_since < self.args.jog_interval:
            return

        pending, count = self.queued, self.queued_count
        self.queued, self.queued_count = [0, 0, 0], 0

        moves = []
        for axis_index, delta_um in enumerate(pending):
            if delta_um == 0:
                continue
            axis = AXES[axis_index]
            if self.limits:
                low, high = self.limits[axis_index]
                target = um(self.position[axis_index]) + delta_um
                if target < low or target > high:
                    log("refused jog %s: %.2f outside %.2f..%.2f"
                        % (axis.upper(), target / 1000.0, low / 1000.0, high / 1000.0))
                    continue
            moves.append((axis, delta_um / 1000.0))
        if not moves:
            return

        feed = min(CARVE_FEED[axis] for axis, _ in moves)
        travel = " ".join("%s%.3f" % (axis.upper(), delta) for axis, delta in moves)
        try:
            self.cnc.command("gcode", "$J=G91 G21 %s F%.0f" % (travel, feed))
            log("jog %s%s" % (travel, "" if count == 1 else " (%d requests)" % count))
        except OSError as error:
            log("jog %s failed: %s" % (travel, error))

    def command(self, fields):
        name = fields[0] if fields else ""
        if name == "home":
            if self.workflow != "idle":
                log("refused home: job %s" % self.workflow)
                return
            command, spelled = "homing", "$H"
        elif name == "stop":
            # A feed hold, not a reset: it stops the machine without throwing
            # away where GRBL thinks the head is.
            command, spelled = "feedhold", "feed hold"
        else:
            log("unknown command %r" % name)
            return
        log("command %s -> %s" % (name, spelled))
        try:
            self.cnc.command(command)
        except OSError as error:
            log("%s failed: %s" % (name, error))


# --------------------------------------------------------------------------
# flashing
# --------------------------------------------------------------------------

def uf2_drive():
    for pattern in ("/run/media/*/*", "/media/*/*", "/media/*", "/mnt/*"):
        for candidate in glob.glob(pattern):
            if os.path.isfile(os.path.join(candidate, "INFO_UF2.TXT")):
                return candidate
    return None


def mount_uf2_drive():
    if not shutil.which("udisksctl"):
        return False
    try:
        listing = subprocess.check_output(["lsblk", "-rno", "NAME,LABEL,RM"], text=True)
    except (OSError, subprocess.CalledProcessError):
        return False
    for row in listing.splitlines():
        parts = row.split()
        if len(parts) >= 3 and parts[2] == "1" and parts[1] in ("NICENANO", "FTHR840BOOT", "NRF52BOOT"):
            subprocess.call(["udisksctl", "mount", "-b", "/dev/" + parts[0]],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return True
    return False


def cmd_flash(args):
    """Reflash over USB: ask the app to reboot, then copy the UF2 in."""
    image = args.image
    if not os.path.isfile(image):
        raise SystemExit("no such image: %s" % image)

    drive = uf2_drive()
    if drive is None:
        path = args.port or find_usb(args.port_glob)
        if path:
            log("asking %s to reboot into UF2 mode" % path)
            link = UsbLink(path)
            # The console key rather than `#r uf2`, because this has to work on
            # whatever firmware is currently on the puck - including builds from
            # before the machine channel existed.
            link.send("b")
            time.sleep(0.3)
            link.close()
        else:
            log("no puck port; double-tap reset to get the bootloader drive")
        print("waiting for the UF2 drive", end="", flush=True)
        for _ in range(args.wait * 2):
            drive = uf2_drive()
            if drive:
                break
            mount_uf2_drive()
            print(".", end="", flush=True)
            time.sleep(0.5)
        print()
    if drive is None:
        raise SystemExit("no UF2 drive appeared. Double-tap reset and try again.")

    log("copying %s -> %s" % (os.path.basename(image), drive))
    # The bootloader reboots the instant it has the last block, so an error on
    # the *last* write is normal - the drive is already gone. An error before
    # that is a real failure, and silently calling it a flash is how you end up
    # staring at a bootloader wondering why the app never came back.
    total = os.path.getsize(image)
    written = 0
    try:
        with open(image, "rb") as source, \
                open(os.path.join(drive, os.path.basename(image)), "wb") as sink:
            while True:
                chunk = source.read(4096)
                if not chunk:
                    break
                sink.write(chunk)
                written += len(chunk)
            sink.flush()
        os.sync()
    except OSError as error:
        if written < total - 4096:
            raise SystemExit("copy failed after %d of %d bytes: %s" % (written, total, error))
    log("flashed %d of %d bytes" % (written, total))
    return 0


def dfu_package(path):
    """Pull the application image and its init packet out of a DFU zip."""
    with zipfile.ZipFile(path) as archive:
        manifest = json.loads(archive.read("manifest.json"))["manifest"]
        entry = manifest.get("application") or manifest.get("softdevice_bootloader_application")
        if entry is None:
            raise SystemExit("%s has no application image" % path)
        return archive.read(entry["bin_file"]), archive.read(entry["dat_file"])


def cmd_ota(args):
    """Reflash over the air, through the bootloader's Nordic legacy DFU."""
    try:
        import asyncio
        from bleak import BleakClient, BleakScanner
    except ImportError:
        raise SystemExit("OTA needs bleak: run this with `uv run tools/pico2joy.py …`")

    firmware, init_packet = dfu_package(args.package)
    log("%s: %d bytes of application, %d byte init packet"
        % (os.path.basename(args.package), len(firmware), len(init_packet)))

    if not args.no_reboot:
        # Ask the running app to come back as the bootloader's DFU target.
        try:
            link = open_link(args)
        except SystemExit as error:
            log("%s - assuming it is already in DFU mode" % error)
            link = None
        if link is not None:
            log("asking the puck to reboot into BLE DFU")
            link.send("#r ota")
            time.sleep(0.5)
            link.close()
            time.sleep(args.settle)

    async def run():
        log("scanning for a DFU target")
        device = await BleakScanner.find_device_by_filter(
            lambda d, ad: DFU_SERVICE in [s.lower() for s in (ad.service_uuids or [])]
            or (ad.local_name or "").lower().endswith("dfu")
            or (ad.local_name or "") in (args.dfu_name, "DfuTarg"),
            timeout=args.scan)
        if device is None:
            raise SystemExit("no DFU target advertising. Is the puck in OTA mode "
                             "(console 'o', or `#r ota`)?")
        log("DFU target %s (%s)" % (device.name, device.address))

        replies = asyncio.Queue()
        async with BleakClient(device, timeout=30.0) as client:
            await client.start_notify(DFU_CONTROL, lambda _h, data: replies.put_nowait(bytes(data)))

            async def expect(opcode):
                """The response to `opcode`. Packet receipts (0x11) arriving in the
                meantime are flow control, not an answer, and are skipped."""
                while True:
                    reply = await asyncio.wait_for(replies.get(), timeout=30.0)
                    if reply and reply[0] == 0x11:
                        continue
                    if len(reply) < 3 or reply[0] != 0x10 or reply[1] != opcode or reply[2] != 0x01:
                        raise SystemExit("DFU refused at opcode %#x: %s" % (opcode, reply.hex()))
                    return

            # START_DFU, application only.
            await client.write_gatt_char(DFU_CONTROL, bytes([0x01, 0x04]), response=True)
            sizes = (0).to_bytes(4, "little") * 2 + len(firmware).to_bytes(4, "little")
            await client.write_gatt_char(DFU_PACKET, sizes, response=False)
            await expect(0x01)

            # The init packet, which is what carries the device type and CRC.
            await client.write_gatt_char(DFU_CONTROL, bytes([0x02, 0x00]), response=True)
            for offset in range(0, len(init_packet), 20):
                await client.write_gatt_char(DFU_PACKET, init_packet[offset:offset + 20],
                                             response=False)
            await client.write_gatt_char(DFU_CONTROL, bytes([0x02, 0x01]), response=True)
            await expect(0x02)

            # Receipt notifications every N packets, so a stall is visible.
            await client.write_gatt_char(DFU_CONTROL, bytes([0x08]) +
                                         args.receipts.to_bytes(2, "little"), response=True)
            await client.write_gatt_char(DFU_CONTROL, bytes([0x03]), response=True)

            sent = 0
            sent_bytes = 0
            for offset in range(0, len(firmware), 20):
                chunk = firmware[offset:offset + 20]
                await client.write_gatt_char(DFU_PACKET, chunk, response=False)
                sent += 1
                sent_bytes += len(chunk)
                if args.receipts and sent % args.receipts == 0:
                    # Flow control: the receipt says how much the target has
                    # actually taken, so wait until that covers what was sent
                    # rather than counting notifications and hoping they line
                    # up. A receipt that never comes is a lost packet, and a
                    # timeout is the right answer to that.
                    while True:
                        reply = await asyncio.wait_for(replies.get(), timeout=30.0)
                        if reply and reply[0] == 0x11 and len(reply) >= 5:
                            if int.from_bytes(reply[1:5], "little") >= sent_bytes:
                                break
                        elif reply and reply[0] == 0x10:
                            raise SystemExit("DFU error mid-image: %s" % reply.hex())
                    print("\r  %d%%" % (100 * sent_bytes // len(firmware)), end="", flush=True)
            print()
            await expect(0x03)

            await client.write_gatt_char(DFU_CONTROL, bytes([0x04]), response=True)
            await expect(0x04)
            log("validated; activating")
            try:
                await client.write_gatt_char(DFU_CONTROL, bytes([0x05]), response=True)
            except Exception:      # noqa: BLE001 - the target resets mid-write, by design
                pass
        log("done - the puck should be back on the new firmware")

    asyncio.run(run())
    return 0


# --------------------------------------------------------------------------
# subcommands
# --------------------------------------------------------------------------

def cmd_scan(args):
    path = find_usb(args.port_glob)
    print("usb: %s" % (path or "not found"))
    try:
        import asyncio
        from bleak import BleakScanner
    except ImportError:
        print("ble: bleak not installed (run with `uv run tools/pico2joy.py scan`)")
        return 0

    async def scan():
        # return_adv, because a non-connectable advertiser's name lives in the
        # advertisement rather than on the device object.
        found = await BleakScanner.discover(timeout=args.scan, return_adv=True)
        hits = []
        for address, (device, advert) in found.items():
            name = advert.local_name or device.name or ""
            if BLE_NAME in name.lower() or name in ("DfuTarg", args.dfu_name):
                hits.append("%s (%s, %d dBm)" % (name, address, advert.rssi))
        if hits:
            for hit in hits:
                print("ble: %s" % hit)
        else:
            print("ble: no puck advertising (menu row `ble`, or console 'w', turns the radio on)")

    asyncio.run(scan())
    return 0


def cmd_monitor(args):
    link = open_link(args)
    log("monitoring - ctrl-c to stop")
    try:
        while True:
            for line in link.lines(0.5):
                print(line, flush=True)
    except KeyboardInterrupt:
        return 0
    finally:
        link.close()


def run_relay(args, only=None):
    """Own the puck's link and serve several apps over it, one at a time.

    The machine channel is already multiplexed by message type - `#s`/`#j` for
    the gantry, `#ns`/`#a`/`#m` for the player - and the puck already demuxes on
    it, so the only thing that made the apps mutually exclusive was running them
    as separate programs, each grabbing the one link. This is the single owner:
    it holds the link, runs both apps, and streams *only* the one whose view is
    on screen (the puck announces it with `#view <name>`). `only` pins one app
    and ignores the announcement, which is what the `gantry` and `spotify`
    wrappers use.

    One owner per link still holds - this *is* the owner - but now the puck can
    switch between jogging and music by turning the knob, not by restarting a
    program.
    """
    need_gantry = only in (None, "gantry")
    need_media = only in (None, "music")
    need_quota = only in (None, "quota")
    # Opt-in on the relay: only the machine next to the X-Carve has a CNCJS to
    # talk to and the secret to talk to it with.
    need_xcarve = only == "xcarve" or (only is None and getattr(args, "cncjs", None))

    tunnel = None
    printer = None
    if need_gantry:
        base = args.moonraker
        if getattr(args, "tunnel", None):
            tunnel, base = start_tunnel(args.tunnel)
        printer = Moonraker(base, args.api_key)
    player = Player() if need_media else None
    subs = subscriptions(args) if need_quota else []
    # Started before the puck is found, so CNCJS is already connected - and its
    # problems already logged - by the time there is a puck to show them on.
    cnc = Cncjs(args.cncjs, args.cncjs_port, args.cncrc) if need_xcarve else None

    # Wait rather than exit: as a service this may start before the puck is
    # plugged in, and "no puck yet" is not a failure worth restarting over.
    def connect():
        while True:
            try:
                return open_link(args)
            except SystemExit as error:
                if not getattr(args, "wait_for_puck", False):
                    raise
                log("%s - waiting" % error)
                time.sleep(5.0)

    link = connect()
    gantry = Gantry(link, printer, args) if need_gantry else None
    media = Media(link, player, args) if need_media else None
    quota = Quota(link, subs, args) if need_quota else None
    carve = Carve(link, cnc, args) if cnc else None

    by_view = {}
    owner = {}
    if gantry:
        by_view["gantry"] = gantry
        owner["j"] = owner["c"] = gantry
    if carve:
        by_view["xcarve"] = carve
        owner["xj"] = owner["xc"] = carve
    if media:
        by_view["music"] = media
        owner["m"] = media
    if quota:
        by_view["quota"] = quota
        owner["q"] = quota

    def set_link(new):
        nonlocal link
        link = new
        if gantry:
            gantry.link = new
        if media:
            media.link = new
        if quota:
            quota.link = new
        if carve:
            carve.link = new

    def resync():
        """Tell a puck everything again. What a fresh connection is owed."""
        if state["active"]:
            activate(state["active"])
        if not only:
            link.send("#?")              # which view is up on this puck now?

    def reopen():
        # A self-healing link is already trying; tearing it down would only
        # restart a scan that is in progress. Let it be.
        if getattr(link, "self_healing", False):
            return True
        try:
            link.close()
        except OSError:
            pass
        for _ in range(20):
            time.sleep(1.0)
            try:
                set_link(open_link(args))
                resync()
                return True
            except SystemExit:
                continue
        log("puck did not come back")
        return False

    def activate(app):
        # Whatever changed while its view was off screen, resend in full.
        if app is gantry:
            gantry.limits_sent = 0.0
            log("relay: gantry")
        elif app is carve:
            carve.limits_sent, carve.sent_limits = 0.0, None
            log("relay: xcarve")
        elif app is media:
            media.last = media.last_art_url = None
            log("relay: music")
        elif app is quota:
            quota.reset()
            # Turning to this screen is itself the question, so ask the vendors
            # again rather than showing whatever the last poll happened to see.
            quota.refresh()
            log("relay: quota")

    # A dict so the nested handlers can rebind it without `nonlocal` gymnastics.
    state = {"active": by_view.get(only) if only else None, "moonraker_ok": None,
             "cncjs_ok": None}

    def route(line):
        if not line.startswith("#"):
            if getattr(args, "verbose", False) and line:
                log("puck: %s" % line)
            return
        fields = line[1:].split()
        if not fields:
            return
        kind = fields[0]
        if kind == "view":
            if only:
                return                      # pinned: the puck's view doesn't steer us
            name = fields[1] if len(fields) > 1 else ""
            new = by_view.get(name)
            if new is not state["active"]:
                state["active"] = new
                if new:
                    activate(new)
                else:
                    log("relay: %s (nothing to stream)" % (name or "?"))
            return
        app = owner.get(kind)
        if app is not None:
            app.handle(line)
        elif kind == "v":
            pass                            # identify reply, already have the view
        elif getattr(args, "verbose", False):
            log("puck: unknown %s" % line)

    if only:
        log("relay: %s, pinned to %s" % (link.name, only))
        activate(state["active"])
    else:
        log("relay: %s, following the puck's view" % link.name)
    if printer:
        log("moonraker at %s" % printer.base)
    if cnc:
        log("cncjs at %s, port %s" % (cnc.base, cnc.port))
    if not only:
        try:
            link.send("#?")                 # which view is up right now?
        except OSError:
            pass

    # Connections seen so far. A link that dropped and came back is talking to a
    # puck that was told everything before the gap and remembers none of it, so
    # every reconnection - not just the first - gets the full state again.
    seen = link.generation
    g_period = 1.0 / getattr(args, "rate", 8.0)
    m_period = 0.25
    # A second is plenty: this only pushes the numbers the worker thread already
    # has, and the puck ticks the countdown itself between them.
    q_period = 1.0
    next_g = next_m = next_q = next_x = 0.0

    try:
        while True:
            now = time.time()
            if link.generation != seen:
                seen = link.generation
                log("relay: link back, resending")
                resync()
            # A link that is down takes nothing: `send` drops it, and pushing
            # state at a puck that isn't there only burns the poll.
            if not link.connected:
                for line in link.lines(0.25):
                    route(line)
                continue
            active = state["active"]
            if active is gantry and now >= next_g:
                next_g = now + g_period
                try:
                    gantry.poll()
                    if state["moonraker_ok"] is not True:
                        log("printer: %s, homed %r" % (printer.base, gantry.homed or "nothing"))
                    state["moonraker_ok"] = True
                except (urllib.error.URLError, urllib.error.HTTPError, OSError,
                        ValueError, KeyError) as error:
                    if state["moonraker_ok"] is not False:
                        log("moonraker: %s" % error)
                        if isinstance(error, urllib.error.HTTPError) and error.code == 401:
                            log("  -> not a trusted client. Use --api-key, or --tunnel"
                                " user@host, or add this host to trusted_clients.")
                    state["moonraker_ok"] = False
                if state["moonraker_ok"]:
                    try:
                        gantry.push(now)
                    except OSError as error:
                        log("puck write failed (%s); reopening" % error)
                        if not reopen():
                            continue
            elif active is carve and now >= next_x:
                next_x = now + g_period
                # No request here: CNCJS pushes, and this reads what it pushed.
                ok = carve.poll()
                if ok != state["cncjs_ok"]:
                    state["cncjs_ok"] = ok
                    log("xcarve: %s" % (carve.describe() if ok else "no machine state from cncjs"))
                if ok:
                    try:
                        carve.push(now)
                    except OSError as error:
                        log("puck write failed (%s); reopening" % error)
                        if not reopen():
                            continue
            elif active is media and now >= next_m:
                next_m = now + m_period
                try:
                    media.poll()
                except OSError as error:
                    log("puck write failed (%s); reopening" % error)
                    if not reopen():
                        continue
            elif active is quota and now >= next_q:
                next_q = now + q_period
                try:
                    quota.poll()
                except OSError as error:
                    log("puck write failed (%s); reopening" % error)
                    if not reopen():
                        continue

            try:
                for line in link.lines(0.05):
                    route(line)
            except OSError as error:
                log("puck read failed (%s); reopening" % error)
                reopen()

            if gantry:
                gantry.flush_jogs(time.time())
            if carve:
                carve.flush_jogs(time.time())
    except KeyboardInterrupt:
        log("stopped")
    finally:
        link.close()
        if tunnel:
            tunnel.terminate()
        if cnc:
            cnc.close()
    return 0


def cmd_relay(args):
    """Own the link and follow the puck's view between the gantry and the player."""
    return run_relay(args, only=None)


def cmd_gantry(args):
    """Drive the gantry only: a relay pinned to the gantry app."""
    return run_relay(args, only="gantry")


def cmd_xcarve(args):
    """Drive the X-Carve only: a relay pinned to the xcarve app."""
    return run_relay(args, only="xcarve")


# --------------------------------------------------------------------------
# the media player (MPRIS, via playerctl)
# --------------------------------------------------------------------------

# The device's OLED is 128x128, one bit per pixel, and its framebuffer is 16
# pages of 128 columns with the low bit at the top - and the panel's RAM sits 90
# degrees to the glass, so a logical pixel (x, y) lands at panel column y, row
# 127 - x (see Display::set_pixel in src/display.rs). We pack the cover into that
# exact layout here so the firmware can blit it in one memcpy.
ART_W = ART_H = 128
ART_BYTES = ART_W * ART_H // 8
ART_CHUNK = 40          # bytes per '#a' line; 80 hex chars, inside the 96 cap
VOL_STEP = 5            # percent per knob detent


class Player:
    """Whatever MPRIS player is active, plus the host's own output volume.

    Control and now-playing go through `playerctl` (following `playerctld`, so it
    tracks the player you last touched); volume is the host sink via `wpctl`, or
    `pactl` if wireplumber isn't the one in charge. Both are plain subprocesses -
    no D-Bus binding to install.
    """

    def __init__(self):
        if not shutil.which("playerctl"):
            raise SystemExit("this needs `playerctl` (pacman -S playerctl, apt install playerctl)")
        self.pc = ["playerctl", "-p", "playerctld"]
        if shutil.which("wpctl"):
            self.sink = "wpctl"
        elif shutil.which("pactl"):
            self.sink = "pactl"
        else:
            self.sink = None
            log("no wpctl or pactl found - volume control disabled")

    def _run(self, args):
        try:
            out = subprocess.run(args, capture_output=True, text=True, timeout=2)
            return out.stdout.strip() if out.returncode == 0 else None
        except (OSError, subprocess.SubprocessError):
            return None

    def now_playing(self):
        """(status, title, artist, art_url). status is 0 stopped / 1 playing / 2 paused."""
        fmt = "{{status}}\x1f{{title}}\x1f{{artist}}\x1f{{mpris:artUrl}}"
        out = self._run(self.pc + ["metadata", "--format", fmt])
        if not out:
            return (0, "", "", "")
        parts = (out.split("\x1f") + ["", "", "", ""])[:4]
        status = {"Playing": 1, "Paused": 2}.get(parts[0], 0)
        return (status, parts[1], parts[2], parts[3])

    def play_pause(self):
        self._run(self.pc + ["play-pause"])

    def next(self):
        self._run(self.pc + ["next"])

    def previous(self):
        self._run(self.pc + ["previous"])

    def volume(self):
        """Host output volume, 0-100, or None if it can't be read."""
        if self.sink == "wpctl":
            out = self._run(["wpctl", "get-volume", "@DEFAULT_AUDIO_SINK@"])
            if out and out.startswith("Volume:"):
                try:
                    return max(0, min(100, round(float(out.split()[1]) * 100)))
                except (IndexError, ValueError):
                    return None
        elif self.sink == "pactl":
            out = self._run(["pactl", "get-sink-volume", "@DEFAULT_SINK@"])
            if out and "%" in out:
                try:
                    return int(out.split("/")[1].strip().rstrip("%"))
                except (IndexError, ValueError):
                    return None
        return None

    def nudge_volume(self, steps):
        pct = abs(steps) * VOL_STEP
        sign = "+" if steps > 0 else "-"
        if self.sink == "wpctl":
            self._run(["wpctl", "set-volume", "-l", "1.0",
                       "@DEFAULT_AUDIO_SINK@", "%d%%%s" % (pct, sign)])
        elif self.sink == "pactl":
            self._run(["pactl", "set-sink-volume", "@DEFAULT_SINK@", "%s%d%%" % (sign, pct)])


def cover_framebuffer(url):
    """Fetch an art URL and pack it into the device's 2048-byte 1bpp framebuffer.

    Returns None if there's no art or it can't be decoded. Pillow does the
    decode, resize and Floyd-Steinberg dither; a lit bit is a bright part of the
    image, so the cover reads the right way round on the panel.
    """
    if not url:
        return None
    try:
        from PIL import Image
    except ImportError:
        raise SystemExit("album art needs pillow: run this with `uv run tools/pico2joy.py …`")
    try:
        if url.startswith("file://"):
            image = Image.open(url[7:])
        else:
            import io
            import urllib.request
            data = urllib.request.urlopen(url, timeout=10).read()
            image = Image.open(io.BytesIO(data))
        mono = image.convert("L").resize((ART_W, ART_H), Image.LANCZOS).convert("1")
    except (OSError, ValueError) as error:
        log("cover: %s" % error)
        return None

    px = mono.load()
    fb = bytearray(ART_BYTES)
    for x in range(ART_W):            # logical column, left to right
        for y in range(ART_H):        # logical row, top to bottom
            if px[x, y]:              # 255 (white) -> lit
                row = ART_H - 1 - x
                fb[(row // 8) * ART_W + y] |= 1 << (row % 8)
    return bytes(fb)


class Media:
    """Relay between the puck and the active player. The puck is a view: it shows
    what the player reports and asks for changes; it is never the source of truth."""

    def __init__(self, link, player, args):
        self.link = link
        self.player = player
        self.args = args
        self.last = None            # (status, title, artist) last sent
        self.last_vol = None
        self.last_art_url = None


    def handle(self, line):
        fields = line.split()
        if not fields or fields[0] not in ("#m", "m"):
            return
        what = fields[1] if len(fields) > 1 else ""
        if what == "p":
            self.player.play_pause(); log("play/pause")
        elif what == "n":
            self.player.next(); log("next")
        elif what == "b":
            self.player.previous(); log("previous")
        elif what == "v" and len(fields) > 2:
            try:
                steps = int(fields[2])
            except ValueError:
                return
            self.player.nudge_volume(steps)
            # Report the new level straight back, so the bar tracks the knob.
            self.push_volume(force=True)

    def send_cover(self, url):
        fb = cover_framebuffer(url)
        if fb is None:
            return
        self.link.send("#ab")
        for seq in range(0, len(fb), ART_CHUNK):
            chunk = fb[seq:seq + ART_CHUNK]
            self.link.send("#a %d %s" % (seq // ART_CHUNK, chunk.hex()))
        self.link.send("#ae")
        log("cover: %d bytes sent" % len(fb))

    def push_volume(self, force=False):
        vol = self.player.volume()
        v = 255 if vol is None else vol
        if force or v != self.last_vol:
            self.last_vol = v
            status = self.last[0] if self.last else 0
            self.link.send("#ns %d %d" % (status, v))

    def poll(self):
        status, title, artist, art_url = self.player.now_playing()
        vol = self.player.volume()
        v = 255 if vol is None else vol
        self.last_vol = v
        # A heartbeat every poll, not just on change: the puck falls back to
        # "no bridge" if `#ns` goes quiet for a couple of seconds, the same way
        # the gantry keeps `#s` flowing. Play state and volume ride along, so
        # both stay live without their own messages.
        self.link.send("#ns %d %d" % (status, v))
        state = (status, title, artist)
        if state != self.last:
            self.last = state
            self.link.send("#nt %s" % title[:88])
            self.link.send("#na %s" % artist[:88])
            log("%s | %s - %s" % (("stopped", "playing", "paused")[status],
                                  title or "(nothing)", artist or ""))
        if art_url != self.last_art_url:
            self.last_art_url = art_url
            self.send_cover(art_url)



def cmd_spotify(args):
    """Bridge the media player only: a relay pinned to the music app."""
    return run_relay(args, only="music")


# --------------------------------------------------------------------------
# the subscription quotas (Claude and Codex rate-limit windows)
# --------------------------------------------------------------------------

# Both vendors already answer "how much of your plan have you spent" to the
# credentials their own CLI leaves on disk, so this asks them the same way their
# CLIs do and relays the answer. Nothing is scraped from transcripts and nothing
# is counted here: a token tally computed locally would be a second, wronger copy
# of a number the vendor is authoritative for - the same reason the gantry screen
# waits for Klipper rather than integrating its own jogs.
CLAUDE_USAGE_URL = "https://api.anthropic.com/api/oauth/usage"
CODEX_USAGE_URL = "https://chatgpt.com/backend-api/wham/usage"

KIND_CLAUDE, KIND_CODEX = 0, 1
# The puck's states, from src/quota.rs. Q_WAIT is what a slot reads as between
# connecting and the first answer coming back - "asking", not "broken".
Q_OK, Q_AUTH, Q_ERROR, Q_WAIT = 0, 1, 2, 3

# Claude names its windows rather than measuring them, so their lengths are the
# only two constants here the vendor didn't hand us. Codex reports its own.
CLAUDE_FIVE_HOUR = 5 * 3600
CLAUDE_SEVEN_DAY = 7 * 86400

# What `String<14>` on the puck can hold (see src/quota.rs).
LABEL_CHARS = 14

# How often to ask, by default. These endpoints rate-limit - a 429 is easy to
# earn, and it takes a while to clear - and a window that moves over hours does
# not want asking every minute. Five minutes of staleness costs nothing when
# holding a button re-checks on demand.
REFRESH_DEFAULT = 300.0
# The longest the backoff may stretch a poll. Holding a button still jumps the
# queue, so this only bounds how long an unattended screen stays stale.
BACKOFF_CAP = 900.0


def count_sessions(pattern, config_dir, env_var):
    """How many of that CLI's sessions are running, for this config directory.

    Neither vendor reports this, so it is counted where the answer actually
    exists: the machine the account is logged in on. Top-level processes only -
    a session spawns helpers with the same name, so counting every `claude` in
    `ps` says nine when three windows are open. A process is a session when its
    parent isn't one of the same kind.

    Returns None rather than 0 when it can't tell, so "no sessions" and "no way
    to look" stay different on screen.
    """
    try:
        out = subprocess.run(["ps", "-eo", "pid,ppid,comm", "--no-headers"],
                             capture_output=True, text=True, timeout=5)
        if out.returncode != 0:
            return None
    except (OSError, subprocess.SubprocessError):
        return None

    procs = {}
    for line in out.stdout.splitlines():
        parts = line.split(None, 2)
        if len(parts) == 3 and parts[0].isdigit() and parts[1].isdigit():
            procs[int(parts[0])] = (int(parts[1]), parts[2].strip())
    same = {pid for pid, (_, comm) in procs.items() if comm == pattern}

    wanted = os.path.abspath(os.path.expanduser(config_dir))
    running = 0
    for pid in same:
        if procs[pid][0] in same:
            continue                    # a helper of another session, not one
        # Which account it belongs to: whatever config directory it was pointed
        # at, or the vendor default. Unreadable (a different user's process)
        # counts as the default rather than being dropped.
        try:
            with open("/proc/%d/environ" % pid) as handle:
                env = dict(item.split("=", 1)
                           for item in handle.read().split("\0") if "=" in item)
        except OSError:
            env = {}
        theirs = env.get(env_var) or ("~/.claude" if env_var == "CLAUDE_CONFIG_DIR"
                                      else "~/.codex")
        if os.path.abspath(os.path.expanduser(theirs)) == wanted:
            running += 1
    return running


class QuotaError(Exception):
    """A reading that failed, carrying which of the puck's states it maps to.

    `keep` marks the failures that say nothing about the account - a 429 from the
    usage endpoint, a machine that is briefly unreachable - where the honest move
    is to hold the last reading and ask again later rather than to blank a screen
    that was right a minute ago.
    """

    def __init__(self, state, message, keep=False, retry_after=0):
        Exception.__init__(self, message)
        self.state = state
        self.keep = keep
        self.retry_after = retry_after


def get_json(url, headers, timeout=8.0):
    request = urllib.request.Request(url, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return json.loads(response.read().decode("utf-8"))
    except urllib.error.HTTPError as error:
        # 401/403 is the one failure the user can act on, and the one worth its
        # own word on a 128-pixel screen. 429 is the usage endpoint itself being
        # rate-limited, which is nothing to do with the plan behind it.
        state = Q_AUTH if error.code in (401, 403) else Q_ERROR
        try:
            after = int(error.headers.get("Retry-After") or 0)
        except (AttributeError, ValueError):
            after = 0
        raise QuotaError(state, "HTTP %d" % error.code, keep=error.code == 429,
                         retry_after=after)
    except (urllib.error.URLError, OSError, ValueError) as error:
        raise QuotaError(Q_ERROR, str(error), keep=True)


def read_json(path):
    try:
        with open(path) as handle:
            return json.load(handle)
    except FileNotFoundError:
        raise QuotaError(Q_AUTH, "no credentials at %s" % path)
    except (OSError, ValueError) as error:
        raise QuotaError(Q_ERROR, str(error))


def seconds_until(when):
    """Seconds from now to an ISO-8601 instant, or -1 if there isn't one.

    -1 rather than None because it goes straight onto the wire, where every
    field is an integer and "didn't say" has to be one too.
    """
    if not when:
        return -1
    import datetime
    text = when.replace("Z", "+00:00")           # 3.9's parser won't take a Z
    try:
        moment = datetime.datetime.fromisoformat(text)
    except ValueError:
        return -1
    if moment.tzinfo is None:
        moment = moment.replace(tzinfo=datetime.timezone.utc)
    return max(0, int(moment.timestamp() - time.time()))


def percent(value):
    """A utilisation figure as 0-100, or 255 for "the vendor didn't say"."""
    if value is None:
        return 255
    try:
        return max(0, min(100, int(round(float(value)))))
    except (TypeError, ValueError):
        return 255


def short_label(text, fallback):
    """A name that fits the puck's row: printable ASCII, no spaces, clipped.

    Returns `fallback` (which may be None) when there's nothing left to use, so
    callers can chain the sources they'd rather have first.
    """
    text = (text or "").strip()
    if "@" in text:                              # an email is its local part
        text = text.split("@")[0]
    text = "".join(c for c in text if 33 <= ord(c) < 127).lstrip(".")
    return text[:LABEL_CHARS] if text else fallback


def span(seconds):
    """A window length in its coarsest unit - "5h", "7d" - as the puck shows it."""
    if seconds is None or seconds < 0:
        return "?"
    if seconds >= 86400:
        return "%dd" % (seconds // 86400)
    if seconds >= 3600:
        return "%dh" % (seconds // 3600)
    return "%dm" % (seconds // 60)


def run_remote(host, kind, path):
    """Take one reading on `host` by shipping *this file* there over ssh.

    `ssh host python3 - probe claude ~/.claude < pico2joy.py` - the script
    arrives on stdin, runs, prints one JSON line and is gone. Nothing is
    installed on the far end and nothing is left behind, which is the same
    bargain the rest of this tool makes (stdlib only, no venv).

    The token stays where it belongs. The alternative - `cat` the credentials
    back here and make the HTTPS call locally - would pull a live OAuth token
    across the network for no reason; the far machine can make its own request
    and send back six numbers.
    """
    argv = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10",
            host, "python3", "-", "probe", kind, path]
    try:
        with open(os.path.abspath(__file__)) as handle:
            source = handle.read()
        out = subprocess.run(argv, input=source, capture_output=True,
                             text=True, timeout=40)
    except (OSError, subprocess.SubprocessError) as error:
        raise QuotaError(Q_ERROR, "ssh %s: %s" % (host, error))
    if out.returncode != 0:
        detail = (out.stderr or "").strip().splitlines()
        raise QuotaError(Q_ERROR, "ssh %s: %s" % (host, detail[-1] if detail else
                                                  "exit %d" % out.returncode))
    try:
        reading = json.loads(out.stdout.strip().splitlines()[-1])
    except (IndexError, ValueError):
        raise QuotaError(Q_ERROR, "ssh %s: unreadable reply" % host)
    if reading.get("error"):
        raise QuotaError(reading.get("state", Q_ERROR), reading["error"])
    # JSON has no tuples, and the wire format is positional.
    for key in ("five", "week", "names"):
        reading[key] = tuple(reading[key])
    return reading


class ClaudeSub:
    """One Claude subscription, read from a Claude Code config directory.

    Read-only, deliberately. The access token in there is refreshed by Claude
    Code itself; using the refresh token here could rotate it out from under the
    running CLI and log the user out of their own editor to draw a bar on a knob.
    So an expired token reports `auth` and waits for Claude Code to renew it.
    """

    kind = KIND_CLAUDE

    def __init__(self, path, label=None, host=None):
        # A remote path is the far machine's to expand, so leave it alone.
        self.host = host
        self.path = path
        self.dir = path if host else os.path.abspath(os.path.expanduser(path))
        self.forced_label = label
        self.label = label or short_label(os.path.basename(self.dir.rstrip("/")),
                                          host.split("@")[-1].split(".")[0] if host
                                          else "claude")

    def _profile_label(self):
        # `.claude.json` sits inside CLAUDE_CONFIG_DIR when one is set, and
        # beside ~/.claude when one isn't. Try both, quietly.
        for candidate in (os.path.join(self.dir, ".claude.json"),
                          os.path.join(os.path.dirname(self.dir), ".claude.json")):
            try:
                with open(candidate) as handle:
                    account = json.load(handle).get("oauthAccount") or {}
            except (OSError, ValueError):
                continue
            name = account.get("displayName") or account.get("emailAddress")
            if name:
                return short_label(name, "claude")
        return None

    def read(self):
        if self.host:
            reading = run_remote(self.host, "claude", self.path)
            if self.forced_label:
                reading["label"] = self.forced_label
            return reading
        return self.read_here()

    def read_here(self):
        creds = read_json(os.path.join(self.dir, ".credentials.json"))
        oauth = creds.get("claudeAiOauth") or {}
        token = oauth.get("accessToken")
        if not token:
            raise QuotaError(Q_AUTH, "no oauth token in %s" % self.dir)
        expires = oauth.get("expiresAt")
        if expires and expires / 1000.0 < time.time():
            raise QuotaError(Q_AUTH, "token expired - run `claude` in %s" % self.dir)

        data = get_json(CLAUDE_USAGE_URL, {
            "Authorization": "Bearer %s" % token,
            "anthropic-beta": "oauth-2025-04-20",
            "Content-Type": "application/json",
        })
        five = data.get("five_hour") or {}
        week, week_name = self._weekly(data)
        return {
            "label": self.forced_label or self._profile_label() or self.label,
            "kind": self.kind,
            "state": Q_OK,
            "five": (percent(five.get("utilization")), seconds_until(five.get("resets_at"))),
            "week": week,
            "names": (span(CLAUDE_FIVE_HOUR), week_name),
            "sessions": self.sessions(),
        }

    @staticmethod
    def _weekly(data):
        """The weekly cap that actually binds, and what to call it.

        `seven_day` is the all-models figure, but the `limits` array also carries
        weekly caps scoped to one model, and those run out first - a week that is
        29% gone overall can be 41% gone on the model you are actually using. The
        bar shows whichever is highest and its label says which, because the
        useful number is the one you will hit, not the flattering one.
        """
        overall = data.get("seven_day") or {}
        best = (percent(overall.get("utilization")),
                seconds_until(overall.get("resets_at")))
        name = span(CLAUDE_SEVEN_DAY)
        if best[0] == 255:
            best = (0, -1)
        for limit in data.get("limits") or []:
            if (limit or {}).get("group") != "weekly":
                continue
            used = percent(limit.get("percent"))
            if used == 255 or used <= best[0]:
                continue
            best = (used, seconds_until(limit.get("resets_at")))
            model = (((limit.get("scope") or {}).get("model")) or {}).get("display_name")
            name = "%s/%s" % (span(CLAUDE_SEVEN_DAY), model) if model else span(CLAUDE_SEVEN_DAY)
        return best, name[:8]

    def sessions(self):
        return count_sessions("claude", self.dir, "CLAUDE_CONFIG_DIR")


class CodexSub:
    """One Codex subscription, read from a `~/.codex`-shaped directory.

    Same bargain as `ClaudeSub`: the token is Codex's to refresh, this only reads
    it. The usage endpoint is the one the Codex TUI's own status line calls.
    """

    kind = KIND_CODEX

    def __init__(self, path, label=None, host=None):
        # A remote path is the far machine's to expand, so leave it alone.
        self.host = host
        self.path = path
        self.dir = path if host else os.path.abspath(os.path.expanduser(path))
        self.forced_label = label
        self.label = label or short_label(os.path.basename(self.dir.rstrip("/")),
                                          host.split("@")[-1].split(".")[0] if host
                                          else "codex")

    @staticmethod
    def _expired(token):
        """Whether a JWT's `exp` has passed. Unreadable means "let the server say"."""
        try:
            payload = token.split(".")[1]
            import base64
            padded = payload + "=" * (-len(payload) % 4)
            claims = json.loads(base64.urlsafe_b64decode(padded).decode("utf-8"))
        except (IndexError, ValueError, TypeError):
            return False
        exp = claims.get("exp")
        return bool(exp) and exp < time.time()

    def read(self):
        if self.host:
            reading = run_remote(self.host, "codex", self.path)
            if self.forced_label:
                reading["label"] = self.forced_label
            return reading
        return self.read_here()

    def read_here(self):
        auth = read_json(os.path.join(self.dir, "auth.json"))
        tokens = auth.get("tokens") or {}
        token = tokens.get("access_token")
        if not token:
            raise QuotaError(Q_AUTH, "no chatgpt token in %s" % self.dir)
        if self._expired(token):
            raise QuotaError(Q_AUTH, "token expired - run `codex` in %s" % self.dir)

        headers = {"Authorization": "Bearer %s" % token}
        if tokens.get("account_id"):
            headers["chatgpt-account-id"] = tokens["account_id"]
        data = get_json(CODEX_USAGE_URL, headers)

        limits = data.get("rate_limit") or {}
        primary = limits.get("primary_window") or {}
        secondary = limits.get("secondary_window") or {}

        def window(spec):
            return (percent(spec.get("used_percent")),
                    int(spec.get("reset_after_seconds", -1) or -1))

        def name(spec):
            return span(int(spec.get("limit_window_seconds", -1) or -1))

        return {
            "label": self.forced_label
                     or short_label(data.get("email"), None)
                     or self.label,
            "kind": self.kind,
            "state": Q_OK,
            "five": window(primary),
            "week": window(secondary),
            # Codex reports its own window lengths, so the bars are labelled with
            # what it said rather than with what it usually says.
            "names": (name(primary), name(secondary)),
            "sessions": count_sessions("codex", self.dir, "CODEX_HOME"),
        }


def subscriptions(args):
    """The accounts to watch: what was asked for, or whatever is logged in here.

    `--claude`/`--codex` take a directory and an optional `=label`, so two Claude
    plans can be told apart on a row eight characters wide. With neither flag it
    falls back to the one place each vendor's CLI logs in by default, which is
    the whole configuration for the common case of one of each.
    """
    def split(spec):
        """`DIR`, `DIR=LABEL`, `HOST:DIR` or `HOST:DIR=LABEL`.

        A colon means the account lives on another machine - scp's spelling, and
        unambiguous here because a local config directory never has one.
        """
        path, _, label = spec.partition("=")
        host = None
        if ":" in path:
            host, _, path = path.partition(":")
        return path, (short_label(label, None) if label else None), host

    subs = []
    for spec in getattr(args, "claude", None) or []:
        subs.append(ClaudeSub(*split(spec)))
    for spec in getattr(args, "codex", None) or []:
        subs.append(CodexSub(*split(spec)))
    if not subs:
        default_claude = os.environ.get("CLAUDE_CONFIG_DIR") or "~/.claude"
        for path in default_claude.split(os.pathsep):
            if os.path.isdir(os.path.expanduser(path)):
                subs.append(ClaudeSub(path))
        if os.path.isdir(os.path.expanduser("~/.codex")):
            subs.append(CodexSub("~/.codex"))
    # Three buttons, three rows: see MAX_ACCOUNTS in src/quota.rs.
    if len(subs) > 3:
        log("quota: %d accounts, showing the first 3 (the puck has three buttons)"
            % len(subs))
    return subs[:3]


class Quota:
    """Relay between the puck and the vendors' usage endpoints.

    The fetching happens on a worker thread rather than in the relay loop. A
    usage request is a round trip to the internet and the relay loop is also what
    reads the knob, so doing it inline would make the puck feel dead for as long
    as the slowest vendor takes to answer. The loop only ever reads the last
    snapshot, which is what the puck wants anyway: rate-limit windows move over
    minutes, so a reading a minute old is as true as a fresh one, and the puck
    counts the reset down from its own clock in between.
    """

    def __init__(self, link, subs, args):
        self.link = link
        self.subs = subs
        self.period = max(10.0, float(getattr(args, "refresh", REFRESH_DEFAULT)))
        # Doubled on a failure that isn't the account's fault, halved back on
        # success: the usage endpoints rate-limit, and a bridge that answers a
        # 429 by asking again in a minute is the reason it got one.
        self.backoff = 1.0
        self.verbose = bool(getattr(args, "verbose", False))
        self.lock = threading.Lock()
        self.snapshot = [{"label": sub.label, "kind": sub.kind, "state": Q_WAIT,
                          "five": (255, -1), "week": (255, -1),
                          "names": ("5h", "7d"), "sessions": None}
                         for sub in subs]
        self.wake = threading.Event()
        self.reset()
        if subs:
            self.worker = threading.Thread(target=self._work, daemon=True)
            self.worker.start()

    def _blank(self, index, sub, state=Q_WAIT):
        """A reading with no numbers in it, keeping the name we already knew.

        The name is the part worth holding on to: `Dylan needs logging in again`
        tells you which of two Claude plans to go and fix, where the directory's
        own name would just say `claude`.
        """
        known = self.snapshot[index]["label"] if index < len(self.snapshot) else None
        unknown = (255, -1)
        return {"label": known or sub.label, "kind": sub.kind, "state": state,
                "five": unknown, "week": unknown, "names": ("5h", "7d"),
                "sessions": None}

    def reset(self):
        """Forget what the puck has been told, so the next poll says all of it."""
        self.sent = [None] * len(self.subs)

    def refresh(self):
        """Ask the worker to go round again now."""
        self.wake.set()

    def _work(self):
        while True:
            held, asked_for = False, 0.0
            for index, sub in enumerate(self.subs):
                try:
                    reading = sub.read()
                except QuotaError as error:
                    if error.keep and self.snapshot[index]["state"] == Q_OK:
                        held = True
                        asked_for = max(asked_for, error.retry_after)
                        if self.verbose:
                            log("quota: %s: %s (keeping the last reading)"
                                % (sub.label, error))
                        continue
                    reading = self._blank(index, sub, error.state)
                    log("quota: %s: %s" % (sub.label, error))
                except Exception as error:              # a vendor changed shape
                    reading = self._blank(index, sub, Q_ERROR)
                    log("quota: %s: %s" % (sub.label, error))
                with self.lock:
                    was, self.snapshot[index] = self.snapshot[index], reading
                # Only when it moved, so leaving this running all afternoon
                # leaves a log of what actually happened rather than a tick a
                # minute saying nothing.
                if self.verbose or was != reading:
                    self._announce(reading)
            self.backoff = min(self.backoff * 2, 8.0) if held else max(self.backoff / 2, 1.0)
            # A server that names its own cooling-off period gets the benefit of
            # the doubt over our guess; Anthropic's sends `Retry-After: 0`, which
            # is no answer, so the doubling stands. Capped either way: this is a
            # screen someone is watching, and a row that says nothing for the
            # best part of an hour reads as broken rather than as patient.
            wait = min(max(self.period * self.backoff, asked_for), BACKOFF_CAP)
            self.wake.wait(wait)
            self.wake.clear()

    @staticmethod
    def _announce(reading):
        if reading["state"] == Q_WAIT:
            return
        if reading["state"] == Q_AUTH:
            log("quota: %s needs logging in again" % reading["label"])
        elif reading["state"] != Q_OK:
            log("quota: %s unreachable" % reading["label"])
        else:
            five, week = reading["five"], reading["week"]
            running = reading["sessions"]
            log("quota: %s %d%% of %s, %d%% of %s (resets in %s)%s"
                % (reading["label"], five[0], reading["names"][0],
                   week[0], reading["names"][1], span(five[1]),
                   "" if not running else ", %d running" % running))

    def handle(self, line):
        fields = line.split()
        if len(fields) > 1 and fields[1] == "r":
            log("quota: refresh, asked by the puck")
            self.refresh()

    def poll(self):
        """One tick: the heartbeat always, the numbers when they moved."""
        with self.lock:
            snapshot = list(self.snapshot)
        # The count is the heartbeat - it arrives whether or not anything
        # changed, which is what lets the puck tell "nothing new" from "nobody
        # home" and say `no bridge` rather than showing an hour-old percentage.
        self.link.send("#qz %d" % len(snapshot))
        for index, reading in enumerate(snapshot):
            if reading == self.sent[index]:
                continue
            was = self.sent[index]
            identity = (reading["label"], reading["kind"], reading["names"])
            if was is None or (was["label"], was["kind"], was["names"]) != identity:
                self.link.send("#qa %d %d %s %s %s" % (
                    index, reading["kind"], reading["names"][0] or "?",
                    reading["names"][1] or "?", reading["label"]))
            sessions = -1 if reading["sessions"] is None else reading["sessions"]
            self.link.send("#qu %d %d %d %d %d %d %d" % (
                (index,) + tuple(reading["five"]) + tuple(reading["week"])
                + (sessions, reading["state"])))
            self.sent[index] = reading


def cmd_probe(args):
    """Take one reading here and print it as JSON. What `run_remote` invokes.

    Not really a user-facing command - though it is a fine way to see what the
    bridge sees - which is why it takes a bare directory and prints machine
    output rather than a log line.
    """
    reader = ClaudeSub if args.kind == "claude" else CodexSub
    try:
        reading = reader(args.dir).read()
    except QuotaError as error:
        reading = {"error": str(error), "state": error.state}
    except Exception as error:
        reading = {"error": str(error), "state": Q_ERROR}
    print(json.dumps(reading))
    return 0


def cmd_quota(args):
    """Report the subscription quotas only: a relay pinned to the quota app."""
    return run_relay(args, only="quota")


def cmd_reset(args):
    link = open_link(args)
    log("asking the puck to reboot into %s mode" % args.mode)
    link.send("#r %s" % args.mode)
    time.sleep(0.5)
    link.close()
    return 0


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--link", choices=("auto", "usb", "ble"), default="auto",
                        help="transport to reach the puck on (default: %(default)s)")
    parser.add_argument("--port", help="serial port of the puck (default: autodetect)")
    parser.add_argument("--port-glob", default=USB_GLOB, help="where to look (default: %(default)s)")
    parser.add_argument("--address", help="BLE address, if you'd rather not scan")
    parser.add_argument("--scan", type=float, default=8.0, help="BLE scan seconds")
    parser.add_argument("--dfu-name", default="AdaDFU", help="bootloader's advertised name (default: %(default)s)")
    parser.add_argument("--verbose", action="store_true", help="echo the puck's console lines")
    subparsers = parser.add_subparsers(dest="command", required=True)

    subparsers.add_parser("scan", help="what can see the puck right now").set_defaults(run=cmd_scan)
    subparsers.add_parser("monitor", help="print what the puck says").set_defaults(run=cmd_monitor)

    def add_quota_opts(sub):
        sub.add_argument("--claude", action="append", metavar="[HOST:]DIR[=LABEL]",
                         help="a Claude Code config directory to report on; repeat for"
                              " each subscription. HOST: reads it over ssh, on the"
                              " machine that account is logged in on"
                              " (default: $CLAUDE_CONFIG_DIR or ~/.claude)")
        sub.add_argument("--codex", action="append", metavar="[HOST:]DIR[=LABEL]",
                         help="a Codex config directory to report on (default: ~/.codex)")
        sub.add_argument("--refresh", type=float, default=REFRESH_DEFAULT,
                         metavar="SECONDS",
                         help="how often to ask the vendors (default: %(default)s)")

    def add_gantry_opts(sub):
        sub.add_argument("--moonraker", default="http://192.168.1.11:7125",
                         help="Moonraker base URL (default: %(default)s)")
        sub.add_argument("--api-key", help="Moonraker API key, if this host isn't trusted")
        sub.add_argument("--tunnel", metavar="USER@HOST",
                         help="forward Moonraker over ssh so requests come from localhost")
        sub.add_argument("--jog-interval", type=float, default=0.12, metavar="SECONDS",
                         help="how long to gather jogs before sending them as one move"
                              " (default: %(default)s)")

    def add_xcarve_opts(sub, url):
        sub.add_argument("--cncjs", default=url, metavar="URL",
                         help="CNCJS base URL (default: %s)" % (url or "off"))
        sub.add_argument("--cncjs-port", default="/dev/ttyUSB0",
                         help="the X-Controller's serial port, spelled as CNCJS spells it"
                              " (default: %(default)s)")
        sub.add_argument("--cncrc", default="~/.cncrc",
                         help="CNCJS's config file, for the secret its tokens are signed"
                              " with (default: %(default)s)")

    relay = subparsers.add_parser(
        "relay", help="own the link and follow the puck's view (gantry, music, quota)")
    add_gantry_opts(relay)
    add_quota_opts(relay)
    add_xcarve_opts(relay, None)
    relay.add_argument("--rate", type=float, default=8.0, help="gantry state updates per second")
    relay.add_argument("--wait-for-puck", action="store_true",
                       help="sit and retry until the puck turns up (for running as a service)")
    relay.set_defaults(run=cmd_relay)

    gantry = subparsers.add_parser("gantry", help="drive a Klipper gantry with the knob")
    add_gantry_opts(gantry)
    gantry.add_argument("--rate", type=float, default=8.0, help="state updates per second")
    gantry.add_argument("--wait-for-puck", action="store_true",
                        help="sit and retry until the puck turns up (for running as a service)")
    gantry.set_defaults(run=cmd_gantry)

    xcarve = subparsers.add_parser("xcarve", help="drive the X-Carve with the knob, through CNCJS")
    add_xcarve_opts(xcarve, "http://127.0.0.1:8000")
    xcarve.add_argument("--jog-interval", type=float, default=0.12, metavar="SECONDS",
                        help="how long to gather jogs before sending them as one move"
                             " (default: %(default)s)")
    xcarve.add_argument("--rate", type=float, default=8.0, help="state updates per second")
    xcarve.add_argument("--wait-for-puck", action="store_true",
                        help="sit and retry until the puck turns up (for running as a service)")
    xcarve.set_defaults(run=cmd_xcarve)

    spotify = subparsers.add_parser("spotify", help="control the active player from the puck")
    spotify.add_argument("--rate", type=float, default=4.0, help="polls per second")
    spotify.add_argument("--wait-for-puck", action="store_true",
                         help="sit and retry until the puck turns up (for running as a service)")
    spotify.set_defaults(run=cmd_spotify)

    probe = subparsers.add_parser(
        "probe", help="print one account's usage as JSON (what `quota` runs over ssh)")
    probe.add_argument("kind", choices=("claude", "codex"))
    probe.add_argument("dir")
    probe.set_defaults(run=cmd_probe)

    quota = subparsers.add_parser(
        "quota", help="show Claude and Codex rate-limit windows on the puck")
    add_quota_opts(quota)
    quota.add_argument("--wait-for-puck", action="store_true",
                       help="sit and retry until the puck turns up (for running as a service)")
    quota.set_defaults(run=cmd_quota)

    flash = subparsers.add_parser("flash", help="reflash over USB")
    flash.add_argument("image", nargs="?", default="out/pico2joy-bringup.uf2")
    flash.add_argument("--wait", type=int, default=20, help="seconds to wait for the drive")
    flash.set_defaults(run=cmd_flash)

    ota = subparsers.add_parser("ota", help="reflash over BLE")
    ota.add_argument("package", nargs="?", default="out/pico2joy-bringup-dfu.zip")
    ota.add_argument("--no-reboot", action="store_true",
                     help="the puck is already sitting in DFU mode")
    ota.add_argument("--settle", type=float, default=2.0,
                     help="seconds to let the bootloader come up")
    ota.add_argument("--receipts", type=int, default=10,
                     help="packets between receipt notifications (0 disables)")
    ota.set_defaults(run=cmd_ota)

    reset = subparsers.add_parser("reset", help="reboot the puck into a bootloader mode")
    reset.add_argument("mode", choices=("uf2", "serial", "ota"), default="uf2", nargs="?")
    reset.set_defaults(run=cmd_reset)

    args = parser.parse_args()
    return args.run(args)


if __name__ == "__main__":
    sys.exit(main())
