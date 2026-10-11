<?php
class TeardownItem { function __destruct() { echo "item\n"; } }
class TeardownHost {
 public int $x = 4;
 function run(int $n): int { return $this->x + $n; }
 function arg(TeardownItem $item): int { return $this->x; }
 function __destruct() { echo "host\n"; }
}
function invoke(TeardownHost $o, int $n): int { return $o->run($n); }
$h = new TeardownHost; $wh = WeakReference::create($h);
echo invoke($h, 2), "\n";
var_dump($wh->get() !== null);
$i = new TeardownItem; $wi = WeakReference::create($i);
echo $h->arg($i), "\n";
unset($i); var_dump($wi->get());
try { invoke($h, 'bad'); } catch (TypeError $e) { echo get_class($e), "\n"; }
echo $h->run(n: 3), "\n";
unset($e); unset($h); var_dump($wh->get());
