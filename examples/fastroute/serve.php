// phpun serve examples/fastroute/serve.php
// curl localhost:8000/user/42
require __DIR__ . '/vendor/autoload.php';

$dispatcher = FastRoute\simpleDispatcher(function (FastRoute\RouteCollector $r) {
    $r->addRoute('GET', '/users', 'all_users');
    $r->addRoute('GET', '/user/{id:\d+}', 'user_detail');
    $r->addRoute('POST', '/echo', 'echo_body');
});

$result = $dispatcher->dispatch($_SERVER['REQUEST_METHOD'], $_SERVER['REQUEST_URI']);

header('Content-Type: application/json');
if ($result instanceof FastRoute\Dispatcher\Result\Matched) {
    echo json_encode(['handler' => $result->handler, 'vars' => $result->variables]);
} else {
    http_response_code(404);
    echo json_encode(['error' => 'not found']);
}
