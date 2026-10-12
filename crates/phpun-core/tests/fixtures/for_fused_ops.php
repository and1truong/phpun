<?php
// Fused for-loop ops: JumpCmp (var-var cond) + IncJump (single var
// inc/dec step). Exercises slot kinds, break/continue, nesting and
// the diagnostics that survive the fusion (undef warn, string inc).
$n = 5;
$s = 0;
for ($i = 0; $i < $n; $i++) {
    $s += $i;
}
echo "lt $s\n";
$s = 0;
for ($i = 10; $i > $n; $i--) {
    $s += $i;
}
echo "gt $s\n";
$j = 0;
$s = 0;
for ($i = 0; $i <= $j; $i++) {
    $s += 1;
}
echo "le $s $i\n";
for ($i = 0; $i < 10; $i++) {
    if ($i == 3) {
        continue;
    }
    if ($i == 6) {
        break;
    }
    echo "$i ";
}
echo "\n";
for ($i = 0; $i < $undef_x; $i++) {
    echo "never\n";
}
echo "done\n";
$x = "a";
for ($i = 0; $i < 2; $i++) {
    $x++;
}
echo "str $x\n";
$t = 0;
for ($i = 0; $i < 3; $i++) {
    for ($j = 0; $j < 2; $j++) {
        $t++;
    }
}
echo "nest $t\n";
// Global-bound slot kind: &$g aliased — cond still fuses, writes
// ride the cell path.
$g = 3;
$h = &$g;
$s = 0;
for ($i = 0; $i < $h; $i++) {
    $s += $g;
}
echo "ref $s $g\n";
