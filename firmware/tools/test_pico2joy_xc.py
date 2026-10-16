#!/usr/bin/env python3
"""Offline test of `pico2joy.py xcarve` against a stub xc-api (and a stub of the
old pendant), with a pseudo-terminal standing in for the puck.

    python3 tools/test_pico2joy_xc.py          # from firmware/, stdlib only

No puck, no Pi, no machine: the relay runs as a subprocess exactly as the
service runs it (`--link usb --port <pty> xcarve ...`), the test plays the puck
on the pty's other end - reading what the relay sends and writing the machine-
channel lines a real puck would - and a ThreadingHTTPServer plays xc-api,
recording every request with its headers (names lower-cased: HTTP
 headers are case-insensitive, and urllib sends `X-xc-actor`).

What it checks, for xc-api:
- state goes down to the puck as `#xl` / `#xr` / `#xs` in work coordinates;
- a knob jog becomes `POST /api/jog {x, feed}`, zero becomes
  `POST /api/zero {axes}`, home `POST /api/home`, stop `POST /api/rt hold`;
- every request carries `X-XC-Actor: puck`;
- a live `spindle_cmd` with on=true on the SSE stream reaches the puck as
  `#quiet 1500`, and a stale one (or on=false) does not;
and for `--pendant`, that a jog still goes to the pendant's `/api/jog`.
"""

import json
import os
import pty
import queue
import select
import subprocess
import sys
import threading
import time
import tty
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
RELAY = os.path.join(HERE, "pico2joy.py")

SETTINGS = {"100": "26.666", "101": "26.666", "102": "200", "110": "8000", "111": "8000",
            "112": "500", "120": "500", "121": "500", "122": "50", "130": "790",
            "131": "790", "132": "100", "13": "0"}


def xc_state(seq):
    """A state snapshot shaped as SPEC.md's `GET /api/state` says."""
    return {
        "seq": seq, "t": time.time(), "connected": True, "port": "/dev/ttyUSB0",
        "machine": {"state": "Idle", "mpos": [-400.0, -300.0, -20.0],
                    "wpos": [10.0, 20.0, 5.0], "wco": [-410.0, -320.0, -25.0],
                    "fs": [0, 0], "alarm": None},
        "facts": {"homed": {"value": True, "valid": True, "actor": "operator"}},
        "lease": None, "job": None, "settings": SETTINGS, "modal": "G0 G54 G17 G21 G90 G94 M5",
    }


def pendant_state():
    """The old pendant's `GET /api/state`, the bits the relay reads."""
    return {"connected": True, "port": "/dev/ttyUSB0", "homed": True,
            "status": {"state": "Idle", "wpos": [10.0, 20.0, 5.0], "wco": [-410.0, -320.0, -25.0]},
            "settings": SETTINGS, "modal": "G0 G54 G17 G21 G90 G94 M5", "job": None}


class Stub:
    """xc-api or the pendant, recording requests; SSE events go out from `events`."""

    def __init__(self, kind):
        self.kind = kind
        self.requests = queue.Queue()
        self.events = queue.Queue()
        self.seq = 100
        stub = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def _json(self, body, code=200):
                data = json.dumps(body).encode()
                self.send_response(code)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                if self.path == "/api/state":
                    stub.requests.put(("GET", self.path, {k.lower(): v for k, v in self.headers.items()}, None))
                    self._json(xc_state(stub.seq) if stub.kind == "xc" else pendant_state())
                elif self.path.startswith("/api/events/stream") and stub.kind == "xc":
                    stub.requests.put(("GET", self.path, {k.lower(): v for k, v in self.headers.items()}, None))
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Cache-Control", "no-cache")
                    self.end_headers()
                    try:
                        self.wfile.write(b": hello\n\n")
                        self.wfile.flush()
                        while True:
                            try:
                                event = stub.events.get(timeout=1.0)
                            except queue.Empty:
                                self.wfile.write(b": heartbeat\n\n")
                                self.wfile.flush()
                                continue
                            text = "id: %d\nevent: %s\ndata: %s\n\n" % (
                                event["seq"], event["kind"], json.dumps(event))
                            self.wfile.write(text.encode())
                            self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError):
                        return
                else:
                    self._json({"error": "no such thing"}, 404)

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = json.loads(self.rfile.read(length) or b"{}")
                stub.requests.put(("POST", self.path, {k.lower(): v for k, v in self.headers.items()}, body))
                self._json({"ok": True})

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = "http://127.0.0.1:%d" % self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def emit(self, kind, **fields):
        self.seq += 1
        event = {"seq": self.seq, "t": time.time(), "kind": kind, "actor": "daemon"}
        event.update(fields)
        self.events.put(event)

    def wait_post(self, path, timeout=5.0):
        """The next POST to `path`, skipping everything else."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            try:
                method, got, headers, body = self.requests.get(timeout=deadline - time.time())
            except queue.Empty:
                break
            if method == "POST" and got == path:
                return headers, body
        raise AssertionError("no POST %s within %.0f s" % (path, timeout))

    def close(self):
        self.server.shutdown()


class Puck:
    """The puck's end of a pty: the relay opens the other end as its ttyACM."""

    def __init__(self):
        self.master, slave = pty.openpty()
        tty.setraw(slave)
        self.path = os.ttyname(slave)
        self.slave = slave            # kept open, so the pty survives the relay's open/close
        self.buffer = b""
        self.lines = []
        self.cursor = 0

    def send(self, line):
        os.write(self.master, (line + "\n").encode())

    def read(self, timeout=0.1):
        ready, _, _ = select.select([self.master], [], [], timeout)
        if ready:
            try:
                self.buffer += os.read(self.master, 65536)
            except OSError:
                return
            while b"\n" in self.buffer:
                line, self.buffer = self.buffer.split(b"\n", 1)
                line = line.decode("utf-8", "replace").strip()
                if line:
                    self.lines.append(line)

    def wait_for(self, predicate, timeout=5.0):
        """The next line, after the last one matched, that satisfies `predicate`."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.read()
            for index in range(self.cursor, len(self.lines)):
                if predicate(self.lines[index]):
                    self.cursor = index + 1
                    return self.lines[index]
        raise AssertionError("puck never saw a matching line; got %r"
                             % self.lines[self.cursor:][-10:])

    def saw(self, predicate):
        self.read()
        return [line for line in self.lines if predicate(line)]

    def close(self):
        os.close(self.master)
        os.close(self.slave)


def start_relay(puck, *backend):
    return subprocess.Popen(
        [sys.executable, RELAY, "--link", "usb", "--port", puck.path, "xcarve", *backend],
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
        env=dict(os.environ, XC_URL=""))


class XcApiRelay(unittest.TestCase):
    def setUp(self):
        self.stub = Stub("xc")
        self.puck = Puck()
        self.relay = start_relay(self.puck, "--xc", self.stub.url)

    def tearDown(self):
        self.relay.terminate()
        try:
            out, _ = self.relay.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            self.relay.kill()
            out, _ = self.relay.communicate()
        if os.environ.get("VERBOSE"):
            print(out)
        self.puck.close()
        self.stub.close()

    def test_state_jog_zero_home_stop_and_quiet(self):
        puck, stub = self.puck, self.stub
        # State down: limits, resolution, then the work position (um).
        puck.wait_for(lambda l: l.startswith("#xl "))
        puck.wait_for(lambda l: l == "#xr 40")
        puck.wait_for(lambda l: l == "#xs 10000 20000 5000 7 1")

        # A stale spindle event (a minute old) and an `off` must not quiet it.
        stub.events.put({"seq": stub.seq + 1, "t": time.time() - 60, "kind": "spindle_cmd",
                         "actor": "daemon", "on": True})
        stub.seq += 1
        stub.emit("spindle_cmd", on=False)

        # Knob: one detent of 0.1 mm on X.
        puck.send("#xj 0 100")
        headers, body = stub.wait_post("/api/jog")
        self.assertEqual(headers.get("x-xc-actor"), "puck")
        self.assertAlmostEqual(body["x"], 0.1)
        self.assertEqual(body["feed"], 3000.0)
        self.assertNotIn("y", body)

        # Zero X: the daemon's zero, not a raw G10 line.
        puck.send("#xc zero 0")
        headers, body = stub.wait_post("/api/zero")
        self.assertEqual(headers.get("x-xc-actor"), "puck")
        self.assertEqual(body["axes"], "x")

        puck.send("#xc home")
        headers, _ = stub.wait_post("/api/home")
        self.assertEqual(headers.get("x-xc-actor"), "puck")

        puck.send("#xc stop")
        headers, body = stub.wait_post("/api/rt")
        self.assertEqual(body, {"what": "hold"})
        self.assertEqual(headers.get("x-xc-actor"), "puck")

        self.assertEqual(puck.saw(lambda l: l.startswith("#quiet")), [],
                         "a stale or off spindle_cmd quieted the puck")

        # The router switched on, live: the puck hears #quiet 1500.
        stub.emit("spindle_cmd", on=True, actor="operator")
        puck.wait_for(lambda l: l == "#quiet 1500")


class PendantFallback(unittest.TestCase):
    def test_jog_goes_to_the_pendant(self):
        stub = Stub("pendant")
        puck = Puck()
        relay = start_relay(puck, "--pendant", stub.url)
        try:
            puck.wait_for(lambda l: l == "#xs 10000 20000 5000 7 1")
            puck.send("#xj 1 -1000")
            _, body = stub.wait_post("/api/jog")
            self.assertAlmostEqual(body["y"], -1.0)
            puck.send("#xc home")
            _, body = stub.wait_post("/api/cmd")
            self.assertEqual(body, {"line": "$H"})
        finally:
            relay.terminate()
            relay.communicate(timeout=5)
            puck.close()
            stub.close()


if __name__ == "__main__":
    unittest.main(verbosity=2)
