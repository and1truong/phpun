<?php
$hay=str_repeat('a',8192)."b\0\xfftail";
$needle=str_repeat('a',257).'b';
foreach([0,1,4096,8192,8199] as $off) {var_dump(strpos($hay,$needle,$off));}
echo bin2hex(strstr($hay,"b\0\xff")),"\n";
echo substr_count('aaaaa','aa'),"\n";
$n=(int)($argv[1]??300);$sum=0;
for($i=0;$i<$n;$i++) {$sum+=strpos($hay,$needle);$sum+=strpos($hay,str_repeat('a',257).'c')===false;}
echo 'RESULT ',$sum,"\n";
