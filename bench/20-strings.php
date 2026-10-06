<?php
// String builtins: concat, str_repeat, str_replace, substr, strlen.
// argv[1]: repetitions (default 20).
$reps = (int)($argv[1] ?? 20);

$acc = 0;
for ($r = 0; $r < $reps; $r++) {
    $s = str_repeat("The quick brown fox jumps over the lazy dog. ", 2000); // ~90KB
    $acc += strlen($s);

    $t = $s;
    for ($i = 0; $i < 20; $i++) {
        $t = str_replace("fox", "wolf", $t);
        $t = str_replace("dog", "cat", $t);
    }
    $acc += strlen($t);

    $u = "";
    for ($i = 0; $i < 4000; $i++) {
        $u .= substr($s, $i % 40, 4);
    }
    $acc += strlen($u);
    $acc += substr_count($t, "wolf");
}

echo "RESULT $acc\n";
