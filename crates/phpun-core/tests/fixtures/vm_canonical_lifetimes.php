<?php
class VmReturnedObject {
    public int $n = 2;
    public function __destruct() { echo 'destroy', "\n"; }
    public static function create() { return new VmReturnedObject(); }
}
VmReturnedObject::create()->n;
echo 'after temporary', "\n";
class VmThrowingConstructor {
    public function __construct() { throw new Exception('construct'); }
}
try { new VmReturnedObject() + new VmThrowingConstructor(); }
catch (Exception $e) { echo 'caught constructor', "\n"; }
function vm_typed_ref_write(&$n) { $n = "bad"; }
$o = new VmReturnedObject();
try { vm_typed_ref_write($o->n); }
catch (TypeError $e) { echo $e->getMessage(), "\n"; }
unset($o);
