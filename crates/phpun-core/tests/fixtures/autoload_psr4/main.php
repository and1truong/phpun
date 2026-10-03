<?php
spl_autoload_register(function ($class) {
    $file = __DIR__ . '/src/' . str_replace('\\', '/', $class) . '.php';
    if (is_file($file)) { require $file; }
});
echo App\Greeter::hello(), "\n";
echo (new App\Sub\Deep)->value(), "\n";
