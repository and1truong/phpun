# bench results

- date: 2026-10-06T07:49:37Z
- phpun: phpun 0.0.1 (php compat target: 8.5) (./target/release/phpun)
- php:   8.5.11 (/home/linuxbrew/.linuxbrew/bin/php)
- host:  Linux 6.8.0-1061-aws x86_64 8 cores
- metric: min wall-clock ms (adaptive reps); ratio = phpun / php

| bench                        |     php ms |   phpun ms |  x slower | stdout |
|------------------------------|----------::|----------::|---------::|:------|
| 00-startup                   |         31 |          5 |       0.2 | ok |
| 10-fib                       |         37 |        669 |      18.1 | ok |
| 11-sieve                     |         35 |      17844 |     509.8 | ok |
| 20-strings                   |         39 |        515 |      13.2 | ok |
| 30-arrays                    |         44 |       1479 |      33.6 | ok |
| 40-objects                   |         32 |        531 |      16.6 | ok |
| 50-regex                     |         33 |         41 |       1.2 | ok |
| 60-json                      |        169 |        642 |       3.8 | ok |

geometric mean slowdown (matched benches): 9.2x
