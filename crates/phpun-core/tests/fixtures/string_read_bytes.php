<?php
$s = "A\0\xffBéC";
var_dump(strlen($s), bin2hex(substr($s, 1, 4)), bin2hex(substr($s, -3)), bin2hex(substr($s, 1, -2)), substr($s, 2, 0));
var_dump(substr_count("A\0A\0", "A\0"), substr_count('aaaaa', 'aa'), substr(12345, 1, 3), strlen(12345));
class StringReadSource { public function __toString() { echo 'convert', "\n"; return 'abcdef'; } }
var_dump(substr(new StringReadSource(), 2, 2));
