<?php
class VmStringCycle {
    public $self;
    public function __toString() { return 'x'; }
}
function cycle_conversion() {
    $o = new VmStringCycle();
    $o->self = $o;
    @$o .= 'y';
    echo gc_collect_cycles(), "\n";
}
cycle_conversion();
class VmReceiver {
    public function value() { return 7; }
    public function __destruct() { echo "destroy\n"; }
}
$o = new VmReceiver();
echo $o->value(), "\n";
unset($o);
echo "after unset\n";
