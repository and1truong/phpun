// Minimal PSR-4 autoloader for the vendored FastRoute subset.
spl_autoload_register(function (string $class) {
    if (str_starts_with($class, 'FastRoute\\')) {
        $file = __DIR__ . '/fast-route/src/' . str_replace('\\', '/', substr($class, 10)) . '.php';
        if (file_exists($file)) { require $file; }
    }
});
require __DIR__ . '/fast-route/src/functions.php';
