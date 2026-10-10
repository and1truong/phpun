# bench results

## Latest performance follow-through — 2026-10-10

Paired **current main dd99cac → runtime 0fe4140** cold CLI comparison, seven
alternating profiler-off release pairs, Rust 1.99.0/LTO=true/codegen-units=1.
Every measured exit/stdout/stderr matches PHP 8.5.11 (`-n`; genuine mbstring 8.5.11
loaded for the full CLI/library oracle). These are before/after phpun timings,
not updated phpun/PHP ratios. Exact binary/runtime/config hashes, argv and raw
samples: [FOLLOW_THROUGH92.md](FOLLOW_THROUGH92.md) and
[data/92/follow-through/foreach-bodies](data/92/follow-through/foreach-bodies).

| Bench | Current main median [min,max] ms | Candidate median [min,max] ms | Change |
|---|---:|---:|---:|
| 00-startup | 5.502 [4.869,6.973] | 5.296 [4.947,7.411] | -3.8% |
| 10-fib | 208.359 [205.300,227.041] | 207.090 [198.418,219.554] | -0.6% |
| 11-sieve | 231.072 [222.908,241.107] | 228.990 [219.682,245.519] | -0.9% |
| 20-strings | 623.345 [608.961,703.656] | 618.089 [607.577,668.999] | -0.8% |
| 30-arrays | 1194.238 [1144.487,1253.653] | 234.631 [216.942,263.061] | -80.4% |
| 40-objects | 546.384 [477.785,583.482] | 474.854 [418.097,570.509] | -13.1% |
| 50-regex | 40.562 [37.262,47.950] | 40.377 [38.584,45.091] | -0.5% |
| 60-json | 960.846 [903.388,1016.241] | 948.608 [923.298,1041.432] | -1.3% |
| 70-db | 138.133 [124.387,153.955] | 131.503 [127.821,155.565] | -4.8% |

Arrays improve 80.4%, objects 13.1% in this paired run. Smaller changes have
overlapping ranges; the real-library demos do not establish a generic application
win. PHPT gates and closure/foreach probes are documented in the linked report.

## Historical PHP-ratio baseline — frozen runtime 308cc05

The following frozen report retains its original binary, oracle and samples.
Do not combine its PHP ratios with the newer before/after comparison.

- date: 2026-10-10T02:14:18.762533+00:00
- source checkout: 8a44d49be37943daea27d83ef991cf0f9c23e876; dirty: False (not binary build provenance)
- phpun: phpun 0.0.1 (php compat target: 8.5)
- phpun binary: /workspace/validation/308cc05/phpun; sha256: 32ceb49586648842817a23ddbb30e76da01e38d53c1b51c63187dec13ef70419
- php: 8.5.11; command: /workspace/php85 -n
- php binary sha256: 32fa863f672cfe0048b95d08fc1593f78bf97971823207a338f3f82d730e564f
- php config: {"ini":false,"scanned":false,"extensions":["Core","date","pcre","sqlite3","hash","json","lexbor","Zend OPcache","uri","SPL","PDO","pdo_sqlite","random","Reflection","tokenizer","standard"],"opcache.enable_cli":"0","opcache.jit":"disable","opcache.jit_buffer_size":"64M"}
- host: Linux-6.18.44-x86_64-with-glibc2.41; 5 logical CPUs
- build provenance: 308cc05345fb2a00991bebf5620769e3361eeebf; clean release build; Rust 1.99.0; LTO=true codegen-units=1
- metric: median wall-clock ms; 7 reps; alternating runtime order; timeout 300s
- scope: cold process startup + parse + execution; min/max show observed dispersion
- gates: zero exit, deterministic stdout and stderr, byte-identical to reference on every rep
- ratio = phpun / php; below 1 means phpun faster; profiler disabled

| bench | PHP median [min,max] ms | phpun median [min,max] ms | ratio | gate |
|---|---:|---:|---:|---|
<!-- commands: /workspace/php85 -n bench/00-startup.php ; /workspace/validation/308cc05/phpun bench/00-startup.php -->
| 00-startup | 3.713 [3.297,4.387] | 4.230 [4.001,4.602] | 1.14× | ok |
<!-- commands: /workspace/php85 -n bench/10-fib.php ; /workspace/validation/308cc05/phpun bench/10-fib.php -->
| 10-fib | 11.094 [10.186,12.130] | 201.370 [195.036,206.517] | 18.15× | ok |
<!-- commands: /workspace/php85 -n bench/11-sieve.php ; /workspace/validation/308cc05/phpun bench/11-sieve.php -->
| 11-sieve | 7.236 [5.926,7.746] | 235.491 [226.928,245.025] | 32.55× | ok |
<!-- commands: /workspace/php85 -n bench/20-strings.php ; /workspace/validation/308cc05/phpun bench/20-strings.php -->
| 20-strings | 13.459 [12.902,14.011] | 598.071 [593.620,645.626] | 44.44× | ok |
<!-- commands: /workspace/php85 -n bench/30-arrays.php ; /workspace/validation/308cc05/phpun bench/30-arrays.php -->
| 30-arrays | 18.053 [17.185,37.086] | 1125.954 [1088.541,1174.942] | 62.37× | ok |
<!-- commands: /workspace/php85 -n bench/40-objects.php ; /workspace/validation/308cc05/phpun bench/40-objects.php -->
| 40-objects | 8.877 [7.879,9.334] | 484.507 [471.774,514.516] | 54.58× | ok |
<!-- commands: /workspace/php85 -n bench/50-regex.php ; /workspace/validation/308cc05/phpun bench/50-regex.php -->
| 50-regex | 7.202 [6.273,7.822] | 37.826 [36.632,38.711] | 5.25× | ok |
<!-- commands: /workspace/php85 -n bench/60-json.php ; /workspace/validation/308cc05/phpun bench/60-json.php -->
| 60-json | 169.199 [146.693,209.790] | 876.671 [863.807,964.245] | 5.18× | ok |
<!-- commands: /workspace/php85 -n bench/70-db.php ; /workspace/validation/308cc05/phpun bench/70-db.php -->
| 70-db | 31.435 [28.862,34.326] | 134.342 [124.903,141.309] | 4.27× | ok |

valid benchmarks: 9/9
geometric mean slowdown (valid benchmarks only): 13.16×
