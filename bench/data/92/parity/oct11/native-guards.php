<?php
function nt_fib(int $n): int { return $n < 2 ? $n : nt_fib($n-1) + nt_fib($n-2); }
function nt_add($n) { return $n + 1; }
function nt_depth(int $n): int { return $n < 1 ? 0 : nt_depth($n-1) + 1; }
function nt_bad(int $n): int { return $n + 1; }
function nt_local($n) { if ($n) { $x=9; } return $x; }
function nt_effect($n) { echo "effect\n"; return $n + 1; }
echo nt_fib(16), "\n";
for ($i=0;$i<70;$i++) { nt_add(1); nt_depth(1); nt_bad(1); nt_local(1); }
var_dump(nt_add(PHP_INT_MAX));
echo nt_depth(100), "\n";
try { nt_bad(PHP_INT_MAX); } catch (TypeError $e) { echo get_class($e), "\n"; }
set_error_handler(function () { echo "uninit\n"; return true; });
var_dump(nt_local(0));
restore_error_handler();
echo nt_effect(2), "\n";
