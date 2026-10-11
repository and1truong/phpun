<?php
$s="a\0B\xffcd";$acc=0;
for($i=0;$i<3;$i++){ $acc+=strlen(substr($s,1,3)); $upper=strtoupper($s); }
echo $acc," ",bin2hex($upper),"\n";
function pure_root_mutate() { global $y; unset($GLOBALS['x']); $GLOBALS['x'] =& $y; return 4; }
$x=2;$y=9;$v=sqrt(pure_root_mutate());$seen=$x;var_dump($v,$seen,$x);
try { strlen([]); } catch(TypeError $e) { echo $e->getMessage(),"\n"; }
$count=0;var_dump(str_replace('c','C',$s,$count),$count);
