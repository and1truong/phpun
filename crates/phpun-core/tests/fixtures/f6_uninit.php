<?php
$r = new ReflectionClass('ArrayObject');
$ao = $r->newInstanceWithoutConstructor();
var_dump(count($ao));
$ao['x'] = 5;
var_dump(count($ao));
var_dump($ao['x']);
