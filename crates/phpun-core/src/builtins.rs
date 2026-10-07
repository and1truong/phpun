//! Builtin function table. `call` returns Ok(Some(v)) when `name` is a
//! builtin, Ok(None) when it isn't (the interpreter then tries userland).

use crate::error::PhpError;
use crate::interp::Interp;
pub(in crate::builtins) use crate::value::{
    compare, format_float_repr, numeric, to_key, ArrKey, Cell, Numeric, PhpArray, PhpObject,
    PhpResource, Value,
};
pub(in crate::builtins) use std::cell::RefCell;
pub(in crate::builtins) use std::collections::HashMap;
pub(in crate::builtins) use std::rc::Rc;

pub(crate) mod array;
mod class;
mod core;
mod crypto;
mod ctype;
mod datetime;
mod filter;
pub(crate) mod fs;
mod json;
mod math;
mod mbstring;
mod out;
mod pcre;
mod proc;
mod spl;
mod string;
mod url;
pub(crate) mod var;

pub(crate) use url::urldecode;

pub(in crate::builtins) fn cell(v: Value) -> Cell {
    Rc::new(RefCell::new(v))
}

fn arg(args: &[Cell], i: usize) -> Value {
    args.get(i)
        .map(|c| c.borrow().clone())
        .unwrap_or(Value::Null)
}

fn arg_str(it: &mut Interp, args: &[Cell], i: usize) -> String {
    it.to_string_of(&arg(args, i))
}

fn arg_bs(it: &mut Interp, args: &[Cell], i: usize) -> Vec<u8> {
    it.to_bytes_of(&arg(args, i))
}

/// PHP's name for a zval's type, used in TypeError messages.
pub(crate) fn zval_word(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Int(_) => "int".into(),
        Value::Float(_) => "float".into(),
        Value::Str(_) => "string".into(),
        Value::Array(_) => "array".into(),
        Value::Object(o) => o.borrow().class.name().to_string(),
        Value::Callable(_) => "Closure".into(),
        Value::Resource(_) => "resource".into(),
    }
}

fn key_str(k: &ArrKey) -> String {
    match k {
        ArrKey::Int(i) => i.to_string(),
        ArrKey::Str(s) => s.to_string(),
        ArrKey::Tomb => String::new(),
    }
}

fn bfind(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Replace every non-overlapping `from` with `to` (byte version of
/// str_replace's core loop).
fn breplace(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while let Some(p) = bfind(hay, from, i) {
        out.extend_from_slice(&hay[i..p]);
        out.extend_from_slice(to);
        i = p + from.len();
    }
    out.extend_from_slice(&hay[i..]);
    out
}

/// Is `i` a UTF-8 char boundary in `b`?
fn utf8_boundary(b: &[u8], i: usize) -> bool {
    i >= b.len() || (b[i] & 0xC0) != 0x80
}

fn err<T>(cls: &'static str, msg: impl Into<String>) -> Result<T, PhpError> {
    Err(PhpError::uncaught(cls, msg, 0))
}

/// `Nesting level too deep` — the catchable Error zend's container
/// compares raise on re-entry into a marked left operand of a cyclic
/// structure. Builtin compare loops (in_array, sort, min, ...) check
/// CMP_DEPTH_ERR and fail this.
fn depth_err<T>() -> Result<T, PhpError> {
    err("Error", "Nesting level too deep - recursive dependency?")
}

/// Dispatch by extension family: each `dispatch` returns `Ok(Some(v))`
/// when `name` is one of its builtins, `Ok(None)` to fall through.
type Dispatch = fn(&mut Interp, &str, &[Cell]) -> Result<Option<Value>, PhpError>;

const FAMILIES: &[Dispatch] = &[
    array::dispatch,
    class::dispatch,
    core::dispatch,
    crypto::dispatch,
    ctype::dispatch,
    datetime::dispatch,
    filter::dispatch,
    fs::dispatch,
    json::dispatch,
    math::dispatch,
    mbstring::dispatch,
    out::dispatch,
    pcre::dispatch,
    proc::dispatch,
    spl::dispatch,
    string::dispatch,
    url::dispatch,
    var::dispatch,
];

pub fn call(it: &mut Interp, name: &str, args: &[Cell]) -> Result<Option<Value>, PhpError> {
    for f in FAMILIES {
        if let Some(v) = f(it, name, args)? {
            return Ok(Some(v));
        }
    }
    Ok(None)
}

/// Parameter names/requiredness for a handful of builtins whose FCC
/// closures appear in var_dump output (Zend/tests/first_class_callable).
/// `(name, required)`.
pub(crate) fn builtin_sig(n: &str) -> Option<Vec<(String, bool)>> {
    let ps: &[(&str, bool)] = match n {
        "strlen" | "strrev" | "strtoupper" | "strtolower" | "md5" | "sha1" => &[("string", true)],
        "sprintf" | "printf" => &[("format", true), ("values", false)],
        "vsprintf" | "vprintf" => &[("format", true), ("values", true)],
        "fprintf" => &[("stream", true), ("format", true), ("values", false)],
        "vfprintf" => &[("stream", true), ("format", true), ("values", true)],
        "str_repeat" => &[("string", true), ("times" /* multi */, true)],
        "substr" => &[("string", true), ("offset", true), ("length", false)],
        "strpos" => &[("haystack", true), ("needle", true), ("offset", false)],
        "assert" => &[("assertion", true), ("description", false)],
        "count" => &[("value", true), ("mode", false)],
        "implode" => &[("separator", false), ("array", true)],
        "explode" => &[("separator", true), ("string", true), ("limit", false)],
        "preg_match" | "preg_match_all" => &[
            ("pattern", true),
            ("subject", true),
            ("matches", false),
            ("flags", false),
            ("offset", false),
        ],
        "preg_replace" | "preg_replace_callback" | "preg_filter" => &[
            ("pattern", true),
            ("replacement", true),
            ("subject", true),
            ("limit", false),
            ("count", false),
            ("flags", false),
        ],
        "preg_replace_callback_array" => &[
            ("pattern", true),
            ("subject", true),
            ("limit", false),
            ("count", false),
            ("flags", false),
        ],
        "preg_split" => &[
            ("pattern", true),
            ("subject", true),
            ("limit", false),
            ("flags", false),
        ],
        "preg_grep" => &[("pattern", true), ("array", true), ("flags", false)],
        "preg_quote" => &[("str", true), ("delimiter", false)],
        "iterator_to_array" => &[("iterator", true), ("preserve_keys", false)],
        // Unary math fns share the single `num` param name (bug75290).
        "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "sinh" | "cosh" | "tanh" | "asinh"
        | "acosh" | "atanh" | "sqrt" | "exp" | "deg2rad" | "rad2deg" => &[("num", true)],
        "log" | "log10" => &[("num", true), ("base", false)],
        _ => return Some(Vec::new()),
    };
    Some(ps.iter().map(|(n, r)| (n.to_string(), *r)).collect())
}

/// Declared type members for builtin params, where builtin_sig
/// tracks only name + required (bug69802_2).
pub(crate) fn builtin_param_ty(f: &str, p: &str) -> Option<Vec<String>> {
    let ms: &[&str] = match (f, p) {
        ("iterator_to_array", "iterator") => &["Traversable", "array"],
        ("iterator_to_array", "preserve_keys") => &["bool"],
        _ => return None,
    };
    Some(ms.iter().map(|m| m.to_string()).collect())
}

pub(crate) fn is_builtin(n: &str) -> bool {
    matches!(
        n,
        "abs"
            | "acos"
            | "addcslashes"
            | "addslashes"
            | "array_change_key_case"
            | "array_chunk"
            | "array_column"
            | "array_combine"
            | "array_count_values"
            | "array_diff"
            | "array_diff_assoc"
            | "array_diff_key"
            | "array_fill"
            | "array_fill_keys"
            | "array_filter"
            | "array_flip"
            | "array_intersect"
            | "array_intersect_assoc"
            | "array_intersect_key"
            | "array_is_list"
            | "array_key_exists_slow"
            | "array_key_last"
            | "array_keys"
            | "array_map"
            | "array_multisort"
            | "array_pad"
            | "array_pop"
            | "array_product"
            | "array_push"
            | "array_rand"
            | "array_reduce"
            | "array_replace"
            | "array_reverse"
            | "array_search"
            | "array_shift"
            | "array_slice"
            | "array_splice"
            | "array_sum"
            | "array_unique"
            | "array_unshift"
            | "array_values"
            | "array_walk"
            | "asin"
            | "assert"
            | "assert_options"
            | "assert_options_now"
            | "atan"
            | "atan2"
            | "base64_decode"
            | "base64_encode"
            | "base_convert"
            | "basename"
            | "bin2hex"
            | "bindec"
            | "boolval"
            | "call_func"
            | "ceil"
            | "chdir"
            | "checkdate"
            | "chr"
            | "chunk_split"
            | "class_exists"
            | "clone"
            | "compact"
            | "compact_obj"
            | "constant"
            | "copy"
            | "cos"
            | "cosh"
            | "count_chars"
            | "crc32"
            | "crc32_combine"
            | "ctype_alnum"
            | "ctype_alpha"
            | "ctype_digit"
            | "ctype_lower"
            | "ctype_space"
            | "ctype_upper"
            | "date_parse"
            | "debug_backtrace"
            | "debug_print_backtrace"
            | "debug_zval_dump"
            | "decbin"
            | "dechex"
            | "decoct"
            | "define"
            | "defined"
            | "deg2rad"
            | "die"
            | "divmod"
            | "dl"
            | "each"
            | "end"
            | "enum_exists"
            | "error_reporting"
            | "escapeshellarg"
            | "escapeshellcmd"
            | "exec"
            | "exit"
            | "exp"
            | "explode"
            | "extension_loaded"
            | "extract"
            | "fastcgi_finish_request"
            | "fclose"
            | "fdiv"
            | "feof"
            | "fflush"
            | "fgetc"
            | "fgetcsv"
            | "fgets"
            | "file"
            | "file_exists"
            | "file_get_contents"
            | "file_put_contents"
            | "fileperms"
            | "filesize"
            | "flock"
            | "floor"
            | "fmod"
            | "fnmatch"
            | "fopen"
            | "fpassthru"
            | "fprintf"
            | "fread"
            | "fseek"
            | "fstat"
            | "ftell"
            | "ftruncate"
            | "function_exists"
            | "gc_enabled"
            | "gc_status"
            | "get_called_class"
            | "get_class"
            | "get_class_methods"
            | "get_class_vars"
            | "get_current_user"
            | "get_debug_type"
            | "get_declared_classes"
            | "get_declared_interfaces"
            | "get_declared_traits"
            | "get_extension_funcs"
            | "get_loaded_extensions"
            | "get_parent_class"
            | "getcwd"
            | "getenv"
            | "getopt"
            | "getmypid"
            | "gettype"
            | "glob"
            | "hash"
            | "hash_equals"
            | "header"
            | "header_register_callback"
            | "header_remove"
            | "headers_list"
            | "headers_sent"
            | "hex2bin"
            | "hexdec"
            | "hrtime"
            | "http_build_query"
            | "http_response_code"
            | "hypot"
            | "ignore_user_abort"
            | "in_array"
            | "ini_get"
            | "ini_get_all"
            | "ini_parse_quantity"
            | "ini_restore"
            | "ini_set"
            | "intdiv"
            | "interface_exists"
            | "is_a"
            | "is_array"
            | "is_bool"
            | "is_callable"
            | "is_countable"
            | "is_dir"
            | "is_executable"
            | "is_file"
            | "is_finite"
            | "is_infinite"
            | "is_iterable"
            | "is_link"
            | "is_nan"
            | "is_null"
            | "is_numeric"
            | "is_object"
            | "is_readable"
            | "is_resource"
            | "is_scalar"
            | "is_string"
            | "is_subclass_of"
            | "iterator_from_array"
            | "json_decode"
            | "json_encode"
            | "json_validate"
            | "key"
            | "lcfirst"
            | "lcg_value"
            | "levenshtein"
            | "log10"
            | "log2"
            | "ltrim"
            | "mb_check_encoding"
            | "mb_chr"
            | "mb_convert_case"
            | "mb_convert_encoding"
            | "mb_detect_encoding"
            | "mb_detect_order"
            | "mb_encoding_aliases"
            | "mb_http_input"
            | "mb_http_output"
            | "mb_internal_encoding"
            | "mb_language"
            | "mb_lcfirst"
            | "mb_list_encodings"
            | "mb_ltrim"
            | "mb_ord"
            | "mb_regex_encoding"
            | "mb_rtrim"
            | "mb_scrub"
            | "mb_split"
            | "mb_str_pad"
            | "mb_strcut"
            | "mb_strtolower_nc"
            | "mb_stripos"
            | "mb_stristr"
            | "mb_strpos"
            | "mb_strrchr"
            | "mb_strrichr"
            | "mb_strripos"
            | "mb_strrpos"
            | "mb_strstr"
            | "mb_substr_count"
            | "mb_substitute_character"
            | "mb_trim"
            | "mb_ucfirst"
            | "mb_strlen"
            | "md5"
            | "memory_get_peak_usage"
            | "memory_get_usage"
            | "memory_reset_peak_usage"
            | "method_exists"
            | "microtime"
            | "microtime_float"
            | "mkdir"
            | "nl2br"
            | "number_format"
            | "ob_clean"
            | "ob_end_clean"
            | "ob_end_flush"
            | "ob_get_clean"
            | "ob_get_contents"
            | "ob_get_flush"
            | "ob_get_length"
            | "ob_get_level"
            | "ob_get_status"
            | "ob_start"
            | "octdec"
            | "ord"
            | "output_reset_rewrite_vars"
            | "parse_str"
            | "parse_url"
            | "passthru"
            | "pathinfo"
            | "php_check_syntax"
            | "php_sapi_name"
            | "php_strip_whitespace"
            | "php_uname"
            | "pi"
            | "pow"
            | "proc_close"
            | "proc_get_status"
            | "proc_nice"
            | "proc_open"
            | "proc_terminate"
            | "preg_jit"
            | "print"
            | "print_r"
            | "printf"
            | "property_exists"
            | "putenv"
            | "quotemeta"
            | "rad2deg"
            | "range"
            | "rawurldecode"
            | "rawurlencode"
            | "readfile"
            | "realpath"
            | "register_shutdown_function"
            | "rename"
            | "reset"
            | "rewind"
            | "rmdir"
            | "round"
            | "scandir"
            | "serialize"
            | "set_error_handler"
            | "set_exception_handler"
            | "set_time_limit"
            | "setcookie"
            | "setlocale"
            | "setrawcookie"
            | "settype"
            | "sha1"
            | "shell_exec"
            | "similar_text"
            | "sin"
            | "sinh"
            | "sleep"
            | "soundex"
            | "sprintf"
            | "sqrt"
            | "str_contains"
            | "str_ends_with"
            | "str_ireplace"
            | "str_pad"
            | "str_repeat"
            | "str_replace"
            | "str_rot13"
            | "str_starts_with"
            | "str_word_count"
            | "stream_get_contents"
            | "stream_copy_to_stream"
            | "stream_get_meta_data"
            | "stream_select"
            | "stream_set_blocking"
            | "strip_tags"
            | "stripslashes"
            | "strlen"
            | "strrev"
            | "strtotime"
            | "strtr"
            | "strval"
            | "substr_count"
            | "substr_replace"
            | "sys_get_temp_dir"
            | "system"
            | "tan"
            | "tanh"
            | "tempnam"
            | "time_nanosleep"
            | "tmpfile"
            | "trait_exists"
            | "trim"
            | "ucfirst"
            | "ucwords"
            | "umask"
            | "uniqid"
            | "unlink"
            | "unserialize"
            | "urldecode"
            | "urlencode"
            | "usleep"
            | "var_dump"
            | "var_export"
            | "version_compare"
            | "vfprintf"
            | "vprintf"
            | "vsprintf"
            | "wordwrap"
            | "zend_version"
    )
}

/// Named-argument resolution for internal functions
/// (Zend/tests/named_params/internal*). Param names follow the Zend
/// stubs; `BDef::Var` as the last entry marks a variadic tail, which
/// rejects unknown named params with a different message.
#[derive(Clone, Copy)]
pub enum BDef {
    Req,
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(&'static str),
    Arr,
    /// "Unknown default" — must be passed explicitly when a later param
    /// is bound (array_keys' $filter_value, named_params/missing_param).
    Unk,
    Var,
}

impl BDef {
    pub fn val(self) -> Value {
        match self {
            BDef::Req | BDef::Var => Value::Null,
            BDef::Null | BDef::Unk => Value::Null,
            BDef::Int(i) => Value::Int(i),
            BDef::Float(f) => Value::Float(f),
            BDef::Bool(b) => Value::Bool(b),
            BDef::Str(s) => Value::str(s),
            BDef::Arr => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        }
    }
}

type BParams = &'static [(&'static str, BDef)];

macro_rules! bp {
    ($(($n:literal, $d:expr)),* $(,)?) => {
        &[$(($n, $d)),*]
    };
}

/// Positional signatures of the internal functions userland code
/// commonly calls with named args. Entries are `(name, default)`;
/// `Req` marks a required param, `Var` a variadic tail.
pub fn builtin_params(name: &str) -> Option<BParams> {
    use BDef::*;
    Some(match name {
        "strlen" | "strtoupper" | "strtolower" | "strrev" | "lcfirst" | "ucfirst" | "ord"
        | "chr" => bp!(("string", Req)),
        "str_pad" => bp!(
            ("string", Req),
            ("length", Req),
            ("pad_string", Str(" ")),
            ("pad_type", Int(1))
        ),
        "str_repeat" => bp!(("string", Req), ("times", Req)),
        "substr" => bp!(("string", Req), ("offset", Req), ("length", Null)),
        "strpos" | "stripos" | "strrpos" | "strripos" => {
            bp!(("haystack", Req), ("needle", Req), ("offset", Int(0)))
        }
        "str_contains" | "str_starts_with" | "str_ends_with" => {
            bp!(("haystack", Req), ("needle", Req))
        }
        "strcmp" | "strcasecmp" => bp!(("string1", Req), ("string2", Req)),
        "strncmp" | "strncasecmp" => {
            bp!(("string1", Req), ("string2", Req), ("length", Req))
        }
        "strspn" | "strcspn" => {
            bp!(
                ("string", Req),
                ("characters", Req),
                ("offset", Int(0)),
                ("length", Null)
            )
        }
        "str_replace" | "str_ireplace" => bp!(
            ("search", Req),
            ("replace", Req),
            ("subject", Req),
            ("count", Null)
        ),
        "substr_replace" => bp!(
            ("string", Req),
            ("replace", Req),
            ("start", Req),
            ("length", Null)
        ),
        "trim" | "ltrim" | "rtrim" => {
            bp!(("string", Req), ("characters", Str(" \n\r\t\u{b}\0")))
        }
        "explode" => bp!(
            ("separator", Req),
            ("string", Req),
            ("limit", Int(i64::MAX))
        ),
        "implode" | "join" => bp!(("separator", Str("")), ("array", Req)),
        "ucwords" => bp!(("string", Req), ("separators", Str(" \t\r\n\u{c}\u{b}"))),
        "wordwrap" => bp!(
            ("string", Req),
            ("width", Int(75)),
            ("break", Str("\n")),
            ("cut_long_words", Bool(false))
        ),
        "nl2br" => bp!(("string", Req), ("use_xhtml", Bool(true))),
        "sprintf" | "printf" => {
            bp!(("format", Req), ("values", Var))
        }
        "vsprintf" | "vprintf" => bp!(("format", Req), ("values", Req)),
        "fprintf" => bp!(("stream", Req), ("format", Req), ("values", Var)),
        "vfprintf" => bp!(("stream", Req), ("format", Req), ("values", Req)),
        "number_format" => bp!(
            ("num", Req),
            ("decimals", Int(0)),
            ("decimal_separator", Str(".")),
            ("thousands_separator", Str(","))
        ),
        "round" => bp!(("num", Req), ("precision", Int(0)), ("mode", Int(1))),
        "intval" | "floatval" | "doubleval" | "strval" | "boolval" => {
            bp!(("value", Req))
        }
        "count" | "sizeof" => bp!(("value", Req), ("mode", Int(0))),
        "array_slice" => bp!(
            ("array", Req),
            ("offset", Req),
            ("length", Null),
            ("preserve_keys", Bool(false))
        ),
        "array_splice" => bp!(
            ("array", Req),
            ("offset", Req),
            ("length", Null),
            ("replacement", Arr)
        ),
        "array_keys" => bp!(
            ("array", Req),
            ("filter_value", Unk),
            ("strict", Bool(false))
        ),
        "array_values"
        | "array_flip"
        | "array_pop"
        | "array_shift"
        | "array_sum"
        | "array_product"
        | "array_rand"
        | "array_change_key_case" => {
            bp!(("array", Req))
        }
        "array_reverse" => bp!(("array", Req), ("preserve_keys", Bool(false))),
        "array_pad" => bp!(("array", Req), ("length", Req), ("value", Req)),
        "array_fill" => bp!(("start_index", Req), ("count", Req), ("value", Req)),
        "array_fill_keys" => bp!(("keys", Req), ("value", Req)),
        "array_combine" => bp!(("keys", Req), ("values", Req)),
        "array_search" | "in_array" => {
            bp!(("needle", Req), ("haystack", Req), ("strict", Bool(false)))
        }
        "array_key_exists" | "key_exists" => bp!(("key", Req), ("array", Req)),
        "assert" => bp!(("assertion", Req), ("description", Null)),
        // call_user_func's variadic is Z_PARAM_VARIADIC('+') — unknown
        // named args forward to the callee (the mod.rs named arm
        // handles them); the *_array stubs are fixed 2-param.
        "call_user_func" | "forward_static_call" => {
            bp!(("callback", Req), ("...", Var))
        }
        "call_user_func_array" | "forward_static_call_array" => {
            bp!(("callback", Req), ("args", Req))
        }
        "array_map" => bp!(("callback", Req), ("array", Req), ("...", Var)),
        "array_filter" => bp!(("array", Req), ("callback", Null), ("mode", Int(0))),
        "array_reduce" => bp!(("array", Req), ("callback", Req), ("initial", Null)),
        "array_walk" | "array_walk_recursive" => {
            bp!(("array", Req), ("callback", Req), ("arg", Null))
        }
        "array_merge"
        | "array_merge_recursive"
        | "array_diff"
        | "array_diff_key"
        | "array_diff_assoc"
        | "array_intersect"
        | "array_intersect_key"
        | "array_intersect_assoc" => bp!(("...", Var)),
        "array_multisort" | "array_replace" | "array_replace_recursive" => {
            bp!(("array", Req), ("...", Var))
        }
        "array_push" | "array_unshift" => bp!(("array", Req), ("...", Var)),
        "max" | "min" => bp!(("value", Req), ("...", Var)),
        "compact" => bp!(("var_name", Req), ("...", Var)),
        "array_column" => bp!(("array", Req), ("column_key", Req), ("index_key", Null)),
        "array_unique" => bp!(("array", Req), ("flags", Int(2))),
        "sort" | "rsort" | "asort" | "arsort" | "ksort" | "krsort" | "natsort" | "natcasesort" => {
            bp!(("array", Req), ("flags", Int(0)))
        }
        "usort" | "uasort" | "uksort" => bp!(("array", Req), ("callback", Req)),
        "range" => bp!(("start", Req), ("end", Req), ("step", Int(1))),
        "preg_match" | "preg_match_all" => bp!(
            ("pattern", Req),
            ("subject", Req),
            ("matches", Null),
            ("flags", Int(0)),
            ("offset", Int(0))
        ),
        "preg_replace" | "preg_filter" | "preg_replace_callback" => bp!(
            ("pattern", Req),
            ("replacement", Req),
            ("subject", Req),
            ("limit", Int(-1)),
            ("count", Null),
            ("flags", Int(0))
        ),
        "preg_replace_callback_array" => bp!(
            ("pattern", Req),
            ("subject", Req),
            ("limit", Int(-1)),
            ("count", Null),
            ("flags", Int(0))
        ),
        "preg_split" => bp!(
            ("pattern", Req),
            ("subject", Req),
            ("limit", Int(-1)),
            ("flags", Int(0))
        ),
        "preg_quote" => bp!(("str", Req), ("delimiter", Null)),
        "preg_grep" => bp!(("pattern", Req), ("array", Req), ("flags", Int(0))),
        "json_encode" => bp!(("value", Req), ("flags", Int(0)), ("depth", Int(512))),
        "json_decode" => bp!(
            ("json", Req),
            ("associative", Null),
            ("depth", Int(512)),
            ("flags", Int(0))
        ),
        "htmlspecialchars" | "htmlentities" => bp!(
            ("string", Req),
            ("flags", Int(11)),
            ("encoding", Null),
            ("double_encode", Bool(true))
        ),
        "htmlspecialchars_decode" | "html_entity_decode" => {
            bp!(("string", Req), ("flags", Int(11)), ("encoding", Null))
        }
        "define" => bp!(
            ("constant_name", Req),
            ("value", Req),
            ("case_insensitive", Bool(false))
        ),
        "defined" | "constant" => bp!(("constant_name", Req)),
        "ini_set" | "ini_alter" => bp!(("option", Req), ("value", Req)),
        "ini_get" | "ini_get_all" => bp!(("option", Req)),
        "error_reporting" => bp!(("error_level", Null)),
        "microtime" => bp!(("as_float", Bool(false))),
        "usleep" => bp!(("microseconds", Req)),
        "sleep" => bp!(("seconds", Req)),
        "proc_open" => bp!(
            ("command", Req),
            ("descriptor_spec", Req),
            ("pipes", Req),
            ("cwd", Null),
            ("env_vars", Null),
            ("options", Null)
        ),
        "proc_close" => bp!(("process", Req)),
        "proc_get_status" => bp!(("process", Req)),
        "proc_terminate" => bp!(("process", Req), ("signal", Int(15))),
        "exec" => bp!(("command", Req), ("output", Null), ("result_code", Null)),
        "system" | "passthru" => bp!(("command", Req), ("result_code", Null)),
        "shell_exec" => bp!(("command", Req)),
        "escapeshellarg" | "escapeshellcmd" => bp!(("arg", Req)),
        "md5" | "sha1" | "crc32" => bp!(("string", Req), ("binary", Bool(false))),
        "file_get_contents" => bp!(
            ("filename", Req),
            ("use_include_path", Bool(false)),
            ("context", Null),
            ("offset", Int(0)),
            ("length", Null)
        ),
        "file_put_contents" => {
            bp!(
                ("filename", Req),
                ("data", Req),
                ("flags", Int(0)),
                ("context", Null)
            )
        }
        "var_export" => bp!(("value", Req), ("return", Bool(false))),
        "getenv" => bp!(("name", Null), ("local_only", Bool(false))),
        "stream_select" => bp!(
            ("read", Req),
            ("write", Req),
            ("except", Req),
            ("seconds", Req),
            ("microseconds", Null)
        ),
        // oracle takes exactly 2 args — zend's $mode has no default.
        "stream_set_blocking" => bp!(("stream", Req), ("mode", Req)),
        "stream_get_meta_data" => bp!(("stream", Req)),
        "flock" => bp!(("stream", Req), ("operation", Req), ("would_block", Null)),
        "header" => bp!(
            ("header", Req),
            ("replace", Bool(true)),
            ("response_code", Int(0))
        ),
        "setcookie" => bp!(("...", Var)),
        _ => return None,
    })
}

/// Zend arginfo parameter types for the internal functions whose
/// `strict_types` argument checks phpun models. Each entry is
/// `(param_name, zpp_type)` where zpp_type is a `|`-joined union of
/// `string`, `int`, `float`, `bool`, `array`, `object`, `callable`,
/// `iterable`, `mixed` with an optional leading `?` for nullable.
/// Under strict mode scalar coercion is disabled (int->float widening
/// is still allowed); a mismatch throws TypeError.
pub fn strict_sig(name: &str) -> Option<Vec<(String, String)>> {
    let str_p = ("string", "string");
    let ps: &[(&str, &str)] = match name {
        "strlen" | "strrev" | "strtoupper" | "strtolower" | "ucfirst" | "lcfirst" | "md5"
        | "sha1" | "str_rot13" | "nl2br" | "quotemeta" | "soundex" => &[str_p],
        "ord" => &[("character", "string")],
        "chr" => &[("codepoint", "int")],
        "str_repeat" | "wordwrap" => &[("string", "string"), ("times", "int")],
        "substr" => &[("string", "string"), ("offset", "int"), ("length", "?int")],
        "strpos" | "stripos" | "strrpos" | "strripos" => &[
            ("haystack", "string"),
            ("needle", "string"),
            ("offset", "int"),
        ],
        "str_contains" | "str_starts_with" | "str_ends_with" => {
            &[("haystack", "string"), ("needle", "string")]
        }
        "strcmp" | "strcasecmp" | "strnatcmp" | "strnatcasecmp" => {
            &[("string1", "string"), ("string2", "string")]
        }
        "strncmp" | "strncasecmp" => &[
            ("string1", "string"),
            ("string2", "string"),
            ("length", "int"),
        ],
        "str_pad" => &[
            ("string", "string"),
            ("length", "int"),
            ("pad_string", "string"),
            ("pad_type", "int"),
        ],
        "trim" | "ltrim" | "rtrim" => &[("string", "string"), ("characters", "string")],
        "str_split" => &[("string", "string"), ("length", "int")],
        "str_replace" | "str_ireplace" => &[
            ("search", "string|array"),
            ("replace", "string|array"),
            ("subject", "string|array"),
        ],
        "explode" => &[
            ("separator", "string"),
            ("string", "string"),
            ("limit", "int"),
        ],
        "implode" | "join" => &[("separator", "?string"), ("array", "?array")],
        "array_map" => &[("callback", "?callable"), ("array", "array")],
        "array_filter" => &[("array", "array"), ("callback", "?callable")],
        "array_reduce" => &[
            ("array", "array"),
            ("callback", "callable"),
            ("initial", "mixed"),
        ],
        "array_walk" | "array_walk_recursive" => {
            &[("array", "array|object"), ("callback", "callable")]
        }
        "usort" | "uasort" | "uksort" => &[("array", "array"), ("callback", "callable")],
        "count" | "sizeof" => &[("value", "array|object"), ("mode", "int")],
        "in_array" => &[
            ("needle", "mixed"),
            ("haystack", "array"),
            ("strict", "bool"),
        ],
        "array_key_exists" | "key_exists" => &[
            ("key", "string|int|float|bool|resource"),
            ("array", "array"),
        ],
        "array_search" => &[
            ("needle", "mixed"),
            ("haystack", "array"),
            ("strict", "bool"),
        ],
        "intdiv" => &[("num1", "int"), ("num2", "int")],
        "abs" => &[("num", "int|float")],
        "array_sum" | "array_product" => &[("array", "array")],
        "range" => &[("start", "mixed"), ("end", "mixed"), ("step", "int|float")],
        "array_slice" => &[
            ("array", "array"),
            ("offset", "int"),
            ("length", "?int"),
            ("preserve_keys", "bool"),
        ],
        "array_splice" => &[
            ("array", "array"),
            ("offset", "int"),
            ("length", "?int"),
            ("replacement", "mixed"),
        ],
        "array_merge" | "array_replace" | "array_merge_recursive" | "array_replace_recursive" => {
            &[("array", "array"), ("arrays", "array")]
        }
        "array_reverse" => &[("array", "array"), ("preserve_keys", "bool")],
        "array_fill" => &[("start_index", "int"), ("count", "int"), ("value", "mixed")],
        "array_fill_keys" => &[("keys", "array"), ("value", "mixed")],
        "array_keys" | "array_values" => &[("array", "array")],
        "array_flip" | "array_unique" | "array_rand" => &[("array", "array")],
        "str_word_count" | "similar_text" => &[("string", "string")],
        "ucwords" | "lcwords" => &[("string", "string"), ("separators", "string")],
        "sprintf" | "printf" => &[("format", "string")],
        "vsprintf" | "vprintf" => &[("format", "string"), ("values", "array")],
        "fprintf" => &[("stream", "resource"), ("format", "string")],
        "vfprintf" => &[
            ("stream", "resource"),
            ("format", "string"),
            ("values", "array"),
        ],
        "number_format" => &[("num", "float"), ("decimals", "int")],
        "preg_match" | "preg_match_all" => &[("pattern", "string"), ("subject", "string")],
        "preg_replace" | "preg_filter" => &[
            ("pattern", "string|array"),
            ("replacement", "string|array"),
            ("subject", "string|array"),
            ("limit", "int"),
        ],
        "preg_replace_callback" => &[
            ("pattern", "string|array"),
            ("callback", "callable"),
            ("subject", "string|array"),
            ("limit", "int"),
        ],
        "preg_replace_callback_array" => &[
            ("pattern", "array"),
            ("subject", "string|array"),
            ("limit", "int"),
        ],
        "preg_split" => &[
            ("pattern", "string"),
            ("subject", "string"),
            ("limit", "int"),
        ],
        "preg_quote" => &[("str", "string"), ("delimiter", "?string")],
        "preg_grep" => &[("pattern", "string"), ("array", "array"), ("flags", "int")],
        "json_encode" => &[("value", "mixed"), ("flags", "int"), ("depth", "int")],
        "json_decode" => &[
            ("json", "string"),
            ("associative", "?bool"),
            ("depth", "int"),
            ("flags", "int"),
        ],
        "serialize" => &[("value", "mixed")],
        "unserialize" => &[("data", "string")],
        "mb_strlen" | "mb_strtoupper" | "mb_strtolower" => {
            &[("string", "string"), ("encoding", "?string")]
        }
        "mb_substr" => &[
            ("string", "string"),
            ("start", "int"),
            ("length", "?int"),
            ("encoding", "?string"),
        ],
        "mb_strpos" | "mb_strrpos" => &[
            ("haystack", "string"),
            ("needle", "string"),
            ("offset", "int"),
            ("encoding", "?string"),
        ],
        "hexdec" => &[("hex_string", "string")],
        "dechex" | "decoct" | "decbin" => &[("num", "int")],
        "base64_encode" => &[("string", "string")],
        "base64_decode" => &[("string", "string"), ("strict", "bool")],
        "bin2hex" => &[("string", "string")],
        "hex2bin" => &[("string", "string")],
        "urlencode" | "urldecode" | "rawurlencode" | "rawurldecode" => &[("string", "string")],
        "strtolower_ascii" => &[("string", "string")],
        "ctype_digit" | "ctype_alpha" | "ctype_alnum" | "ctype_space" | "ctype_upper"
        | "ctype_lower" | "ctype_punct" | "ctype_xdigit" => &[("text", "mixed")],
        "is_string" | "is_int" | "is_float" | "is_bool" | "is_array" | "is_object" | "is_null"
        | "is_scalar" | "is_numeric" => &[("value", "mixed")],
        "strtolower_mb" => &[("string", "string")],
        _ => return None,
    };
    Some(
        ps.iter()
            .map(|(n, t)| (n.to_string(), t.to_string()))
            .collect(),
    )
}
