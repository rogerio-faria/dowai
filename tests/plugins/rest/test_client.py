"""Testes do plugin oficial `rest` (ENT-058) contra API falsa local.

Executar: python3 -I -m unittest discover -s tests/plugins/rest -t . (a partir da raiz do repositório)
"""

import json
import os
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "plugins", "rest"))

import client  # noqa: E402


class FakeApi:
    """Servidor HTTP falso. `routes[path]` é uma lista de respostas consumidas em ordem."""

    def __init__(self):
        self.routes = {}
        self.requests = []
        api = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def _serve(self, method):
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                api.requests.append(
                    {
                        "method": method,
                        "path": self.path,
                        "auth": self.headers.get("Authorization"),
                        "body": body.decode() if body else "",
                    }
                )
                key = urlsplit(self.path).path
                queue = api.routes.get(key, [])
                if not queue:
                    self.send_response(404)
                    self.end_headers()
                    return
                status, payload, headers, delay = queue.pop(0) if len(queue) > 1 else queue[0]
                if delay:
                    import time

                    time.sleep(delay)
                data = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                for name, value in headers.items():
                    self.send_header(name, value)
                self.end_headers()
                try:
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def do_GET(self):
                self._serve("GET")

            def do_POST(self):
                self._serve("POST")

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.base = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()


class RestPluginTests(unittest.TestCase):
    def setUp(self):
        self.api = FakeApi()

    def tearDown(self):
        self.api.close()

    def test_get_pages_reads_all_pages_by_cursor_without_duplicates(self):
        # Respostas consumidas em ordem: primeira página, depois ?cursor=c2 e ?cursor=c3.
        self.api.routes["/items"] = [
            (200, {"data": [{"id": 1}, {"id": 2}], "next_cursor": "c2"}, {}, 0),
            (200, {"data": [{"id": 3}], "next_cursor": "c3"}, {}, 0),
            (200, {"data": [{"id": 4}], "next_cursor": None}, {}, 0),
        ]
        result = client.get_pages(
            url=f"{self.api.base}/items", include_items=True, backoff_seconds=0
        )
        self.assertEqual(result["pages"], 3)
        self.assertEqual(result["records"], 4)
        self.assertEqual([item["id"] for item in result["items"]], [1, 2, 3, 4])
        self.assertEqual(result["retries"], 0)
        paths = [request["path"] for request in self.api.requests]
        self.assertEqual(paths[0], "/items")
        self.assertIn("cursor=c2", paths[1])
        self.assertIn("cursor=c3", paths[2])

    def test_429_is_retried_with_retry_after_and_counted(self):
        self.api.routes["/rl"] = [
            (429, {"error": "slow"}, {"Retry-After": "0"}, 0),
            (200, {"data": [{"id": 9}], "next_cursor": None}, {}, 0),
        ]
        result = client.get_pages(url=f"{self.api.base}/rl", backoff_seconds=0)
        self.assertEqual(result["pages"], 1)
        self.assertEqual(result["records"], 1)
        self.assertEqual(result["retries"], 1)
        self.assertEqual(len(self.api.requests), 2)

    def test_persistent_429_becomes_stable_code(self):
        self.api.routes["/busy"] = [(429, {"error": "slow"}, {"Retry-After": "0"}, 0)]
        with self.assertRaises(client.RestError) as raised:
            client.get(f"{self.api.base}/busy", max_attempts=3, backoff_seconds=0)
        self.assertEqual(raised.exception.code, "DOWAI_REST_RATE_LIMITED")
        self.assertEqual(len(self.api.requests), 3)

    def test_timeout_becomes_stable_code(self):
        self.api.routes["/slow"] = [(200, {"ok": True}, {}, 1.5)]
        with self.assertRaises(client.RestError) as raised:
            client.get(f"{self.api.base}/slow", timeout_seconds=1, max_attempts=1)
        self.assertEqual(raised.exception.code, "DOWAI_REST_TIMEOUT")

    def test_token_is_sent_as_bearer_and_never_returned(self):
        self.api.routes["/me"] = [(200, {"id": "u1"}, {}, 0)]
        result = client.get(f"{self.api.base}/me", token="segredo-123")
        self.assertEqual(self.api.requests[0]["auth"], "Bearer segredo-123")
        self.assertNotIn("segredo-123", json.dumps(result))

    def test_post_sends_json_body(self):
        self.api.routes["/orders"] = [(201, {"id": 7}, {}, 0)]
        result = client.post(f"{self.api.base}/orders", body={"sku": "A1"})
        self.assertEqual(result["status"], 201)
        self.assertEqual(result["body"], {"id": 7})
        self.assertEqual(json.loads(self.api.requests[0]["body"]), {"sku": "A1"})

    def test_redirect_is_not_followed(self):
        self.api.routes["/moved"] = [(302, {}, {"Location": "/items"}, 0)]
        with self.assertRaises(client.RestError) as raised:
            client.get(f"{self.api.base}/moved", max_attempts=1)
        self.assertEqual(raised.exception.code, "DOWAI_REST_HTTP_ERROR")
        self.assertEqual(len(self.api.requests), 1)

    def test_repeated_cursor_stops_the_read(self):
        self.api.routes["/loop"] = [(200, {"data": [], "next_cursor": "same"}, {}, 0)]
        with self.assertRaises(client.RestError) as raised:
            client.get_pages(url=f"{self.api.base}/loop", backoff_seconds=0)
        self.assertEqual(raised.exception.code, "DOWAI_REST_CURSOR_REPEATED")

    def test_page_limit_is_enforced(self):
        self.api.routes["/many"] = [(200, {"data": [], "next_cursor": "n"}, {}, 0)]
        with self.assertRaises(client.RestError) as raised:
            client.get_pages(url=f"{self.api.base}/many", max_pages=1, backoff_seconds=0)
        self.assertEqual(raised.exception.code, "DOWAI_REST_PAGE_LIMIT")

    def test_invalid_limits_are_refused_before_any_request(self):
        with self.assertRaises(client.RestError) as raised:
            client.get(f"{self.api.base}/x", timeout_seconds=0)
        self.assertEqual(raised.exception.code, "DOWAI_REST_INPUT_INVALID")
        self.assertEqual(self.api.requests, [])


if __name__ == "__main__":
    unittest.main()
