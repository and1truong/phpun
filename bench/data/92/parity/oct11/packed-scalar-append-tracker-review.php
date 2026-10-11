<?php
$a=[];for($i=0;$i<50;$i++)$a[]=$i*3;
$b=$a;$b[]=7;
$r=&$a[4];$r=19;$a[]=20;echo json_encode([$a,$b]),"\n";
$a=[];$a[]=1;$a[]=2;unset($a[1]);$a[]=3;$a['key']=4;$a[]=5;echo json_encode($a),"\n";
$x=[];$alias=&$x;$x[]=1;$alias[]=2;echo json_encode($x),"\n";
$x=[];$x[]=1;$x[]='a';$x[]=[];$x[]=null;echo json_encode($x),"\n";
$x=[];$x[]=1;foreach($x as &$v){if(count($x)<4)$x[]=count($x);}unset($v);echo json_encode($x),"\n";
class ReadOnlyAppend {public readonly array $a;function __construct(){$this->a=[];}}
$o=new ReadOnlyAppend;try{$o->a[]=1;}catch(Error $e){echo 'readonly',"\n";}
$x=[];$x[]=$x=[];echo json_encode($x),"\n";
