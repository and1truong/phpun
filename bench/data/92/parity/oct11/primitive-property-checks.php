<?php
function floating(float $n): float { return $n; }
function nullable(?float $n): ?float { return $n; }
function widened(): float { return 7; }
function rejected(): float { return 'bad'; }
var_dump(floating(1.5), floating(7), floating('2.5'), nullable(null), nullable(2), widened());
try { rejected(); } catch (Throwable $e) { echo get_class($e),"\n"; }
class OrdinaryName { public function __construct(public int $n) {} }
$o=new OrdinaryName(3); echo get_class($o),' ',$o->n,"\n";
$a=new class { public function read() { return 4; } }; echo strpos(get_class($a),'@anonymous') !== false ? 'anonymous' : 'bad', ' ', $a->read(),"\n";
class PrimitivePropertyProbe { public float $f; public int $i; public bool $b; public string $s; }
$p = new PrimitivePropertyProbe;
$p->f = 7; $p->i = 3; $p->b = true; $p->s = "a\0b";
var_dump($p->f, $p->i, $p->b, bin2hex($p->s));
$ref =& $p->f; $ref = 9; var_dump($p->f);
try { $p->i = []; } catch (TypeError $e) { echo "property type\n"; }
