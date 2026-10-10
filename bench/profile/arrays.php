<?php
// Independent phases; CLI timing includes input preparation.
$mode = $argv[1] ?? 'append';
$n = (int)($argv[2] ?? 20000);
$a = [];
for ($i = 0; $i < $n; $i++) { $a[] = ($i * 2654435761) % 1000003; }
switch ($mode) {
case 'append': $b = $a; break;
case 'map': $b = array_map(fn(int $x): int => $x * 3 + 1, $a); break;
case 'usort': $b = $a; usort($b, fn(int $x, int $y): int => $x <=> $y); break;
case 'filter': $b = array_filter($a, fn(int $x): bool => $x % 7 === 0); break;
case 'keys':
    $b = [];
    foreach ($a as $v) { $k = 'k' . ($v % 97); $b[$k] = ($b[$k] ?? 0) + 1; }
    break;
default: throw new InvalidArgumentException($mode);
}
echo 'RESULT ', array_sum($b), ':', count($b), "\n";
