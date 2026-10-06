# bench

Basic side-by-side benchmarks: phpun vs reference PHP (8.5.x).

## Run

```sh
cargo build --release
PHP=/path/to/php PHPUN=./target/release/phpun bench/run.sh [--save bench/RESULTS.md]
```

Defaults: `PHP=php`, `PHPUN=./target/release/phpun`, `TIMEOUT=300` (per run, seconds).

## How it works

- Each `bench/*.php` script does a fixed workload and prints one deterministic
  `RESULT <checksum>` line. Workload size is set in the script (some accept an
  override via `$argv[1]`/`$argv[2]`).
- The runner times each script under both runtimes — **min wall-clock ms** over
  an adaptive number of reps (5 reps under 150ms, 3 under 1s, 2 above) — and
  reports `phpun / php` per bench plus a geometric mean.
- Times include process startup, parse, and execution. `00-startup` isolates
  startup+parse alone.
- **Correctness gate:** the runner byte-compares stdout from both runtimes and
  marks `MISMATCH` when they differ. A mismatched bench reports its times but
  is excluded from the geometric mean — don't compare speeds on divergent
  semantics.
- Wall-clock on a shared machine is noisy; treat ratios as orders of magnitude,
  not precise numbers. For tighter numbers, run on an idle box and raise the
  rep counts.

## Latest

See `RESULTS.md` (regenerate with `--save`).

## Adding a bench

Drop a `NN-name.php` file that does a self-contained deterministic workload
and prints `RESULT <checksum>`. Keep total phpun runtime in the 0.5–20s band.
