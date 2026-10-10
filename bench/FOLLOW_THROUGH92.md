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
