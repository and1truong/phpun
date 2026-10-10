# Issue #92: verified stack results (2026-10-10)

The stack fixes VM semantics, adds hybrid VM coverage, and reduces selected allocation/copy costs. It does not make every workload faster. The nine-benchmark geometric mean of candidate/baseline median wall times is **0.8604** against the initial snapshot and **0.9282** against the integrated-main snapshot. These are same-host comparisons, with all output gates passing.

## Provenance and method

- Initial snapshot: `7468797e0e80664fb661e8b2aa5aaef776ad57b7`.
- Integrated main reference: `ff93d67ee161e28c92e64963e8dc4b1c41a5f090` (first ten stack PRs plus main trace pooling). This is a pinned comparison, not a claim about future main HEAD.
- Candidate runtime: `308cc05345fb2a00991bebf5620769e3361eeebf` (stack part 23).
- Each binary was built from a clean checkout with Rust 1.99.0, release, LTO=true, codegen-units=1. Benchmark helper checkout: `8a44d49be37943daea27d83ef991cf0f9c23e876`, clean during timing.
- Host: Linux 6.18.44 x86_64, glibc 2.41, five logical CPUs. No builds or PHPT jobs ran during final speed measurements.
- PHP oracle: frozen PHP 8.5.11, `-n`, no loaded/scanned ini, OPcache CLI off, JIT disabled. PHP configuration and binary hashes are in [RESULTS.md](RESULTS.md). The lower PHP startup cost than historical reports changes ratios; it is not a runtime speedup.
- Seven alternating before/after reps per case. Metric is cold startup + parse + execution; min/max are observed dispersion, not confidence intervals. Every measured repetition must exit zero and match the PHP stdout/stderr bytes. Profilers are off for speed measurements.
- Raw samples, command argv, binary hashes and build provenance: [initial comparison](data/92/paired-initial-final.json), [main comparison](data/92/paired-main-final.json), [phase comparison](data/92/paired-phases-final.json). Nine-benchmark inputs are default argv; the phase JSON records every size.
- Phase/real-application PHP oracle additionally loads a PHP 8.5.11 shared mbstring module. Source/config/module hash: [extra provenance](data/92/extra-provenance.json). No polyfill or failed PHP run is used as a performance reference.

## Same-host nine-benchmark comparison

Each cell is median [min,max] milliseconds. A negative change is less wall time.

| Workload | Initial | Candidate in initial pair | Change | Integrated main | Candidate in main pair | Change |
|---|---:|---:|---:|---:|---:|---:|
| 00-startup | 4.251 [3.869,4.643] | 4.050 [3.978,4.421] | -4.7% | 4.207 [3.962,5.260] | 4.317 [4.027,4.744] | +2.6% |
| 10-fib | 275.772 [263.191,292.563] | 209.192 [196.629,219.228] | -24.1% | 251.243 [237.346,270.613] | 200.523 [193.575,220.952] | -20.2% |
| 11-sieve | 274.448 [271.206,310.318] | 232.456 [226.836,267.541] | -15.3% | 270.579 [253.670,311.575] | 225.700 [217.025,247.222] | -16.6% |
| 20-strings | 856.917 [772.587,886.862] | 605.116 [589.466,634.427] | -29.4% | 829.703 [816.675,969.876] | 622.390 [599.814,691.581] | -25.0% |
| 30-arrays | 1073.667 [1046.135,1178.935] | 1118.426 [1051.075,1186.721] | +4.2% | 1077.290 [1031.634,1137.273] | 1134.273 [1074.758,1194.961] | +5.3% |
| 40-objects | 832.142 [790.352,878.291] | 510.838 [479.499,601.728] | -38.6% | 511.655 [500.998,543.682] | 483.245 [465.693,490.165] | -5.6% |
| 50-regex | 40.362 [37.715,43.931] | 39.662 [36.575,44.054] | -1.7% | 39.421 [35.806,46.703] | 38.561 [36.096,50.476] | -2.2% |
| 60-json | 915.847 [872.025,958.353] | 874.133 [856.506,909.885] | -4.6% | 892.563 [872.969,958.036] | 887.630 [866.305,956.629] | -0.6% |
| 70-db | 131.923 [128.435,183.887] | 131.486 [127.085,136.147] | -0.3% | 121.253 [119.675,130.026] | 125.035 [119.431,130.044] | +3.1% |

Fib, sieve, strings and objects improve in these comparisons. The combined arrays workload is **4.2% slower than initial** and **5.3% slower than integrated main**, despite fewer allocations. Do not treat VM entry or allocation reduction as proof of a speedup. Regex/JSON/database and startup changes are small and have overlapping ranges; avoid strong claims from these samples.

## Decomposition, scaling and applications

Phase probes include preparation and cold startup. They cannot be summed or subtracted to produce isolated execution costs. These compare integrated main with candidate.

| argv | Main median [min,max] ms | Candidate median [min,max] ms | Change |
|---|---:|---:|---:|
| `bench/profile/arrays.php append 20000` | 151.875 [142.160,165.614] | 150.988 [141.278,161.129] | -0.6% |
| `bench/profile/arrays.php map 20000` | 316.461 [306.259,349.382] | 307.901 [301.340,315.487] | -2.7% |
| `bench/profile/arrays.php usort 20000` | 849.090 [836.246,887.223] | 818.588 [760.484,881.908] | -3.6% |
| `bench/profile/arrays.php filter 20000` | 188.930 [182.337,229.303] | 189.003 [180.580,203.780] | +0.0% |
| `bench/profile/arrays.php keys 20000` | 178.890 [175.179,188.299] | 179.074 [176.494,218.578] | +0.1% |
| `bench/profile/strings.php repeat 400` | 6.814 [6.223,7.793] | 6.559 [5.773,8.557] | -3.7% |
| `bench/profile/strings.php replace 400` | 208.762 [201.539,257.061] | 206.862 [189.753,220.362] | -0.9% |
| `bench/profile/strings.php slice 400` | 7.086 [6.808,7.799] | 6.279 [5.920,7.049] | -11.4% |
| `bench/profile/strings.php concat 400` | 4.983 [4.750,7.114] | 5.198 [4.808,5.380] | +4.3% |
| `bench/profile/objects.php ctor 5000` | 71.301 [67.559,73.003] | 69.421 [68.674,72.463] | -2.6% |
| `bench/profile/objects.php norm 5000` | 103.843 [103.245,109.628] | 103.393 [101.491,108.543] | -0.4% |
| `bench/profile/objects.php scaled 5000` | 136.850 [136.355,146.582] | 141.463 [138.132,147.398] | +3.4% |
| `bench/profile/fib.php typed 26 1` | 251.813 [245.612,269.283] | 206.608 [197.200,215.826] | -18.0% |
| `bench/profile/fib.php untyped 26 1` | 253.885 [250.806,279.839] | 202.318 [196.355,219.632] | -20.3% |
| `bench/profile/fib.php typed 29 2` | 2084.444 [2009.021,2182.676] | 1717.658 [1663.714,1752.524] | -17.6% |
| `bench/profile/fib.php untyped 29 2` | 2024.069 [1982.822,2122.074] | 1618.155 [1583.925,1653.754] | -20.1% |
| `bench/11-sieve.php 1 10000` | 29.383 [27.768,35.693] | 25.810 [24.236,30.161] | -12.2% |
| `bench/11-sieve.php 1 20000` | 55.375 [52.873,59.644] | 47.864 [47.163,78.188] | -13.6% |
| `bench/11-sieve.php 1 40000` | 108.143 [104.877,115.482] | 97.152 [89.905,103.530] | -10.2% |
| `bench/11-sieve.php 1 80000` | 219.648 [211.833,227.330] | 186.783 [176.347,211.123] | -15.0% |
| `bench/50-regex.php 1` | 5.523 [5.365,11.006] | 5.776 [5.443,6.272] | +4.6% |
| `bench/60-json.php 1` | 395.420 [388.667,432.700] | 393.251 [377.499,422.522] | -0.5% |
| `bench/70-db.php 1` | 25.699 [24.196,27.235] | 26.229 [24.025,30.160] | +2.1% |
| `bench/app/cli.php 100 1` | 7.221 [6.765,7.839] | 7.082 [6.613,8.371] | -1.9% |
| `bench/app/cli.php 100 20` | 44.765 [42.204,56.438] | 44.564 [43.483,48.010] | -0.4% |
| `examples/composer/run.php` | 6.246 [5.884,7.358] | 7.497 [6.360,8.297] | +20.0% |
| `examples/doctrine-inflector/demo.php` | 45.577 [42.350,52.774] | 45.225 [42.746,51.067] | -0.8% |
| `examples/symfony-console/console.php app:greet World --yell -i 2 --no-ansi` | 83.491 [80.725,94.107] | 88.662 [82.158,100.854] | +6.2% |

Typed/untyped fib(29), two iterations, improves about 18–20%, so the gain also appears in longer workloads. Sieve scaling is measured at 10k/20k/40k/80k rather than inferred from one input. All 28 cases pass the byte/exit oracle.

Independent array phases do not reproduce the combined workload regression. Sorting has the largest elapsed phase among the array probes, but input preparation and differing live scopes prevent attributing the whole regression to sorting. A next optimization should bisect the combined workload and its scope/materialization costs.

Composer autoload cold timing increases 6.246 → 7.497 ms; Symfony Console increases 83.491 → 88.662 ms; Doctrine Inflector is approximately unchanged. These small demos establish real library compatibility and expose cold-cost tradeoffs; they do not predict production framework throughput.

## Allocation and VM diagnostics

Runs below are separate from speed measurements. Allocation bytes are cumulative requested bytes, not peak live memory. Site events are not necessarily actual allocations.

| Workload | Initial allocations | Main allocations | Candidate allocations | Initial requested bytes | Candidate requested bytes |
|---|---:|---:|---:|---:|---:|
| 00-startup | 19,956 | 20,417 | 20,417 | 3,634,538 | 3,495,868 |
| 10-fib | 4,341,657 | 1,985,281 | 413,992 | 93,747,009 | 16,230,731 |
| 11-sieve | 3,217,404 | 3,217,865 | 1,858,627 | 96,901,137 | 31,913,011 |
| 20-strings | 3,098,183 | 3,098,659 | 2,457,013 | 10,059,963,421 | 2,843,897,395 |
| 30-arrays | 21,975,321 | 19,523,337 | 19,033,257 | 1,278,010,928 | 1,165,448,433 |
| 40-objects | 8,996,234 | 7,021,784 | 6,971,829 | 415,525,253 | 167,993,383 |
| 50-regex | 318,150 | 318,617 | 318,248 | 29,164,610 | 27,969,591 |
| 60-json | 15,098,463 | 15,098,930 | 15,095,712 | 512,592,700 | 498,703,281 |
| 70-db | 1,231,168 | 1,231,635 | 1,190,849 | 40,671,686 | 40,128,925 |

Verified changes address three concrete costs: borrowed read-only string inputs remove large source copies; stable method declarations remove repeated declaration/cache construction; argument-cell and arena-token pools reuse storage only after escaping aliases and charges are gone. In the whole strings workload requested bytes fall by about 7.2 GB; fib allocations fall from 4.34 million to 0.414 million. The before/after wall times above cover the whole stack, not isolated attribution to one PR.

Final VM diagnostics: fib has 392,835 body entries; arrays 350,351; objects 50,000, including 25,000 hybrid bodies. Sieve, strings, arrays, objects, regex, JSON and database each have one top-level VM entry. These are counts, not CPU time shares. Array/property/call/iterator operations in hybrid bodies still use canonical AST handlers. Native coverage must be assessed separately. Raw [allocation data](data/92/profiles/allocations.json) and per-workload VM logs are included.

Call-profiler fib(26) counts 392,835 calls. Values below normalize accumulated ns by that count; `exec` is inclusive of nested calls and cannot be added to other phases. Clock reads/counters perturb timing; these numbers are diagnostic only.

| ns/call | Main | Candidate |
|---|---:|---:|
| pre | 80.9 | 78.5 |
| cells | 190.6 | 59.6 |
| site | 89.3 | 83.8 |
| bind | 41.6 | 43.0 |
| exec | 16688.1 | 14688.0 |
| post | 140.3 | 161.8 |

CPU sampling was attempted using gprofng 2.44 but rejected: the collector reported **"Collection interval timer period was changed (1000 -> 0); profile data may be unreliable"**; a second default-interval attempt also warned. Consequently this report makes no sampled CPU percentage or verified CPU top-three claim. A working collector/host is required before that gate is complete. Allocation and phase data do not substitute for CPU sampling.

## HTTP and lifecycle

The [HTTP report](HTTP_RESULTS.md) compares four dev-server configurations using the same load policy, 10,000 requests per rep, three reps, concurrency eight, rotating order, reference status/Content-Type/full body, readiness/warmup checks and owned-process-group cleanup. PHP WORKERS counts the serving parent plus children; phpun warm WORKERS counts handler threads. It is not a PHP-FPM/OPcache production comparison. The Python fresh-connection load driver may limit reported throughput.

Persistent-worker lifecycle, 200 iterations, release build at candidate commit; full printed body matches PHP independently:

```text
Interp::new: 1884.507 us/op (200 reps)
parse: 108.833 us/op (200 reps)
bootstrap_including_parse: 98.992 us/op (200 reps)
reset_request: 0.041 us/op (200 reps)
handler: 29.942 us/op (200 reps)
end_request: 0.094 us/op (200 reps)
layout bytes: Value=24 Cell=8 TraceFrame=160
response: bench-ok:{"id":65735,"name":"alice","greeting":"hello ALICE","tags":["t1","t2","t3","t4"],"ts":"2026-10-06T00:00:00Z"}
```

Bootstrap includes parsing and overlaps the separate parse timing; do not add them. Request reset/end timings apply to this explicit persistent-handler API, not general classic-request isolation. Measurements suggest initialization is material in the fresh-interpreter endpoint; they do not justify changing default worker semantics.

## Regression gates

Workspace tests, fmt, clippy (`--workspace --all-targets -- -D warnings`), all VM oracle fixtures, benchmark/HTTP/VM diagnostic self-checks, and 18 small workload/application oracle checks pass on the candidate. PHPT uses official PHP 8.5.11 tests and the pinned PHP oracle. All 3,841 statuses match the pinned baseline: 2,891 pass, 830 fail, 68 skip, 44 unsupported, seven crash and one timeout. These are existing failures, not 3,841 passing tests. Detailed final summaries and status differences are in [validation.json](data/92/validation.json).

Core suites (1,467): `tests/lang` and `Zend/tests/{functions,function_arguments,type_declarations,closures,classes,traits,magic_methods,objects,return_types,typehints}`. Additional coverage/string suites (2,184): `Zend/tests/{ArrayAccess,namespaces,arrow_functions,foreach,match,named_params,first_class_callable,lsb,property_hooks,asymmetric_visibility}`, `ext/standard/tests/{array,strings}`. Lifetime suites (190): `Zend/tests/{gc,exceptions,backtrace}`. These groups are disjoint.

PHPT runner commands use `phpun phpt <suite paths> --sut <pinned binary> --php <PHP8.5.11> --json <report>`, timeout 10s/core and 30s/additional/lifetime. Core/additional use eight jobs; final lifetime gate is serialized because simultaneous GC-heavy tests and builds introduced two extra timeouts. The serialized rerun restores every pinned-baseline status; the raw concurrent result and isolated rerun are documented in validation.json. Existing failures/crashes/timeouts are retained, not counted as successes.

## Remaining conditional work

- Restore reliable CPU sampling and bisect the combined arrays regression; verify cold application costs before more coverage changes.
- Native array/property operations and packed/mixed storage changes require measured benefit and reference/CoW/destructor probes; hybrid bridges do not complete those redesigns.
- Generator compilation remains AST fallback. JIT/native compilation is not committed by this stack.
- Production HTTP evaluation requires PHP-FPM + OPcache and representative applications. Default worker changes require separate request-isolation tests.

No merges or default worker changes are performed by this stack. Issue #92 remains the canonical tracker; all progress and dependencies belong in its body.
