<?php
$ao = new ArrayObject(['a'=>1]);
$ao->setFlags(ArrayObject::ARRAY_AS_PROPS);
$ao->p = 2;
var_dump($ao->getArrayCopy());
var_dump(count($ao));
var_dump($ao->p);
