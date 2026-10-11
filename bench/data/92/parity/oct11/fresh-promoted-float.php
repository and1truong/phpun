<?php
class FreshScalar {
 function __construct(private float $x=1.5, public float $y=2.5) {}
 function values() { return [$this->x,$this->y]; }
}
for ($i=0;$i<8;$i++) echo json_encode((new FreshScalar($i,4.5))->values()), "\n";
echo json_encode((new FreshScalar)->values()), "\n";
echo json_encode((new FreshScalar(y:7,x:6))->values()), "\n";
class ChildScalar extends FreshScalar { private float $x=99; }
echo json_encode((new ChildScalar(8,9))->values()), "\n";
class ReadonlyScalar { function __construct(public readonly float $x) {} }
$r=new ReadonlyScalar(3); echo json_encode($r), "\n";
try { $r->__construct(4); } catch (Error $e) { echo get_class($e), "\n"; }
class HookScalar { function __construct(public float $x { set { echo debug_backtrace()[1]['function'], ':', $value, "\n"; $this->x=$value*2; } }) {} }
echo json_encode(new HookScalar(5)), "\n";
class BodyScalar { function __construct(public float $x) { echo 'body:',json_encode(func_get_args()), "\n"; } }
new BodyScalar(6);
class ReferenceScalar { function __construct(public float &$x) { $x++; } }
$x=1.0; $ref=new ReferenceScalar($x); echo $x, ':', $ref->x, "\n";
try { new FreshScalar([]); } catch (TypeError $e) { echo get_class($e), "\n"; }
try { new FreshScalar('bad',4); } catch (TypeError $e) { echo get_class($e), "\n"; }
class PrivateScalar { private function __construct(public float $x) {} }
try { new PrivateScalar(2); } catch (Error $e) { echo get_class($e), "\n"; }
