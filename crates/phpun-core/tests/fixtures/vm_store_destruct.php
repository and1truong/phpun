<?php
// Single-quoted literals keep the target bodies inside the VM subset.
class VmDtor {
    function __destruct() { echo 'destroy '; }
}
function vm_make_dtor() { return new VmDtor; }
function vm_overwrite() {
    $local = vm_make_dtor();
    echo 'held ';
    $local = null;
    echo 'after ';
}
function vm_alias() {
    $local = vm_make_dtor();
    $alias = $local;
    $local = null;
    echo 'alias ';
    $alias = null;
    echo 'after ';
}
class VmThrowingDtor {
    function __destruct() { echo 'throw '; throw new Exception('dtor'); }
}
function vm_make_thrower() { return new VmThrowingDtor; }
function vm_overwrite_thrower() {
    $local = vm_make_thrower();
    $local = null;
    echo 'unreachable ';
}
vm_overwrite();
vm_alias();
try { vm_overwrite_thrower(); } catch (Exception $e) { echo 'caught '; }
echo 'done';
