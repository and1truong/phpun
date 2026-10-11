<?php
class PackedReadonly { public readonly array $a; function __construct() { $this->a=array_fill(0,3,true); } }
$r=new PackedReadonly;
try { $r->a[0]=false; } catch(Error $e) { echo $e->getMessage(),"\n"; }
$a=array_fill(0,4,true);$b=$a;foreach($a as &$v) {$v=false;}unset($v);echo json_encode([$a,$b]),"\n";
$a=array_fill(0,4,true);$ref=&$a[0];array_shift($a);$ref=false;array_unshift($a,'front');echo json_encode($a),"\n";var_dump($ref);
$a=array_fill(0,3,true);$it=new ArrayIterator($a);$a[0]=false;echo json_encode(iterator_to_array($it))," ",json_encode($a),"\n";

$a=array_fill(0,3,true);$a[1]=false;sort($a);echo json_encode($a),"\n";
