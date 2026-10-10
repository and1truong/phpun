#!/usr/bin/env python3
"""Opt-in coverage diagnostics must preserve script output and explain fallback."""
import os
from pathlib import Path
import subprocess
import tempfile

binary = os.environ.get('PHPUN', './target/release/phpun')
with tempfile.TemporaryDirectory() as directory:
    script = Path(directory) / 'profile.php'
    script.write_text("""<?php
function scalar($n) { return $n + 1; }
function array_body() { $a = []; return 7; }
function foreach_body($a) { foreach ($a as $v) { return $v; } }
echo scalar(1), scalar(2), array_body(), array_body(), foreach_body([8]), '\n';
""")
    plain = subprocess.run([binary, str(script)], capture_output=True)
    prof = subprocess.run([binary, str(script)], env=dict(os.environ, PHPUN_VMPROF='1'), capture_output=True)
    assert plain.returncode == prof.returncode == 0
    assert plain.stdout == prof.stdout == b'23778\n', (plain, prof)
    assert plain.stderr == b''
    log = prof.stderr.decode()
    assert log.count('array_body reason=array') == 1, log
    assert log.count('foreach_body reason=foreach') == 1, log
    counters = dict(line.removeprefix('vm-coverage: ').split('=')
                    for line in log.splitlines() if line.startswith('vm-coverage: '))
    assert int(counters['executed-body']) == 2, log
    assert int(counters['array']) >= 2 and int(counters['foreach']) >= 1, log
print('VM coverage diagnostics: ok')
