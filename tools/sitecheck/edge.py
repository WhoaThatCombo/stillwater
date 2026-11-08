# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""A hidden Microsoft Edge to compare Stillwater against, driven over the Chrome DevTools Protocol.

No dependencies (a minimal WebSocket client is included). Edge runs headless and muted, with its
own throwaway profile, and its debugging port listens on 127.0.0.1 only. Scrollbars are hidden
so its page is as wide as Stillwater's.

    with Edge() as edge:
        page = edge.load("https://example.com/", width=1280, height=800, settle=4)
        page["shot"]      # PNG bytes of the viewport
        page["measure"]   # what compare-collect.js returned (a dict), or None
"""

from __future__ import annotations

import base64
import json
import os
import shutil
import socket
import struct
import subprocess
import tempfile
import time
import urllib.request
from pathlib import Path

EDGE_PATHS = [
    r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
    r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
]
COLLECT_JS = (Path(__file__).resolve().parents[2] / "ports" / "servoshell" / "desktop" /
              "compare-collect.js").read_text(encoding="utf-8")


def find_edge() -> str | None:
    return next((path for path in EDGE_PATHS if os.path.exists(path)), None)


class WebSocket:
    """Just enough RFC 6455 for CDP: masked text frames out, text frames in."""

    def __init__(self, url: str, timeout: float = 60):
        assert url.startswith("ws://")
        host_port, _, path = url[5:].partition("/")
        host, _, port = host_port.partition(":")
        self.sock = socket.create_connection((host, int(port)), timeout=timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        request = (f"GET /{path} HTTP/1.1\r\nHost: {host_port}\r\nUpgrade: websocket\r\n"
                   f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
        self.sock.sendall(request.encode())
        response = b""
        while b"\r\n\r\n" not in response:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("WebSocket handshake failed")
            response += chunk
        if b" 101 " not in response.split(b"\r\n", 1)[0]:
            raise ConnectionError(response.split(b"\r\n", 1)[0].decode(errors="replace"))
        self.buffer = response.split(b"\r\n\r\n", 1)[1]

    def send(self, text: str) -> None:
        payload = text.encode()
        header = bytearray([0x81])
        if len(payload) < 126:
            header.append(0x80 | len(payload))
        elif len(payload) < 65536:
            header.append(0x80 | 126)
            header += struct.pack(">H", len(payload))
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", len(payload))
        mask = os.urandom(4)
        header += mask
        masked = bytes(byte ^ mask[i % 4] for i, byte in enumerate(payload))
        self.sock.sendall(bytes(header) + masked)

    def _read(self, count: int) -> bytes:
        while len(self.buffer) < count:
            chunk = self.sock.recv(1 << 20)
            if not chunk:
                raise ConnectionError("WebSocket closed")
            self.buffer += chunk
        data, self.buffer = self.buffer[:count], self.buffer[count:]
        return data

    def receive(self) -> str:
        message = b""
        while True:
            first, second = self._read(2)
            length = second & 0x7F
            if length == 126:
                length = struct.unpack(">H", self._read(2))[0]
            elif length == 127:
                length = struct.unpack(">Q", self._read(8))[0]
            payload = self._read(length)
            opcode = first & 0x0F
            if opcode == 0x8:
                raise ConnectionError("WebSocket closed by Edge")
            if opcode in (0x9, 0xA):  # ping / pong
                continue
            message += payload
            if first & 0x80:
                return message.decode(errors="replace")

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


class Edge:
    def __init__(self, exe: str | None = None):
        self.exe = exe or find_edge()
        if not self.exe:
            raise FileNotFoundError("Microsoft Edge isn't installed")
        self.profile = Path(tempfile.mkdtemp(prefix="compare-edge-"))
        self.process = None
        self.ws = None
        self.next_id = 0

    def __enter__(self) -> "Edge":
        self.start()
        return self

    def __exit__(self, *exc) -> None:
        self.stop()

    def start(self) -> None:
        args = [self.exe, "--headless=new", "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1", f"--user-data-dir={self.profile}",
                "--mute-audio", "--hide-scrollbars", "--no-first-run", "--no-default-browser-check",
                "--disable-extensions", "--disable-sync", "--disable-background-networking",
                "--force-device-scale-factor=1", "--window-size=1280,800", "about:blank"]
        self.process = subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                        creationflags=0x08000000)  # CREATE_NO_WINDOW
        port_file = self.profile / "DevToolsActivePort"
        deadline = time.time() + 30
        while not port_file.exists() or not port_file.read_text().strip():
            if time.time() > deadline or self.process.poll() is not None:
                raise RuntimeError("Edge didn't start")
            time.sleep(0.1)
        port = port_file.read_text().split()[0]
        version = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=10))
        self.ws = WebSocket(version["webSocketDebuggerUrl"])

    def stop(self) -> None:
        """Close Edge (gracefully, then only the process started here) and delete its profile."""
        if self.ws:
            try:
                self.send("Browser.close")
            except (OSError, ConnectionError):
                pass
            self.ws.close()
            self.ws = None
        if self.process:
            try:
                self.process.wait(10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(10)
            self.process = None
        shutil.rmtree(self.profile, ignore_errors=True)

    def send(self, method: str, params: dict | None = None, session: str | None = None,
             timeout: float = 60, wait_event: tuple[str, str] | None = None) -> dict:
        """Send a command and return its result. Events that arrive meanwhile are dropped,
        except `wait_event` = (method, session): returns once that event has also arrived."""
        self.next_id += 1
        message = {"id": self.next_id, "method": method, "params": params or {}}
        if session:
            message["sessionId"] = session
        self.ws.send(json.dumps(message))
        self.ws.sock.settimeout(timeout)
        result = None
        event_seen = wait_event is None
        deadline = time.time() + timeout
        while result is None or not event_seen:
            if time.time() > deadline:
                raise TimeoutError(method)
            reply = json.loads(self.ws.receive())
            if reply.get("id") == self.next_id:
                if "error" in reply:
                    raise RuntimeError(f"{method}: {reply['error'].get('message')}")
                result = reply.get("result", {})
            elif wait_event and reply.get("method") == wait_event[0] and reply.get("sessionId") == wait_event[1]:
                event_seen = True
        return result

    def wait_for(self, event: str, session: str, timeout: float) -> bool:
        deadline = time.time() + timeout
        while time.time() < deadline:
            self.ws.sock.settimeout(max(0.1, deadline - time.time()))
            try:
                message = json.loads(self.ws.receive())
            except (socket.timeout, TimeoutError):
                return False
            if message.get("method") == event and message.get("sessionId") == session:
                return True
        return False

    def load(self, url: str, width: int = 1280, height: int = 800, settle: float = 4,
             timeout: float = 30, color_scheme: str = "light") -> dict:
        """Open `url` in a fresh tab, wait for its load event and `settle` seconds, then measure
        it and screenshot the viewport."""
        target = self.send("Target.createTarget", {"url": "about:blank"})["targetId"]
        session = self.send("Target.attachToTarget", {"targetId": target, "flatten": True})["sessionId"]
        page = {"url": url, "shot": None, "measure": None, "loaded": False}
        try:
            self.send("Emulation.setDeviceMetricsOverride",
                      {"width": width, "height": height, "deviceScaleFactor": 1, "mobile": False}, session)
            # Match Stillwater's test runs (light), whatever Windows' own theme is.
            self.send("Emulation.setEmulatedMedia", {"features": [
                {"name": "prefers-color-scheme", "value": color_scheme}]}, session)
            self.send("Page.enable", session=session)
            try:
                self.send("Page.navigate", {"url": url}, session, timeout=timeout,
                          wait_event=("Page.loadEventFired", session))
                page["loaded"] = True
            except TimeoutError:
                pass
            time.sleep(settle)
            result = self.send("Runtime.evaluate", {"expression": COLLECT_JS, "returnByValue": True},
                               session, timeout=20)
            value = result.get("result", {}).get("value")
            if isinstance(value, str):
                page["measure"] = json.loads(value)
            shot = self.send("Page.captureScreenshot", {"format": "png"}, session, timeout=30)
            page["shot"] = base64.b64decode(shot["data"])
        finally:
            try:
                self.send("Target.closeTarget", {"targetId": target}, timeout=10)
            except (RuntimeError, TimeoutError, OSError, ConnectionError):
                pass
        return page
