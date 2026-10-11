<?php
class Alive { function __destruct() {echo "D\n";} }
$a=new Alive;$b=$a;unset($a);echo "S\n";unset($b);
class Cycle {public $next;function __destruct(){echo "C\n";}}
$x=new Cycle;$x->next=$x;unset($x);echo gc_collect_cycles(),"\n";
