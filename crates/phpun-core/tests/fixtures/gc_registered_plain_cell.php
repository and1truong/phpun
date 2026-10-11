<?php
// A `=&`-registered array whose only live holder is a cell inside a
// plain (unregistered) array must not be swept as garbage — before
// the per-cell external-ref accounting, reachability read the plain
// array's hold as internal and freed it (review F1, gc_023 shape).
$a = [1];
$a[0] =& $a;          // registers $a in the GC array universe
$h = [&$a];           // plain array holds the only external cell
unset($a);            // drop the var — now only $h[0] holds it
var_dump(gc_collect_cycles());
var_dump($h[0][0]);

$scalar = 4;
$values = [null, true, 3, 1.5, "a\0b", &$scalar];
$alias = $values;
unset($values, $alias);
$scalar = 42;
echo $scalar, "\n";
$cycle = new stdClass;
$cycle->self = $cycle;
$mixed = [$cycle, &$scalar];
unset($cycle, $mixed);
var_dump(gc_collect_cycles());

// Pointer-table stress keeps externally rooted and dead components separate.
class PointerGraph { public mixed $edge; }
$keep = new PointerGraph;
$keep->edge = $keep;
for ($i = 0; $i < 40; $i++) {
    $left = new PointerGraph;
    $right = new PointerGraph;
    $left->edge = $right;
    $right->edge = $left;
    unset($left, $right);
}
var_dump(gc_collect_cycles());
var_dump($keep === $keep->edge);
unset($keep);
var_dump(gc_collect_cycles());
$node = new PointerGraph;
$node->edge = function () use (&$node) { return $node; };
unset($node);
var_dump(gc_collect_cycles());
