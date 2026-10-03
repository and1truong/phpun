<?php
// phpun serve --worker demo: this file runs ONCE at boot — everything
// below top level (the counter, the DB stand-in object) stays hot.
// The returned callable handles every request; superglobals
// ($_GET/$_POST/$_SERVER/php://input) are rebuilt per request.
$counter = 0;
$started = microtime(true);
$state = new class {
    public array $log = [];
};

return function (array $req) use (&$counter, $started, $state) {
    $counter++;
    $state->log[] = $req['method'] . ' ' . $req['path'];
    $in = file_get_contents('php://input');
    return [
        'status' => 200,
        'headers' => ['Content-Type: application/json'],
        'body' => json_encode([
            'counter' => $counter,               // proves warm state
            'uptime_ms' => (int)((microtime(true) - $started) * 1000),
            'method' => $_SERVER['REQUEST_METHOD'],
            'query' => $_GET,
            'input' => $in === '' ? null : json_decode($in, true),
            'seen' => count($state->log),
        ]) . "\n",
    ];
};
