<?php
class PromotedHot {
    function __construct(private float $x, private float $y) {}
}
$n = (int)($argv[1] ?? 20000);
for ($i=0; $i<$n; ++$i) { $p = new PromotedHot($i % 100, ($i * 7) % 100); }
echo "RESULT $n\n";
