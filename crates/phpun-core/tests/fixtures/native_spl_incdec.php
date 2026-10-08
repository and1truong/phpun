<?php
// r4 regression net: ++/--/compound on native-stub spl dims write the
// store cell directly (no offsetSet dispatch, no overloaded-dim notice).
function dump($l, $v) { echo "$l="; var_dump($v); }
$a = new ArrayObject(['k'=>5, 's'=>'abc']);
$a['k']++; dump('ao-inc', $a['k']);
$a['k']--; dump('ao-dec', $a['k']);
++$a['k']; dump('ao-pre', $a['k']);
$a['missing']++; dump('ao-miss', $a['missing']);
$a['s']++; dump('ao-str', $a['s']);
$i = new ArrayIterator(['k'=>7]);
$i['k']++; dump('ai-inc', $i['k']);
class Sub extends ArrayObject {}
$s = new Sub(['k'=>9]);
$s['k']--; dump('sub-dec', $s['k']);
$b = new ArrayObject(['k'=>1]);
$b['k'] .= 'x'; dump('ao-cat', $b['k']);
$b['k'] += 10; dump('ao-add', $b['k']);
class SO extends ArrayObject { public function offsetSet($k,$v): void { echo "sSET\n"; parent::offsetSet($k,$v); } }
$o = new SO(['k'=>1]);
$o['k']++; dump('so-inc', $o['k']);
$f = new SplFixedArray(2); $f[1] = 'abc';
$f[1]++; dump('fa-str', $f[1]);
$t = new ArrayObject([3,1,2]);
try {
    $t->uasort(function($x,$y) use ($t) { $t['x']++; return $x <=> $y; });
} catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
// r5: a throwing warn handler still materializes the missed bucket
// (zend_error returns, read_dimension creates the slot); compound ops
// land their write before the pending throwable surfaces at op end.
set_error_handler(function($n,$s){ echo "[w:$s]"; throw new ErrorException($s); });
$h = new ArrayObject(['a'=>1]);
try { $h['miss']++; } catch (Throwable $e) { echo "inc:", get_class($e), " "; }
dump('inc-miss', count($h));
try { $h['m2'] += 5; } catch (Throwable $e) { echo "pe:", get_class($e), " "; }
dump('pe-miss', $h['m2']);
class SG extends ArrayObject { public function offsetGet($k): mixed { throw new Exception('gf'); } }
$g = new SG(['a'=>1]);
try { $g['a'] += 5; } catch (Throwable $e) { echo "gpe:", $e->getMessage(), " "; }
dump('gpe-a', $g->getArrayCopy()['a']);
?>

