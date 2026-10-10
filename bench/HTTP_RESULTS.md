# HTTP benchmark results

- date: 2026-10-10T02:19:35.316590+00:00
- source checkout: 8a44d49be37943daea27d83ef991cf0f9c23e876; dirty: False
- binary build provenance: 308cc05345fb2a00991bebf5620769e3361eeebf; clean release build; Rust 1.99.0; LTO=true codegen-units=1
- phpun: /workspace/validation/308cc05/phpun; sha256: 32ceb49586648842817a23ddbb30e76da01e38d53c1b51c63187dec13ef70419
- PHP: 8.5.11; command: /workspace/php85 -n; sha256: 32fa863f672cfe0048b95d08fc1593f78bf97971823207a338f3f82d730e564f
- PHP config: [false,["Core","date","pcre","sqlite3","hash","json","lexbor","Zend OPcache","uri","SPL","PDO","pdo_sqlite","random","Reflection","tokenizer","standard"],"0","disable"]
- host: Linux-6.18.44-x86_64-with-glibc2.41; 5 logical CPUs
- load: 10000 requests/rep, concurrency 8, 3 reps; fresh connection/request; GET /?name=bench
- lifecycle: fresh interpreters and warm persistent handlers reported separately
- PHP worker policy: WORKERS-1 forked children plus the serving parent; warm phpun has WORKERS handler threads
- gate: every response matches reference status, Content-Type and full body, including warmup; invalid reps have no throughput result
- metric: median throughput [min,max]; median per-rep latency percentiles; readiness/startup and warmup excluded
- scope: dev servers only; results do not represent PHP-FPM + OPcache

<!-- rep 1, php -S (1 process): /workspace/php85 -n -S 127.0.0.1:18301 -t bench/http bench/http/app.php -->
- oracle body sha256: 41e9ef0b49012b495c6e0ea12fb4707751e3455c1ee89b70e654ac4b6ec05b81
<!-- rep 1, php -S (8 total processes): /workspace/php85 -n -S 127.0.0.1:18302 -t bench/http bench/http/app.php -->
<!-- rep 1, phpun serve (fresh interpreter/request): /workspace/validation/308cc05/phpun serve bench/http/app.php --port 18303 -->
<!-- rep 1, phpun serve (8 warm workers): /workspace/validation/308cc05/phpun serve bench/http/app-worker.php --port 18304 --workers 8 -->
<!-- rep 2, php -S (8 total processes): /workspace/php85 -n -S 127.0.0.1:18306 -t bench/http bench/http/app.php -->
<!-- rep 2, phpun serve (fresh interpreter/request): /workspace/validation/308cc05/phpun serve bench/http/app.php --port 18307 -->
<!-- rep 2, phpun serve (8 warm workers): /workspace/validation/308cc05/phpun serve bench/http/app-worker.php --port 18308 --workers 8 -->
<!-- rep 2, php -S (1 process): /workspace/php85 -n -S 127.0.0.1:18305 -t bench/http bench/http/app.php -->
<!-- rep 3, phpun serve (fresh interpreter/request): /workspace/validation/308cc05/phpun serve bench/http/app.php --port 18311 -->
<!-- rep 3, phpun serve (8 warm workers): /workspace/validation/308cc05/phpun serve bench/http/app-worker.php --port 18312 --workers 8 -->
<!-- rep 3, php -S (1 process): /workspace/php85 -n -S 127.0.0.1:18309 -t bench/http bench/http/app.php -->
<!-- rep 3, php -S (8 total processes): /workspace/php85 -n -S 127.0.0.1:18310 -t bench/http bench/http/app.php -->

| server | req/s median [min,max] | p50 ms | p95 ms | p99 ms | gate |
|---|---:|---:|---:|---:|---|
| php -S (1 process) | 4080.4 [4053.8,4390.5] | 1.761 | 3.409 | 4.534 | ok |
| php -S (8 total processes) | 4019.4 [3860.9,4055.1] | 1.828 | 3.444 | 4.479 | ok |
| phpun serve (fresh interpreter/request) | 1120.6 [1100.4,1155.4] | 6.134 | 16.094 | 22.405 | ok |
| phpun serve (8 warm workers) | 4253.6 [4005.1,4415.4] | 1.691 | 3.202 | 4.199 | ok |
- php -S (1 process) measured wall seconds per rep: 2.277631, 2.450739, 2.466811
- php -S (8 total processes) measured wall seconds per rep: 2.466056, 2.487923, 2.590062
- phpun serve (fresh interpreter/request) measured wall seconds per rep: 8.923956, 8.655197, 9.087949
- phpun serve (8 warm workers) measured wall seconds per rep: 2.350923, 2.496799, 2.264791
