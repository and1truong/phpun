<?php
// JSON encode/decode round trips over a realistic payload.
// argv[1]: repetitions (default 30).
$reps = (int)($argv[1] ?? 30);

$payload = [];
for ($i = 0; $i < 3000; $i++) {
    $payload[] = [
        "id" => $i,
        "name" => "user-$i",
        "email" => "user$i@example.com",
        "active" => $i % 3 === 0,
        // +0.1 keeps the value non-whole: phpun emits whole floats as "5.0"
        // where zend emits "5" (known divergence, not what this bench measures).
        "score" => $i * 0.5 + 0.1,
        "tags" => ["a" . ($i % 10), "b" . ($i % 7)],
        "meta" => ["created" => "2026-10-06", "visits" => $i % 500],
    ];
}

$acc = 0;
for ($r = 0; $r < $reps; $r++) {
    $s = json_encode($payload);
    $acc += strlen($s);
    $back = json_decode($s, true);
    $acc += count($back) + $back[123]["id"] + strlen($back[99]["email"]);
}

echo "RESULT $acc\n";
