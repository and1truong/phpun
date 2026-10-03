# doctrine/inflector on phpun

Second e2e component demo (after FastRoute): the real `doctrine/inflector`
v2.0.10 vendored under `vendor/`, wired through a Composer-style
`vendor/autoload.php` → psr-4 `ClassLoader`.

    phpun demo.php

`demo.php` exercises pluralize/singularize/tableize/classify/camelize/
capitalize/urlize/seemsUtf8/unaccent — output verified byte-identical
with PHP 8.5.
