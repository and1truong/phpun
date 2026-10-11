<?php
class EmptyPromoted { function __construct(public float $n=1.5) {} }
class InheritedEmpty extends EmptyPromoted { function nothing():void {} }
function create_empty($n) { return new InheritedEmpty($n); }
for($i=0;$i<2;$i++) { $o=create_empty($i);echo $o->n,"\n";var_dump($o->nothing()); }
$o=new InheritedEmpty;echo $o->n,"\n";
try { create_empty([]); } catch(TypeError $e) { echo $e->getMessage(),"\n"; }
