# bench results

- date: 2026-10-09T10:14:52Z
- phpun: phpun 0.0.1 (php compat target: 8.5) (./target/release/phpun)
- php:   8.5.11 (/home/linuxbrew/.linuxbrew/bin/php)
- host:  Linux 6.8.0-1061-aws x86_64 8 cores
- metric: min wall-clock ms (adaptive reps); ratio = phpun / php

| bench                        |     php ms |   phpun ms |  x slower | stdout |
|------------------------------|----------::|----------::|---------::|:------|
| 00-startup                   |         23 |          4 |       0.2 | ok |
| 10-fib                       |         27 |        727 |      26.9 | ok |
| 11-sieve                     |         26 |        195 |       7.5 | ok |
| 20-strings                   |         33 |        505 |      15.3 | ok |
| 30-arrays                    |         35 |       1182 |      33.8 | ok |
| 40-objects                   |         27 |        467 |      17.3 | ok |
| 50-regex                     |         27 |         36 |       1.3 | ok |
| 60-json                      |        152 |        845 |       5.6 | ok |
| 70-db                        |         46 |        105 |       2.3 | ok |

geometric mean slowdown (matched benches): 5.6x

# bench http results

- date: 2026-10-09T10:15:33Z
- phpun: phpun 0.0.1 (php compat target: 8.5)
- php:   8.5.11 (/home/linuxbrew/.linuxbrew/bin/php)
- host:  Linux 6.8.0-1061-aws x86_64 8 cores
- load:  400 requests, 8 concurrent, fresh connection per request

| server                             |        req/s | errors |
|------------------------------------|----------::|-----::|
| php -S (1 proc)                    |       3950.5 |      0 |
| php -S (8 workers)                 |       3269.1 |      0 |
| phpun serve (fresh interp)         |       1788.4 |      0 |
| phpun serve (8 warm workers)       |       4622.6 |      0 |
