<?php
date_default_timezone_set('UTC');
$base = mktime(12, 34, 56, 6, 15, 2025); // Sun 2025-06-15 12:34:56 UTC
foreach ([
    '+1 day', '-2 hours', 'tomorrow', 'yesterday', 'next monday',
    'last friday', 'first day of next month', 'last day of this month',
    'next week', 'midnight', 'noon', '+1 week 2 days 4 hours',
    '2024-01-01', '15-Jun-2025', '10:30', '10:30:45pm', 'now',
    '@1000000', 'first monday of july 2025', 'third wednesday of august 2025',
    '-1 week ago', '2 weekdays', 'weekday', 'June 30', '2025-06-30 15:30:45',
] as $s) {
    $t = strtotime($s, $base);
    echo $s, ' => ', $t === false ? 'false' : date('Y-m-d H:i:s D', $t), "\n";
}
var_export(strtotime('')); echo "\n";
var_export(strtotime('not a date')); echo "\n";
