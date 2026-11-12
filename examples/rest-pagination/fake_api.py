"""API HTTP falsa para o exemplo rest-pagination (ENT-085, plano §17.6).

Serve `/items` em 127.0.0.1 com três páginas por cursor. A segunda página responde 429 uma vez
com `Retry-After: 0`, para exercitar a reexecução transitória do plugin `rest`.
Uso stdlib apenas; não há dependências externas.
"""

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

PAGES = {
    None: {"data": [{"id": 1}, {"id": 2}], "next_cursor": "c2"},
    "c2": {"data": [{"id": 3}, {"id": 4}], "next_cursor": "c3"},
    "c3": {"data": [{"id": 5}], "next_cursor": None},
}


class FakeApi:
    def __init__(self):
        self.throttled = set()
        self.requests = []
        api = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                parts = urlsplit(self.path)
                if parts.path != "/items":
                    self._send(404, {"error": "not_found"})
                    return
                cursor = parse_qs(parts.query).get("cursor", [None])[0]
                api.requests.append(cursor)
                if cursor == "c2" and cursor not in api.throttled:
                    api.throttled.add(cursor)
                    self._send(429, {"error": "throttled"}, {"Retry-After": "0"})
                    return
                self._send(200, PAGES[cursor])

            def _send(self, status, payload, headers=None):
                body = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                for key, value in (headers or {}).items():
                    self.send_header(key, value)
                self.end_headers()
                self.wfile.write(body)

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.port = self._server.server_address[1]
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    def url(self, path="/items"):
        return f"http://127.0.0.1:{self.port}{path}"

    def __enter__(self):
        self._thread.start()
        return self

    def __exit__(self, *exc):
        self._server.shutdown()
        self._server.server_close()
