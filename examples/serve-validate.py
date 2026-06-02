#!/usr/bin/env python3
"""Tiny local validation endpoint for the wizard-widgets demo.

No third-party deps, no signup, no network egress. Serves on 127.0.0.1:8787.

    GET /validate?key=<value>

Returns 200 {"valid": true, "key": ...} when the key starts with "demo-",
otherwise 401 {"valid": false, "error": ...}. The wizard's [page.field.api]
block points here so `insmaller setup` (without --no-api-validate) exercises
the real API-validation path entirely on localhost.

Run it in one terminal, then run the wizard in another:
    python serve-validate.py
    insmaller setup --wizard wizard-widgets.toml --config demo.installer.toml
"""
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import urlparse, parse_qs

HOST, PORT = "127.0.0.1", 8787


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        u = urlparse(self.path)
        if u.path != "/validate":
            self._send(404, {"error": "not found"})
            return
        key = (parse_qs(u.query).get("key") or [""])[0]
        if key.startswith("demo-") and len(key) >= 8:
            self._send(200, {"valid": True, "key": key})
        else:
            self._send(401, {"valid": False, "error": "key must start with 'demo-' (min 8 chars)"})

    def _send(self, status, body):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, fmt, *args):
        print("validate:", fmt % args)


if __name__ == "__main__":
    print(f"local validator on http://{HOST}:{PORT}/validate?key=...  (Ctrl+C to stop)")
    print("  valid keys start with 'demo-' and are >= 8 chars, e.g. demo-123")
    HTTPServer((HOST, PORT), Handler).serve_forever()
