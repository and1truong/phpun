<?php
set_error_handler(function($level, $msg) { echo $msg, '\n'; });
function before_assignment($assign) { if ($assign) { $x = 1; } return $x; }
function compound_before_assignment() { $x += 2; return $x; }
function increment_before_assignment() { return ++$x; }
function initialize_ref(&$x) { $x = 9; }
function ref_before_assignment() { initialize_ref($x); $later = $x; $x = 1; return $later; }
var_dump(before_assignment(false), before_assignment(true), compound_before_assignment(), increment_before_assignment(), ref_before_assignment());
