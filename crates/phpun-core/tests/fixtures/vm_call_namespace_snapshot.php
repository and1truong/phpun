<?php
namespace VmResolve {
    function define_override() {
        if (!function_exists('VmResolve\\strlen')) {
            eval('namespace VmResolve; function strlen($x) { return 99; }');
        }
        return 'abc';
    }
    function call_before_override() { return strlen(define_override()); }
    var_dump(call_before_override());
    var_dump(call_before_override());
}
