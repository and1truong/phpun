<?php
class ConstructorMutator { function __construct() { global $y; unset($GLOBALS['x']); $GLOBALS['x'] =& $y; } }
$x=2;$y=9;
for($i=0;$i<1;$i++) { $o=new ConstructorMutator(); $seen=$x; }
var_dump($seen,$x);
