<?php
$mode = $argv[1] ?? 'slice';
$n = (int)($argv[2] ?? 4000);
$s = str_repeat('The quick brown fox jumps over the lazy dog. ', 2000);
$acc = 0;
$u = '';
for ($i = 0; $i < $n; $i++) {
    switch ($mode) {
    case 'repeat': $acc += strlen(str_repeat('x', 2000)); break;
    case 'replace': $acc += strlen(str_replace('fox', 'wolf', $s)); break;
    case 'slice': $acc += strlen(substr($s, $i % 40, 4)); break;
    case 'concat': $u .= 'word'; break;
    default: throw new InvalidArgumentException($mode);
    }
}
echo 'RESULT ', $acc + strlen($u), "\n";
