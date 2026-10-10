<?php
$scalar = fn(int $x, int $y): int => $x * 3 + $y;
echo $scalar(2, 3), ':', $scalar('2', 3), ':', $scalar(y: 4, x: 3), "\n";
$default = fn(int $x = 7): int => $x + 1;
echo $default(), ':', $default(3), "\n";
$variadic = fn(int ...$xs): int => array_sum($xs);
echo $variadic(1, 2, 3), ':', $variadic(), "\n";
$byref = fn(&$x): int => ++$x;
$x = 5;
echo $byref($x), ':', $x, "\n";
$capture = fn(int $n): int => $n + $x;
$x = 9;
echo $capture(1), ':', $capture(1), "\n";
foreach ([fn() => $scalar(1), fn() => $scalar([], 2)] as $bad) {
    try { $bad(); } catch (Throwable $e) {
        $args = $e->getTrace()[0]['args'];
        echo get_class($e), ':', count($args), "\n";
    }
}
class ClosureReceiver {
    private int $n = 11;
    public function make() { return fn(int $x): int => $this->n + $x; }
    public function make_scalar() { return fn(int $x): int => $x + 1; }
    public function __destruct() { echo "receiver-destroyed\n"; }
}
$receiver = new ClosureReceiver();
$bound = $receiver->make();
$scalar_bound = $receiver->make_scalar();
unset($receiver);
echo $bound(2), ':', $scalar_bound(2), "\n";
unset($bound);
echo "held-native-closure\n";
unset($scalar_bound);
echo implode(',', array_map($scalar, [1, 2], [3, 4])), "\n";
$a = [1]; array_walk($a, function ($v) { $v = 2; }); echo $a[0], "\n";
$b = [1]; $f = function ($v) { $v = 2; }; $f(...$b); echo $b[0], "\n";
$c = ['7']; call_user_func_array(function (int $v) { $v += 1; }, $c); var_dump($c[0]);
$d = [1]; array_walk($d, function (&$v) { $v = 2; }); echo $d[0], "\n";
function named_value($v) { $v = 3; }
function named_int(int $v) { $v += 1; }
$named = [1]; array_walk($named, 'named_value'); named_value(...$named); echo $named[0], "\n";
$typed = ['7']; call_user_func_array('named_int', $typed); var_dump($typed[0]);
$alias = 5; $refargs = [&$alias]; $byvalue = function (float $v) { $v = 9.0; }; $byvalue(...$refargs); var_dump($alias);
