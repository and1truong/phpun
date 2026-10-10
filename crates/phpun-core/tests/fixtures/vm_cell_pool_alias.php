<?php
$held = [];
function keep(&$x) { global $held; $held[] =& $x; }
function escape($x) { keep($x); return $x; }
function scalar($x) { return $x + 1; }
function trace_cell($x) { return debug_backtrace(); }
for ($i = 0; $i < 3; $i++) { echo escape($i), ':', scalar(100), "\n"; }
echo json_encode($held), "\n";
$trace = trace_cell(7);
for ($i = 0; $i < 10; $i++) { scalar($i); }
echo $trace[0]['args'][0], ':', json_encode($held), "\n";
$held[0] = 50;
echo json_encode($held), "\n";
