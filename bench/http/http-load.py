#!/usr/bin/env python3
"""Fresh-connection load with full response gates and latency samples."""
import argparse
import concurrent.futures
import http.client
import json
import time


def request(port, path, timeout=10):
    conn = http.client.HTTPConnection('127.0.0.1', port, timeout=timeout)
    try:
        conn.request('GET', path)
        response = conn.getresponse()
        return response.status, response.getheader('Content-Type'), response.read()
    finally:
        conn.close()


def percentile(samples, fraction):
    ordered = sorted(samples)
    index = (len(ordered) - 1) * fraction
    lo = int(index)
    hi = min(lo + 1, len(ordered) - 1)
    return ordered[lo] + (ordered[hi] - ordered[lo]) * (index - lo)


def load(port, total, concurrency, expected, path='/?name=bench', timeout=10):
    if total < 1 or concurrency < 1:
        raise ValueError('requests and concurrency must be positive')

    def sample(_):
        start = time.perf_counter()
        try:
            valid = request(port, path, timeout) == expected
        except (OSError, http.client.HTTPException):
            valid = False
        return (time.perf_counter() - start) * 1000, valid

    start = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as pool:
        samples = list(pool.map(sample, range(total)))
    wall = time.perf_counter() - start
    latency = [ms for ms, _ in samples]
    errors = sum(not valid for _, valid in samples)
    return {'requests': total, 'wall_s': wall, 'rps': total / wall,
            'errors': errors, 'valid': errors == 0,
            'p50_ms': percentile(latency, .5), 'p95_ms': percentile(latency, .95),
            'p99_ms': percentile(latency, .99)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('port', type=int)
    parser.add_argument('requests', type=int)
    parser.add_argument('concurrency', type=int)
    parser.add_argument('--expected-body', required=True)
    parser.add_argument('--content-type', default='application/json')
    parser.add_argument('--path', default='/?name=bench')
    args = parser.parse_args()
    with open(args.expected_body, 'rb') as stream:
        expected = (200, args.content_type, stream.read())
    result = load(args.port, args.requests, args.concurrency, expected, args.path)
    print(json.dumps(result))
    return 0 if result['valid'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
