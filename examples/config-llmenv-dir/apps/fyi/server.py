#!/usr/bin/env python3
"""Localhost server for the living fyi. Stdlib only, single process.

  GET  /            -> index.html
  GET  /api/data    -> the living list (data/data.json)
  GET  /api/status  -> {lastScan, date, refreshing}
  POST /api/toggle  -> {id, checked}: persist a check-off
  POST /api/refresh -> kick refresh.sh (detached); 409 if already running
"""
import json
import os
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.abspath(__file__))
DATA = os.path.join(ROOT, "data", "data.json")
INDEX = os.path.join(ROOT, "index.html")
REFRESH = os.path.join(ROOT, "refresh.sh")
PORT = int(os.environ.get("MT_PORT", "8787"))

_lock = threading.Lock()
_proc = None  # the running refresh subprocess, if any
_NOT_FOUND = b'{"error":"not found"}'
_MAX_BODY = 1_048_576  # 1 MiB cap on request bodies (localhost tool, tiny POSTs)


def _read_data():
    try:
        with open(DATA, encoding="utf-8") as f:
            return json.load(f)
    except (FileNotFoundError, json.JSONDecodeError):
        return {"date": None, "lastScan": None, "items": []}


def _write_data(d):
    os.makedirs(os.path.dirname(DATA), exist_ok=True)
    tmp = DATA + ".tmp"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(d, f, indent=2)
    os.replace(tmp, DATA)  # atomic; never leaves a half-written file


def _refreshing():
    return _proc is not None and _proc.poll() is None


class Handler(BaseHTTPRequestHandler):
    def _send(self, code, body, ctype="application/json"):
        b = body if isinstance(body, bytes) else body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        if self.path in ("/", "/index.html"):
            try:
                with open(INDEX, "rb") as f:
                    self._send(200, f.read(), "text/html; charset=utf-8")
            except FileNotFoundError:
                self._send(404, b"index.html missing")
        elif self.path == "/api/data":
            self._send(200, json.dumps(_read_data()))
        elif self.path == "/api/status":
            d = _read_data()
            self._send(200, json.dumps({"lastScan": d.get("lastScan"),
                                        "date": d.get("date"),
                                        "refreshing": _refreshing()}))
        else:
            self._send(404, _NOT_FOUND)

    def do_POST(self):
        global _proc
        n = min(int(self.headers.get("Content-Length") or 0), _MAX_BODY)
        raw = self.rfile.read(n)
        if self.path == "/api/toggle":
            try:
                body = json.loads(raw or b"{}")
                tid, checked = body["id"], bool(body["checked"])
            except (json.JSONDecodeError, KeyError):
                return self._send(400, b'{"error":"bad body"}')
            with _lock:
                d = _read_data()
                hit = next((it for it in d["items"] if it["id"] == tid), None)
                if hit is None:
                    return self._send(404, b'{"error":"unknown id"}')
                hit["checked"] = checked
                hit["manual"] = True
                _write_data(d)
            self._send(200, b'{"ok":true}')
        elif self.path == "/api/refresh":
            with _lock:
                if _refreshing():
                    return self._send(409, b'{"error":"already refreshing"}')
                try:
                    _proc = subprocess.Popen(["bash", REFRESH], cwd=ROOT)
                except OSError as e:
                    return self._send(
                        500, json.dumps({"error": f"could not start refresh: {e}"}))
            self._send(200, b'{"started":true}')
        else:
            self._send(404, _NOT_FOUND)

    def log_message(self, *_a):
        pass  # quiet — this is a personal localhost tool


if __name__ == "__main__":
    print(f"fyi on http://127.0.0.1:{PORT}")
    ThreadingHTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
