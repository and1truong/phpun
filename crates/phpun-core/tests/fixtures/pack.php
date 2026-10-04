<?php
echo bin2hex(pack("C", 0x41)), "\n";
echo bin2hex(pack("n", 0x1234)), "\n";
echo bin2hex(pack("N", 0x12345678)), "\n";
echo bin2hex(pack("V", 0x12345678)), "\n";
echo bin2hex(pack("a3", "abcdef")), "\n";
echo bin2hex(pack("A5", "ab")), "\n";
echo bin2hex(pack("h5", "01234")), "\n";
echo bin2hex(pack("H*", "deadbe")), "\n";
echo bin2hex(pack("f", 1.5)), "\n";
echo bin2hex(pack("d", -2.25)), "\n";
echo bin2hex(pack("q", -1)), "\n";
echo bin2hex(pack("J", 1)), "\n";
echo bin2hex(pack("CCX", 1, 2)), "\n";
echo bin2hex(pack("cs", -1, 258)), "\n";
var_export(unpack("Cx", "A"));
echo "\n";
var_export(unpack("Nval", pack("N", 0x12345678)));
echo "\n";
var_export(unpack("a*x", "abc"));
echo "\n";
var_export(unpack("c2n/nd", pack("ccn", -1, 127, 513)));
echo "\n";
try { pack("Q"); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
try { unpack("N", "ab"); } catch (\Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
