#!/usr/bin/env python3
"""Bucket perf cpu-clock stacks by their nearest VM/AST dispatch frame.

Input: perf script --fields comm,pid,time,event,period,ip,sym,dso.
VM includes runtime helpers below that frame; this is not native-op coverage.
"""
import argparse
from collections import Counter
import json
from pathlib import Path
import re


def summarize(text):
    counts, weights = Counter(), Counter()
    for block in text.strip().split('\n\n'):
        if not block.strip():
            continue
        lines = block.splitlines()
        period = re.search(r':\s+(\d+)\s+cpu-clock:u:', lines[0])
        if period is None or len(lines) < 2:
            raise ValueError('expected a cpu-clock:u sample with a call chain')
        kind = 'other-or-incomplete'
        for line in lines[1:]:
            if '<phpun_core::interp::Interp>::vm_' in line:
                kind = 'VM-nearest-dispatch'
                break
            if re.search(r'<phpun_core::interp::Interp>::(?:eval\b|exec\b|exec_at\b|exec_block\w*)', line):
                kind = 'AST-nearest-dispatch'
                break
        counts[kind] += 1
        weights[kind] += int(period[1])
    total = sum(weights.values())
    if not total:
        raise ValueError('no positive sample periods')
    return {'samples': sum(counts.values()), 'counts': dict(counts),
            'estimated_cpu_ns': total,
            'sample_weight_pct': {k: 100 * v / total for k, v in weights.items()}}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--input', type=Path)
    mode.add_argument('--self-check', action='store_true')
    args = parser.parse_args()
    if args.self_check:
        # An outer AST frame cannot turn an inner VM callee into AST time.
        text = '''phpun 1 1.0: 10 cpu-clock:u:
 0 malloc (libc)
 1 <phpun_core::interp::Interp>::vm_exec (phpun)
 2 <phpun_core::interp::Interp>::eval (phpun)

phpun 1 2.0: 20 cpu-clock:u:
 0 <phpun_core::interp::Interp>::eval (phpun)
 1 <phpun_core::interp::Interp>::vm_exec (phpun)

phpun 1 3.0: 70 cpu-clock:u:
 0 parser::parse (phpun)
'''
        report = summarize(text)
        assert report['samples'] == 3 and report['estimated_cpu_ns'] == 100
        assert report['sample_weight_pct'] == {
            'VM-nearest-dispatch': 10, 'AST-nearest-dispatch': 20, 'other-or-incomplete': 70}
        print('CPU stack classification: ok')
    else:
        print(json.dumps(summarize(args.input.read_text()), indent=2))
