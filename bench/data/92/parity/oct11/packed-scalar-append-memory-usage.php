<?php
$a=[];
$start=memory_get_usage();
for($i=0;$i<64;$i++) { $a[]=$i; }
$grown=memory_get_usage();
$b=$a;
$beforeCow=memory_get_usage();
$b[]=64;
$afterCow=memory_get_usage();
echo 'RESULT ', $grown-$start, ':', $afterCow-$beforeCow, ':', count($a), ':', count($b), "\n";
