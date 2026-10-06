<?php
// PDO (sqlite :memory:): schema, prepared bulk inserts, indexed lookups,
// join + aggregate. argv[1]: row scale k (default 6 => 6000 users, 12000 orders).
$k = (int)($argv[1] ?? 6);
$users = $k * 1000;
$orders = $k * 2000;

$db = new PDO("sqlite::memory:");
$db->exec("CREATE TABLE users(id INTEGER PRIMARY KEY, name TEXT, email TEXT, age INT)");
$db->exec("CREATE TABLE orders(id INTEGER PRIMARY KEY, user_id INT, amount REAL, created TEXT)");
$db->exec("CREATE INDEX idx_orders_user ON orders(user_id)");
$db->exec("CREATE INDEX idx_users_age ON users(age)");

$db->beginTransaction();
$st = $db->prepare("INSERT INTO users VALUES(?,?,?,?)");
for ($i = 1; $i <= $users; $i++) {
    $st->execute([$i, "user-$i", "user$i@example.com", 18 + ($i % 60)]);
}
$st = $db->prepare("INSERT INTO orders VALUES(?,?,?,?)");
for ($i = 1; $i <= $orders; $i++) {
    $st->execute([$i, 1 + ($i % $users), ($i % 1000) * 0.99 + 0.01, "2026-10-" . (10 + $i % 20)]);
}
$db->commit();

$acc = (int)$db->query("SELECT COUNT(*) FROM users")->fetchColumn();

// Indexed point lookups through a prepared statement.
$sel = $db->prepare("SELECT name, age FROM users WHERE id = ?");
for ($i = 1; $i <= $users; $i += 7) {
    $sel->execute([$i]);
    $row = $sel->fetch(PDO::FETCH_ASSOC);
    $acc += strlen($row["name"]) + $row["age"];
}

// Join + aggregate over the range.
$rows = $db->query(
    "SELECT u.age, COUNT(o.id) c, SUM(o.amount) s
     FROM users u JOIN orders o ON o.user_id = u.id
     WHERE u.age BETWEEN 25 AND 50
     GROUP BY u.age ORDER BY u.age"
)->fetchAll();
foreach ($rows as $r) {
    $acc += (int)$r["c"] + (int)$r["s"];
}

echo "RESULT $acc\n";
