<?php
function sum_loop(int $n): int {
    $s=0; for ($i=0;$i<$n;$i++) { $s += $i; }
    return $s;
}
function nested_loop(int $n): int {
    $s=0; for ($i=0;$i<$n;$i++) { for ($j=0;$j<7;$j++) { if ($j===3) continue; $s += $i-$j; } }
    return $s;
}
function while_loop(int $n): int {
    $s=0; while ($n>0) { $s += $n; $n--; } return $s;
}
for ($i=0;$i<70;$i++) { sum_loop(1); nested_loop(1); while_loop(1); }
$n=(int)($argv[1]??100000);
echo sum_loop($n), ':', nested_loop(1000), ':', while_loop($n), "\n";
