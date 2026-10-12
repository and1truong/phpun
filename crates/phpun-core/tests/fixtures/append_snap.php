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

// Referenced container stays canonical too.
$e = [];
$r = &$e;
$e[] = 9;
var_dump($e, $r);

// RHS rebound to a live array lands the append on the rebound value.
function f(){ $a=[1]; $a[] = ($a=[7,8]); return $a; }
var_dump(f());

// Rebound to null vivifies to [null].
function g(){ $a=[1]; $a[] = ($a=null); return $a; }
var_dump(g());

// Scalar rebound to array keeps the append too.
function h(){ $a=5; $a[] = ($a=[9]); return $a; }
var_dump(h());

// Aliased container sees the write on both cells.
function i(){ $a=[1]; $b =& $a; $a[] = ($a=[9]); return [$a,$b]; }
var_dump(i());

// In-place vivify of a false container deprecates once and writes.
function j(){ $a=false; $a[] = 1; return $a; }
var_dump(j());

// RHS rebind to scalar throws, like the canonical fetch.
$c = [7];
$c[] = $c = 3;
var_dump($c);
