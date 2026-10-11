<?php
ini_set('memory_limit','4M');
$s=str_repeat('a',1500000);
for($i=0;$i<3;$i++) {$t=substr($s,0,1300000);echo strlen($t),"\n";}
