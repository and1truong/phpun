#!/usr/bin/env python3
"""Linux single-thread CLI: out-of-line copy lower bounds, never speed timings."""
import argparse
import bisect
import hashlib
import json
import os
import pathlib
import subprocess

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--php', required=True)
p.add_argument('--binary', required=True)
p.add_argument('--library', required=True)
p.add_argument('--save', required=True)
p.add_argument('--runtime', required=True)
p.add_argument('--check', action='store_true', help='use copy-check for both binaries; verify 2 calls/384 bytes')
p.add_argument('case', nargs='*')
a = p.parse_args()
binary, library = str(pathlib.Path(a.binary).resolve()), str(pathlib.Path(a.library).resolve())
env = dict(os.environ)
env.pop('LD_PRELOAD', None)
oracle = subprocess.run([a.php, *([] if a.check else ['-n']), *a.case], capture_output=True, timeout=300, env=env)
actual = subprocess.run([binary, *a.case], capture_output=True, timeout=300, env=dict(env, LD_PRELOAD=library))
rows, other, overflow = [], [], None
for line in actual.stderr.splitlines(keepends=True):
    if line.startswith(b'copy-profile\t'):
        _, dso, pc, calls, size = line.decode().strip().split('\t')
        rows.append(dict(dso=dso, pc=int(pc, 16), calls=int(calls), bytes=int(size)))
    elif line.startswith(b'copy-profile-overflow\t'):
        _, calls, size = line.decode().strip().split('\t')
        overflow = dict(calls=int(calls), bytes=int(size))
    else:
        other.append(line)
if (actual.returncode, actual.stdout, b''.join(other)) != (oracle.returncode, oracle.stdout, oracle.stderr):
    raise SystemExit('PHP exit/stdout/stderr mismatch after collector records removed')
if overflow is None or overflow['calls'] or overflow['bytes']:
    raise SystemExit('missing/overflowed copy collector')
symbols = []
for line in subprocess.check_output(['nm', '-n', '-C', '--defined-only', binary], text=True).splitlines():
    parts = line.split(None, 2)
    if len(parts) == 3 and parts[1] in ('t', 'T'):
        symbols.append((int(parts[0], 16), parts[2]))
addresses = [s[0] for s in symbols]
groups = {}
for row in rows:
    if row['dso'] == binary:
        i = bisect.bisect_right(addresses, row['pc']) - 1
        row['caller'] = symbols[i][1] if i >= 0 else 'unknown'
    else:
        row['caller'] = 'library:' + row['dso']
    group = groups.setdefault(row['caller'], dict(calls=0, bytes=0))
    for key in ('calls', 'bytes'):
        group[key] += row[key]
total, calls = sum(r['bytes'] for r in rows), sum(r['calls'] for r in rows)
if a.check and (actual.returncode, actual.stdout, total, calls) != (0, b'copy-ok\n', 384, 2):
    raise SystemExit('copy-check must record memcpy256 + overlapping memmove128')
result = dict(runtime=a.runtime, binary=binary, sha256=hashlib.sha256(pathlib.Path(binary).read_bytes()).hexdigest(),
              argv=a.case, gate='oracle exit/stdout/stderr exact after collector records removed',
              coverage='out-of-line memcpy/memmove only; inline/hidden/realloc movement unobserved; cold startup/parse included',
              overflow=overflow, observed_copy_bytes=total, observed_copy_calls=calls,
              callers=[dict(caller=k, **v, percent_observed_bytes=100*v['bytes']/total if total else 0)
                       for k, v in sorted(groups.items(), key=lambda item: item[1]['bytes'], reverse=True)], sites=rows)
pathlib.Path(a.save).write_text(json.dumps(result, indent=2) + '\n')
print(f'{calls} observed copies, {total} bytes; gate passed')
