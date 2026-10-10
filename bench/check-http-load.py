#!/usr/bin/env python3
"""A marker in a wrong response must fail the HTTP load gate."""
import importlib.util
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading

spec = importlib.util.spec_from_file_location('loader', 'bench/http/http-load.py')
loader = importlib.util.module_from_spec(spec)
spec.loader.exec_module(loader)


class Handler(BaseHTTPRequestHandler):
    mode = 'ok'

    def do_GET(self):
        self.send_response(503 if self.mode == 'status' else 200)
        self.send_header('Content-Type', 'text/plain' if self.mode == 'header' else 'application/json')
        self.end_headers()
        self.wfile.write(b'bench-ok:wrong' if self.mode == 'body' else b'bench-ok:expected')

    def log_message(self, *_):
        pass


with ThreadingHTTPServer(('127.0.0.1', 0), Handler) as server:
    thread = threading.Thread(target=server.serve_forever)
    thread.start()
    try:
        for mode in ('ok', 'status', 'header', 'body'):
            Handler.mode = mode
            result = loader.load(server.server_port, 8, 2, (200, 'application/json', b'bench-ok:expected'))
            assert result['valid'] == (mode == 'ok'), result
            assert result['p50_ms'] <= result['p95_ms'] <= result['p99_ms'], result
        result = loader.load(1, 2, 1, (200, 'application/json', b'bench-ok:expected'), timeout=.1)
        assert not result['valid'] and result['errors'] == 2, result
    finally:
        server.shutdown()
        thread.join()
print('HTTP response gates: ok')
