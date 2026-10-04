<?php
class A { static function who() { return 'A::who'; } }
class A2 extends A {}
var_dump(call_user_func(['A2','parent::who']));
