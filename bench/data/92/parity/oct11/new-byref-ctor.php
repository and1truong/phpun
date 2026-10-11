<?php
class ReferenceConstructor { function __construct(public int &$n) { $n++; } }
$n = 1;
for ($i = 0; $i < 1; $i++) { $o = new ReferenceConstructor($n); }
var_dump($n, $o->n);
$o->n = 9;
var_dump($n);
