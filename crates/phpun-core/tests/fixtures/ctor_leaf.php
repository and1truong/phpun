<?php
// ctor_leaf: all-promoted empty-body ctors publish without a frame.
class P {
    function __construct(private float $x = 0.0, private float $y = 0.0) {}
    function norm(): float { return sqrt($this->x**2 + $this->y**2); }
}
$p = new P(3, 4);
var_dump($p->norm());

// Defaults + int->float widening at bind.
$q = new P;
var_dump($q->norm());
$r = new P(6);
var_dump($r->norm());

// new inside a function — scope borrowed on the caller frame.
function mk(): P { return new P(1, 2); }
var_dump(mk()->norm());

// Public promoted with default + extra ignored args.
class Pub {
    function __construct(public int $n = 5) {}
}
var_dump((new Pub)->n);
var_dump((new Pub(9))->n);

// Non-promoted param mixed in keeps the canonical call.
class Hybrid {
    private float $v;
    function __construct(float $v, private string $tag = "t") { $this->v = $v; }
    function get(): float { return $this->v; }
}
var_dump((new Hybrid(2.5))->get());

// A ctor that throws leaves the half-built object undestructed.
class Noisy {
    function __construct(private int $x) { throw new Exception("boom"); }
    function __destruct() { echo "dtor\n"; }
}
try { new Noisy(1); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
