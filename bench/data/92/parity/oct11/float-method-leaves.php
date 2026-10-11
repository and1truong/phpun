<?php
class Leaf {
 private float $x=3.0;
 private float $y=4.0;
 function norm(): float { return sqrt($this->x*$this->x+$this->y*$this->y); }
 function scaled(float $f): float { return $this->x*$f; }
}
class ChildLeaf extends Leaf { private float $x=99.0; }
$a = new Leaf; $b = new ChildLeaf;
for ($i=0;$i<20;$i++) { echo $a->norm(), ' ', $b->norm(), ' ', $a->scaled(2), "\n"; }
class HookLeaf {
 public float $x { get { echo "hook\n"; return 3.0; } }
 function norm(): float { return sqrt($this->x*$this->x); }
}
$o = new HookLeaf;
for ($i=0;$i<3;$i++) echo $o->norm(), "\n";
class TraceLeaf {
 function norm(): float { echo debug_backtrace()[0]['function'], "\n"; return 7.0; }
}
$t = new TraceLeaf; echo $t->norm(), "\n";
try { $a->scaled([]); } catch (TypeError $e) { echo get_class($e), "\n"; }
