<?php
$s="fox\0DOG\xfffox"; $alias=$s;
$r=str_replace('fox','wolf',$s,$count); echo bin2hex($r),' ',$count,' ',bin2hex($s),"\n";
$r=str_ireplace(['FOX','dog'],['X','Y'],$s,$count); echo bin2hex($r),' ',$count,"\n";
$r=str_replace(['ab','x'],['x',''],'abab',$count); var_dump($r,$count);
$r=str_replace(['a','b'],['X'],['k'=>'ab',4=>'ba'],$count); var_dump($r,$count);
$r=str_replace(['','absent'],['x','y'],$s,$count); echo bin2hex($r),' ',$count,' ',bin2hex($alias),"\n";
$r=str_replace('aa','a','aaaaa',$count); var_dump($r,$count);
$r=str_replace('o','O',$s,$s); echo bin2hex($r),' ',$s,"\n";
$r=str_replace("\0","\xff","a\0\0b",$count); echo bin2hex($r),' ',$count,"\n";
var_dump(strpos("\xffa\xff","\xff",1), strpos('a','abc'), strpos('','a'));
class Text { public function __toString(): string { echo "cast\n"; return 'fox'; } }
$r=str_replace(new Text(), new Text(), new Text(),$count); var_dump($r,$count);
class BadText { public function __toString(): string { throw new Exception('cast-error'); } }
$count=99;
try { str_replace('x','y',new BadText(),$count); } catch(Throwable $e) { echo $e->getMessage()," ",$count,"\n"; }
