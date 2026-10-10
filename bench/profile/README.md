# Workload and lifecycle probes

These scripts decompose the existing workloads. Phase scripts include input
preparation and cold process startup, so their timings do not add up to the full
benchmark. They live outside the nine-script rolling baseline glob.

- `arrays.php append|map|usort|filter|keys [size]`
- `strings.php repeat|replace|slice|concat [iterations]`
- `objects.php ctor|norm|scaled [size]`
- `fib.php typed|untyped [n] [iterations]`
- `../app/cli.php [rows] [iterations]`: PSR-4 style autoload and a report component
  with callback sorting and JSON; an application-shaped probe, not a framework
  production benchmark.

Run `PHP=/path/php PHPUN=/path/phpun python3 bench/check-profile-workloads.py` for
small byte/exit oracle checks. Run `compare.py --before /path/baseline --after
/path/candidate --php /path/php --before-build 'commit and flags' --after-build
'commit and flags' --save /tmp/comparison.json [--phases]` for alternating,
unprofiled comparisons. It saves every sample and rejects invalid outputs;
`--phases` also includes typed/untyped long runs, sieve scaling and native wrapper
cases. Use seven reps and inspect min/max before interpreting small differences.

Run the same argv with `PHPUN_ALLOC=1` or `PHPUN_VMPROF=1` separately. Allocation
bytes are cumulative requested bytes, not peak live memory. VM counters report
entries/lookups, not elapsed time percentages or native expression coverage.
CPU sampling is an additional measurement; allocation counts cannot replace it.

`cargo run --release -p phpun-core --example profile-lifecycle -- 200` measures
`Interp::new`, parsing, bootstrap including parsing, request reset, handler and
request cleanup for the explicit warm-worker endpoint. Bootstrap overlaps the
separate parse measurement. This reports the persistent-worker API; it does not
assert classic PHP request isolation or change worker defaults. The response
must be deterministic; compare its printed body against PHP independently.
