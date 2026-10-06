<?php
// PCRE builtins: match, match-all, replace over a repeated corpus.
// argv[1]: repetitions (default 30).
$reps = (int)($argv[1] ?? 30);

$text = str_repeat(
    "Contact alice@example.com or bob+tag@mail.org before 2026-10-06. Order #4512 costs $1,299.99. ",
    400
); // ~37KB

$acc = 0;
for ($r = 0; $r < $reps; $r++) {
    preg_match_all('/[\w.+-]+@[\w-]+\.[\w.]+/', $text, $m);
    $acc += count($m[0]);

    preg_match_all('/\d{4}-\d{2}-\d{2}/', $text, $d);
    $acc += count($d[0]);

    $t = preg_replace('/\$\d{1,3}(?:,\d{3})*\.\d{2}/', '<price>', $text);
    $acc += strlen($t);

    $acc += preg_match('/order #(\d+)/i', $text, $one) ? (int)$one[1] : 0;
}

echo "RESULT $acc\n";
