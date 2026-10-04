<?php
$src = new ArrayObject([1,2,3]);
$dst = new ArrayObject($src);
$dst[0] = 99;
$dst['new'] = 7;
var_dump($src[0], count($src));
var_dump($dst[0], count($dst));
$src['s'] = 5;
var_dump($dst['s']);
// exchangeArray with ArrayObject
$e = new ArrayObject(['a'=>1]);
$e2 = new ArrayObject([9]);
$e2->exchangeArray($e);
var_dump($e2->getArrayCopy());
