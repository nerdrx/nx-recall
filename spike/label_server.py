"""Local clip-labeling server. stdlib only, binds 127.0.0.1 — nothing leaves the box.

    python label_server.py            # serves http://127.0.0.1:7743

Labels land incrementally in clips/labels.json (safe to stop and resume any time).
"""
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

D = Path(__file__).parent
CLIPS = D / "clips"
LABELS = CLIPS / "labels.json"
_lock = threading.Lock()


def load_labels() -> dict:
    if LABELS.is_file():
        return json.loads(LABELS.read_text())
    return {}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):  # quiet
        pass

    def _send(self, code, body: bytes, ctype="application/json"):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path in ("/", "/index.html"):
            self._send(200, (D / "label_ui.html").read_bytes(), "text/html; charset=utf-8")
        elif self.path == "/state":
            st = {"regions": json.loads((CLIPS / "regions.json").read_text()),
                  "labels": load_labels()}
            self._send(200, json.dumps(st).encode())
        elif self.path.startswith("/clip/"):
            name = self.path.split("/")[-1]
            f = CLIPS / name
            if f.is_file() and f.suffix == ".wav" and f.name.startswith("clip_"):
                self._send(200, f.read_bytes(), "audio/wav")
            else:
                self._send(404, b"{}")
        else:
            self._send(404, b"{}")

    def do_POST(self):
        if self.path != "/label":
            self._send(404, b"{}")
            return
        n = int(self.headers.get("Content-Length", 0))
        rec = json.loads(self.rfile.read(n))
        cid, who = str(rec["id"]), rec["who"]
        with _lock:
            lab = load_labels()
            if who is None:
                lab.pop(cid, None)
            else:
                lab[cid] = who
            tmp = LABELS.with_suffix(".tmp")
            tmp.write_text(json.dumps(lab, indent=0))
            tmp.replace(LABELS)
        self._send(200, json.dumps({"n": len(lab)}).encode())


if __name__ == "__main__":
    srv = ThreadingHTTPServer(("127.0.0.1", 7743), H)
    print("labeling at http://127.0.0.1:7743  (Ctrl+C to stop; progress auto-saves)")
    srv.serve_forever()
