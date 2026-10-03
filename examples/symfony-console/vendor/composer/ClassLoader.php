<?php
namespace Composer\Autoload;
class ClassLoader {
    private array $psr4 = [];
    private array $classMap = [];
    public function __construct() {
        spl_autoload_register([$this, 'loadClass']);
        \Closure::bind(static function ($f) { require $f; }, null, null);
    }
    public function setPsr4($prefix, $dir) { $this->psr4[$prefix] = $dir; }
    public function addClassMap(array $map) { $this->classMap += $map; }
    public function loadClass($class) {
        $class = ltrim($class, '\\');
        if (isset($this->classMap[$class])) {
            (\Closure::bind(static function ($f) { require $f; }, null, null))($this->classMap[$class]);
            return true;
        }
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
