<?php
$mode = $argv[1] ?? 'norm';
$n = (int)($argv[2] ?? 5000);
class ProbePoint {
    public function __construct(public float $x, public float $y) {}
    public function norm(): float { return sqrt($this->x * $this->x + $this->y * $this->y); }
    public function scaled(float $f): ProbePoint { return new ProbePoint($this->x * $f, $this->y * $f); }
}
$points = [];
for ($i = 0; $i < $n; $i++) { $points[] = new ProbePoint($i % 100, ($i * 7) % 100); }
$acc = 0.0;
foreach ($points as $p) {
    switch ($mode) {
    case 'ctor': $acc += $p->x; break;
    case 'norm': $acc += $p->norm(); break;
    case 'scaled': $q = $p->scaled(2.0); $acc += $q->x; break;
    default: throw new InvalidArgumentException($mode);
    }
}
echo 'RESULT ', (int)$acc, "\n";
