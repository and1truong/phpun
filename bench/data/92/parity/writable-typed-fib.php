<?php
$n=(int)($argv[1]??29);
function wtf(int $n): int { if ($n < 2) return $n; $n--; return wtf($n) + wtf($n - 1); }
echo 'RESULT ', wtf($n), "\n";
