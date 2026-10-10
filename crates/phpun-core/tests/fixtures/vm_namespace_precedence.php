<?php
namespace {
    function same_name() { return 'global'; }
    function global_fallback() { return 'fallback'; }
}
namespace VmPrecedence {
    function same_name() { return 'namespace'; }
    function vm_literal() { return same_name(); }
    function ast_literal() { $unsupported = []; return same_name(); }
    function qualified() { return \same_name(); }
    function dynamic() { $f = 'same_name'; return $f(); }
    function fallback() { return global_fallback(); }
    function first_class() { $f = same_name(...); return $f(); }
    var_dump(vm_literal(), ast_literal(), qualified(), dynamic(), fallback(), first_class());
}
