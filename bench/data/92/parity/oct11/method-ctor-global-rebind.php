<?php
function replaceGlobal() { unset($GLOBALS['x']); $GLOBALS['x'] = 9; return 1; }
class Mutator { function run() { return replaceGlobal(); } }
class ConstructorMutator { function __construct() { unset($GLOBALS['x']); $GLOBALS['x'] = 9; } }
$x=2; $o=new Mutator();
for ($i=0; $i<1; $i++) { $o->run(); $seen=$x; }
var_dump($seen,$x);
$x=2;
for ($i=0; $i<1; $i++) { $o=new ConstructorMutator(); $seen=$x; }
var_dump($seen,$x);
