<?php
class ScalarLeaf {
    public float $number = 1.5;
    public mixed $alias;
    function __destruct() { global $deaths; $deaths++; }
}
$deaths = 0; $scalar = 11;
$leaf = new ScalarLeaf;
$leaf->alias =& $scalar;
$weak = WeakReference::create($leaf);
unset($leaf);
echo $deaths, ' ', $scalar, "\n"; var_dump($weak->get());
$leaf = new ScalarLeaf;
$leaf->alias = "a\0b";
unset($leaf);
echo $deaths, "\n";
$resource = fopen('php://memory', 'w+');
$leaf = new ScalarLeaf;
$leaf->alias = $resource;
unset($leaf);
echo $deaths, ' ', get_resource_type($resource), "\n";
