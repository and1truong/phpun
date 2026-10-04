<?php
$o = new stdClass; $o->p = 1;
$ao = new ArrayObject($o);
$o->p = 5;
var_dump($ao['p']);
$o->r = 3;
var_dump($ao['r'] ?? 'miss');
$ao['z'] = 9;
var_dump($o->z ?? 'miss');
$cp = $ao->getArrayCopy(); $cp['q'] = 7;
var_dump($ao->getArrayCopy());
