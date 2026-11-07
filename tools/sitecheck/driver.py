# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

"""A small WebDriver client for driving Stillwater in tests, with no dependencies.

    with Stillwater(exe, profile_dir, log_file) as browser:
        browser.navigate("https://example.com/")
        print(browser.execute("return document.title"))
        browser.screenshot(Path("shot.png"))

Each `Stillwater` is its own process with its own profile, started with `--webdriver` on a free
port. Any W3C WebDriver client (Selenium, WebdriverIO) can drive it the same way: start
Stillwater with `--webdriver=PORT` and connect to http://127.0.0.1:PORT.
"""

from __future__ import annotations

import base64
import json
import os
import socket
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path


class WebDriverError(Exception):
    def __init__(self, error: str, message: str = ""):
        super().__init__(f"{error}: {message}" if message else error)
        self.error = error


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


class Stillwater:
    def __init__(self, exe: Path, profile: Path, log_file: Path | None = None, *,
                 args: list[str] = (), env: dict[str, str] | None = None, output=None,
                 headless: bool = True, window_size: str = "1280x800",
                 page_load_seconds: float = 30, script_seconds: float = 10,
                 slow_script_seconds: float | None = None):
        self.port = free_port()
        environment = dict(os.environ if env is None else env)
        environment["STILLWATER_CONFIG_DIR"] = str(profile)
        environment["STILLWATER_NO_UPDATE_CHECK"] = "1"
        if log_file:
            environment["STILLWATER_LOG_FILE"] = str(log_file)
        if slow_script_seconds is not None:
            # When to stop a page script that runs without a break (Stillwater's default is
            # 60 s; 0 never stops them).
            environment["STILLWATER_SLOW_SCRIPT_SECONDS"] = str(slow_script_seconds)
        command = [str(exe), f"--webdriver={self.port}", f"--window-size={window_size}", *args]
        if headless:
            command.insert(1, "--headless")
        self.process = subprocess.Popen(command, env=environment, stdout=output,
                                        stderr=subprocess.STDOUT if output else None)
        self.session = None
        self.timeouts = {"pageLoad": int(page_load_seconds * 1000), "script": int(script_seconds * 1000)}

    # Process and session lifetime.

    def __enter__(self) -> "Stillwater":
        try:
            self.start_session()
        except BaseException:
            self.quit()
            raise
        return self

    def __exit__(self, *exc) -> None:
        self.quit()

    def alive(self) -> bool:
        return self.process.poll() is None

    def start_session(self, wait_seconds: float = 30) -> None:
        deadline = time.monotonic() + wait_seconds
        while True:
            if not self.alive():
                raise WebDriverError("browser exited", f"exit code {self.process.returncode}")
            try:
                if self._request("GET", "/status", timeout=2).get("ready"):
                    break
            except (OSError, WebDriverError):
                pass
            if time.monotonic() > deadline:
                raise WebDriverError("timeout", "Stillwater's WebDriver server didn't start")
            time.sleep(0.2)
        value = self._request("POST", "/session", {"capabilities": {"alwaysMatch": {"timeouts": self.timeouts}}})
        self.session = value["sessionId"]

    def quit(self, grace_seconds: float = 3) -> None:
        if self.session and self.alive():
            try:
                self._request("DELETE", f"/session/{self.session}", timeout=grace_seconds)
            except (OSError, WebDriverError):
                pass
        self.session = None
        if self.alive():
            self.process.kill()
        self.process.wait()

    # WebDriver commands.

    def navigate(self, url: str) -> None:
        """Load `url` and wait for its load event (up to the page load timeout)."""
        self._command("POST", "/url", {"url": url}, timeout=self.timeouts["pageLoad"] / 1000 + 15)

    def execute(self, script: str, *args):
        """Run `script` as a function body in the page and return its result."""
        return self._command("POST", "/execute/sync", {"script": script, "args": list(args)},
                             timeout=self.timeouts["script"] / 1000 + 15)

    def execute_async(self, script: str, *args):
        """Run `script`; it finishes by calling its last argument with the result."""
        return self._command("POST", "/execute/async", {"script": script, "args": list(args)},
                             timeout=self.timeouts["script"] / 1000 + 15)

    def screenshot(self, path: Path, timeout: float = 20) -> None:
        png = base64.b64decode(self._command("GET", "/screenshot", timeout=timeout))
        Path(path).write_bytes(png)

    @property
    def title(self) -> str:
        return self._command("GET", "/title")

    @property
    def url(self) -> str:
        return self._command("GET", "/url")

    # Plumbing.

    def _command(self, method: str, path: str, body=None, timeout: float = 30):
        if not self.session:
            raise WebDriverError("no session")
        return self._request(method, f"/session/{self.session}{path}", body, timeout)

    def _request(self, method: str, path: str, body=None, timeout: float = 30):
        data = json.dumps(body).encode() if body is not None else None
        request = urllib.request.Request(f"http://127.0.0.1:{self.port}{path}", data=data, method=method,
                                         headers={"Content-Type": "application/json"})
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                payload = response.read()
        except urllib.error.HTTPError as error:
            payload = error.read()
            try:
                value = json.loads(payload).get("value", {})
            except ValueError:
                value = {}
            raise WebDriverError(value.get("error", f"HTTP {error.code}"), value.get("message", "")) from None
        except (socket.timeout, TimeoutError):
            raise WebDriverError("timeout", f"no answer to {method} {path} in {timeout:.0f}s") from None
        return json.loads(payload or b"{}").get("value")
