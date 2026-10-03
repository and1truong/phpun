<?php
class SymfonyConsoleAutoloader {
    public static function getLoader() {
        $vendor = __DIR__ . '/..';
        $loader = new \Composer\Autoload\ClassLoader();
        $loader->setPsr4('Symfony\\Component\\Console\\', $vendor . '/symfony/console');
        $loader->setPsr4('Symfony\\Component\\String\\', $vendor . '/symfony/string');
        $loader->setPsr4('Symfony\\Contracts\\Service\\', $vendor . '/symfony/service-contracts');
        $loader->setPsr4('Symfony\\Polyfill\\Intl\\Normalizer\\', $vendor . '/symfony/polyfill-intl-normalizer');
        $loader->setPsr4('Symfony\\Polyfill\\Intl\\Grapheme\\', $vendor . '/symfony/polyfill-intl-grapheme');
        $loader->addClassMap([
            'Normalizer' => $vendor . '/symfony/polyfill-intl-normalizer/Resources/stubs/Normalizer.php',
        ]);
        require $vendor . '/symfony/deprecation-contracts/function.php';
        require $vendor . '/symfony/string/Resources/functions.php';
        require $vendor . '/symfony/polyfill-intl-normalizer/bootstrap.php';
        require $vendor . '/symfony/polyfill-intl-grapheme/bootstrap.php';
        return $loader;
    }
}
