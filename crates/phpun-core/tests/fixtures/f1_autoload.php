<?php
spl_autoload_register(function($c){ eval("class $c { static function sm() { return 'foo'; } }"); });
var_dump(call_user_func(['Lazy','sm']));
