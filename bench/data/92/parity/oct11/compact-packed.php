<?php
$a = array_fill(0, 4, true); $a[0] = $a[1] = false;
$b = $a; $b[2] = false;
$c = array_fill(0, 4, true); $ref =& $c[0]; $ref = false;
$count = 0; for ($i = 0; $i < 4; $i++) { if ($a[$i]) $count++; }
var_dump($count, $b[2], $c[0]);
foreach ([null,false,7,1.5,"a\0b\xff"] as $v) {
 $a=array_fill(0,3,$v); $b=$a; $b[1]=9;
 echo json_encode([$a,$b],JSON_PARTIAL_OUTPUT_ON_ERROR),"\n";
 $a[7]='sparse';$a['key']='mixed';echo json_encode($a,JSON_PARTIAL_OUTPUT_ON_ERROR),"\n";
 $ref=&$b[0];$ref='ref';echo json_encode($b,JSON_PARTIAL_OUTPUT_ON_ERROR),"\n";
}
