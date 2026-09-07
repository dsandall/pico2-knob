#!/usr/bin/env python3
# /// script
# requires-python = ">=3.9"
# dependencies = ["bleak>=0.22", "pillow>=10"]
# ///
"""One program for talking to the pico2joy puck, over USB or Bluetooth.

    tools/pico2joy.py scan                    # what can see the puck right now
    tools/pico2joy.py monitor                 # console passthrough
    tools/pico2joy.py relay                    # gantry + music, following the screen
    tools/pico2joy.py gantry                  # drive a Klipper gantry
    tools/pico2joy.py spotify                 # transport + album art for the player
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
import errno
import glob
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
import urllib.request
import zipfile

AXES = ("x", "y", "z")

# Klipper's names for what it is doing, mapped onto the small enum the puck
# displays. Anything unlisted reads as "idle", which is the honest answer.
STATES = {"standby": 1, "complete": 1, "cancelled": 1, "printing": 2, "paused": 3, "error": 4}

# Jog feedrates, mm/min. Z is slower because a Z jog usually means the nozzle is
# near something.
FEED = {"x": 6000.0, "y": 6000.0, "z": 900.0}

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

    @property
    def name(self):
        return self.path

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


class BleLink:
    """The same lines over GATT.

    bleak is asyncio and everything above here is a plain loop, so the event
    loop lives in a thread of its own and the two sides meet at two queues.
    """

    kind = "ble"

    def __init__(self, address=None, name=BLE_NAME, timeout=20.0):
        import asyncio
        from bleak import BleakClient, BleakScanner

        self._asyncio = asyncio
        self.address = address
        self.want_name = name
        self.rx = queue.Queue()
        self.outgoing = queue.Queue()
        self.ready = threading.Event()
        self.error = None
        self.buffer = b""
        self.name = address or name

        async def worker():
            target = self.address
            if target is None:
                device = await BleakScanner.find_device_by_filter(
                    lambda d, ad: (ad.local_name or d.name or "") == self.want_name, timeout=timeout)
                if device is None:
                    self.error = "no BLE puck advertising as %r" % self.want_name
                    self.ready.set()
                    return
                target = device.address
            self.name = target
            try:
                async with BleakClient(target, timeout=timeout) as client:
                    def on_notify(_handle, data):
                        self.buffer += bytes(data)
                        while b"\n" in self.buffer:
                            line, self.buffer = self.buffer.split(b"\n", 1)
                            self.rx.put(line.decode("utf-8", "replace").strip())
                    await client.start_notify(PUCK_TX, on_notify)
                    self.ready.set()
                    while not self._stop.is_set():
                        try:
                            line = self.outgoing.get_nowait()
                        except queue.Empty:
                            await asyncio.sleep(0.02)
                            continue
                        await client.write_gatt_char(PUCK_RX, (line + "\n").encode(),
                                                     response=False)
            except Exception as failure:          # noqa: BLE001 - reported, not swallowed
                self.error = str(failure)
                self.ready.set()

        self._stop = threading.Event()
        self._thread = threading.Thread(target=lambda: asyncio.run(worker()), daemon=True)
        self._thread.start()
        if not self.ready.wait(timeout + 5):
            raise RuntimeError("BLE connect timed out")
        if self.error:
            raise RuntimeError(self.error)

    def close(self):
        self._stop.set()

    def send(self, line):
        self.outgoing.put(line)

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

    tunnel = None
    printer = None
    if need_gantry:
        base = args.moonraker
        if getattr(args, "tunnel", None):
            tunnel, base = start_tunnel(args.tunnel)
        printer = Moonraker(base, args.api_key)
    player = Player() if need_media else None

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

    by_view = {}
    owner = {}
    if gantry:
        by_view["gantry"] = gantry
        owner["j"] = owner["c"] = gantry
    if media:
        by_view["music"] = media
        owner["m"] = media

    def set_link(new):
        nonlocal link
        link = new
        if gantry:
            gantry.link = new
        if media:
            media.link = new

    def reopen():
        try:
            link.close()
        except OSError:
            pass
        for _ in range(20):
            time.sleep(1.0)
            try:
                set_link(open_link(args))
                if state["active"]:
                    activate(state["active"])
                elif not only:
                    link.send("#?")
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
        elif app is media:
            media.last = media.last_art_url = None
            log("relay: music")

    # A dict so the nested handlers can rebind it without `nonlocal` gymnastics.
    state = {"active": by_view.get(only) if only else None, "moonraker_ok": None}

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
    if not only:
        try:
            link.send("#?")                 # which view is up right now?
        except OSError:
            pass

    g_period = 1.0 / getattr(args, "rate", 8.0)
    m_period = 0.25
    next_g = next_m = 0.0

    try:
        while True:
            now = time.time()
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
            elif active is media and now >= next_m:
                next_m = now + m_period
                try:
                    media.poll()
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
    except KeyboardInterrupt:
        log("stopped")
    finally:
        link.close()
        if tunnel:
            tunnel.terminate()
    return 0


def cmd_relay(args):
    """Own the link and follow the puck's view between the gantry and the player."""
    return run_relay(args, only=None)


def cmd_gantry(args):
    """Drive the gantry only: a relay pinned to the gantry app."""
    return run_relay(args, only="gantry")


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

    def add_gantry_opts(sub):
        sub.add_argument("--moonraker", default="http://192.168.1.11:7125",
                         help="Moonraker base URL (default: %(default)s)")
        sub.add_argument("--api-key", help="Moonraker API key, if this host isn't trusted")
        sub.add_argument("--tunnel", metavar="USER@HOST",
                         help="forward Moonraker over ssh so requests come from localhost")
        sub.add_argument("--jog-interval", type=float, default=0.12, metavar="SECONDS",
                         help="how long to gather jogs before sending them as one move"
                              " (default: %(default)s)")

    relay = subparsers.add_parser(
        "relay", help="own the link and follow the puck's view (gantry + music)")
    add_gantry_opts(relay)
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

    spotify = subparsers.add_parser("spotify", help="control the active player from the puck")
    spotify.add_argument("--rate", type=float, default=4.0, help="polls per second")
    spotify.add_argument("--wait-for-puck", action="store_true",
                         help="sit and retry until the puck turns up (for running as a service)")
    spotify.set_defaults(run=cmd_spotify)

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
