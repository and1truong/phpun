<?php
$a=substr('abcdef',1,4);$b=$a;$a[0]='X';echo $a,'|',$b,"\n";
echo bin2hex(substr("a\0bz",1,2)),"\n";
$t=substr(str_repeat('x',50),10,23);$alias=$t;$t.='y';echo strlen($t),':',strlen($alias),':',substr($t,-1),"\n";
$utf=substr("xéz",1,2);var_dump(preg_match('//u',$utf));$utf[0]="\xff";var_dump(preg_match('//u',$utf));echo preg_last_error(),"\n";
for($i=0;$i<3;$i++){ $u='';$u.=substr('abcd',1,2);echo $u,"\n";}
