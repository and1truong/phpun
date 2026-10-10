<?php
error_reporting(0);
function remove_number() { unset($GLOBALS['number']); }
$number = 7;
for ($i = 0; $i < 1; $i++) { remove_number(); $number++; }
echo $number, "\n";
$number = 9;
$alias =& $number;
for ($i = 0; $i < 1; $i++) { remove_number(); $number++; }
echo $number, ':', $alias, "\n";
