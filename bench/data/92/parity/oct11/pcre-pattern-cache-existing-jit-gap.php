<?php
for ($i = 0; $i < 3; $i++) {
    echo preg_match('/a/i', 'A'), ':', preg_match('/a/', 'A'), ':', preg_match("/a\0/", "a\0"), "\n";
    preg_match_all('/(?<x>a)(b)?/', 'a ab', $m, PREG_SET_ORDER | PREG_UNMATCHED_AS_NULL);
    echo json_encode($m), "\n";
    echo preg_replace('/a/', 'z', 'aab'), ':', preg_replace_callback('/a/', fn($m) => strtoupper($m[0]), 'aab'), "\n";
    echo json_encode(preg_split('/[,;]/', 'a,b;c')), ':', json_encode(preg_grep('/a/i', ['A', 'b', 'a'])), "\n";
    ini_set('pcre.backtrack_limit', '0'); var_dump(preg_match('/a/', 'a')); echo preg_last_error(), "\n";
    ini_set('pcre.backtrack_limit', '1000000'); var_dump(preg_match('/a/', 'a')); echo preg_last_error(), "\n";
    var_dump(preg_match('/./u', "\xff")); echo preg_last_error(), "\n";
    var_dump(preg_match('/./u', 'a')); echo preg_last_error(), "\n";
    set_error_handler(function($n, $s) { echo "compile-error\n"; return true; });
    var_dump(preg_match('/[/', 'a')); restore_error_handler();
}
for ($i = 0; $i < 300; $i++) preg_match('/x'.$i.'/', 'x'.$i);
echo preg_replace_callback('/a/', function($m) { preg_match('/b/', 'b'); return 'z'; }, 'aa'), "\n";
