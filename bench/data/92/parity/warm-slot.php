<?php
function warm_fib(int $n): int { return $n < 2 ? $n : warm_fib($n - 1) + warm_fib($n - 2); }
$start = microtime(true); $first = warm_fib(10); $firstMs = (microtime(true)-$start)*1000;
$start = microtime(true); $sum = 0; for ($i=0; $i<3; $i++) $sum += warm_fib(29); $warmMs = (microtime(true)-$start)*1000/3;
file_put_contents($argv[1], json_encode([$firstMs, $warmMs]));
echo "RESULT $first $sum\n";
