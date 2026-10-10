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
    public function __destruct() { echo "receiver-destroyed\n"; }
}
$receiver = new ClosureReceiver();
$bound = $receiver->make();
unset($receiver);
echo $bound(2), "\n";
unset($bound);
echo implode(',', array_map($scalar, [1, 2], [3, 4])), "\n";
