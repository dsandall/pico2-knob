"""Minimal Konnect MCP stdio client for scripted/agent use.

Usage:
    from konnect_client import Konnect
    with Konnect() as k:
        k.load("sch_wiring")
        print(k.call("list_schematic_nets", schematic="board_pico2knob/pico2-knob.kicad_sch"))
"""
import json
import subprocess

KONNECT_BIN = "/home/thebu/newhome/projects/konnect/target/release/konnect"


class KonnectError(RuntimeError):
    pass


class Konnect:
    def __init__(self, binary=KONNECT_BIN):
        self.proc = subprocess.Popen(
            [binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        self._id = 0
        self._rpc("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "konnect_client.py", "version": "0"},
        })
        self._notify("notifications/initialized")

    def _send(self, msg):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def _notify(self, method, params=None):
        self._send({"jsonrpc": "2.0", "method": method, **({"params": params} if params else {})})

    def _rpc(self, method, params=None):
        self._id += 1
        rid = self._id
        msg = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            msg["params"] = params
        self._send(msg)
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise KonnectError("konnect exited unexpectedly")
            resp = json.loads(line)
            if resp.get("id") == rid:
                if "error" in resp:
                    raise KonnectError(resp["error"])
                return resp["result"]
            # skip notifications (e.g. tools/list_changed)

    def load(self, *toolsets):
        for ts in toolsets:
            self.call("load_toolset", name=ts)

    def tool_schema(self, name):
        tools = self._rpc("tools/list")["tools"]
        for t in tools:
            if t["name"] == name:
                return t["inputSchema"]
        raise KonnectError(f"tool {name} not in tools/list (toolset loaded?)")

    def call(self, tool, **arguments):
        result = self._rpc("tools/call", {"name": tool, "arguments": arguments})
        text = result["content"][0]["text"]
        try:
            payload = json.loads(text)
        except ValueError:
            payload = text
        if result.get("isError"):
            raise KonnectError(payload)
        return payload

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=10)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
