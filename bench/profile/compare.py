#!/usr/bin/env python3
"""Same-host alternating before/after timings, with raw samples and byte gates."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from measure import binary_path, capture, metadata

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--before', required=True)
parser.add_argument('--after', required=True)
parser.add_argument('--php', required=True)
parser.add_argument('--before-build', required=True)
parser.add_argument('--after-build', required=True)
parser.add_argument('--reps', type=int, default=7)
parser.add_argument('--save', type=Path, required=True)
parser.add_argument('--phases', action='store_true')
args = parser.parse_args()
if args.reps < 2:
    parser.error('at least two repetitions')
if any(os.environ.get(flag) is not None for flag in ('PHPUN_CALLPROF', 'PHPUN_ALLOC', 'PHPUN_VMPROF')):
    parser.error('disable profilers for speed measurements')
before, after, php = map(binary_path, (args.before, args.after, args.php))
cases = [(str(p),) for p in sorted(Path('bench').glob('[0-9]*.php'))]
if args.phases:
    cases = [('bench/profile/arrays.php', mode, '20000') for mode in ('append', 'map', 'usort', 'filter', 'keys')]
    cases += [('bench/profile/strings.php', mode, '400') for mode in ('repeat', 'replace', 'slice', 'concat')]
    cases += [('bench/profile/objects.php', mode, '5000') for mode in ('ctor', 'norm', 'scaled')]
    cases += [('bench/profile/fib.php', mode, '26', '1') for mode in ('typed', 'untyped')]
    cases += [('bench/profile/fib.php', mode, '29', '2') for mode in ('typed', 'untyped')]
    cases += [('bench/11-sieve.php', '1', n) for n in ('10000', '20000', '40000', '80000')]
    cases += [('bench/50-regex.php', '1'), ('bench/60-json.php', '1'), ('bench/70-db.php', '1')]
    cases += [('bench/app/cli.php', '100', '1'), ('bench/app/cli.php', '100', '20')]
report = {'host': platform.platform(), 'cpu_count': os.cpu_count(),
          'source': metadata(['git', 'rev-parse', 'HEAD']),
          'dirty': bool(metadata(['git', 'status', '--porcelain'])),
          'before': {'build': args.before_build, 'binary': before},
          'after': {'build': args.after_build, 'binary': after},
          'php': {'binary': php, 'args': ['-n'], 'version': metadata([php, '-n', '-r', 'echo PHP_VERSION;'])},
          'metric': 'cold startup + parse + execution; median [min,max] ms; profiler off', 'cases': []}
for key in ('before', 'after', 'php'):
    report[key]['sha256'] = hashlib.sha256(Path(report[key]['binary']).read_bytes()).hexdigest()
for case in cases:
    samples = [[], []]
    oracle = capture([php, '-n', *case], 300)
    assert oracle[1] == 0, (case, oracle[1:])
    for rep in range(args.reps):
        for index in (0, 1) if rep % 2 == 0 else (1, 0):
            result = capture([(before, after)[index], *case], 300)
            assert result[1] == 0 and result[2:] == oracle[2:], (case, index, result[1:])
            samples[index].append(result[0])
    row = {'argv': case, 'before_ms': samples[0], 'after_ms': samples[1],
           'before_median': statistics.median(samples[0]), 'after_median': statistics.median(samples[1]), 'gate': 'ok'}
    row['after/before'] = row['after_median'] / row['before_median']
    report['cases'].append(row)
    args.save.write_text(json.dumps(report, indent=2) + '\n')
    print(f'{" ".join(case)}: {row["before_median"]:.3f} -> {row["after_median"]:.3f} ms ({row["after/before"]:.3f}x)', flush=True)
