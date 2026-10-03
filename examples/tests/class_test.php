<?php
class Stack {
    private array $items = [];
    public function push($v) { $this->items[] = $v; return $this; }
    public function pop() { return array_pop($this->items); }
    public function count(): int { return count($this->items); }
}
$s = (new Stack)->push(1)->push(2);
eq($s->count(), 2, 'count');
eq($s->pop(), 2, 'pop');
eq($s->count(), 1, 'count after pop');
echo "all class tests ok\n";
