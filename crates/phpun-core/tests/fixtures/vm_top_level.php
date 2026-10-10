<?php
namespace Top;
function read_global() { global $sum; return $sum; }
function strlen($s) { return 99; }
class Lifetime { public function __destruct() { echo "destroy\n"; } }
$sum = 0;
for ($i = 0; $i < 3; $i++) {
    $sum += $i;
    echo read_global(), ':', __FUNCTION__, ':', count(debug_backtrace()), "\n";
}
include __DIR__ . '/vm-top-level/scope.inc';
echo $sum, ':', $GLOBALS['from_include'], ':', strlen('abc'), ':', \strlen('abc'), "\n";
$alias =& $sum;
for ($i = 0; $i < 2; $i++) { $alias++; }
echo read_global(), "\n";
$o = new Lifetime();
for ($i = 0; $i < 1; $i++) { unset($o); echo "after unset\n"; }
$shutdown = new Lifetime();
echo "end\n";
