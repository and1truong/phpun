<?php
namespace Composer\Autoload;
class ClassLoader {
    private array $psr4 = [];
    public function __construct() {
        spl_autoload_register([$this, 'loadClass']);
        \Closure::bind(static function ($f) { require $f; }, null, null);
    }
    public function setPsr4($prefix, $dir) { $this->psr4[$prefix] = $dir; }
    public function loadClass($class) {
        foreach ($this->psr4 as $prefix => $dir) {
            if (str_starts_with($class, $prefix)) {
                $file = $dir . '/' . str_replace('\\', '/', substr($class, strlen($prefix))) . '.php';
                if (is_file($file)) {
                    (\Closure::bind(static function ($f) { require $f; }, null, null))($file);
                    return true;
                }
            }
        }
        return false;
    }
}
