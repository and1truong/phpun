<?php
$ao = new ArrayObject([1,2]);
try { $ao->exchangeArray(42); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { $ao->exchangeArray('s'); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { $ao->exchangeArray(); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { $ao->append(); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
$o = new stdClass; $o->p = 1;
$ao2 = new ArrayObject;
var_dump($ao2->exchangeArray($o));
$ao3 = new ArrayObject;
$ao3['x'] = 7;
var_dump($ao3->getArrayCopy());
$src = new ArrayObject([1,2,3]);
$nx = new ArrayObject($src);
$nx[0] = 9;
var_dump($src[0], $nx[1], count($nx));
