<?php
function replaceThree($f,$r,$s) { return str_replace($f,$r,$s); }
function replaceCI($f,$r,$s) { return str_ireplace($f,$r,$s); }
function caseUpper($s) { return strtoupper($s); }
function caseLower($s) { return strtolower($s); }
foreach (["a\0b\xffab",'',"AaéZ"] as $s) {
 foreach (['','a','ab',"\0","\xff"] as $f) {
  foreach (['','X',"a\0"] as $r) echo bin2hex(replaceThree($f,$r,$s)),':',bin2hex(replaceCI($f,$r,$s)),':';
 }
 echo "\n",bin2hex(caseUpper($s)),':',bin2hex(caseLower($s)),"\n";
}
$c=99; var_dump(str_replace('a','X','abba',$c),$c);
var_dump(str_replace(['a','b'],['X','Y'],['ab','ba'],$c),$c);
echo str_replace(search:'a',replace:'X',subject:'ab'),"\n";
class StringProbe { function __toString(): string { echo "cast\n"; return 'abc'; } }
echo replaceThree('a','X',new StringProbe),"\n";
try { caseUpper([]); } catch(Throwable $e) { echo $e->getMessage(),"\n"; }
