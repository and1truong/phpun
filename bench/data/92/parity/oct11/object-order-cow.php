<?php
#[AllowDynamicProperties]
class OrderBase { private $a = 1; public $b = 2; }
#[AllowDynamicProperties]
class OrderChild extends OrderBase { private $a = 3; public $c = 4; }
$a = new OrderChild; $b = new OrderChild; $copy = clone $a;
echo json_encode((array)$a), "\n";
unset($a->b); $a->extra = 5; $a->b = 9;
echo json_encode((array)$a), "\n", json_encode((array)$b), "\n", json_encode((array)$copy), "\n";
foreach ($a as $k => &$v) { $v++; } unset($v);
echo json_encode((array)$a), "\n";
echo serialize($a), "\n", json_encode((array)unserialize(serialize($a))), "\n";
$s = new stdClass; $s->x = 1; $t = clone $s; $t->y = 2;
echo json_encode($s), ':', json_encode($t), "\n";
class OrderReadonly { public readonly int $x; function __construct() { $this->x=1; } }
$r = new OrderReadonly; try { $r->x=2; } catch(Error $e) { echo get_class($e), "\n"; }
echo json_encode((array)$r), "\n";
