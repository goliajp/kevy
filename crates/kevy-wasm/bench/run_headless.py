#!/usr/bin/env python3
"""Headless-Chrome driver for the kevy-wasm browser harness.

Python stdlib only: serves the repo over HTTP with COOP/COEP headers
(cross-origin isolation gives performance.now() 5 microsecond resolution),
launches Chrome with `--headless=new`, opens the target page (plus an
echo tab for the cross-tab axes) and awaits `window.__kevyResult` over
the DevTools protocol on a minimal hand-rolled RFC 6455 WebSocket client.

Usage:
  python3 run_headless.py                      # e2e (dual tab)
  python3 run_headless.py --page bench.html    # full bench (dual tab)
  python3 run_headless.py --no-echo --query dualtab=0   # single tab e2e
  python3 run_headless.py --out results.json --timeout 600
"""

import argparse
import base64
import json
import os
import secrets
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import urllib.request
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
PAGE_DIR = "/crates/kevy-wasm/bench/"
CHROME_CANDIDATES = [
    os.environ.get("CHROME"),
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    shutil.which("google-chrome"),
    shutil.which("chromium"),
    shutil.which("chromium-browser"),
]


class Handler(SimpleHTTPRequestHandler):
    extensions_map = {
        **SimpleHTTPRequestHandler.extensions_map,
        ".wasm": "application/wasm",
        ".js": "text/javascript",
        ".mjs": "text/javascript",
    }

    def end_headers(self):
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def log_message(self, *args):
        pass


def start_http_server():
    srv = ThreadingHTTPServer(("127.0.0.1", 0), partial(Handler, directory=REPO))
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, srv.server_address[1]


def launch_chrome(user_data_dir):
    exe = next((c for c in CHROME_CANDIDATES if c and os.path.exists(c)), None)
    if exe is None:
        sys.exit("chrome not found; set CHROME=/path/to/chrome")
    proc = subprocess.Popen(
        [
            exe,
            "--headless=new",
            f"--user-data-dir={user_data_dir}",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-timer-throttling",
            "--mute-audio",
            "about:blank",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    port_file = os.path.join(user_data_dir, "DevToolsActivePort")
    deadline = time.time() + 30
    while time.time() < deadline:
        if os.path.exists(port_file):
            with open(port_file, encoding="utf-8") as f:
                line = f.readline().strip()
            if line.isdigit():
                return proc, int(line)
        time.sleep(0.05)
    proc.kill()
    sys.exit("chrome did not expose a DevTools port")


def devtools_json(port, path, method):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", method=method)
    with urllib.request.urlopen(req, timeout=10) as resp:
        return json.load(resp)


def open_tab(port, url):
    path = "/json/new?" + urllib.parse.quote(url, safe="")
    try:
        return devtools_json(port, path, "PUT")
    except urllib.error.HTTPError:
        return devtools_json(port, path, "GET")  # pre-111 Chrome used GET


class CDP:
    """Minimal DevTools client: RFC 6455 handshake + frame codec + call."""

    def __init__(self, ws_url):
        u = urllib.parse.urlparse(ws_url)
        self.sock = socket.create_connection((u.hostname, u.port), timeout=30)
        key = base64.b64encode(secrets.token_bytes(16)).decode()
        handshake = (
            f"GET {u.path} HTTP/1.1\r\nHost: {u.hostname}:{u.port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        self.sock.sendall(handshake.encode())
        status = self._read_until(b"\r\n\r\n")
        if b" 101 " not in status.split(b"\r\n", 1)[0]:
            raise RuntimeError(f"websocket handshake failed: {status[:120]!r}")
        self.buf = b""
        self.next_id = 1

    def _read_until(self, marker):
        data = b""
        while marker not in data:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("socket closed during handshake")
            data += chunk
        return data

    def _read_exact(self, n):
        while len(self.buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError("socket closed")
            self.buf += chunk
        out, self.buf = self.buf[:n], self.buf[n:]
        return out

    def _send_frame(self, payload, opcode=0x1):
        header = bytes([0x80 | opcode])
        n = len(payload)
        if n < 126:
            header += bytes([0x80 | n])
        elif n < 1 << 16:
            header += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            header += bytes([0x80 | 127]) + struct.pack(">Q", n)
        mask = secrets.token_bytes(4)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self.sock.sendall(header + mask + masked)

    def _recv_message(self):
        message = b""
        while True:
            b0, b1 = self._read_exact(2)
            fin, opcode = b0 & 0x80, b0 & 0x0F
            n = b1 & 0x7F
            if n == 126:
                (n,) = struct.unpack(">H", self._read_exact(2))
            elif n == 127:
                (n,) = struct.unpack(">Q", self._read_exact(8))
            payload = self._read_exact(n)
            if opcode == 0x9:  # ping -> pong
                self._send_frame(payload, opcode=0xA)
                continue
            if opcode == 0x8:
                raise ConnectionError("websocket closed by peer")
            message += payload
            if fin:
                return message

    def call(self, method, params=None, timeout=60):
        mid = self.next_id
        self.next_id += 1
        self._send_frame(json.dumps({"id": mid, "method": method, "params": params or {}}).encode())
        self.sock.settimeout(timeout)
        deadline = time.time() + timeout
        while time.time() < deadline:
            msg = json.loads(self._recv_message())
            if msg.get("id") == mid:
                if "error" in msg:
                    raise RuntimeError(f"{method}: {msg['error']}")
                return msg.get("result", {})
        raise TimeoutError(method)

    def evaluate(self, expression, await_promise=False, timeout=60):
        r = self.call(
            "Runtime.evaluate",
            {
                "expression": expression,
                "awaitPromise": await_promise,
                "returnByValue": True,
            },
            timeout=timeout,
        )
        exc = r.get("exceptionDetails")
        if exc:
            raise RuntimeError(f"page exception: {json.dumps(exc)[:400]}")
        return r.get("result", {}).get("value")

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


def await_result(cdp, timeout):
    deadline = time.time() + 30
    while time.time() < deadline:
        if cdp.evaluate("!!window.__kevyResult"):
            break
        time.sleep(0.1)
    else:
        raise TimeoutError("page never installed window.__kevyResult")
    return cdp.evaluate("window.__kevyResult", await_promise=True, timeout=timeout)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--page", default="e2e.html")
    ap.add_argument("--query", default="", help="extra query string for the main tab")
    ap.add_argument("--no-echo", action="store_true", help="skip the echo tab")
    ap.add_argument("--timeout", type=float, default=300)
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    srv, http_port = start_http_server()
    tmp = tempfile.mkdtemp(prefix="kevy-wasm-headless-")
    proc = None
    try:
        proc, cdp_port = launch_chrome(tmp)
        base = f"http://127.0.0.1:{http_port}{PAGE_DIR}{args.page}"
        if not args.no_echo:
            echo = open_tab(cdp_port, base + "?role=echo")
            echo_cdp = CDP(echo["webSocketDebuggerUrl"])
            await_result(echo_cdp, timeout=60)  # echo is up once its promise resolves
        main_url = base + ("?" + args.query if args.query else "")
        tab = open_tab(cdp_port, main_url)
        cdp = CDP(tab["webSocketDebuggerUrl"])
        result = await_result(cdp, timeout=args.timeout)
        print(json.dumps(result, indent=1))
        if args.out:
            with open(args.out, "w", encoding="utf-8") as f:
                json.dump(result, f, indent=1)
        ok = isinstance(result, dict) and result.get("pass", False)
        sys.exit(0 if ok else 1)
    finally:
        if proc:
            proc.terminate()
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                proc.kill()
        srv.shutdown()
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
