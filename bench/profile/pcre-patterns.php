<?php
// Repeated small subjects expose pattern setup; optional working-set exceeds cache.
$reps = (int)($argv[1] ?? 10000);
$patterns = (int)($argv[2] ?? 4);
$sum = 0;
for ($i = 0; $i < $reps; $i++) {
    $key = $i % $patterns;
    $pattern = '/^item'.$key.'$/';
    $subject = 'item'.$key;
    $sum += preg_match($pattern, $subject);
    $sum += strlen(preg_replace($pattern, 'ok', $subject));
}
echo "RESULT $sum\n";
