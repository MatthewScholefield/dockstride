"""Small development fixture, not a production authentication service."""
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

# Honor Docker's secret file interface explicitly. Never print the key.
key = Path(os.environ["AUTH_KEY_FILE"]).read_bytes().strip()
if len(key) < 32:
    raise RuntimeError("Authentication key is too short")


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path not in ("/", "/health"):
            self.send_error(404)
            return
        body = json.dumps({"status": "ok", "application": "dockstride-sample"}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


ThreadingHTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
