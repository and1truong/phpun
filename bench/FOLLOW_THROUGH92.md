# Performance follow-through for issue 92

## Arrow lexical imports — runtime ecc3184 vs main dd99cac

Arrow functions previously retained every caller variable, then copied those
captures into every callback frame. PHP imports lexical variables used in the
expression, excluding parameters, `$this`, and auto-globals. The parser now
computes that import list once, including nested closures and interpolation;
creation silently skips undefined imports until their first read.

Seven alternating repetitions on the same host, release Rust 1.99.0 with
LTO/codegen-units=1, all profilers off. Every measured exit/stdout/stderr matches
PHP 8.5.11 `-n`. Values are cold CLI median [min,max] milliseconds.

| Workload | Before | After | Change |
|---|---:|---:|---:|
| `bench/30-arrays.php` | 1142.623 [1083.243, 1223.841] | 580.128 [557.844, 664.587] | -49.2% |
| `bench/40-objects.php` | 488.335 [459.206, 525.912] | 483.871 [465.231, 548.834] | -0.9% |
| `bench/profile/closures.php 20000` | 258.979 [240.448, 281.438] | 164.303 [156.624, 178.586] | -36.6% |
| `examples/composer/run.php` | 6.974 [6.327, 7.412] | 7.120 [6.475, 8.433] | +2.1% |

The combined arrays improvement is substantial and its ranges are separated.
Objects and Composer ranges overlap; this change establishes no improvement
for those cases. These measurements compare the stated current builds, not the
older frozen results in ROADMAP92_RESULTS.md.

Separate allocation runs: arrays requested 19,036,460 -> 8,215,493 allocations
and 1,165,960,199 -> 265,702,134 cumulative bytes; diagnostic cell events fell
5,605,781 -> 700,826. This confirms removal of capture work from each callback.
Counters describe requested allocations, not peak live memory. `PHPUN_CALLPROF`
currently prints no phases for these bound closure calls; do not substitute an
empty profile or VM entry counts for CPU samples. That instrumentation/dispatch
gap remains follow-up work.

Workspace tests and clippy pass. A new oracle fixture covers unused-object
destruction, parameter shadowing, nested arrows and explicit by-reference
closures, dynamic names, interpolation, match/property reads, auto-global
updates, missing imports and assignment isolation. 1,467 core PHPT statuses
match current main (1,341 pass, 97 pre-existing fail, 14 skip, 15 unsupported).
144 closure/arrow PHPTs improve from 120 to 122 pass: arrow_functions/003 and 005
are fixed, with no new failures. The two groups overlap and are not additive.

Raw samples, binary SHA-256 hashes, build metadata, diagnostics and PHPT
summaries: [data/92/follow-through/arrow](data/92/follow-through/arrow).
