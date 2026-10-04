<?php
$e = new ArrayObject(['a'=>1]);
$e2 = new ArrayObject([9]);
$e2->exchangeArray($e);
$e2['b'] = 2;
var_dump($e['b'], count($e));
// plain object exchange: does $e2 bind to $o's props live?
$o = new stdClass; $o->p = 1;
$e3 = new ArrayObject;
$e3->exchangeArray($o);
$e3['q'] = 9;
var_dump($o->q ?? 'nope');
$o->r = 3;
var_dump($e3['r']);
// ARRAY_AS_PROPS object backed: prop read/write
$e4 = new ArrayObject($o, ArrayObject::ARRAY_AS_PROPS);
$e4->z = 11;
var_dump($o->z);
var_dump($e4->p);
