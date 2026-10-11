<?php
class Mutator implements JsonSerializable {
 public function jsonSerialize(): mixed { global $a; $a[1]=7; return 0; }
}
$a=[new Mutator,1];$r=&$a[1];unset($r);echo json_encode($a), "\n";
$a=[new Mutator,1];$r=&$a[1];echo json_encode($a), "\n";
$a=[new Mutator,1];echo json_encode($a), "\n";
