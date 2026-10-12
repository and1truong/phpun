<?php
// Inline foreach ops: FeInit binds the source once (cursor when the
// table is sole-owned and unwritten, snapshot otherwise), FeNext
// fetches into slots and the body ops run inline. Covers the shared-
// table, body-write, key, break/continue, nested, iterator-replay
// and by-ref-canonical edges.
$pts = [1, 2, 3];
$acc = 0;
foreach ($pts as $p) {
    $acc += $p;
}
echo "sum $acc\n";

$share = [1, 2];
$share2 = $share;
foreach ($share as $v) {
    $share2[] = $v * 10;
}
echo "shared " . count($share) . " " . count($share2) . "\n";

$w = [1, 2, 3];
foreach ($w as $v) {
    $w[] = $v * 10;
}
echo "wrote " . count($w) . "\n";

$kv = ['x' => 1, 'y' => 2];
$s = '';
foreach ($kv as $k => $v) {
    $s .= "$k$v";
}
echo "kv $s\n";

$out = [];
foreach ([1, 2, 3, 4] as $v) {
    if ($v == 2) {
        continue;
    }
    if ($v == 4) {
        break;
    }
    $out[] = $v;
}
echo "ctl " . implode(',', $out) . "\n";

$out = [];
foreach ([1, 2] as $v) {
    foreach ([3, 4] as $w) {
        if ($w == 4) {
            break 2;
        }
        $out[] = "$v$w";
    }
}
echo "brk2 " . implode(',', $out) . "\n";

$it = new ArrayIterator([7, 8]);
$acc = 0;
foreach ($it as $v) {
    $acc += $v;
}
echo "iter $acc\n";

$refs = [1, 2, 3];
foreach ($refs as &$v) {
    $v *= 10;
}
unset($v);
echo "byref " . implode(',', $refs) . "\n";

function rowsum(array $rows): int
{
    $t = 0;
    foreach ($rows as $r) {
        $t += $r;
    }
    return $t;
}
echo "fn " . rowsum([4, 5, 6]) . "\n";

foreach ([[1, 'a'], [2, 'b']] as [$n, $ch]) {
    echo "list $n$ch\n";
}
