<?php
function liveCallbackArg() { return debug_backtrace(0, 2)[1]['args'][0]; }
var_dump(array_map(function(int $x): int { $x++; echo json_encode(func_get_args()), "\n"; return liveCallbackArg(); }, ['a'=>1,'b'=>2]));
echo json_encode(array_map(function($x) { $x = 1.5; return func_get_args(); }, [null, true])), "\n";
echo json_encode(array_filter([1,2,3], fn(int $x): bool => $x % 2 === 0)), "\n";
$a=[3,1,2]; usort($a, fn(int $x,int $y):int => $x <=> $y); echo json_encode($a), "\n";
$a=[1=>3,2=>1,3=>2]; uasort($a, fn(int $x,int $y):int => $x <=> $y); echo json_encode($a), "\n";
uksort($a, fn(int $x,int $y):int => $y <=> $x); echo json_encode($a), "\n";
$seen=0; echo json_encode(array_map(function($x) use (&$seen) { $seen += $x; return $seen; }, [1,2])), "\n";
echo json_encode(array_map(function($x,$y=10) { return $x+$y; }, [1,2])), "\n";
echo json_encode(array_map(fn()=>func_num_args(), [1,2])), "\n";
echo json_encode(array_map(fn(int $x)=>$x+1, ['2','3'])), "\n";
class CallbackScope { private int $k=4; function run() { return array_map(fn($x)=>$this->k+$x, [1,2]); } }
echo json_encode((new CallbackScope)->run()), "\n";
$warnings=0; set_error_handler(function($code,$message) use (&$warnings) { $warnings++; return true; });
$a=[2,1]; usort($a, function(&$x,$y) { return $x <=> $y; }); echo json_encode($a),' ', $warnings>0?'warning':'missing', "\n";
$a=[2,1,2]; usort($a, fn(int $x,int $y):bool=>$x>$y); echo json_encode($a), "\n";
restore_error_handler();
try { $a=[1,1]; usort($a, fn(int $x,int $y):int=>intdiv(1,$x-$y)); } catch (Throwable $e) { echo get_class($e), ':', $e->getMessage(), "\n"; }
echo json_encode(array_map(fn(int $x):int=>$x*2,[3,4])), "\n";
function liveCallbackArgs() { return debug_backtrace(0, 2)[1]['args']; }
$a=[2,1]; usort($a, function(int $x,int $y):int { $x++; echo json_encode(func_get_args()), ' ', json_encode(liveCallbackArgs()), "\n"; return ($x-1)<=>$y; });
echo json_encode($a), "\n";
