<?php
$src = new ArrayObject([1,2,3]);
$dst = new ArrayObject($src);
var_dump(count($dst));
var_dump($dst->getArrayCopy());
foreach($dst as $k=>$v) echo "$k=>$v ";
echo "\n";
$it = new ArrayIterator([4,5]);
$dst2 = new ArrayObject($it);
var_dump(count($dst2));
var_dump($dst2->getArrayCopy());
$o = new stdClass; $o->p=1; $o->q=2;
$dst3 = new ArrayObject($o);
var_dump(count($dst3));
var_dump($dst3->getArrayCopy());
var_dump($dst3['p']);
