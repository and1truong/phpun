# bench results

- date: 2026-10-09T10:49:55Z
- phpun: phpun 0.0.1 (php compat target: 8.5) (./target/release/phpun)
- php:   8.5.11 (/home/linuxbrew/.linuxbrew/bin/php)
- host:  Linux 6.8.0-1061-aws x86_64 8 cores
- metric: min wall-clock ms (adaptive reps); ratio = phpun / php

| bench                        |     php ms |   phpun ms |  x slower | stdout |
|------------------------------|----------::|----------::|---------::|:------|
| 00-startup                   |         23 |          5 |       0.2 | ok |
| 10-fib                       |         28 |        527 |      18.8 | ok |
| 11-sieve                     |         27 |        200 |       7.4 | ok |
| 20-strings                   |         32 |        482 |      15.1 | ok |
| 30-arrays                    |         40 |       1102 |      27.6 | ok |
| 40-objects                   |         26 |        443 |      17.0 | ok |
| 50-regex                     |         25 |         35 |       1.4 | ok |
| 60-json                      |        135 |        948 |       7.0 | ok |
| 70-db                        |         46 |        115 |       2.5 | ok |

geometric mean slowdown (matched benches): 5.6x
# bench http results

- date: 2026-10-09T10:50:19Z
- phpun: phpun 0.0.1 (php compat target: 8.5)
- php:   8.5.11 (/home/linuxbrew/.linuxbrew/bin/php)
- host:  Linux 6.8.0-1061-aws x86_64 8 cores
- load:  400 requests, 8 concurrent, fresh connection per request

| server                             |        req/s | errors |
|------------------------------------|----------::|-----::|
| php -S (1 proc)                    |       5057.6 |      0 |
| php -S (8 workers)                 |       5049.3 |      0 |
| phpun serve (fresh interp)         |       1504.1 |      0 |
| phpun serve (8 warm workers)       |       2225.8 |      0 |
