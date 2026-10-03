<?php
eq(strtoupper('php'), 'PHP', 'strtoupper');
eq(implode(',', [1, 2, 3]), '1,2,3', 'implode');
eq(substr('hello world', 6), 'world', 'substr');
ok(str_contains('needle in haystack', 'needle'), 'str_contains');
echo "all string tests ok\n";
