<?php
$n=(int)($argv[1]??29);
function wf($n) { if ($n < 2) return $n; $n--; return wf($n) + wf($n - 1); }
echo 'RESULT ', wf($n), "\n";
