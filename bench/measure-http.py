#!/usr/bin/env python3
"""Compare dev-server lifecycles against one PHP HTTP response oracle."""
import argparse
import datetime
import hashlib
import importlib.util
import os
from pathlib import Path
import platform
import shlex
import signal
import statistics
import subprocess
import tempfile
import time

from measure import binary_path, metadata

spec = importlib.util.spec_from_file_location('http_load', 'bench/http/http-load.py')
loader = importlib.util.module_from_spec(spec)
spec.loader.exec_module(loader)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--save', type=Path)
    parser.add_argument('--reps', type=int, default=int(os.environ.get('HTTP_REPS', '3')))
    args = parser.parse_args()
    total, concurrency, workers, port0 = [int(os.environ.get(k, d)) for k, d in
                                         [('REQ', '400'), ('CONC', '8'), ('WORKERS', '8'), ('PORT0', '18300')]]
    if min(args.reps, total, concurrency, workers) < 1:
        parser.error('reps, requests, concurrency and workers must be positive')
    if workers == 2:
        parser.error('use WORKERS=1 or >=3: PHP requires at least two forked children')
    if any(os.environ.get(flag) is not None for flag in ('PHPUN_CALLPROF', 'PHPUN_ALLOC', 'PHPUN_VMPROF')):
        parser.error('disable profiling for speed measurements')
    php = [binary_path(os.environ.get('PHP', 'php')), *shlex.split(os.environ.get('PHP_ARGS', '-n'))]
    phpun = binary_path(os.environ.get('PHPUN', './target/release/phpun'))
    labels = ['php -S (1 process)', f'php -S ({workers} total processes)',
              'phpun serve (fresh interpreter/request)', f'phpun serve ({workers} warm workers)']
    rows, errors = [[] for _ in labels], []
    config = metadata([*php, '-r', 'echo json_encode([php_ini_loaded_file(), get_loaded_extensions(), ini_get("opcache.enable_cli"), ini_get("opcache.jit")]);'])
    report = ['# HTTP benchmark results', '',
              f'- date: {datetime.datetime.now(datetime.timezone.utc).isoformat()}',
              f'- source checkout: {metadata(["git", "rev-parse", "HEAD"])}; dirty: {bool(metadata(["git", "status", "--porcelain"]))}',
              f'- binary build provenance: {os.environ.get("PHPUN_BUILD_INFO", "not supplied")}',
              f'- phpun: {phpun}; sha256: {hashlib.sha256(Path(phpun).read_bytes()).hexdigest()}',
              f'- PHP: {metadata([*php, "-r", "echo PHP_VERSION;"])}; command: {shlex.join(php)}; sha256: {hashlib.sha256(Path(php[0]).read_bytes()).hexdigest()}',
              f'- PHP config: {config}',
              f'- host: {platform.platform()}; {os.cpu_count()} logical CPUs',
              f'- load: {total} requests/rep, concurrency {concurrency}, {args.reps} reps; fresh connection/request; GET /?name=bench',
              '- lifecycle: fresh interpreters and warm persistent handlers reported separately',
              '- PHP worker policy: WORKERS-1 forked children plus the serving parent; warm phpun has WORKERS handler threads',
              '- gate: every response matches reference status, Content-Type and full body, including warmup; invalid reps have no throughput result',
              '- metric: median throughput [min,max]; median per-rep latency percentiles; readiness/startup and warmup excluded',
              '- scope: dev servers only; results do not represent PHP-FPM + OPcache', '']
    expected = None
    with tempfile.TemporaryDirectory() as directory:
        for rep in range(args.reps):
            order = list(range(4)) if rep == 0 else [(i + rep) % 4 for i in range(4)]
            for index in order:
                port = port0 + rep * 4 + index + 1
                env = dict(os.environ)
                env.pop('PHP_CLI_SERVER_WORKERS', None)
                if index == 1 and workers > 1:
                    env['PHP_CLI_SERVER_WORKERS'] = str(workers - 1)
                if index < 2:
                    command = [*php, '-S', f'127.0.0.1:{port}', '-t', 'bench/http', 'bench/http/app.php']
                else:
                    script = 'bench/http/app-worker.php' if index == 3 else 'bench/http/app.php'
                    command = [phpun, 'serve', script, '--port', str(port)]
                    if index == 3:
                        command.extend(['--workers', str(workers)])
                report.append(f'<!-- rep {rep + 1}, {labels[index]}: {shlex.join(command)} -->')
                log = Path(directory) / f'{rep}-{index}.log'
                with log.open('w') as stream:
                    process = subprocess.Popen(command, env=env, stdout=stream, stderr=stream, start_new_session=True)
                    try:
                        deadline = time.monotonic() + 10
                        while True:
                            if process.poll() is not None:
                                raise RuntimeError(f'server exited {process.returncode}')
                            try:
                                probe = loader.request(port, '/?name=bench', timeout=.5)
                                break
                            except (OSError, loader.http.client.HTTPException):
                                if time.monotonic() >= deadline:
                                    raise RuntimeError('readiness timeout')
                                time.sleep(.05)
                        if expected is None:
                            if index != 0 or rep != 0:
                                raise RuntimeError('PHP HTTP oracle unavailable')
                            if probe[0] != 200 or probe[1] != 'application/json' or not probe[2].startswith(b'bench-ok:'):
                                raise RuntimeError('invalid PHP oracle response')
                            expected = probe
                            report.append(f'- oracle body sha256: {hashlib.sha256(expected[2]).hexdigest()}')
                        if probe != expected:
                            raise RuntimeError('readiness response differs from PHP oracle')
                        warmup = loader.load(port, concurrency * 2, concurrency, expected)
                        if not warmup['valid']:
                            raise RuntimeError(f'warmup has {warmup["errors"]} invalid responses')
                        result = loader.load(port, total, concurrency, expected)
                        if not result['valid']:
                            raise RuntimeError(f'{result["errors"]} invalid measured responses')
                        if process.poll() is not None:
                            raise RuntimeError(f'server exited {process.returncode} during load')
                        rows[index].append(result)
                    except (OSError, RuntimeError) as error:
                        errors.append(f'- rep {rep + 1}, {labels[index]}: {error}; server log: {log.read_text()[-800:]}')
                    finally:
                        try:
                            os.killpg(process.pid, signal.SIGTERM)
                        except ProcessLookupError:
                            pass
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            os.killpg(process.pid, signal.SIGKILL)
                            process.wait()
    report.extend(['', '| server | req/s median [min,max] | p50 ms | p95 ms | p99 ms | gate |',
                   '|---|---:|---:|---:|---:|---|'])
    for label, results in zip(labels, rows):
        if len(results) != args.reps:
            report.append(f'| {label} | n/a | n/a | n/a | n/a | INVALID |')
        else:
            rates = [r['rps'] for r in results]
            latency = [statistics.median(r[key] for r in results) for key in ('p50_ms', 'p95_ms', 'p99_ms')]
            report.append(f'| {label} | {statistics.median(rates):.1f} [{min(rates):.1f},{max(rates):.1f}] | ' +
                          ' | '.join(f'{value:.3f}' for value in latency) + ' | ok |')
    for label, results in zip(labels, rows):
        if len(results) == args.reps:
            report.append(f'- {label} measured wall seconds per rep: ' + ', '.join(f'{r["wall_s"]:.6f}' for r in results))
    if errors:
        report.extend(['', 'Invalid measurements:', '', *errors])
    text = '\n'.join(report) + '\n'
    print(text, end='')
    if args.save:
        args.save.write_text(text)
    return 1 if errors else 0


if __name__ == '__main__':
    raise SystemExit(main())
