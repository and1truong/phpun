<?php
$node = new stdClass;
$node->left = null;
$node->right =& $node->left;
$node->left = $node;
$root = [$node];
unset($node);
var_dump(gc_collect_cycles());
var_dump($root[0] === $root[0]->left && $root[0]->left === $root[0]->right);
unset($root);
var_dump(gc_collect_cycles());
$node = new stdClass;
$node->gone = 1;
$node->left = null;
$node->right =& $node->left;
unset($node->gone);
$node->left = $node;
unset($node);
var_dump(gc_collect_cycles());
