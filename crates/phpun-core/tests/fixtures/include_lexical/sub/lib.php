<?php
function sub_dir_magic() { return __DIR__; }
class Lib {
    const D = __DIR__;
    public static $sd = __DIR__;
    public static function file_dir() { return __FILE__; }
    public static function const_dir() { return self::D . '|' . self::$sd; }
}
