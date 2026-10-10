<?php
// A small application-shaped workload: PSR-4 style autoload, component calls,
// callback sorting and JSON output. No framework or Composer package dependency.
spl_autoload_register(function (string $class): void {
    $prefix = 'BenchApp\\';
    if (str_starts_with($class, $prefix)) {
        require __DIR__ . '/src/' . substr($class, strlen($prefix)) . '.php';
    }
});
$n = (int)($argv[1] ?? 100);
$reps = (int)($argv[2] ?? 20);
$rows = [];
for ($i = $n; $i > 0; $i--) { $rows[] = ['id' => $i, 'name' => 'user-' . $i]; }
$app = new BenchApp\Report();
$sum = 0;
for ($r = 0; $r < $reps; $r++) { $sum += strlen($app->render($rows)); }
echo 'RESULT ', $sum, "\n";
