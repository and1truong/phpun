<?php
$tmp = tempnam(sys_get_temp_dir(), 'csv');
$f = fopen($tmp, 'w');
fwrite($f, "a,b,c\n1,2,3\n\"x,y\",z\nplain,\"q\"\nlast\n");
fclose($f);
$f = fopen($tmp, 'r');
while (($row = fgetcsv($f, 0, ',', '"', '\\')) !== false) {
    echo json_encode($row), "\n";
}
fclose($f);
$f = fopen($tmp, 'r');
fgetcsv($f, 0, ',', '"', '');
while (($row = fgetcsv($f, 0, ';', '"', '\\')) !== false) {
    echo count($row), "\n";
}
fclose($f);
unlink($tmp);
