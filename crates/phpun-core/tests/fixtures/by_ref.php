<?php
function &get_arr() {
    static $a = [1];
    return $a;
}
$ref =& get_arr();
$ref[] = 2;
var_export(get_arr());
echo "\n";
function bump(&$x) { $x++; }
$n = 1; bump($n); bump($n);
echo $n, "\n";
