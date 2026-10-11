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

function native_algebra() {
 $a=array_fill(0,40,true); $n=0; $negative=-21;
 for($i=0;$i<40;$i++) {
  if(!($i%3)) {$a[$i]=false;} else {$n=$n+$i*2-3;}
  $negative++;
 }
 return [$a,$n,$negative,$i];
}
echo json_encode(native_algebra()),"\n";
function native_overflow() {
 $a=array_fill(0,2,true);$v=PHP_INT_MAX;
 for($i=0;$i<2;$i++) {$a[$i]=false;$v++;}
 return [$a,is_float($v),$i];
}
echo json_encode(native_overflow()),"\n";
function native_bounds() {
 $a=array_fill(0,2,true);
 for($i=0;$i<3;$i++) $a[$i]=false;
 return [$a,$i];
}
echo json_encode(native_bounds()),"\n";
function native_bool_compare() {
 $a=array_fill(0,4,true);$n=0;
 for($i=0;$i<4;$i++) if($a[$i]===true) $n++;
 return $n;
}
var_dump(native_bool_compare());
