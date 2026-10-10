#!/usr/bin/env python3
"""Cold CLI timings with byte/exit gates on every alternating repetition."""

import argparse
import datetime
import hashlib
import math
import os
from pathlib import Path
import platform
import shlex
import shutil
import statistics
import subprocess
import time


def capture(command, timeout):
    start = time.perf_counter_ns()
    try:
        proc = subprocess.run(command, capture_output=True, timeout=timeout)
        return (time.perf_counter_ns() - start) / 1e6, proc.returncode, proc.stdout, proc.stderr
    except subprocess.TimeoutExpired:
        return 0, "timeout", b"", b""
    except OSError as exc:
        return 0, str(exc), b"", b""


def metadata(command):
    _, code, out, err = capture(command, 10)
    return (out or err).decode(errors="replace").strip() if code == 0 else "unavailable"


def binary_path(name):
    path = shutil.which(name)
    if path is None:
        raise SystemExit(f"binary not found: {name}")
    return str(Path(path).resolve())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--save", type=Path)
    parser.add_argument("--reps", type=int, default=int(os.environ.get("BENCH_REPS", "7")))
    parser.add_argument("--timeout", type=float, default=float(os.environ.get("TIMEOUT", "300")))
    parser.add_argument("--bench", action="append", type=Path, help="limit scripts (repeatable)")
    parser.add_argument("--script-arg", action="append", default=[], help="argv for each script")
    parser.add_argument("--php-arg", action="append", default=[], help="reference flags, e.g. --php-arg=-n")
    args = parser.parse_args()
    if args.reps < 2 or args.timeout <= 0:
        parser.error("use at least two repetitions and a positive timeout")
    if os.environ.get("PHPUN_CALLPROF") is not None or os.environ.get("PHPUN_ALLOC") is not None or os.environ.get("PHPUN_VMPROF") is not None:
        parser.error("disable PHPUN_CALLPROF, PHPUN_ALLOC and PHPUN_VMPROF when measuring speed")
    php = [binary_path(os.environ.get("PHP", "php")), *args.php_arg]
    phpun = [binary_path(os.environ.get("PHPUN", "./target/release/phpun"))]
    scripts = args.bench if args.bench is not None else sorted(Path("bench").glob("[0-9]*.php"))
    if not scripts or any(not script.is_file() for script in scripts):
        parser.error("benchmark scripts must exist")
    source_commit = metadata(["git", "rev-parse", "HEAD"])
    dirty = bool(metadata(["git", "status", "--porcelain", "--untracked-files=normal"]))
    php_config = metadata([*php, "-r", "echo json_encode(['ini'=>php_ini_loaded_file(),"
                           "'scanned'=>php_ini_scanned_files(),'extensions'=>get_loaded_extensions(),"
                           "'opcache.enable_cli'=>ini_get('opcache.enable_cli'),"
                           "'opcache.jit'=>ini_get('opcache.jit'),"
                           "'opcache.jit_buffer_size'=>ini_get('opcache.jit_buffer_size')]);"])
    report = ["# bench results", "",
              f"- date: {datetime.datetime.now(datetime.timezone.utc).isoformat()}",
              f"- source checkout: {source_commit}; dirty: {dirty} (not binary build provenance)",
              f"- phpun: {metadata([*phpun, '--version'])}",
              f"- phpun binary: {phpun[0]}; sha256: {hashlib.sha256(Path(phpun[0]).read_bytes()).hexdigest()}",
              f"- php: {metadata([*php, '-r', 'echo PHP_VERSION;'])}; command: {shlex.join(php)}",
              f"- php binary sha256: {hashlib.sha256(Path(php[0]).read_bytes()).hexdigest()}",
              f"- php config: {php_config}",
              f"- host: {platform.platform()}; {os.cpu_count()} logical CPUs",
              f"- build provenance: {os.environ.get('PHPUN_BUILD_INFO', 'not supplied; set PHPUN_BUILD_INFO')}",
              f"- metric: median wall-clock ms; {args.reps} reps; alternating runtime order; timeout {args.timeout:g}s",
              "- scope: cold process startup + parse + execution; min/max show observed dispersion",
              "- gates: zero exit, deterministic stdout and stderr, byte-identical to reference on every rep",
              "- ratio = phpun / php; below 1 means phpun faster; profiler disabled", "",
              "| bench | PHP median [min,max] ms | phpun median [min,max] ms | ratio | gate |",
              "|---|---:|---:|---:|---|"]
    ratios, errors = [], []
    for script in scripts:
        times = [[], []]
        oracle = None
        error = None
        report.append(f"<!-- commands: {shlex.join([*php, str(script), *args.script_arg])} ; "
                      f"{shlex.join([*phpun, str(script), *args.script_arg])} -->")
        for rep in range(args.reps):
            outputs = [None, None]
            for index in (0, 1) if rep % 2 == 0 else (1, 0):
                runtime = (php, phpun)[index]
                ms, code, out, err = capture([*runtime, str(script), *args.script_arg], args.timeout)
                if code != 0:
                    error = f"rep {rep + 1}, {('PHP', 'phpun')[index]} exit={code}"
                    break
                times[index].append(ms)
                outputs[index] = (out, err)
            if error:
                break
            if outputs[0] != outputs[1]:
                error = f"rep {rep + 1}, stdout/stderr mismatch"
                break
            if oracle is not None and outputs[0] != oracle:
                error = f"rep {rep + 1}, nondeterministic output"
                break
            oracle = outputs[0]
        if error:
            report.append(f"| {script.stem} | n/a | n/a | n/a | INVALID |")
            errors.append(f"- {script}: {error}")
            continue
        medians = [statistics.median(samples) for samples in times]
        ratio = medians[1] / medians[0]
        ratios.append(ratio)
        cells = [f"{statistics.median(s):.3f} [{min(s):.3f},{max(s):.3f}]" for s in times]
        report.append(f"| {script.stem} | {cells[0]} | {cells[1]} | {ratio:.2f}× | ok |")
    report.extend(["", f"valid benchmarks: {len(ratios)}/{len(scripts)}"])
    if ratios:
        report.append(f"geometric mean slowdown (valid benchmarks only): "
                      f"{math.exp(statistics.mean(math.log(r) for r in ratios)):.2f}×")
    if errors:
        report.extend(["", "Invalid measurements:", "", *errors])
    text = "\n".join(report) + "\n"
    print(text, end="")
    if args.save:
        args.save.write_text(text)
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
