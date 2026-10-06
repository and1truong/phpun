# bench results

- date: 2026-10-06T08:00:30Z
- phpun: phpun 0.0.1 (php compat target: 8.5) (./target/release/phpun)
- php:   8.5.11 (/home/linuxbrew/.linuxbrew/bin/php)
- host:  Linux 6.8.0-1061-aws x86_64 8 cores
- metric: min wall-clock ms (adaptive reps); ratio = phpun / php

| bench                        |     php ms |   phpun ms |  x slower | stdout |
|------------------------------|----------::|----------::|---------::|:------|
| 00-startup                   |         29 |          5 |       0.2 | ok |
| 10-fib                       |         35 |        673 |      19.2 | ok |
| 11-sieve                     |         33 |      17644 |     534.7 | ok |
| 20-strings                   |         42 |        519 |      12.4 | ok |
| 30-arrays                    |         46 |       1449 |      31.5 | ok |
| 40-objects                   |         35 |        524 |      15.0 | ok |
| 50-regex                     |         35 |         41 |       1.2 | ok |
| 60-json                      |        176 |        681 |       3.9 | ok |
| 70-db                        |         62 |        118 |       1.9 | ok |

geometric mean slowdown (matched benches): 7.6x
# bench http results

- date: 2026-10-06T08:01:23Z
- phpun: phpun 0.0.1 (php compat target: 8.5)
- php:   8.5.11 (/home/linuxbrew/.linuxbrew/bin/php)
- host:  Linux 6.8.0-1061-aws x86_64 8 cores
- load:  400 requests, 8 concurrent, fresh connection per request

| server                             |        req/s | errors |
|------------------------------------|----------::|-----::|
| php -S (1 proc)                    |       3963.8 |      0 |
| php -S (8 workers)                 |       3792.7 |      0 |
| phpun serve (fresh interp)         |       1739.3 |      0 |
| phpun serve (8 warm workers)       |       3994.4 |      0 |
