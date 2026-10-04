<?php
class K { static function sm() { return 'K::sm'; } }
try { var_dump(call_user_func(['K','sm','x'])); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { var_dump(call_user_func([])); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { var_dump(call_user_func(['K'])); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { var_dump(call_user_func([9=>'K',10=>'sm'])); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { var_dump(call_user_func(['a'=>'K','b'=>'sm'])); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
var_dump(is_callable(['K','sm','x']));
var_dump(is_callable([9=>'K',10=>'sm']));
