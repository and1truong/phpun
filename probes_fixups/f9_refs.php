<?php
$r = 1;
$ao = new ArrayObject(['k'=>&$r]);
$r = 99;
var_dump($ao['k']);
$ao['k'] = 5;
var_dump($r);
