"""Loopback HTTP fixture for the CTX-1344 net component acceptance rows.

Usage: python3 server.py <port>

The harness starts this under an isolated PID and stops it by that PID only.
It never listens on a non-loopback address and serves two fixed routes.
"""

import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):  # noqa: N802 - stdlib handler name
        body = b"hello-component"
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        print(f"served GET {self.path}", flush=True)

    def log_message(self, fmt, *args):
        print(f"http {fmt % args}", flush=True)


def main() -> int:
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 38082
    HTTPServer(("127.0.0.1", port), Handler).serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
