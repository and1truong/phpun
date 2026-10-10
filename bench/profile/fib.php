<?php
$mode = $argv[1] ?? 'typed';
$n = (int)($argv[2] ?? 26);
$reps = (int)($argv[3] ?? 1);
function typed_fib(int $n): int { return $n < 2 ? $n : typed_fib($n - 1) + typed_fib($n - 2); }
function untyped_fib($n) { return $n < 2 ? $n : untyped_fib($n - 1) + untyped_fib($n - 2); }
$acc = 0;
for ($r = 0; $r < $reps; $r++) { $acc += $mode === 'typed' ? typed_fib($n) : untyped_fib($n); }
echo 'RESULT ', $acc, "\n";
