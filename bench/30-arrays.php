<?php
// Array machinery: push, map, sort, filter, sum, string keys.
// argv[1]: element count scale in thousands (default 20).
$n = (int)($argv[1] ?? 20) * 1000;

$a = [];
for ($i = 0; $i < $n; $i++) {
    $a[] = ($i * 2654435761) % 1000003; // deterministic pseudo-random order
}

$b = array_map(fn(int $x): int => $x * 3 + 1, $a);
usort($b, fn(int $x, int $y): int => $x <=> $y);
$c = array_filter($b, fn(int $x): bool => $x % 7 === 0);

$h = [];
foreach ($a as $v) {
    $k = "k" . ($v % 97);
    $h[$k] = ($h[$k] ?? 0) + 1;
}

$sum = array_sum($b) + array_sum($h) + count($c);

echo "RESULT $sum\n";
