<?php
class CtorLabelBase { function __construct(&$x) {} }
class CtorLabelChild extends CtorLabelBase {}
try { new cToRlAbElChIlD(1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
class TypedLabelBase { function __construct(int $x) {} }
class TypedLabelChild extends TypedLabelBase {}
try { new tYpEdLaBeLcHiLd('bad'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
try { new TypedLabelChild(unknown: 1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
$anonymous = new class { function __construct(&$x=null) {} };
$name = get_class($anonymous);
try { new $name(1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
