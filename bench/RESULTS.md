# bench results

- date: 2026-10-09T22:19:41Z
- phpun: phpun 0.0.1 (php compat target: 8.5) (./target/release/phpun)
- php:   8.5.11 (/home/linuxbrew/.linuxbrew/bin/php)
- host:  Linux 6.8.0-1061-aws x86_64 8 cores
- metric: min wall-clock ms (adaptive reps); ratio = phpun / php

| bench                        |     php ms |   phpun ms |  x slower | stdout |
|------------------------------|----------::|----------::|---------::|:------|
| 00-startup                   |         24 |          5 |       0.2 | ok |
| 10-fib                       |         30 |        260 |       8.7 | ok |
| 11-sieve                     |         27 |        305 |      11.3 | ok |
| 20-strings                   |         34 |       1090 |      32.1 | ok |
| 30-arrays                    |         43 |       1500 |      34.9 | ok |
| 40-objects                   |         26 |        680 |      26.2 | ok |
| 50-regex                     |         27 |         35 |       1.3 | ok |
| 60-json                      |        138 |        843 |       6.1 | ok |
| 70-db                        |         46 |        102 |       2.2 | ok |

geometric mean slowdown (matched benches): 6.0x
