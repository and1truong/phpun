<?php
// Compiled foreach over an Iterator whose methods replace $GLOBALS['v']
// between iterations: the body's cached slot cell must rebind.
for(;;){break;}
class It implements Iterator {
    private int $i = 0;
    public function current(): mixed { return [10,20,30][$this->i] ?? null; }
    public function key(): mixed { return $this->i; }
    public function next(): void { $this->i++; }
    public function valid(): bool {
        if ($this->i === 1) unset($GLOBALS['v']);
        return $this->i < 3;
    }
    public function rewind(): void {}
}
foreach (new It as $v) { echo $v, "\n"; }
function gen() { for($j=0;$j<3;$j++) yield $j*5; }
foreach (gen() as $v) { echo $v, "\n"; }
