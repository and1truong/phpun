<?php
class ComposerAutoloaderInitInflector {
    public static function getLoader() {
        $loader = new \Composer\Autoload\ClassLoader();
        $loader->setPsr4('Doctrine\\Inflector\\', __DIR__ . '/../doctrine/inflector/lib/Doctrine/Inflector');
        return $loader;
    }
}
