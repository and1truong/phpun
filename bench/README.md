# bench

Basic side-by-side benchmarks: phpun vs reference PHP (8.5.x).

## Run

```sh
cargo build --release
PHP=/path/to/php PHPUN=./target/release/phpun bench/run.sh [--save bench/RESULTS.md]
```

Defaults: `PHP=php`, `PHPUN=./target/release/phpun`, `TIMEOUT=300` (per run,
seconds), `BENCH_REPS=7`. Python 3 is required (also used by the HTTP driver).

`run.sh` delegates to `measure.py`. Use `--reps N`, `--bench PATH`
(repeatable), `--script-arg VALUE`, and `--php-arg=-n` to record an explicit
workload/reference configuration. For example, a longer call workload:

```sh
bench/run.sh --bench bench/10-fib.php --script-arg 32 --reps 7 --php-arg=-n
python3 bench/check-measure.py # failure/timeout/output gate self-check
```

Set `PHPUN_BUILD_INFO` to the binary's exact source commit, dirty state,
Rust version and build flags. Reports record the current checkout separately
from binary hashes; a checkout commit alone does not prove where an existing
binary was built. Disable `PHPUN_CALLPROF`, `PHPUN_ALLOC` and `PHPUN_VMPROF` for speed timings.

For coverage diagnostics, run `PHPUN_VMPROF=1 phpun script.php`. Stderr
reports the first compiler rejection per declaration (file, line, function
and construct), compile-cache lookup counts grouped by rejection reason,
and actual VM body entries, including direct cached calls. Lookup counts
are not call counts or elapsed-time shares: direct VM cache hits bypass
compiler lookup, and a bound call can consult the cache more than once.
The first rejected construct can hide later unsupported constructs. Profiling
adds overhead; compare speed only with profiling disabled.

### HTTP (concurrent server load)

```sh
PHP=/path/to/php PHPUN=./target/release/phpun REQ=400 CONC=8 WORKERS=8 \
    bench/run-http.sh [--save bench/RESULTS.md]
```

Compares four server configs on `bench/http/app*.php` (same response body;
the driver asserts a `bench-ok` marker): `php -S` single process,
`php -S` with `PHP_CLI_SERVER_WORKERS`, `phpun serve` classic (fresh
interp per request), and `phpun serve --workers N` (warm interpreter
reused per request — the script returns a `fn(array $req)` handler,
see `http/app-worker.php`). Load driver is `http/http-load.py` (python3
stdlib threads; one connection per request, matching how these dev
servers respond). `bench/http/` is intentionally outside `run.sh`'s
`bench/[0-9]*.php` glob.

## How it works

- Each `bench/*.php` script does a fixed workload and prints one deterministic
  `RESULT <checksum>` line. Workload size is set in the script (some accept an
  override via `$argv[1]`/`$argv[2]`).
- The runner reports **median wall-clock ms** plus min/max across all reps,
  alternating which runtime runs first. It reports `phpun / php` per bench
  plus a geometric mean and the number of valid benchmarks.
- Times include process startup, parse, and execution. `00-startup` isolates
  startup+parse alone.
- **Correctness gate on every repetition:** zero exit status, no timeout,
  deterministic stdout/stderr, and byte-identical stdout/stderr between the
  runtimes. Invalid benches show no speedup, are excluded from the geometric
  mean and make the runner exit nonzero, including when a later rep fails.
- Wall-clock on a shared machine is noisy; treat ratios as orders of magnitude,
  not precise numbers. For tighter numbers, run on an idle box, raise the
  rep counts and choose workloads long enough that startup does not dominate.
  These timings remain cold CLI measurements; do not subtract the startup
  time of a different script and label the result steady-state execution.

## Latest

See `RESULTS.md` (regenerate with `--save`).

## Adding a bench

Drop a `NN-name.php` file that does a self-contained deterministic workload
and prints `RESULT <checksum>`. Keep total phpun runtime in the 0.5–20s band.
