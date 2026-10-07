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
