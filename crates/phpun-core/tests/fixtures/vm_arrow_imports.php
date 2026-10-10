<?php
class UnusedCapture {
    public function __destruct() { echo "unused-destroyed\n"; }
}
function make_arrow() {
    $unused = new UnusedCapture();
    $large = range(1, 1000);
    $used = 7;
    return fn(int $param): int => $used + $param;
}
$f = make_arrow();
echo $f(3), "\n";
$a = 11; $b = 13; $unused = 99;
$f = fn($a) => fn() => $a + $b;
echo ($f(5))(), "\n";
$f = fn() => function () use (&$a) { return ++$a; };
$inner = $f();
echo $inner(), ':', $a, "\n";
$name = 'a';
$f = fn() => $$name;
echo "dynamic-name-created\n";
$f = fn() => "{$a}:$b";
echo $f(), "\n";
$obj = (object)['v' => 17]; $prop = 'v';
$f = fn() => match ($a) { 11 => $obj->$prop, default => $b };
echo $f(), "\n";
$_GET['v'] = 19;
$f = fn() => $_GET['v'];
$_GET['v'] = 23;
echo $f(), "\n";
$f = fn() => $missing;
echo "missing-created\n";
$f = fn() => ($a = 29);
echo $f(), ':', $a, "\n";
