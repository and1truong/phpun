<?php
ini_set("memory_limit", "4M");
$a=[];
for($i=0;$i<200000;$i++) { $a[]=$i; }
echo count($a), "\n";
