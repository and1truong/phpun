#!/usr/bin/env python3
"""Minimal concurrent HTTP load driver (stdlib only).

Each of C threads issues requests sequentially over fresh connections
(these dev servers close the connection per response). Reports total
requests, wall time, req/s, error count, and whether every response
contained the expected marker.

usage: http-load.py <port> <total_requests> <concurrency> [path]
prints one line: <total> <wall_s> <rps> <errors> <ok_marker>
"""
import http.client
import sys
import threading
import time


def main() -> int:
    port = int(sys.argv[1])
    total = int(sys.argv[2])
    conc = int(sys.argv[3])
    path = sys.argv[4] if len(sys.argv) > 4 else "/?name=bench"
    marker = b"bench-ok"

    counters = {"done": 0, "errors": 0, "bad_body": 0}
    lock = threading.Lock()

    def worker() -> None:
        while True:
            with lock:
                if counters["done"] >= total:
                    return
                counters["done"] += 1
            try:
                conn = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
                conn.request("GET", path)
                resp = conn.getresponse()
                body = resp.read()
                conn.close()
                if resp.status != 200:
                    with lock:
                        counters["errors"] += 1
                elif marker not in body:
                    with lock:
                        counters["bad_body"] += 1
            except Exception:
                with lock:
                    counters["errors"] += 1

    threads = [threading.Thread(target=worker) for _ in range(conc)]
    t0 = time.monotonic()
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    wall = time.monotonic() - t0

    bad = counters["errors"] + counters["bad_body"]
    rps = total / wall if wall > 0 else 0.0
    print(f"{total} {wall:.3f} {rps:.1f} {bad} {'ok' if bad == 0 else 'BAD'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
