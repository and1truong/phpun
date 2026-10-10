<?php
// Closure creation with unrelated live variables, followed by invocation.
$n = (int)($argv[1] ?? 20000);
$mode = $argv[2] ?? 'captured';
function arrows(int $n, string $mode): int {
    $unrelated = range(1, 1000);
    $used = 7;
    $sum = 0;
    for ($i = 0; $i < $n; ++$i) {
        $f = $mode === 'scalar' ? fn(int $x): int => $x + 7 : fn(int $x): int => $x + $used;
        $sum += $f($i);
    }
    return $sum;
}
echo 'RESULT ', arrows($n, $mode), "\n";
