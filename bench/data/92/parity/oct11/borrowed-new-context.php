<?php
class FloatProof {function __construct(public float $x, private float $y) {} function vals(){return [$this->x,$this->y];}}
for($i=0;$i<5;$i++) {$o=new FloatProof($i,2.5);echo json_encode($o->vals()),"\n";}
$r=&$o->x;$r=9;echo json_encode($o->vals()),"\n";
$c=clone $o;$c->x=8;echo json_encode([$o->vals(),$c->vals()]),"\n";
$o=new FloatProof(y:4,x:3);echo json_encode($o->vals()),"\n";
class ChildProof extends FloatProof {} $o=new ChildProof(6,7);echo json_encode($o->vals()),"\n";
class BodyProof {function __construct(public float $x) {echo "body:",$x,"\n";}}
for($i=0;$i<2;$i++) new BodyProof($i);
try {new FloatProof([],1);}catch(TypeError $e){echo "type\n";}
try {new FloatProof(1);}catch(ArgumentCountError $e){echo "argc\n";}
