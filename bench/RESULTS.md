# bench results

- date: 2026-10-08T23:03:32Z
- commit: 81915c4 (main)
- phpun: phpun 0.0.1 (php compat target: 8.5) (./target/release/phpun)
- php:   8.5.11 (/home/linuxbrew/.linuxbrew/bin/php)
- host:  Linux 6.8.0-1061-aws x86_64 8 cores
- metric: min wall-clock ms (adaptive reps); ratio = phpun / php

| bench                        |     php ms |   phpun ms |  x slower | stdout |
|------------------------------|----------:|----------:|---------:|:------|
| 00-startup                   |         31 |          6 |       0.2 | ok |
| 10-fib                       |         40 |       1206 |      30.1 | ok |
| 11-sieve                     |         36 |      13995 |     388.8 | ok |
| 20-strings                   |         43 |        634 |      14.7 | ok |
| 30-arrays                    |         47 |       1928 |      41.0 | ok |
| 40-objects                   |         35 |        734 |      21.0 | ok |
| 50-regex                     |         37 |         44 |       1.2 | ok |
| 60-json                      |        191 |       1235 |       6.5 | ok |
| 70-db                        |         62 |        158 |       2.5 | ok |

geometric mean slowdown (matched benches): 9.4x
# bench http results

- date: 2026-10-08T23:06:17Z
- commit: 81915c4 (main)
- phpun: phpun 0.0.1 (php compat target: 8.5)
- php:   8.5.11 (/home/linuxbrew/.linuxbrew/bin/php)
- host:  Linux 6.8.0-1061-aws x86_64 8 cores
- load:  400 requests, 8 concurrent, fresh connection per request

| server                             |        req/s | errors |
|------------------------------------|----------:|-----:|
| php -S (1 proc)                    |       2821.1 |      0 |
| php -S (8 workers)                 |       2360.1 |      0 |
| phpun serve (fresh interp)         |       1431.6 |      0 |
| phpun serve (8 warm workers)       |       3651.9 |      0 |

previous baseline: v0.0.1, 2026-10-06, geomean 7.6x (issue #92)
