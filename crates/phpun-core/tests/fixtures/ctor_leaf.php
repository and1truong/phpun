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

// Readonly promoted at top level — the globals pseudo-frame borrows
// the ctor scope (caller_scope falls back to it).
class Ro {
    function __construct(public readonly int $x = 1) {}
}
var_dump((new Ro(0))->x);

class RoP {
    function __construct(private readonly int $x = 1) {}
    function get(): int { return $this->x; }
}
var_dump((new RoP(7))->get());

class RoS {
    function __construct(protected(set) readonly int $x = 1) {}
}
var_dump((new RoS(5))->x);

// Hooked promoted at top level — StoreProp runs the set hook with
// the lent scope.
class RoH {
    function __construct(public int $x { set => $value * 2; }) {}
}
var_dump((new RoH(3))->x);
