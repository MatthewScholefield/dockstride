#!/usr/bin/env python3
"""Small real HTTP service: verifies identity, readiness, and secret access."""
import hashlib
import json
import os
from pathlib import Path
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SECRET = Path(os.environ.get("AUTH_KEY_FILE", "/run/secrets/authKey")).read_bytes()
if not SECRET:
    raise RuntimeError("authKey is empty")


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if os.environ.get("FAIL_HEALTH") == "true":
            self.send_error(503, "deliberate readiness failure")
            return
        if os.environ.get("REQUIRE_MIGRATION") == "true" and not Path("/data/migrated").exists():
            self.send_error(503, "migration has not completed")
            return
        if self.path not in ("/", "/health"):
            self.send_error(404)
            return
        payload = json.dumps({
            "application": "dockstride-sample",
            "project": os.environ["PROJECT"],
            "role": os.environ.get("ROLE", "api"),
            "revision": Path("/app/content/revision.txt").read_text().strip(),
            "secretFingerprint": hashlib.sha256(SECRET).hexdigest(),
            "uid": os.getuid(),
            "migrated": Path("/data/migrated").exists(),
        }).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


print("dockstride-sample ready; secret accessible; uid=" + str(os.getuid()), flush=True)
ThreadingHTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
