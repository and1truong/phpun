<?php
class RegionPoint {
    public function __construct(private float $x, private float $y) {}
    public function norm(): float { return sqrt($this->x * $this->x + $this->y * $this->y); }
    public function weighted(float $factor): float { return $this->norm() * $factor; }
}
function object_region_sum($points) {
    $sum = 0.0;
    foreach ($points as $point) $sum += $point->norm();
    return $sum;
}
function object_region_args($points) {
    $sum = 0.0;
    foreach ($points as $point) $sum += $point->weighted(1.5);
    return $sum;
}
function object_region_scalars($values) {
    $sum = 0.0; $count = 0; $limit = 2;
    foreach ($values as $key => $value) {
        $sum += $value + 2;
        $count++;
        if ($key < $limit) continue;
        if ($value > 10) break;
    }
    return [$sum, $count];
}
function object_region_overflow($values) {
    $sum = PHP_INT_MAX - 1; $seen = 0;
    foreach ($values as $value) { $seen++; $sum += $value; }
    return [$seen, is_float($sum)];
}
function object_region_overflow_stack($values) {
    $sum = 1; $seen = 0;
    foreach ($values as $value) { $seen++; $sum += $value; }
    return [$seen, $sum];
}
function object_region_nested($groups, $points) {
    $sum = 0.0;
    foreach ($groups as $group) {
        foreach ($points as $point) $sum += $point->norm();
    }
    return $sum;
}
function object_region_escape($groups, $values) {
    $count = 0;
    foreach ($groups as $group) {
        foreach ($values as $value) {
            $count++;
            if ($group == 0) continue 2;
            if ($value == 2) break 2;
        }
    }
    return $count;
}
function object_region_ref($values) {
    foreach ($values as &$value) $value += 1;
    unset($value);
    return $values;
}
function object_region_alias($points) {
    $point = null; $alias =& $point; $sum = 0.0;
    foreach ($points as $point) $sum += $point->norm();
    return [$sum, $alias === $point];
}
function object_region_generator($points) {
    foreach ($points as $point) yield $point->norm();
}
class RegionThrow {
    public function norm(): float {
        $GLOBALS['region_calls']++;
        throw new Exception("region");
    }
}
function object_region_throw($points) {
    $sum = 0.0;
    try {
        foreach ($points as $point) $sum += $point->norm();
    } catch (Exception $e) {
        return [$sum, $GLOBALS['region_calls'], $e->getMessage()];
    }
}
class RegionMutate {
    public function norm(): float {
        $GLOBALS['region_items'][] = new RegionPoint(6.0, 8.0);
        return 2.5;
    }
}
function object_region_mutate() {
    global $region_items;
    $sum = 0.0;
    foreach ($region_items as $point) $sum += $point->norm();
    return [$sum, count($region_items)];
}
class RegionCtor {
    public function __construct(public float $value) { $GLOBALS['region_ctor_calls']++; }
    public function norm(): float { return $this->value * 1.0; }
}
function object_region_ctor($values) {
    $points = [];
    foreach ($values as $value) $points[] = new RegionCtor($value);
    return [object_region_sum($points), $GLOBALS['region_ctor_calls']];
}
function object_region_compare($values) {
    $lt = 0; $ge = 0; $eq = 0;
    foreach ($values as $value) {
        if ($value < 0) $lt++;
        if ($value >= 0) $ge++;
        if ($value == 0) $eq++;
    }
    return [$lt, $ge, $eq];
}
class RegionDtor {
    public function __destruct() { $GLOBALS['region_dtor_calls']++; }
}
function object_region_dtor($points) {
    $point = new RegionDtor; $sum = 0.0;
    foreach ($points as $point) $sum += $point->norm();
    return [$sum, $GLOBALS['region_dtor_calls']];
}
$points = [new RegionPoint(3.0, 4.0), new RegionPoint(6.0, 8.0)];
// Repeated invocations warm the same cached method/property leaves.
for ($i = 0; $i < 3; $i++) echo json_encode(object_region_sum($points)), "\n";
echo json_encode(object_region_args($points)), "\n";
echo json_encode(object_region_scalars([1.5, 3.0, 12.0, 50.0])), "\n";
echo json_encode(object_region_scalars([1, 2, "3", 12])), "\n";
echo json_encode(object_region_overflow([1, 1, 1])), "\n";
echo json_encode(object_region_overflow_stack([PHP_INT_MAX, -PHP_INT_MAX, 3])), "\n";
echo json_encode(object_region_nested([0, 1, 2], $points)), "\n";
echo json_encode(object_region_escape([0, 1, 2], [1, 2, 3])), "\n";
echo json_encode(object_region_ref([1, 2, 3])), "\n";
echo json_encode(object_region_alias($points)), "\n";
echo json_encode(iterator_to_array(object_region_generator($points))), "\n";
$region_calls = 0;
echo json_encode(object_region_throw([new RegionThrow, new RegionThrow])), "\n";
$region_items = [new RegionMutate, new RegionPoint(3.0, 4.0)];
echo json_encode(object_region_mutate()), "\n";
$region_ctor_calls = 0;
echo json_encode(object_region_ctor([1.5, 2.5, 3.0])), "\n";
echo json_encode(object_region_compare([NAN, INF, -INF, -0.0, 1.0])), "\n";
$region_dtor_calls = 0;
echo json_encode(object_region_dtor($points)), "\n";
