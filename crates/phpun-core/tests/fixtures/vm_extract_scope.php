<?php
function vm_extract_existing($data) {
    $x = 0;
    extract($data);
    return $x;
}
function vm_extract_param($x, $data) {
    extract($data);
    return $x;
}
var_dump(vm_extract_existing(['x' => 9]));
var_dump(vm_extract_param(1, ['x' => 8]));
