<?php
// Closure creation with unrelated live variables, followed by invocation.
$n = (int)($argv[1] ?? 20000);
function arrows(int $n): int {
    $unrelated = range(1, 1000);
    $used = 7;
    $sum = 0;
    for ($i = 0; $i < $n; ++$i) {
        $f = fn(int $x): int => $x + $used;
        $sum += $f($i);
    }
    return $sum;
}
echo 'RESULT ', arrows($n), "\n";
