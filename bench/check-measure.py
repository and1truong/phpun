#!/usr/bin/env python3
"""Check that failed later reps, stderr divergence and timeout cannot win."""

import os
from pathlib import Path
import subprocess
import tempfile


with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    script = root / "case.php"
    script.write_text("ignored")
    runtime = root / "runtime"
    runtime.write_text("""#!/usr/bin/env python3
import os, pathlib, sys, time
if any(x in sys.argv for x in ['-r', '--version']):
    print('mock metadata')
    sys.exit(0)
mode = os.environ.get('MODE', 'ok') if pathlib.Path(sys.argv[0]).name == 'sut' else 'ok'
count = pathlib.Path(sys.argv[0] + '.count')
n = int(count.read_text()) + 1 if count.exists() else 1
count.write_text(str(n))
if mode == 'timeout': time.sleep(1)
if mode == 'later-fail' and n == 2: sys.exit(9)
print('RESULT 42')
if mode == 'stderr': print('unexpected warning', file=sys.stderr)
if mode == 'nondeterministic': print(n)
""")
    runtime.chmod(0o755)
    sut = root / "sut"
    sut.write_bytes(runtime.read_bytes())
    sut.chmod(0o755)
    for mode in ('ok', 'later-fail', 'stderr', 'timeout', 'nondeterministic'):
        for counter in root.glob('*.count'):
            counter.unlink()
        env = dict(os.environ, PHP=str(runtime), PHPUN=str(sut), MODE=mode)
        run = subprocess.run(['python3', 'bench/measure.py', '--bench', str(script),
                              '--reps', '3', '--timeout', '0.2'],
                             env=env, capture_output=True, text=True)
        assert (run.returncode == 0) == (mode == 'ok'), (mode, run.stdout, run.stderr)
        if mode != 'ok':
            assert '| n/a | n/a | n/a | INVALID |' in run.stdout, run.stdout
            assert 'geometric mean' not in run.stdout, run.stdout
    print('benchmark gates: ok')
