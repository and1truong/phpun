<?php declare(strict_types=1);
function rem_edge(int $n,int $d): int { return $n % $d; }
function rem_zero(int $n,int $d): int { return $n % $d; }
function bool_child(int $n): bool { return $n === 0; }
function integer_parent(int $n): int { return bool_child($n) === 0 ? 7 : 9; }
function bad_bool_return(int $n): bool { return $n; }
for ($i=0;$i<70;$i++) {
    rem_edge(9,7); rem_zero(9,7);
    if (integer_parent(0) !== 9) throw new Exception('bool entered integer stack');
    try { bad_bool_return(1); } catch (TypeError $e) { continue; }
    throw new Exception('bool return check bypassed');
}
echo rem_edge(PHP_INT_MIN,7), ' ', rem_edge(PHP_INT_MIN,-3), ' ', rem_edge(PHP_INT_MIN,-1), "\n";
try { rem_zero(9,0); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
