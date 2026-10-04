<?php
$ao = new ArrayObject(['b'=>2,'a'=>1]);
var_dump(method_exists($ao,'uasort'));
var_dump(method_exists($ao,'uksort'));
var_dump(method_exists($ao,'getIteratorClass'));
var_dump(method_exists($ao,'setIteratorClass'));
var_dump(method_exists($ao,'serialize'));
var_dump(method_exists($ao,'unserialize'));
var_dump($ao->getIteratorClass());
$ao2 = new ArrayObject([], 0, 'RecursiveArrayIterator');
var_dump(get_class($ao2->getIterator()));
var_dump(serialize($ao));
$s = serialize($ao);
var_dump(unserialize($s));
