<?php
function side_effect() { echo 'side '; return 1; }
function call_missing() { return no_such_function(side_effect()); }
try { call_missing(); } catch (Error $e) { echo 'caught '; }
function add($x, $y) { return $x + $y; }
function nested_calls() { return add(add(1, 2), add(3, 4)); }
var_dump(nested_calls());
