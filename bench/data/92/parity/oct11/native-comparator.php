<?php
$f = fn(int $a, int $b): int => $a <=> $b;
$g = fn(int $a, int $b): int => $a + $b;
for ($i = 0; $i < 100; $i++) {
    $a = [PHP_INT_MAX, 0, PHP_INT_MIN, -1, 0, PHP_INT_MAX];
    usort($a, $f);
    $b = [3, 2, 1]; usort($b, $g);
}
echo json_encode($a), "\n";
try { usort($a, $g); } catch (Throwable $e) { echo get_class($e), ':', $e->getMessage(), "\n"; }
$seen=0; $h=function(int $a,int $b) use (&$seen):int { $seen++; return $a<=>$b; };
for ($i=0;$i<100;$i++) { $a=[3,1,2]; usort($a,$h); }
echo json_encode($a), ':', $seen, "\n";
$k=fn(int $a,int $b):bool=>$a>$b;
set_error_handler(fn()=>true);
for ($i=0;$i<100;$i++) { $a=[3,1,2]; usort($a,$k); }
restore_error_handler(); echo json_encode($a), "\n";
