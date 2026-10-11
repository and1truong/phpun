<?php
class CtorValues {
    public function __construct(public float $x = 1, public int $n = 2) {
        echo json_encode(func_get_args()), "\n";
    }
}
class ChildValues extends CtorValues {}
foreach ([[3,4], [4], [], [5,6,7]] as $args) {
    $o = new ChildValues(...$args);
    echo json_encode([$o->x,$o->n]), "\n";
}
$o = new CtorValues(8, 9); $w = WeakReference::create($o); unset($o); var_dump($w->get());
try { new CtorValues('bad'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
class RefCtor { public function __construct(public int &$n) { ++$n; } }
$n = 1; $o = new RefCtor($n); $n = 7; var_dump($n, $o->n); $o->n = 9; var_dump($n);
