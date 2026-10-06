<?php
// Tight array/loop workload: Sieve of Eratosthenes.
// argv[1]: repetitions (default 1), argv[2]: limit (default 100000).
$reps = (int)($argv[1] ?? 1);
$lim = (int)($argv[2] ?? 100000);

$count = 0;
for ($r = 0; $r < $reps; $r++) {
    $sieve = array_fill(0, $lim + 1, true);
    $sieve[0] = $sieve[1] = false;
    for ($i = 2; $i * $i <= $lim; $i++) {
        if ($sieve[$i]) {
            for ($j = $i * $i; $j <= $lim; $j += $i) {
                $sieve[$j] = false;
            }
        }
    }
    $count = 0;
    for ($i = 2; $i <= $lim; $i++) {
        if ($sieve[$i]) {
            $count++;
        }
    }
}

echo "RESULT $count\n";
