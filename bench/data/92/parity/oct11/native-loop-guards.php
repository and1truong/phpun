<?php
function nt_fib(int $n): int { return $n < 2 ? $n : nt_fib($n-1) + nt_fib($n-2); }
function nt_add($n) { return $n + 1; }
function nt_depth(int $n): int { return $n < 1 ? 0 : nt_depth($n-1) + 1; }
function nt_bad(int $n): int { return $n + 1; }
function nt_local($n) { if ($n) { $x=9; } return $x; }
function nt_effect($n) { echo "effect\n"; return $n + 1; }
function nt_loop(int $n): int { $s=0; for ($i=0;$i<$n;$i++) { for ($j=0;$j<3;$j++) { if ($j===1) continue; $s+=$i; } if ($i===4) break; } return $s; }
function nt_loop_bad(int $n): int { for ($i=0;$i<2;$i++) $n++; return $n; }
function nt_loop_local($n) { while ($n<0) { $x=1; $n++; } return $x; }
echo nt_fib(16), "\n";
for ($i=0;$i<70;$i++) { nt_add(1); nt_depth(1); nt_bad(1); nt_local(1); nt_loop(10); nt_loop_bad(1); nt_loop_local(-1); }
echo nt_loop(10), "\n";
try { nt_loop_bad(PHP_INT_MAX-1); } catch (TypeError $e) { echo get_class($e), "\n"; }
var_dump(nt_add(PHP_INT_MAX));
echo nt_depth(100), "\n";
try { nt_bad(PHP_INT_MAX); } catch (TypeError $e) { echo get_class($e), "\n"; }
set_error_handler(function () { echo "uninit\n"; return true; });
var_dump(nt_local(0));
var_dump(nt_loop_local(0));
restore_error_handler();
echo nt_effect(2), "\n";
