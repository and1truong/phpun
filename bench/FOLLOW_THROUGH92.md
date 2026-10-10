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

## Capture-free closure dispatch — runtime 744d302 vs ecc3184

`call_value` established a closure frame then always entered the full binder,
even when the existing scalar VM binder was applicable. Reuse that binder for
capture-free closures with all parameters supplied positionally. Captures,
named/default/missing arguments, reference parameters and hybrid bodies retain
the canonical binder. The gate is deliberately conservative.

Seven alternating profiler-off release runs, PHP exit/stdout/stderr gates:

| Workload | Before median ms | After median ms | Change |
|---|---:|---:|---:|
| Combined arrays | 579.791 | 463.100 | -20.1% |
| Objects | 472.676 | 473.552 | +0.2% |
| Captured arrows (control) | 163.018 | 167.585 | +2.8% |
| Composer | 6.343 | 6.405 | +1.0% |
| Capture-free arrows | 140.870 | 125.639 | -10.8% |

Arrays and capture-free arrows improve; the other ranges overlap and establish
no speed change. Arrays allocation requests fall 8,215,493 -> 2,769,890,
cumulative bytes 265,702,134 -> 92,714,236 and cell events 700,826 -> 40,124.
The existing CALLPROF now observes 350,351 gated callback calls. Its pre/cells
fields are zero on this entry path and phases exclude closure-frame construction;
these are VM phase diagnostics, not a complete CPU profile or wall-time budget.

Workspace tests/clippy pass, all VM fixtures match PHP including the new dispatch
probe, and 18 decomposition smoke cases match. The probe covers weak type
coercion, named/default/missing args, variadic/by-ref args, captured-local isolation,
exception args and bound receivers surviving until the last closure is released.
899 PHPT cases run **sequentially before then after**, one worker, 60-second limit:
752 pass, 130 existing fail, 3 skip, 14 unsupported, no crash/timeout and no status
changes. Suites: arrows, closures, named params, function arguments, type
declarations, GC, exceptions and backtrace. The previously failing gc_045 remains
a fail at this cutoff; this is not a claim that GC parity is complete.

Raw samples/ranges/hashes and diagnostics:
[data/92/follow-through/closure-dispatch](data/92/follow-through/closure-dispatch).
The capture-free benchmark mode was added after the runtime change; its source
revision is recorded separately in the PHPT/provenance summary.

## Plain method property reads — runtime 55d8646 vs 744d302

A `$this->literalProperty` opcode shares a positive backing-slot proof with
canonical property reads. Visible initialized slots need no expression walk or
scope bridge; hooks, magic getters, uninitialized properties and missing `$this`
retain full handlers and scope synchronization. Functions needing canonical
binding still use it; plain reads can use the existing scalar VM binder.
`__toString` retains the binder's implicit return contract, fixing bug26166.

Seven alternating unprofiled release repetitions with PHP 8.5.11 byte/exit gates;
cold CLI median [min,max] ms:

| Workload | Before | After | Change |
|---|---:|---:|---:|
| `bench/40-objects.php` | 499.817 [468.429, 510.091] | 463.964 [448.560, 497.755] | -7.2% |
| `bench/profile/objects.php norm 5000` | 104.160 [100.027, 107.153] | 99.766 [98.019, 105.267] | -4.2% |
| `bench/profile/objects.php scaled 5000` | 145.296 [139.190, 156.170] | 146.510 [138.148, 153.502] | +0.8% |
| `bench/30-arrays.php` | 474.265 [463.860, 514.397] | 475.378 [452.605, 500.729] | +0.2% |
| `examples/composer/run.php` | 6.589 [6.323, 7.351] | 6.940 [6.281, 7.142] | +5.3% |

The objects median improves 7.2%; ranges and all samples are retained. Small
changes for other cases are not treated as established wins. Separate ALLOC,
CALLPROF and VMPROF logs show the new entry path; those diagnostics do not claim
native CPU coverage or total call overhead.

Tests/clippy pass and the new oracle fixture covers inheritance/private/readonly
reads, bound closures, hooks and receiver destruction, magic getter side effects,
uninitialized/static `$this` errors and `__toString` return enforcement.
1,775 core/property/scope PHPT cases improve 1,594 -> 1,595 pass, fixing
magic_methods/bug26166, no new failures (148 existing fail). A separate 190
GC/exception/backtrace gate keeps all parent statuses at the same 60s cutoff:
116 pass, 68 existing fail, 1 skip, 5 unsupported, no crash/timeout.

Raw samples/ranges/hashes and diagnostics:
[data/92/follow-through/method-properties](data/92/follow-through/method-properties).

## Integer insert absence proof — runtime bbbd74d vs 55d8646

A working `perf cpu-clock:u` collector found **81.25%** of sampled userspace CPU
in `PhpArray::pos_of` for arrays at 100,000 elements. Both array-map result inserts
and dimension binding searched a growing table for keys that were provably new.
Use the existing append cursor's upper bound before lookup; move bind_cell's
cursor update after lookup so it can use the same proof. Negative/wrapped cursors
retain searching. Integer gaps, existing keys, tombstones, references and CoW
keep their handlers. Raw key-layout callers were inspected, including the common
sort renumber/cursor tail and clones; no new index/cache is introduced.

Seven alternating unprofiled release repetitions, PHP 8.5.11 exit/stdout/stderr
gates, cold CLI median [min,max] ms:

| Workload | Before | After | Change |
|---|---:|---:|---:|
| `bench/30-arrays.php` | 457.571 [447.887, 539.102] | 225.909 [208.967, 239.634] | -50.6% |
| `bench/profile/arrays.php map 20000` | 279.855 [268.544, 306.617] | 35.411 [34.131, 41.208] | -87.3% |
| `bench/profile/arrays.php filter 20000` | 163.538 [156.948, 187.415] | 36.006 [33.922, 47.304] | -78.0% |
| `bench/30-arrays.php 100` | 7502.984 [7175.843, 7673.136] | 1305.549 [1212.810, 1380.953] | -82.6% |
| `bench/40-objects.php` | 476.242 [450.998, 495.312] | 429.748 [415.155, 461.873] | -9.8% |
| `examples/composer/run.php` | 6.780 [6.127, 7.178] | 6.948 [6.704, 7.563] | +2.5% |

This removes quadratic integer insert search in the measured map/filter paths.
It does not optimize missing string-key search. Allocation requests are unchanged:
arrays 2,769,890, objects 6,825,045; the CPU improvement comes from avoided work,
not fewer allocations. Workspace tests/clippy and all VM oracle fixtures pass.
987 array/foreach/ArrayAccess/argument/named-parameter PHPT statuses are identical:
694 pass, 277 existing fail, 7 skip, 5 unsupported, **4 pre-existing crashes**,
no new failures/crashes. A new fixture covers sparse keys, aliases/rebinding,
unset/reinsert, CoW, map/filter, negative keys, prepend, sort cursor reset and a
large integer key. Existing crashes are not claimed fixed.

### CPU sampling restored

Linux perf 6.1.176 was extracted in the workspace with its shared libraries;
no host configuration or repository dependency was changed. Hardware cycles are
unsupported here; userspace **software cpu-clock** is available. The previous
gprofng timer-warning experiments remain rejected. Commands:

```sh
perf record -e cpu-clock:u -F 199 --call-graph dwarf,8192 -o run.data -- phpun SCRIPT ARGS
perf report --stdio --no-children --call-graph none --sort symbol -i run.data
perf script --fields comm,pid,time,event,period,ip,sym,dso -i run.data > run.stacks
python3 bench/profile/cpu-stacks.py --input run.stacks
python3 bench/profile/cpu-stacks.py --self-check
```

Before: arrays (100k), **1,520 samples, zero lost**; observed exclusive top three:
`PhpArray::pos_of` 81.25%, `Interp::vm_run` 2.24%, `Interp::vm_exec` 1.84%.
Objects (50x5000), **1,093 samples, zero lost**: `StrSearcher::new` 8.78%,
`PhpArray::pos_of` 6.40%, `cfree` 5.95%. After the bound fix, the same 100k arrays
case completes much sooner and supplies only **260 samples**: observed top three
`vm_run` 13.08%, sort callback 10.00%, `vm_exec` 8.46%; lower sample count limits
confidence in close ranks. These are sampled percentages, not wall-time speedups.

Nearest-dispatch stack buckets (weighted sample periods): before arrays
VM 55.3%, AST 43.3%, other/incomplete 1.4%; objects VM 30.6%, AST 49.0%,
other/incomplete 20.3%; after arrays VM 77.3%, AST 16.9%, other/incomplete 5.8%.
The **nearest** VM/eval/exec frame wins: an outer AST caller must not classify
its inner VM callee as AST time. VM buckets include shared runtime/builtin work;
they are not proof that all operations execute native bytecode. DWARF stack size
is capped at 8192 bytes, release inlining/stripped libraries limit attribution,
and small differences/rank order require more samples. Raw text stacks are
archived compressed, together with CPU symbol tables, sample counts, hashes and
bucket summaries. The new classifier's self-check tests overlapping caller/callee
stacks and keeps other/incomplete samples explicit.

Raw data: [data/92/follow-through/array-insert-bound](data/92/follow-through/array-insert-bound).

## Foreach body bytecode — runtime 0fe4140 vs bbbd74d

Eligible foreach bodies compile into a separate body program while the existing
array/reference/object/Iterator driver owns cursor, binding, append and unwind
semantics. Locals synchronize at iteration boundaries and remain alive until the
enclosing frame teardown, including return/error paths. There is no synthetic
function frame or new iterator implementation. Scope/unwind statements and
nonlocal jumps retain canonical body execution. Empty bodies remain canonical;
root files containing only foreach do not acquire a new cold compilation path.
Nested compiled loops preserve the enclosing loop-depth context.

The new `loop-body-entry` counter records actual entries separately from function
body, hybrid function body and root entries. It is not a CPU-time share.
Oracle fixture covers arrays and ArrayIterator, by-reference dynamic append and
last-variable aliases, try/finally+break/continue fallback, nested loops, early
return and destructor/local lifetime. Workspace fmt/tests/clippy pass; all VM
oracle fixtures plus 19 workload probes and the CPU-classifier self-check pass.

1,232 foreach/ArrayAccess/switch/try/match/generator/standard-array PHPT tests run
sequentially before/after at 30s with four workers: **858 pass, 353 existing fail,
7 skip, 5 unsupported, 7 existing crashes, 2 existing timeouts**; every status is
identical. Existing incompatibilities are not claimed fixed. Generators still
use AST fallback and native iterator/unwind expansion remains conditional in #92.

Seven alternating profiler-off release pairs, each exit/stdout/stderr gated
against PHP 8.5.11 `-n`, measured after PHPT completed:

| Workload | Before median [min,max] ms | After median [min,max] ms | Change |
|---|---:|---:|---:|
| `bench/profile/foreach.php 100000` | 63.086 [59.722, 70.210] | 55.139 [49.858, 59.711] | -12.6% |
| `bench/30-arrays.php` | 234.972 [220.588, 239.699] | 235.135 [222.396, 244.313] | +0.1% |
| `bench/40-objects.php` | 480.773 [435.760, 524.302] | 451.431 [427.999, 500.026] | -6.1% |
| `bench/profile/arrays.php keys 20000` | 68.985 [63.307, 74.091] | 69.851 [64.875, 77.439] | +1.3% |
| `examples/composer/run.php` | 8.525 [6.955, 10.498] | 10.560 [7.043, 13.480] | +23.9% |

The foreach probe improves 12.6% with separated ranges in this run. Other ranges
overlap; do not claim generic object/startup improvements from this step. The
7-pair Composer result is noisy. A **31-pair** cold check gives Composer
7.026→7.082 ms (+0.8%) and startup 5.057→5.079 ms (+0.4%), with raw samples retained.
The first timing run overlapped PHPT and is archived as a rejected speed basis.
Profiler diagnostics are separate; no claim that iterator handling is native.

## Whole follow-through versus current main dd99cac

Exact frozen current-main `dd99cac1c27910b331ff7cdeace545777ea3ea73` versus final
runtime `0fe4140814a3035396c52f654277d061ce5e29cf`, same host, seven alternating
profiler-off release reps. Every pair matches PHP 8.5.11 output, stderr and exit.
The full CLI/real-library set uses PHP `-n` plus genuine mbstring 8.5.11; focused
closure/foreach/Composer cases use `-n`. Runtime/binary/config hashes, argv, raw
samples and ranges are archived; raw `source` is the working directory's benchmark
source head, not the binary's runtime commit. Runtime provenance is explicit.

| Workload | Before median [min,max] ms | After median [min,max] ms | Change |
|---|---:|---:|---:|
| `bench/00-startup.php` | 5.502 [4.869, 6.973] | 5.296 [4.947, 7.411] | -3.8% |
| `bench/10-fib.php` | 208.359 [205.300, 227.041] | 207.090 [198.418, 219.554] | -0.6% |
| `bench/11-sieve.php` | 231.072 [222.908, 241.107] | 228.990 [219.682, 245.519] | -0.9% |
| `bench/20-strings.php` | 623.345 [608.961, 703.656] | 618.089 [607.577, 668.999] | -0.8% |
| `bench/30-arrays.php` | 1194.238 [1144.487, 1253.653] | 234.631 [216.942, 263.061] | -80.4% |
| `bench/40-objects.php` | 546.384 [477.785, 583.482] | 474.854 [418.097, 570.509] | -13.1% |
| `bench/50-regex.php` | 40.562 [37.262, 47.950] | 40.377 [38.584, 45.091] | -0.5% |
| `bench/60-json.php` | 960.846 [903.388, 1016.241] | 948.608 [923.298, 1041.432] | -1.3% |
| `bench/70-db.php` | 138.133 [124.387, 153.955] | 131.503 [127.821, 155.565] | -4.8% |
| `examples/doctrine-inflector/demo.php` | 48.734 [46.277, 55.253] | 49.516 [45.683, 53.512] | +1.6% |
| `examples/symfony-console/console.php app:greet World --yell -i 2 --no-ansi` | 92.874 [87.310, 102.285] | 94.929 [89.130, 100.501] | +2.2% |

Additional focused cases, a separate paired run:

| Workload | Before median [min,max] ms | After median [min,max] ms | Change |
|---|---:|---:|---:|
| `bench/profile/foreach.php 100000` | 63.107 [59.678, 69.641] | 50.102 [48.879, 52.655] | -20.6% |
| `bench/profile/closures.php 20000` | 244.870 [236.556, 272.306] | 155.713 [153.243, 185.799] | -36.4% |
| `bench/profile/closures.php 20000 scalar` | 222.940 [217.607, 248.562] | 121.002 [118.234, 137.621] | -45.7% |
| `examples/composer/run.php` | 6.548 [6.199, 10.726] | 6.379 [6.177, 7.675] | -2.6% |

Arrays improve **80.4%** against the current-main baseline, objects **13.1%**;
captured/scalar closure probes improve 36.4%/45.7% and foreach 20.6% in their
separate paired run. Small changes in the remaining cases overlap their ranges;
Doctrine/Symfony cold demos do not establish an application-wide win. No production
PHP-FPM/OPcache comparison is claimed. These new paired results supersede neither
the old frozen `308cc05` report nor its differently based historical gains.
Do not add successive step percentages or compare PHP startup between hosts.

Raw data, rejected/noisy runs and PHPT status lists:
[data/92/follow-through/foreach-bodies](data/92/follow-through/foreach-bodies).
