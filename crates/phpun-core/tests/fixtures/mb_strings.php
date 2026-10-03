<?php
var_dump(mb_strpos("héllo", "llo"), mb_strrpos("héllo", "l"), mb_stripos("HÉLLO", "é"));
var_dump(mb_strstr("hé,llo", ","), mb_strrichr("a,b,c", ","), mb_substr_count("aécéa", "é"));
var_dump(mb_convert_case("héllo w", MB_CASE_TITLE), mb_convert_case("ÉX", MB_CASE_LOWER));
var_dump(mb_detect_encoding("héllo"), mb_check_encoding("\xff", "UTF-8"), mb_detect_encoding("\x80\x81", ["UTF-8","ISO-8859-1"]));
var_dump(mb_ord("é"), mb_chr(233), mb_ucfirst("éllo"), mb_str_pad("é", 5, "-", STR_PAD_BOTH));
var_dump(mb_strcut("héllo", 0, 4), bin2hex(mb_convert_encoding("hé", "ISO-8859-1", "UTF-8")));
var_dump(implode("|", mb_split(",", "a,b,cd")), mb_language(), mb_substitute_character(), mb_trim(" héllo "));
