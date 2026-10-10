<?php
function sum_values($input) {
    $sum = 0; $last = null;
    foreach ($input as $key => $v) { $sum += $v; $last = $key; }
    echo $sum, ':', $last, ':', $v, "\n";
}
sum_values(['a' => 3, 'b' => 5]);
sum_values(new ArrayIterator([7, 11]));
function live_references() {
    $a = [1, 2]; $sum = 0;
    foreach ($a as &$v) {
        $sum += $v;
        $v *= 2;
        if (count($a) < 3) { $a[] = 3; }
    }
    unset($v);
    echo implode(',', $a), ':', $sum, "\n";
}
live_references();
function loop_fallback() {
    $sum = 0;
    foreach ([1, 2, 3] as $v) {
        if ($v === 2) { continue; }
        try { $sum += $v; } finally { echo 'finally:', $v, "\n"; }
        if ($v === 3) { break; }
    }
    return $sum;
}
echo loop_fallback(), "\n";
function nested_values() {
    $sum = 0;
    foreach ([[1, 2], [3]] as $row) {
        foreach ($row as $v) { $sum += $v; }
    }
    return $sum;
}
echo nested_values(), "\n";
class ForeachValue {
    public function __construct(public int $n) {}
    public function __destruct() { echo 'destroy:', $this->n, "\n"; }
}
function scope_lifetime() {
    foreach ([1, 2] as $v) { $local = new ForeachValue($v); }
    echo 'kept:', $local->n, "\n";
}
scope_lifetime();
function early_return() {
    foreach ([1, 2] as $v) { return new ForeachValue($v); }
}
$value = early_return();
echo 'returned:', $value->n, "\n";
unset($value);
function foreach_read_order() {
    foreach (['A' => 'A'] as $key => $v) {
        echo $v . ($v = 'B'), ':', $key . ($key = 'C'), "\n";
        echo $local . ($local = 'D'), "\n";
    }
    $a = ['A'];
    foreach ($a as &$ref) { echo $ref . ($ref = 'E'), "\n"; }
    unset($ref);
    foreach ([['A']] as [$item]) { echo $item . ($item = 'F'), "\n"; }
    echo $a[0], "\n";
}
foreach_read_order();
