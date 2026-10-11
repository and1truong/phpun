<?php
class Promoted {
    function __construct(public int $x = 1, private float $y = 2.0) {
        var_dump(func_get_args(), $this->x, $this->y);
        $x = 99; echo "local $x property $this->x\n";
    }
}
new Promoted(3, 4);
new Promoted;
new Promoted(y: 5);
class ChildPromoted extends Promoted {}
new ChildPromoted(...[7, 8]);
try { new Promoted([]); } catch (TypeError $e) { echo "bad type\n"; }
class Locked {
    function __construct(public readonly int $x) { echo "locked $x\n"; }
}
$r = new Locked(6);
try { $r->__construct(8); } catch (Error $e) { echo $e->getMessage(), "\n"; }
class HookedCtor {
    function __construct(public int $x {
        set {
            $bt = debug_backtrace();
            echo "hook $value ", $bt[1]['function'], ' ', $bt[1]['args'][0], "\n";
            if ($value < 0) throw new Exception('negative');
            $this->x = $value * 2;
        }
    }) { echo "body $x $this->x\n"; }
}
new HookedCtor(4);
try { new HookedCtor(-1); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
class DefaultCtor {
    const N = 12;
    function __construct(public int $x = self::N) { echo "default $x\n"; }
}
new DefaultCtor;
class NativeCtorProbe extends ArrayIterator {
    function __construct($a) { parent::__construct($a); echo 'native ', $this->count(), "\n"; }
}
new NativeCtorProbe([1,2]);
class ThrowableCtorProbe extends Exception {
    function __construct() { parent::__construct('engine'); echo $this->getMessage(), "\n"; }
}
new ThrowableCtorProbe;
