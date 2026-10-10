<?php
$n = (int)($argv[1] ?? 100000);
function foreach_sum(int $n): int {
    $a = range(1, $n);
    $sum = 0;
    foreach ($a as $v) { $sum += $v * 3 + 1; }
    return $sum;
}
echo 'RESULT ', foreach_sum($n), "\n";
