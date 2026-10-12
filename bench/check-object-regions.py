#!/usr/bin/env python3
"""Native object-region fixtures and benchmark must match PHP byte for byte."""
import os
from pathlib import Path
import subprocess

root = Path(__file__).resolve().parent.parent
binary = os.environ.get("PHPUN", str(root / "target/release/phpun"))
php = os.environ.get("PHP", "/home/linuxbrew/.linuxbrew/bin/php")
fixture = root / "crates/phpun-core/tests/fixtures/vm_object_regions.php"
for source in (fixture, root / "bench/40-objects.php"):
    oracle = subprocess.run([php, "-n", str(source)], capture_output=True, check=True)
    for _ in range(3):
        sut = subprocess.run([binary, str(source)], capture_output=True, check=True)
        assert sut.stdout == oracle.stdout, (source, sut.stdout, oracle.stdout, sut.stderr)
        assert not sut.stderr, (source, sut.stderr)
    print(f"{source.name}: byte-identical in 3 repetitions")
assert fixture.with_suffix(".expect").read_bytes() == subprocess.run(
    [php, "-n", str(fixture)], capture_output=True, check=True
).stdout
