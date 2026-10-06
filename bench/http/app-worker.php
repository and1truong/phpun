<?php
// Worker-mode endpoint for `phpun serve --worker N`: the script boots once
// and returns a handler invoked per request as fn(array $req).
// Same workload shape as app.php.

return function (array $req): array {
    parse_str($req["query"] ?? "", $q);
    $name = preg_replace('/[^a-zA-Z0-9_-]/', "", $q["name"] ?? "anon");
    $id = crc32($name) % 100000;

    $user = [
        "id" => $id,
        "name" => $name,
        "greeting" => "hello " . strtoupper($name),
        "tags" => array_map(fn(int $i): string => "t$i", range(1, 4)),
        "ts" => "2026-10-06T00:00:00Z",
    ];

    return [
        "status" => 200,
        "headers" => ["Content-Type: application/json"],
        "body" => "bench-ok:" . json_encode($user),
    ];
};
