#!/usr/bin/env python3
"""Byte/exit checks for the application and workload decomposition probes."""
import os
import shlex
import subprocess

php = os.environ.get('PHP', 'php')
sut = os.environ.get('PHPUN', './target/release/phpun')
cases = [('bench/profile/arrays.php', mode, '30') for mode in ('append', 'map', 'usort', 'filter', 'keys')]
cases += [('bench/profile/strings.php', mode, '10') for mode in ('repeat', 'replace', 'slice', 'concat')]
cases += [('bench/profile/objects.php', mode, '10') for mode in ('ctor', 'norm', 'scaled')]
cases += [('bench/profile/fib.php', mode, '10', '2') for mode in ('typed', 'untyped')]
cases += [('bench/profile/closures.php', '30')]
cases += [('bench/profile/closures.php', '30', 'scalar')]
cases += [('bench/profile/foreach.php', '30')]
cases += [('bench/app/cli.php', '10', '2')]
cases += [('examples/composer/run.php',)]
if os.environ.get('BENCH_REAL_APPS') == '1':
    cases += [('examples/doctrine-inflector/demo.php',),
              ('examples/symfony-console/console.php', 'app:greet', 'World', '--yell', '-i', '2', '--no-ansi')]
for case in cases:
    ref = subprocess.run([php, *shlex.split(os.environ.get('PHP_ARGS', '-n')), *case], capture_output=True, timeout=30)
    actual = subprocess.run([sut, *case], capture_output=True, timeout=30)
    assert ref.returncode == actual.returncode == 0, (case, ref, actual)
    assert (ref.stdout, ref.stderr) == (actual.stdout, actual.stderr), (case, ref, actual)
print(f'{len(cases)} workload oracle checks: ok')
