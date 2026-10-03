require __DIR__ . '/vendor/autoload.php';

$dispatcher = FastRoute\simpleDispatcher(function (FastRoute\RouteCollector $r) {
    $r->addRoute('GET', '/users', 'all_users');
    $r->addRoute('GET', '/user/{id:\d+}', 'user_detail');
    $r->addRoute('GET', '/articles/{title}', 'article');
});

$info = $dispatcher->dispatch('GET', '/user/42');
var_dump($info->offsetGet(0));   // 1 = Dispatcher::FOUND
print_r($info);
