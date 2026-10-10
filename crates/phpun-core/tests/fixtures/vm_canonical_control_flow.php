<?php
function foreach_refs($a) { foreach ($a as &$v) { $v += 1; } unset($v); return $a; }
function iterator_sum($a) { $n = 0; foreach (new ArrayIterator($a) as $k => $v) { $n += $v; } return $n; }
function mixed_breaks() {
    $sum = 0;
    for ($i = 0; $i < 5; $i++) {
        foreach ([1, 2, 3] as $v) {
            if ($i == 2) { break 2; }
            if ($v == 2) { continue 2; }
            $sum += $v;
        }
        $sum += 100;
    }
    return $sum;
}
function mixed_finally() {
    $sum = 0;
    for ($i = 0; $i < 3; $i++) {
        try { if ($i == 1) { continue; } $sum += 1; }
        finally { $sum += 10; }
    }
    try { throw new Exception('inside'); }
    catch (Exception $e) { $sum += 2; }
    finally { $sum += 3; }
    return $sum;
}
function switch_inside_loop() {
    $sum = 0;
    for ($i = 0; $i < 4; $i++) {
        switch ($i) { case 1: continue 2; case 2: break; default: $sum += 1; }
        $sum += 10;
    }
    return $sum;
}
function static_scope() { static $n = 0; return ++$n; }
function args_snapshot($a = 7, ...$extras) { $a = 9; return [func_num_args(), func_get_args()]; }
var_dump(foreach_refs([1, 2]), iterator_sum([3, 4]), mixed_breaks(), mixed_finally(), switch_inside_loop(), static_scope(), static_scope(), args_snapshot(), args_snapshot(1, 2, 3));
