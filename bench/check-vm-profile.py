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
function fallback_body() { goto done; done: return 7; }
function foreach_body($a) { foreach ($a as $v) { return $v; } }
class ProfileMethod { public function method_fallback() { goto done; done: return 7; } }
$obj = new ProfileMethod();
echo scalar(1), scalar(2), fallback_body(), fallback_body(), foreach_body([8]), $obj->method_fallback(), $obj->method_fallback(), '\n';
""")
    plain = subprocess.run([binary, str(script)], capture_output=True)
    prof = subprocess.run([binary, str(script)], env=dict(os.environ, PHPUN_VMPROF='1'), capture_output=True)
    assert plain.returncode == prof.returncode == 0
    assert plain.stdout == prof.stdout == b'2377877\n', (plain, prof)
    assert plain.stderr == b''
    log = prof.stderr.decode()
    assert log.count('fallback_body reason=other') == 1, log
    assert log.count('method_fallback reason=other') == 1, log
    counters = dict(line.removeprefix('vm-coverage: ').split('=')
                    for line in log.splitlines() if line.startswith('vm-coverage: '))
    assert int(counters['executed-body']) == 3, log
    assert int(counters['other']) >= 2 and int(counters['hybrid-body']) == 1, log
print('VM coverage diagnostics: ok')

# Root loops must actually use bytecode while preserving global scope.
root = Path('crates/phpun-core/tests/fixtures/vm_top_level.php')
prof = subprocess.run([binary, str(root)], env=dict(os.environ, PHPUN_VMPROF='1'), capture_output=True)
assert prof.returncode == 0 and prof.stdout == root.with_suffix('.expect').read_bytes(), prof
assert b'vm-coverage: top-level-entry=1\n' in prof.stderr, prof.stderr

loops = Path('crates/phpun-core/tests/fixtures/vm_foreach_bodies.php')
prof = subprocess.run([binary, str(loops)], env=dict(os.environ, PHPUN_VMPROF='1'), capture_output=True)
assert prof.returncode == 0 and prof.stdout == loops.with_suffix('.expect').read_bytes(), prof
counters = dict(line.removeprefix('vm-coverage: ').split('=')
                for line in prof.stderr.decode().splitlines() if line.startswith('vm-coverage: '))
assert int(counters['loop-body-entry']) > 0, prof.stderr
