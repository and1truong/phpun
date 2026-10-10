<?php
class VmScopeObject { public function __destruct() { echo 'destroy', "\n"; } }
function unset_local() { $o = new VmScopeObject(); unset($o); echo 'after unset', "\n"; }
function unset_alias() { $o = new VmScopeObject(); $a = $o; unset($o); echo 'alias alive', "\n"; unset($a); echo 'after alias', "\n"; }
unset_local();
unset_alias();
