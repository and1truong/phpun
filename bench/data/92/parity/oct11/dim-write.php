<?php
function read_at($a, $k) { return $a[$k]; }
function read_zero($a) { return $a[0]; }
$a = [3, 1, 2]; $copy = $a; echo read_at($a, 1), ' ', read_zero($a), "\n";
$r =& $a[1]; $r = 9; echo read_at($a, 1), ' ', read_at($copy, 1), "\n";
unset($a[0]); $a[0] = 8; echo read_zero($a), "\n";
sort($a); echo read_zero($a), "\n";
array_unshift($a, 7); echo read_zero($a), "\n";
array_splice($a, 0, 1, [4, 5]); echo read_zero($a), ' ', read_at($a, 1), "\n";
foreach ($a as &$v) $v += 1; unset($v); echo read_zero($a), "\n";
foreach ([null, false, 1, 1.5, "x\0z"] as $v) { $x=read_zero([$v]); echo gettype($x), ':', is_string($x) ? bin2hex($x) : json_encode($x), "\n"; }
echo read_at(['k'=>'str', 2=>'num'], 'k'), ' ', read_at([2=>'num'], '2'), "\n";
$o = new stdClass; $o->x=6; echo read_at([$o], 0)->x, ' ', read_at([[2]], 0)[0], "\n";
class ReadAccess implements ArrayAccess {
 function offsetExists(mixed $k): bool { return true; }
 function offsetGet(mixed $k): mixed { echo "get:$k\n"; return 12; }
 function offsetSet(mixed $k, mixed $v): void {}
 function offsetUnset(mixed $k): void {}
}
echo read_at(new ReadAccess, 3), "\n";
set_error_handler(function($errno, $message) { echo "warning:$message\n"; });
var_dump(read_at([], 7));
function missing_key($a) { return $a[$unknown]; }
var_dump(missing_key([0=>2]));
restore_error_handler();
echo read_at(k: 0, a: [11]), "\n";
function clear_at(&$a, $k) { return $a[$k] = false; }
$a=[true,true]; var_dump(clear_at($a,0)); $copy=$a; clear_at($a,1);
echo json_encode($a), ' ', json_encode($copy), "\n";
$r =& $a[1]; $r=true; clear_at($a,1); var_dump($r);
clear_at($a,7); clear_at($a,'key'); echo json_encode($a), "\n";
class WriteTyped { public int $v=1; public readonly array $a; function __construct() { $this->a=[true]; } }
$o=new WriteTyped; $a=[0]; $a[0] =& $o->v;
var_dump(clear_at($a,0)); var_dump($o->v);
$a=$o->a; clear_at($a,0); echo json_encode($a), ' ', json_encode($o->a), "\n";
try { $o->a[0]=false; } catch (Error $e) { echo $e->getMessage(), "\n"; }
class WriteAccess implements ArrayAccess {
 function offsetExists(mixed $k):bool { return true; }
 function offsetGet(mixed $k):mixed { return true; }
 function offsetSet(mixed $k,mixed $v):void { echo "set:$k:", json_encode($v), "\n"; }
 function offsetUnset(mixed $k):void {}
}
$a=new WriteAccess; clear_at($a,3);
$g=[1]; for ($i=0; $i<2; $i++) { echo $g[0], "\n"; $GLOBALS['g']=[5]; }
echo $GLOBALS['g'][0], "\n";
