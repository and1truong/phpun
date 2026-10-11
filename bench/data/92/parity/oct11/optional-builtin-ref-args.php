<?php
function replace_value($s) { return str_replace('a', 'x', $s); }
echo bin2hex(replace_value("a\0a\xff")), "\n";
echo str_ireplace('AB', 'x', 'abABa'), "\n";
$count = 0; $alias =& $count;
echo str_replace('a', 'x', 'aba', $count), ' ', $alias, "\n";
try { str_replace('a', 'x', 'aba', 0); } catch (Error $e) { echo get_class($e), ':', $e->getMessage(), "\n"; }
echo str_replace(subject: 'aba', search: 'a', replace: 'x', count: $count), ' ', $alias, "\n";
echo str_replace(...['a', 'x', 'aba']), "\n";
echo preg_match('/a/', 'aba'), "\n";
$matches = []; echo preg_match('/a/', 'aba', $matches), ' ', json_encode($matches), "\n";
echo preg_replace('/a/', 'x', 'aba'), "\n";
echo preg_replace('/a/', 'x', 'aba', -1, $count), ' ', $alias, "\n";
echo is_callable('replace_value'), "\n";
$name = null; echo is_callable('replace_value', false, $name), ' ', $name, "\n";
try { str_replace('a', 'x'); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
try { str_replace('a', 'x', new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
function optional_ref($x, &$out = null) { $out = 7; return func_num_args(); }
echo optional_ref(4), "\n"; $out = 0; echo optional_ref(4, $out), ' ', $out, "\n";
function variadic_refs(&$a, &...$rest) { $a++; foreach ($rest as &$x) $x++; }
$a=1;$b=2;$c=3;variadic_refs($a,$b,$c);echo "$a $b $c\n";
