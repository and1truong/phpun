<?php
// Shared bootstrap — loaded before each test file.
function ok($cond, string $name) {
    if (!$cond) {
        fwrite(STDERR, "not ok - $name\n");
        exit(1);
    }
}
function eq($a, $b, string $name) {
    if ($a !== $b) {
        fwrite(STDERR, "not ok - $name: expected " . var_export($b, true)
            . ", got " . var_export($a, true) . "\n");
        exit(1);
    }
}
