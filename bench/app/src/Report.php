<?php
namespace BenchApp;
final class Report {
    public function render(array $rows): string {
        $items = array_map(fn(array $r): array => ['id' => $r['id'], 'name' => strtoupper($r['name'])], $rows);
        usort($items, fn(array $a, array $b): int => $a['id'] <=> $b['id']);
        return json_encode(['users' => $items, 'count' => count($items)]);
    }
}
