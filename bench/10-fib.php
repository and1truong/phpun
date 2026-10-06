<?php
// Recursive function-call throughput: naive fibonacci.
// argv[1]: n (default 26).
$n = (int)($argv[1] ?? 26);

function fib(int $n): int {
    return $n < 2 ? $n : fib($n - 1) + fib($n - 2);
}

echo "RESULT " . fib($n) . "\n";
