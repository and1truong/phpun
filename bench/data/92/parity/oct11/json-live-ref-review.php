<?php
class LiveRef implements JsonSerializable {
    function jsonSerialize(): mixed { global $next; $next = 7; return 0; }
}
$next = 1;
$a = [new LiveRef, &$next];
echo json_encode($a), "\n";
