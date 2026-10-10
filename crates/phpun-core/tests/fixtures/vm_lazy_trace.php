<?php
function native_trace(int $x): array { $x = 7; return debug_backtrace(); }
function native_recur(int $n): array {
    if ($n === 0) { return debug_backtrace(); }
    return native_recur($n - 1);
}
function native_div(int $n): int { return intdiv($n, 0); }
function native_ret(int $n): int { return 'bad'; }
function native_identity($o) { return $o; }
class TraceLifetime { public function __destruct() { echo "destroy\n"; } }
$f = native_trace(3);
echo $f[0]['function'], ':', $f[0]['args'][0], "\n";
$g = native_trace(4);
echo $f[0]['args'][0], ':', $g[0]['args'][0], "\n";
foreach (native_recur(2) as $frame) { echo $frame['function'], ':', $frame['args'][0], "\n"; }
foreach (['native_div', 'native_ret'] as $name) {
    try { $name(5); } catch (Throwable $e) {
        echo get_class($e), "\n";
        foreach ($e->getTrace() as $frame) {
            echo $frame['function'], ':', json_encode($frame['args']), "\n";
        }
    }
}
$o = native_identity(new TraceLifetime());
echo "alive\n";
unset($o);
echo "done\n";
function lazy_date_interval(int $n): void { DateInterval::createFromDateString('bad'); }
try { lazy_date_interval(77); } catch (Throwable $e) {
    $saved = $e->getTrace();
    native_trace(88);
    foreach ($saved as $frame) {
        if (($frame['function'] ?? '') === 'lazy_date_interval') { echo 'date-args:', $frame['args'][0], "\n"; }
    }
}
