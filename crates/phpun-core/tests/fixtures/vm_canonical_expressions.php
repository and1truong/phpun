<?php
set_error_handler(function($level, $msg) { echo 'warning:', $msg, "\n"; });
function arrays($n) {
    $a = [];
    for ($i = 0; $i < $n; $i++) { $a[] = $i * 2; }
    $copy = $a;
    $a[1] = 9;
    $a['nested']['08'] = 7;
    $a['nested'][true] = 8;
    return $a[1] + $copy[1] + $a['nested']['08'] + $a['nested'][1];
}
function missing_key($a) { return $a['missing']; }
function dimensions_and_order() {
    $a = [10, 20]; $i = 0;
    $a[$i] = ($i = 1);
    return $a[0] + $a[1];
}
function closures() {
    $n = 2;
    $value = fn(int $x): int => $x + $n;
    $reference = function($x) use (&$n) { $n += $x; return $n; };
    $n = 5;
    return $value(1) + $reference(2) + $n;
}
class VmExpressionPoint {
    private int $x = 3;
    public function property($n) { $this->x += $n; return $this->x; }
    public function callback() { return fn($n) => $this->x + $n; }
    public function dispatch() { return $this->property(2); }
    public static function context() { return self::class . ':' . static::class; }
}
function methods() {
    $p = new VmExpressionPoint();
    $f = $p->callback();
    return $p->dispatch() + $f(1);
}
function match_value($x) { return match($x) { 1 => 10, 2 => 20, default => 30 }; }
function interpolation($n) { return "value=$n"; }
var_dump(arrays(3), missing_key([]), dimensions_and_order(), closures(), methods(), match_value(2), interpolation(4), VmExpressionPoint::context());
