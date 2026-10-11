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

class VmReferenceConstructor { function __construct(public int &$n) { $n++; } }
$n = 1;
for ($i = 0; $i < 1; $i++) { $reference = new VmReferenceConstructor($n); }
var_dump($n, $reference->n);
$reference->n = 9;
var_dump($n);

function vm_rebind_global() {
    global $vm_target;
    unset($GLOBALS['vm_binding']); $GLOBALS['vm_binding'] =& $vm_target;
    return 1;
}
class VmGlobalMethod { function run() { return vm_rebind_global(); } }
$vm_binding = 2; $vm_target = 9; $mutator = new VmGlobalMethod;
for ($i = 0; $i < 1; $i++) { $mutator->run(); $seen = $vm_binding; }
var_dump($seen, $vm_binding);
class VmGlobalConstructor {
    function __construct() {
        global $vm_ctor_target;
        unset($GLOBALS['vm_ctor_binding']); $GLOBALS['vm_ctor_binding'] =& $vm_ctor_target;
    }
}
$vm_ctor_binding = 2; $vm_ctor_target = 9;
for ($i = 0; $i < 1; $i++) { $mutator = new VmGlobalConstructor; $seen = $vm_ctor_binding; }
var_dump($seen, $vm_ctor_binding);
