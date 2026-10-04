<?php
$a = [3, 1, 2];
$b = ['c', 'a', 'b'];
array_multisort($a, $b);
echo json_encode($a), json_encode($b), "\n";
$a = ['img10', 'img2', 'img1'];
array_multisort($a, SORT_ASC, SORT_NATURAL);
echo json_encode($a), "\n";
$a = ['B', 'a', 'c'];
array_multisort($a, SORT_STRING | SORT_FLAG_CASE);
echo json_encode($a), "\n";
$a = [2, 1, 3];
$b = [10, 20];
try { array_multisort($a, $b); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
$a = [2, 1];
try { array_multisort($a, SORT_ASC, SORT_DESC); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { array_multisort($a, 99); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
$a = [2, 1];
array_multisort($a, SORT_NUMERIC, SORT_DESC);
echo json_encode($a), "\n";
$m = ['x' => 2, 'y' => 1, 'z' => 3];
array_multisort($m);
echo json_encode($m), "\n";
$nat = ['f10', 'f9', 'f1a', 'f1'];
natsort($nat);
echo json_encode($nat), "\n";
$nc = ['F10', 'f9', 'F1a', 'f1'];
natcasesort($nc);
echo json_encode($nc), "\n";
