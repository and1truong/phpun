<?php
set_error_handler(function($level, $message) { echo 'notice '; return true; });
function vm_ref_side() { echo 'side '; return 2; }
function vm_ref_target(&$x, $y = 0) { echo 'target '; $x = 5; }
function vm_ref_literal() { vm_ref_target(1, vm_ref_side()); }
try { vm_ref_literal(); } catch (Error $e) { echo 'caught '; }
function vm_ref_value() { return 1; }
function vm_ref_temporary() { vm_ref_target(vm_ref_value()); }
vm_ref_temporary();
function &vm_ref_return() { static $x = 1; return $x; }
function vm_ref_escaping() { vm_ref_target(vm_ref_return()); return vm_ref_return(); }
var_dump(vm_ref_escaping());
function vm_ref_variadic(&...$xs) { foreach ($xs as &$x) { $x++; } }
function vm_ref_variadic_literal() { vm_ref_variadic(1); }
try { vm_ref_variadic_literal(); } catch (Error $e) { echo 'caught2 '; }
function vm_ref_variadic_locals() { $a = 1; $b = 2; vm_ref_variadic($a, $b); return $a + $b; }
var_dump(vm_ref_variadic_locals());
function vm_ref_sort($a) { sort($a); return implode(',', $a); }
var_dump(vm_ref_sort([3, 1, 2]));
function vm_ref_alias_order($x) { vm_ref_target($x, $x = 8); return $x; }
var_dump(vm_ref_alias_order(1));
