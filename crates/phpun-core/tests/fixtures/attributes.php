<?php
#[Attribute(Attribute::TARGET_CLASS)]
class Route {
    public function __construct(
        public string $path = '/',
        public array $methods = ['GET'],
    ) {}
}
#[Route('/user/{id}', methods: ['GET', 'POST'])]
class UserController {}
$attrs = (new ReflectionClass('UserController'))->getAttributes();
echo count($attrs), "\n";
echo $attrs[0]->getName(), "\n";
var_export($attrs[0]->getArguments());
echo "\n";
$r = $attrs[0]->newInstance();
echo $r->path, " ", implode(',', $r->methods), "\n";
