<?php
// $cv[] = expr compiled append: fast path, bind_cell path, and the
// canonical tail for rebind/scalar/ref cases.
$a = [];
$a[] = 1;
$a[] = "s";
$a[] = 2.5;
var_dump($a);

// Non-scalar RHS takes bind_cell's typed-slot store.
$b = [];
$b[] = [10, 20];
$b[] = new stdClass;
var_dump(count($b));

// RHS ops can rebind the container mid-statement — zend detaches and
// the append lands on the rebound value via the canonical tail.
$c = [7];
$c[] = $c = 3;
var_dump($c);

// Scalar containers keep canonical behavior.
$d = "abc";
$d[] = "x";
var_dump($d);

// Referenced container stays canonical too.
$e = [];
$r = &$e;
$e[] = 9;
var_dump($e, $r);
