<?php
// Classic-mode endpoint: works under both `php -S` and `phpun serve`.
// Does a small realistic amount of per-request work (query parse,
// string ops, a JSON response) so the bench isn't pure socket overhead.

parse_str($_SERVER["QUERY_STRING"] ?? "", $q);
$name = preg_replace('/[^a-zA-Z0-9_-]/', "", $q["name"] ?? "anon");
$id = crc32($name) % 100000;

$user = [
    "id" => $id,
    "name" => $name,
    "greeting" => "hello " . strtoupper($name),
    "tags" => array_map(fn(int $i): string => "t$i", range(1, 4)),
    "ts" => "2026-10-06T00:00:00Z",
];

header("Content-Type: application/json");
echo "bench-ok:" . json_encode($user);
