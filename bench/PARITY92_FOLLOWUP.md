# Parity follow-up: direct scalar arguments

Runtime baseline `8688c2a` (dynamic builtin semantic fixes, PR #189) to
candidate `db51239`. Rust 1.99.0, release/LTO/codegen-units=1. Same host,
7 alternating pairs, profilers off, every exit/stdout/stderr matched PHP 8.5.11
`-n`. Binary hashes, samples, host, argv and build metadata: [raw data](data/92/parity/).

| Workload | Before → after median ms | After/before |
|---|---:|---:|
| fib default | 223.024 → 195.910 | 0.878 |
| typed fib(29), one iteration | 873.876 → 746.132 | 0.854 |
| untyped fib(29), one iteration | 884.529 → 784.123 | 0.886 |
| strings | 670.470 → 676.437 | 1.009 |
| arrays | 228.843 → 225.119 | 0.984 |
| objects | 453.198 → 437.419 | 0.965 |
| JSON | 971.871 → 941.488 | 0.969 |

The benefit is specific to direct calls with read-only scalar parameters and
already-proven type checks. Missing/default/named/extra/coercing/by-ref/hybrid
calls retain the cell binder. A cold by-reference operation materializes cells
before canonical SEND; introspection/backtrace can observe live argument values.
No general application or PHP-parity speedup is claimed.

The initial 7-pair application probe had +7.8% wall time with overlapping ranges.
Because that remained uncertain, 31 further alternating pairs were run: app CLI
42.480 → 42.320 ms (0.996), Composer 7.185 → 7.106 ms (0.989), ranges overlap.
Both datasets are retained. These cold demos do not establish production performance.

Allocation diagnostics are **requests/counters, not a count of host mallocs**.
Pooling already kept the cell count low: typed fib(29) cell counters 41 → 13;
total reported requests remain ~1.689M and bytes ~57.5M. The optimization removes
per-call Rc/RefCell manipulation; it does not remove arena accounting/trace shells.

Validation: workspace tests, fmt, clippy; new argument-observation/fallback test;
21 VM fixtures and 19 workload oracle checks. 709 binding PHPT: 636 pass,
62 existing fail, 2 skip, 9 unsupported, zero crashes/timeouts; no status changes.

## Trace and dispatch decision (#184 / #188)

Candidate fib(29), 10 iterations: perf6.1.176 `cpu-clock:u`199Hz, DWARF8192,
1,631 samples, zero lost. `call_site_frame` appears in about 9.6% of weighted
stacks, inclusive of helper/libc work. Its exclusive symbol samples are 2.51%;
`vm_exec` 37.22%, `vm_run` 10.79%. These are function samples, not proof of pure
opcode dispatch shares. Unresolved libc addresses and stack-depth limits remain.

Trace arguments are already deferred. The remaining context cost is measurable,
but these observations do not justify combining a full trace rewrite with the
argument ABI PR. Keep #184 as a measured, separate follow-up; prioritize shared
string/constructor costs. Register/native-tier selection (#188) still needs
instruction-level attribution and a separate prototype after shared-runtime fixes.

Strings baseline after ABI: 639 CPU samples, zero lost. Repeated libc comparisons
occur below `str_replace_one`/`breplace`; the old code scans for count and output
separately, materializes subject bytes and copies no-match results. #185 targets
that shared search/replace path first. Raw stacks/flat reports are preserved.

## Shared replacement pass (#185)

Frozen ABI runtime db51239 → replacement runtime 54da20e, seven alternating
release pairs, every rep PHP exit/stdout/stderr gate: strings 650.254→440.361 ms
(0.677×); replace phase 216.959→121.042 (0.558×). Concat 14.530→14.726, objects
454.669→449.984, JSON904.711→900.463, app39.185→39.071: small differences
with overlapping ranges; no gain claimed there. New replacement check covers
binary bytes, case folding, cascading searches, short replacement arrays, keys,
count/subject aliasing, stringable objects and throwing conversion. 59 relevant
string/search PHPT:26pass/33existingfail, all statuses unchanged, no crashes/timeouts.

Replacement now borrows input, returns shared string for no matches, and produces
output/count in one scan per search term. Case-insensitive search still has its
existing byte matcher. Full concat/capacity storage remains a separate decision.

Review arity fix49a3fff is inherited by this branch after frozen step timings;
reported runtime54da20e is not relabelled as the later branch head.

## Shared byte-search filter (#185)

Replacement runtime54da20e → search runtime9961ea5 (includes review arity fix49a3fff),
seven alternating PHP-gated release pairs: strings451.602→263.136ms (0.583×);
replace phase130.805→37.681 (0.288×). Objects435.964→446.988, JSON972.486→969.060,
app42.379→43.353 have overlapping ranges; no gain claimed. These are a separate
paired run, not cumulative percentages or a new PHP ratio measurement.

The shared byte search now filters candidate positions by the first byte before
checking the full needle. Empty needle, binary bytes and offset behavior are
unchanged. Worst-case O(n*m) remains; a two-way search requires evidence on
adversarial/repeated-prefix workloads. No new dependency or builtin-only shortcut.

Validation: exhaustive small binary haystack/needle/all-offset check, workspace
tests/fmt/clippy/release, 24 VM/standalone oracle probes, 19 workload checks;
59 relevant PHPT unchanged (26pass/33existingfail), zero crashes/timeouts.
