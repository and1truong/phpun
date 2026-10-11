<?php
function cross_a(int $n): int { if ($n < 1) { return 1; } return cross_b($n-1); }
function cross_b(int $n): int { return $n < 1 ? 0 : cross_a($n-1); }
function factorial($n) { if ($n <= 1) { return 1; } return $n * factorial($n-1); }
function local_math(int $n): int { $x=$n; $x++; ++$x; $x--; --$x; $x=$x*3+2; return $x; }
function cond_int(int $n): int { if ($n !== 2) { $x=4; } else { $x=9; } return $x; }
for ($i=0;$i<80;$i++) { cross_a(6); factorial(5); local_math(3); cond_int(2); }
echo cross_a(6), ':', cross_b(6), ':', local_math(7), ':', cond_int(1), "\n";
var_dump(factorial(20), factorial(21));
