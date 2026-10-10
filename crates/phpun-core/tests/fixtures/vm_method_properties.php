<?php
class VmPropertyBase {
    private int $x = 3;
    protected int $y = 5;
    public readonly int $ro;
    public function __construct() { $this->ro = 7; }
    public function sum(int $n): int { return $this->x + $this->y + $this->ro + $n; }
    public function closure() { return fn(int $n): int => $this->x + $n; }
    public static function bad_this() { return $this->x; }
}
class VmPropertyChild extends VmPropertyBase {
    private int $x = 99;
    public function own(): int { return $this->x; }
    public function parent_sum(): int { return parent::sum(1); }
}
$p = new VmPropertyChild();
echo $p->sum(2), ':', $p->own(), ':', $p->parent_sum(), ':', ($p->closure())(4), "\n";
class VmPropertyMagic {
    public int $uninitialized;
    public function read(int $n): int { return $this->missing + $n; }
    public function typed() { return $this->uninitialized; }
    public function __get($name) { $GLOBALS['changed_by_getter'] = 11; return 13; }
}
$m = new VmPropertyMagic();
echo $m->read(2), ':', $changed_by_getter, "\n";
foreach ([fn() => $m->typed(), fn() => VmPropertyBase::bad_this()] as $bad) {
    try { $bad(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
}
class VmPropertyHook {
    public int $v = 17 { get => $this->v + 2; }
    public function read(): int { return $this->v; }
    public function __destruct() { echo "hook-destroyed\n"; }
}
$h = new VmPropertyHook();
echo $h->read(), "\n";
unset($h);
class VmPropertyString {
    public $value = 'ok';
    public function __toString() { return $this->value; }
}
$s = new VmPropertyString();
echo $s->__toString(), "\n";
$s->value = [];
try { $s->__toString(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
unset($p, $m, $s);
