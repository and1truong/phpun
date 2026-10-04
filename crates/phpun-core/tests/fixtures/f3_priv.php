<?php
class F { private function priv() {} function m() {} }
$f = new F;
var_dump(is_callable([$f,'priv']));
try { call_user_func([$f,'priv']); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
var_dump(is_callable([$f,'m']));
