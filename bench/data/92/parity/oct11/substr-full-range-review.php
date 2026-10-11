<?php
ini_set('memory_limit','4M');
$s=str_repeat('a',1500000);
for($i=0;$i<3;$i++) {$s=substr($s,0);echo strlen($s),"\n";}
$s=substr($s,-1500000);echo strlen($s),"\n";
$s=substr($s,-3000000,3000000);echo strlen($s),"\n";
$s=substr($s,0,null);echo strlen($s),"\n";
echo substr(1234,0),"\n";
