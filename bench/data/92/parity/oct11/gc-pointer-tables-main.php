<?php
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
