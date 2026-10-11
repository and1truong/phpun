<?php
$a=json_decode('{"name":"first","1":true,"name":"last","nested":{"x":2},"01":"s","-2":"n"}', true);
echo json_encode($a), "\n";
$b=$a;$b['name']='copy';echo $a['name'],' ',$b['name'],"\n";
$r=&$a['name'];$r='ref';echo $a['name'],"\n";
foreach ($a as $k=>&$v) { if ($k===1) $v=false; } unset($v);
echo json_encode($a), "\n";
unset($a['01']);$a[]='append';echo json_encode($a),"\n";
$c=json_decode('{"2":2,"0":0,"a":4}',true);ksort($c);echo json_encode($c),"\n";
$d=json_decode('{"0":0,"1":1,"2":2}',true);echo json_encode(array_splice($d,1,1,[7])),':',json_encode($d),"\n";
$large=[];for($i=0;$i<70;$i++)$large['k'.$i]=$i;$d=json_decode(json_encode($large),true);echo count($d),' ',$d['k60'],"\n";
class MapReadonly { public readonly array $a; function __construct() { $this->a=json_decode('{"x":1}',true); } }
$o=new MapReadonly;try{$o->a['x']=2;}catch(Error $e){echo get_class($e),"\n";}
