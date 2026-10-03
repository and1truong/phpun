<?php
class ComposerAutoloaderInitFixture {
    public static function getLoader() {
        $loader = new \Composer\Autoload\ClassLoader();
        $loader->setPsr4('Acme\\', __DIR__ . '/../../src');
        return $loader;
    }
}
