<?php
namespace ScalarKernelProbe {
 function sqrt($x) { return 41; }
 function substr($s,$o,$n=1) { return 'override'; }
 function run() { echo sqrt(4.0), ' ', substr('abc',0,1), "\n"; }
}
namespace {
 function sliceProbe($s,$o,$n) { return substr($s,$o,$n); }
 ScalarKernelProbe\run();
 foreach (["a\0b\xff",'','abcd'] as $s) {
  foreach ([PHP_INT_MIN,-8,-1,0,1,8,PHP_INT_MAX] as $o) {
   foreach ([PHP_INT_MIN,-8,-1,0,1,8,PHP_INT_MAX,null] as $n) {
    echo bin2hex(sliceProbe($s,$o,$n)), ':';
   }
  }
  echo "\n";
 }
 echo bin2hex(substr(string:"a\0b",offset:1,length:2)), "\n";
 $s='1234'; $alias=&$s; echo substr($alias,1,2),' ', $s, "\n";
 foreach ([0,4,9,0.0,-0.0,4.0,-1.0,INF,-INF,NAN] as $n) {
  $x=sqrt($n); echo is_nan($x)?'nan':(is_infinite($x)?'inf':bin2hex(pack('d',$x))), ':';
 }
 echo "\n";
 foreach ([[[],0,1],['abc',[],1],['abc',0,[]]] as $args) {
  try { substr(...$args); } catch (\Throwable $e) { echo $e->getMessage(), "\n"; }
 }
 try { substr('abc'); } catch (\Throwable $e) { echo $e->getMessage(), "\n"; }
 try { substr('abc',0,1,4); } catch (\Throwable $e) { echo $e->getMessage(), "\n"; }
 echo substr('abc','1','2'), "\n";
 foreach (["a\0b\xff", '', 'abcd'] as $s) {
  foreach ([-8,-1,0,1,8] as $o) {
   foreach ([null,0,1,-1,8] as $n) echo bin2hex(substr_replace($s,'X',$o,$n)), ':';
  }
  echo "\n";
 }
 echo substr_replace(string:'abcd',replace:'X',offset:1,length:null), "\n";
 foreach (['strspn','strcspn'] as $fn) {
  foreach ([-8,-1,0,1,8] as $o) {
   foreach ([null,0,1,-1,8] as $n) echo $fn("abba!\0b",'ab',$o,$n), ':';
  }
  echo "\n";
 }
}
