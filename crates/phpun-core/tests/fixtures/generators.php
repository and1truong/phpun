<?php
function seq() { yield 1; yield 'a' => 2; yield 3; return 'done'; }
$g = seq();
var_dump($g instanceof Iterator, $g instanceof Generator);
foreach ($g as $k => $v) echo "$k=$v ";
echo "\n", $g->getReturn(), "\n";
function spliced() { yield from [10, 20]; yield 'x' => 7; yield 30; }
foreach (spliced() as $k => $v) echo "$k>$v ";
echo "\n";
$m = seq();
echo $m->current(), '/', $m->key(), "\n";
$m->next();
echo $m->current(), '/', $m->key(), "\n";
echo count(iterator_to_array(spliced(), false)), "\n";
try { foreach ($g as $v) {} } catch (Exception $e) { echo "EX:", $e->getMessage(), "\n"; }
function sendable() { $x = yield 1; yield $x + 1; yield 99; }
$s = sendable();
echo $s->send(10), "\n";
echo $s->current(), "\n";
class Lazy { public function items() { yield 'lazy-ok'; } }
foreach ((new Lazy())->items() as $v) echo $v, "\n";
