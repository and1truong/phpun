<?php
function region($n,$step) {
 $a=array_fill(0,$n,true);
 for($i=0;$i<$n;$i++) { if($i%$step===0) $a[$i]=false; }
 $count=0; for($i=0;$i<$n;$i++) if($a[$i]) $count++;
 return [$count,$a,$i];
}
foreach([0,1,2,3,7,23,100] as $n) foreach([1,2,5] as $step) echo json_encode(region($n,$step)),"\n";
$a=array_fill(0,6,true);$copy=$a;
for($i=0;$i<6;$i++) $a[$i]=false;
echo json_encode([$a,$copy]),"\n";
$a=array_fill(0,5,true);$alias=&$a;
for($i=0;$i<5;$i++) $a[$i]=false;
echo json_encode([$a,$alias]),"\n";
$a=array_fill(0,4,true);$r=&$a[2];
for($i=0;$i<4;$i++) $a[$i]=false;
var_dump($r);
$a=[true,true];$v=PHP_INT_MAX;
for($i=0;$i<2;$i++) {$a[$i]=false;$v++;}
echo json_encode([$a,is_float($v)]),"\n";
$a=[true];$i=0;set_error_handler(function($n,$s){echo "warning:$s\n";});
for($i=0;$i<2;$i++) $a[$i]=false;
echo json_encode($a),"\n";restore_error_handler();
$a=[true,false,true];$n=0;
for($i=0;$i<3;$i++) {if($i==1)continue;$n+=(int)$a[$i];}
var_dump($n,$i);
class LoopReadonly { public readonly array $a; function __construct() {$this->a=[true,true];} }
$o=new LoopReadonly;$a=$o->a;for($i=0;$i<2;$i++)$a[$i]=false;echo json_encode([$a,$o->a]),"\n";
