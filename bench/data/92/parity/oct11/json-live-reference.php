<?php
class StreamJson implements JsonSerializable { function jsonSerialize(): mixed { echo "serialize\n"; return ['ok' => 3]; } }
class HookJson {
    public string $name { get { echo "get\n"; return 'hook'; } }
    private int $hidden = 9;
    public int $x = 2;
}
$payload = ['text' => "<>&'\"/\n\r\t\0\\é😀", 'nested' => [[null, true, false, 12, 2.5], [2 => 'two', 'x' => 'key']], 'empty' => []];
foreach ([0, 15, 64, 256, 320] as $flags) echo json_encode($payload, $flags), "\n";
echo json_encode([new StreamJson, new HookJson]), "\n";
$s = '{"ascii":"user-123@example.com","utf8":"é😀","escaped":"a\\u0000b\\n","a":[1,true,null]}';
$a = json_decode($s, true);
echo $a['ascii'], ' ', bin2hex($a['utf8']), ' ', bin2hex($a['escaped']), ' ', json_encode($a['a']), "\n";
echo json_encode(json_decode('{"x":1,"nested":{"k":"v"}}')), "\n";
foreach (['"' . chr(92), '"ascii' . chr(92), '{"key":"value' . chr(92)] as $broken) {
    var_dump(json_decode($broken));
    echo json_last_error(), "\n";
    var_dump(json_validate($broken));
}
class StreamLiveRef implements JsonSerializable {
    function jsonSerialize(): mixed { global $next; $next = 7; return 0; }
}
$next = 1; $refs = [new StreamLiveRef, &$next];
echo json_encode($refs), "\n";
