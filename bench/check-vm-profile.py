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
function scope_body() { global $external; return 7; }
function foreach_body($a) { foreach ($a as $v) { return $v; } }
class ProfileMethod { public function method_scope() { global $external; return 7; } }
$obj = new ProfileMethod();
echo scalar(1), scalar(2), scope_body(), scope_body(), foreach_body([8]), $obj->method_scope(), $obj->method_scope(), '\n';
""")
    plain = subprocess.run([binary, str(script)], capture_output=True)
    prof = subprocess.run([binary, str(script)], env=dict(os.environ, PHPUN_VMPROF='1'), capture_output=True)
    assert plain.returncode == prof.returncode == 0
    assert plain.stdout == prof.stdout == b'2377877\n', (plain, prof)
    assert plain.stderr == b''
    log = prof.stderr.decode()
    assert log.count('scope_body reason=scope') == 1, log
    assert log.count('foreach_body reason=foreach') == 1, log
    assert log.count('method_scope reason=scope') == 1, log
    counters = dict(line.removeprefix('vm-coverage: ').split('=')
                    for line in log.splitlines() if line.startswith('vm-coverage: '))
    assert int(counters['executed-body']) == 2, log
    assert int(counters['scope']) >= 2 and int(counters['foreach']) >= 1, log
print('VM coverage diagnostics: ok')
