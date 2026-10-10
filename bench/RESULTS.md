# bench results

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
