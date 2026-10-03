<?php
function test($a = 'a', $b = 'b', $c = 'c') {
    echo "a: $a, b: $b, c: $c\n";
}
test(b: 'B', a: 'A');
test('x', c: 'C');
test(...['y', 'c' => 'Z']);
