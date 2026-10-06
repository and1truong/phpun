<?php
// Object instantiation + method/property dispatch.
// argv[1]: repetitions (default 5), argv[2]: objects per rep (default 5000).
$reps = (int)($argv[1] ?? 5);
$n = (int)($argv[2] ?? 5000);

class Point {
    public function __construct(
        private float $x = 0.0,
        private float $y = 0.0,
    ) {}

    public function norm(): float {
        return sqrt($this->x * $this->x + $this->y * $this->y);
    }

    public function scaled(float $f): Point {
        return new Point($this->x * $f, $this->y * $f);
    }
}

$acc = 0.0;
for ($r = 0; $r < $reps; $r++) {
    $pts = [];
    for ($i = 0; $i < $n; $i++) {
        $pts[] = new Point($i % 100, ($i * 7) % 100);
    }
    foreach ($pts as $p) {
        $acc += $p->norm();
    }
    $pts = null; // force GC/destructor pass between reps
}

echo "RESULT " . (int)$acc . "\n";
