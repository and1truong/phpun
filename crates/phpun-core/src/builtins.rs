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
        match f(it, name, args) {
            Ok(None) => continue,
            r => {
                // A string conversion that failed mid-builtin aborts
                // the call — zend dies at the Z_PARAM_* before the
                // builtin would have run, so the deferred error wins
                // over whatever it went on to return.
                if let Some(e) = it.take_cast_err() {
                    return Err(e);
                }
                return r;
            }
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
        "clone" => &[("object", true), ("withProperties", false)],
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

/// Every declared internal-function name — the table
/// `get_defined_functions()['internal']` walks and `function_exists`
/// probes (kept sorted; add a name here when a new family arm lands).
///
/// Sync rule: every name needs a dispatch arm in builtins/*.rs.
/// Listed-but-undispatched (deliberate — the whole family is
/// unimplemented and belongs to another ticket): fputcsv, fscanf,
/// sscanf, get_meta_tags, output_add_rewrite_var, filegroup,
/// fileinode, fileowner, filetype. function_exists stays true like
/// zend (the functions are declared there too); calling one fatals
/// "Call to undefined function" until that family lands.
pub(crate) const BUILTIN_NAMES: &[&str] = &[
    "abs",
    "acos",
    "addcslashes",
    "addslashes",
    "array_change_key_case",
    "array_chunk",
    "array_column",
    "array_combine",
    "array_count_values",
    "array_diff",
    "array_diff_assoc",
    "array_diff_key",
    "array_diff_uassoc",
    "array_diff_ukey",
    "array_fill",
    "array_fill_keys",
    "array_filter",
    "array_first",
    "array_flip",
    "array_intersect",
    "array_intersect_assoc",
    "array_intersect_key",
    "array_intersect_uassoc",
    "array_intersect_ukey",
    "array_is_list",
    "array_key_exists",
    "array_key_first",
    "array_key_last",
    "array_keys",
    "array_map",
    "array_merge",
    "array_merge_recursive",
    "array_multisort",
    "array_pad",
    "array_pop",
    "array_product",
    "array_push",
    "array_rand",
    "array_reduce",
    "array_replace",
    "array_replace_recursive",
    "array_reverse",
    "array_search",
    "array_shift",
    "array_slice",
    "array_splice",
    "array_sum",
    "array_udiff",
    "array_udiff_assoc",
    "array_udiff_uassoc",
    "array_uintersect",
    "array_uintersect_assoc",
    "array_uintersect_uassoc",
    "array_unique",
    "array_unshift",
    "array_values",
    "array_walk",
    "array_walk_recursive",
    "arsort",
    "asin",
    "asort",
    "assert",
    "assert_options",
    "atan",
    "atan2",
    "base64_decode",
    "base64_encode",
    "base_convert",
    "basename",
    "bin2hex",
    "bindec",
    "boolval",
    "call_user_func",
    "call_user_func_array",
    "ceil",
    "chdir",
    "checkdate",
    "chgrp",
    "chmod",
    "chop",
    "chown",
    "chr",
    "chunk_split",
    "class_alias",
    "class_exists",
    "class_implements",
    "class_parents",
    "class_uses",
    "clearstatcache",
    "cli_get_process_title",
    "cli_set_process_title",
    "clone",
    "closedir",
    "closelog",
    "compact",
    "connection_aborted",
    "connection_status",
    "constant",
    "copy",
    "cos",
    "cosh",
    "count",
    "count_chars",
    "crc32",
    "ctype_alnum",
    "ctype_alpha",
    "ctype_cntrl",
    "ctype_digit",
    "ctype_graph",
    "ctype_lower",
    "ctype_print",
    "ctype_punct",
    "ctype_space",
    "ctype_upper",
    "ctype_xdigit",
    "current",
    "date",
    "date_default_timezone_get",
    "date_default_timezone_set",
    "date_parse",
    "date_sun_info",
    "date_sunrise",
    "date_sunset",
    "debug_backtrace",
    "debug_print_backtrace",
    "debug_zval_dump",
    "decbin",
    "dechex",
    "decoct",
    "define",
    "defined",
    "deg2rad",
    "die",
    "dirname",
    "disk_free_space",
    "disk_total_space",
    "diskfreespace",
    "dl",
    "doubleval",
    "end",
    "enum_exists",
    "error_clear_last",
    "error_get_last",
    "error_reporting",
    "escapeshellarg",
    "escapeshellcmd",
    "exec",
    "exit",
    "exp",
    "explode",
    "extension_loaded",
    "extract",
    "fastcgi_finish_request",
    "fclose",
    "fdiv",
    "feof",
    "fflush",
    "fgetc",
    "fgetcsv",
    "fgets",
    "file",
    "file_exists",
    "file_get_contents",
    "file_put_contents",
    "fileatime",
    "filectime",
    "filegroup",
    "fileinode",
    "filemtime",
    "fileowner",
    "fileperms",
    "filesize",
    "filetype",
    "filter_var",
    "floatval",
    "flock",
    "floor",
    "flush",
    "fmod",
    "fnmatch",
    "fopen",
    "forward_static_call",
    "forward_static_call_array",
    "fpassthru",
    "fprintf",
    "fputcsv",
    "fputs",
    "fread",
    "fscanf",
    "fseek",
    "fstat",
    "ftell",
    "ftruncate",
    "func_get_arg",
    "func_get_args",
    "func_num_args",
    "function_exists",
    "fwrite",
    "gc_collect_cycles",
    "gc_disable",
    "gc_enable",
    "gc_enabled",
    "gc_mem_caches",
    "gc_status",
    "get_called_class",
    "get_cfg_var",
    "get_class",
    "get_class_methods",
    "get_class_vars",
    "get_current_user",
    "get_debug_type",
    "get_declared_classes",
    "get_declared_interfaces",
    "get_declared_traits",
    "get_defined_functions",
    "get_extension_funcs",
    "get_include_path",
    "get_loaded_extensions",
    "get_mangled_object_vars",
    "get_meta_tags",
    "get_object_vars",
    "get_parent_class",
    "get_resource_id",
    "get_resource_type",
    "getcwd",
    "getenv",
    "getmygid",
    "getmyinode",
    "getmypid",
    "getmyuid",
    "getopt",
    "getrandmax",
    "gettype",
    "glob",
    "gmdate",
    "gmmktime",
    "hash",
    "hash_equals",
    "hash_pbkdf2",
    "header",
    "header_register_callback",
    "header_remove",
    "headers_list",
    "headers_sent",
    "hex2bin",
    "hexdec",
    "highlight_file",
    "highlight_string",
    "hrtime",
    "html_entity_decode",
    "htmlentities",
    "htmlspecialchars",
    "htmlspecialchars_decode",
    "http_build_query",
    "http_response_code",
    "hypot",
    "ignore_user_abort",
    "implode",
    "in_array",
    "ini_alter",
    "ini_get",
    "ini_get_all",
    "ini_parse_quantity",
    "ini_restore",
    "ini_set",
    "intdiv",
    "interface_exists",
    "intval",
    "ip2long",
    "is_a",
    "is_array",
    "is_bool",
    "is_callable",
    "is_countable",
    "is_dir",
    "is_double",
    "is_executable",
    "is_file",
    "is_finite",
    "is_float",
    "is_infinite",
    "is_int",
    "is_integer",
    "is_iterable",
    "is_link",
    "is_long",
    "is_nan",
    "is_null",
    "is_numeric",
    "is_object",
    "is_readable",
    "is_resource",
    "is_scalar",
    "is_string",
    "is_subclass_of",
    "is_uploaded_file",
    "is_writable",
    "is_writeable",
    "iterator_apply",
    "iterator_count",
    "iterator_to_array",
    "join",
    "json_decode",
    "json_encode",
    "json_last_error",
    "json_last_error_msg",
    "json_validate",
    "key",
    "key_exists",
    "krsort",
    "ksort",
    "lcfirst",
    "lcg_value",
    "levenshtein",
    "link",
    "linkinfo",
    "log",
    "log10",
    "lstat",
    "ltrim",
    "mail",
    "max",
    "mb_check_encoding",
    "mb_chr",
    "mb_convert_case",
    "mb_convert_encoding",
    "mb_convert_variables",
    "mb_detect_encoding",
    "mb_detect_order",
    "mb_encoding_aliases",
    "mb_http_input",
    "mb_http_output",
    "mb_internal_encoding",
    "mb_language",
    "mb_lcfirst",
    "mb_list_encodings",
    "mb_ltrim",
    "mb_ord",
    "mb_regex_encoding",
    "mb_rtrim",
    "mb_scrub",
    "mb_split",
    "mb_str_pad",
    "mb_str_split",
    "mb_strcut",
    "mb_stripos",
    "mb_stristr",
    "mb_strlen",
    "mb_strpos",
    "mb_strrchr",
    "mb_strrichr",
    "mb_strripos",
    "mb_strrpos",
    "mb_strstr",
    "mb_strtolower",
    "mb_strtoupper",
    "mb_substitute_character",
    "mb_substr",
    "mb_substr_count",
    "mb_trim",
    "mb_ucfirst",
    "md5",
    "memory_get_peak_usage",
    "memory_get_usage",
    "memory_reset_peak_usage",
    "metaphone",
    "method_exists",
    "microtime",
    "min",
    "mkdir",
    "mktime",
    "move_uploaded_file",
    "mt_getrandmax",
    "mt_rand",
    "mt_srand",
    "natcasesort",
    "natsort",
    "next",
    "nl2br",
    "number_format",
    "ob_clean",
    "ob_end_clean",
    "ob_end_flush",
    "ob_flush",
    "ob_get_clean",
    "ob_get_contents",
    "ob_get_flush",
    "ob_get_length",
    "ob_get_level",
    "ob_get_status",
    "ob_implicit_flush",
    "ob_list_handlers",
    "ob_start",
    "octdec",
    "opendir",
    "openlog",
    "openssl_random_pseudo_bytes",
    "openssl_x509_parse",
    "ord",
    "output_add_rewrite_var",
    "output_reset_rewrite_vars",
    "pack",
    "parse_ini_file",
    "parse_ini_string",
    "parse_str",
    "parse_url",
    "passthru",
    "pathinfo",
    "pathinfo_dirname",
    "pclose",
    "php_ini_loaded_file",
    "php_ini_scanned_files",
    "php_sapi_name",
    "php_strip_whitespace",
    "php_uname",
    "phpcredits",
    "phpinfo",
    "phpversion",
    "pi",
    "popen",
    "pos",
    "posix_isatty",
    "pow",
    "preg_filter",
    "preg_grep",
    "preg_jit",
    "preg_last_error",
    "preg_last_error_msg",
    "preg_match",
    "preg_match_all",
    "preg_quote",
    "preg_replace",
    "preg_replace_callback",
    "preg_replace_callback_array",
    "preg_split",
    "prev",
    "print",
    "print_r",
    "printf",
    "proc_close",
    "proc_get_status",
    "proc_nice",
    "proc_open",
    "proc_terminate",
    "property_exists",
    "putenv",
    "quotemeta",
    "rad2deg",
    "rand",
    "random_bytes",
    "random_int",
    "range",
    "rawurldecode",
    "rawurlencode",
    "readdir",
    "readfile",
    "readlink",
    "realpath",
    "register_shutdown_function",
    "register_tick_function",
    "rename",
    "reset",
    "restore_error_handler",
    "restore_exception_handler",
    "rewind",
    "rewinddir",
    "rmdir",
    "round",
    "rsort",
    "rtrim",
    "scandir",
    "serialize",
    "set_error_handler",
    "set_exception_handler",
    "set_include_path",
    "set_time_limit",
    "setcookie",
    "setlocale",
    "setrawcookie",
    "settype",
    "sha1",
    "shell_exec",
    "show_source",
    "shuffle",
    "similar_text",
    "sin",
    "sinh",
    "sizeof",
    "sleep",
    "sort",
    "soundex",
    "spl_autoload_call",
    "spl_autoload_functions",
    "spl_autoload_register",
    "spl_autoload_unregister",
    "spl_object_hash",
    "spl_object_id",
    "sprintf",
    "sqrt",
    "srand",
    "sscanf",
    "stat",
    "str_contains",
    "str_ends_with",
    "str_ireplace",
    "str_pad",
    "str_repeat",
    "str_replace",
    "str_rot13",
    "str_split",
    "str_starts_with",
    "str_word_count",
    "strcasecmp",
    "strchr",
    "strcmp",
    "strcspn",
    "stream_bucket_append",
    "stream_bucket_make_writeable",
    "stream_bucket_new",
    "stream_bucket_prepend",
    "stream_context_create",
    "stream_context_get_default",
    "stream_context_get_options",
    "stream_context_set_option",
    "stream_copy_to_stream",
    "stream_filter_append",
    "stream_filter_prepend",
    "stream_filter_register",
    "stream_filter_remove",
    "stream_get_contents",
    "stream_get_filters",
    "stream_get_meta_data",
    "stream_get_wrappers",
    "stream_isatty",
    "stream_select",
    "stream_set_blocking",
    "stream_set_chunk_size",
    "stream_set_read_buffer",
    "stream_set_timeout",
    "stream_set_write_buffer",
    "stream_socket_pair",
    "stream_wrapper_register",
    "stream_wrapper_unregister",
    "strip_tags",
    "stripcslashes",
    "stripos",
    "stripslashes",
    "stristr",
    "strlen",
    "strncasecmp",
    "strncmp",
    "strpos",
    "strrev",
    "strripos",
    "strrpos",
    "strspn",
    "strstr",
    "strtolower",
    "strtotime",
    "strtoupper",
    "strtr",
    "strval",
    "substr",
    "substr_count",
    "substr_replace",
    "symlink",
    "sys_get_temp_dir",
    "syslog",
    "system",
    "tan",
    "tanh",
    "tempnam",
    "time",
    "time_nanosleep",
    "time_sleep_until",
    "tmpfile",
    "token_get_all",
    "token_name",
    "touch",
    "trait_exists",
    "trigger_error",
    "trim",
    "uasort",
    "ucfirst",
    "ucwords",
    "uksort",
    "umask",
    "uniqid",
    "unlink",
    "unpack",
    "unregister_tick_function",
    "unserialize",
    "urldecode",
    "urlencode",
    "user_error",
    "usleep",
    "usort",
    "var_dump",
    "var_export",
    "version_compare",
    "vfprintf",
    "vprintf",
    "vsprintf",
    "wordwrap",
    "zend_version",
];

pub(crate) fn is_builtin(n: &str) -> bool {
    // BUILTIN_NAMES is the declared internal function table — keep it
    // in sync with the family dispatch arms when adding builtins
    // (a dispatch arm without a name here is unreachable).
    BUILTIN_NAMES.binary_search(&n).is_ok()
}

/// The internal names, in zend's function-table order (alphabetical —
/// the const list is kept sorted so binary_search works).
pub(crate) fn builtin_names() -> &'static [&'static str] {
    BUILTIN_NAMES
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
    /// Required in arginfo but optional at runtime: Zend's ZPP accepts
    /// the call without these while reflection and the named-arg arity
    /// check still count them as required (rand/mt_rand min,max).
    OptReq,
    Var,
}

impl BDef {
    pub fn val(self) -> Value {
        match self {
            BDef::Req | BDef::Var | BDef::OptReq => Value::Null,
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
        "implode" | "join" => bp!(("separator", Req), ("array", Null)),
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
        "clone" => bp!(("object", Req), ("withProperties", Arr)),
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
            bp!(("callback", Req), ("args", Var))
        }
        "call_user_func_array" | "forward_static_call_array" => {
            bp!(("callback", Req), ("args", Req))
        }
        "array_map" => bp!(("callback", Req), ("array", Req), ("arrays", Var)),
        "array_filter" => bp!(("array", Req), ("callback", Null), ("mode", Int(0))),
        "array_reduce" => bp!(("array", Req), ("callback", Req), ("initial", Null)),
        "array_walk" | "array_walk_recursive" => {
            bp!(("array", Req), ("callback", Req), ("arg", Null))
        }
        "array_merge" | "array_merge_recursive" => bp!(("arrays", Var)),
        "array_diff"
        | "array_diff_key"
        | "array_diff_assoc"
        | "array_intersect"
        | "array_intersect_key"
        | "array_intersect_assoc" => bp!(("array", Req), ("arrays", Var)),
        "array_multisort" => bp!(("array", Req), ("rest", Var)),
        "array_replace" | "array_replace_recursive" => {
            bp!(("array", Req), ("replacements", Var))
        }
        "array_push" | "array_unshift" => bp!(("array", Req), ("...", Var)),
        "reset" | "end" | "next" | "prev" | "current" | "pos" | "shuffle" => {
            bp!(("array", Req))
        }
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
        "htmlspecialchars_decode" => bp!(("string", Req), ("flags", Int(11))),
        "html_entity_decode" => {
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
        "proc_nice" => bp!(("priority", Req)),
        "exec" => bp!(("command", Req), ("output", Null), ("result_code", Null)),
        "system" | "passthru" => bp!(("command", Req), ("result_code", Null)),
        "shell_exec" => bp!(("command", Req)),
        "escapeshellarg" | "escapeshellcmd" => bp!(("arg", Req)),
        "md5" | "sha1" => bp!(("string", Req), ("binary", Bool(false))),
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
        "stream_set_blocking" => bp!(("stream", Req), ("enable", Req)),
        "header" => bp!(
            ("header", Req),
            ("replace", Bool(true)),
            ("response_code", Int(0))
        ),
        "setcookie" => bp!(
            ("name", Req),
            ("value", Str("")),
            ("expires_or_options", Int(0)),
            ("path", Str("")),
            ("domain", Str("")),
            ("secure", Bool(false)),
            ("httponly", Bool(false))
        ),
        // fs / stream arginfo — names + requiredness mirror Zend stubs
        // so reflection arity and named-arg binding match (fwrite
        // (stream,data,length) → 3/2).
        "fwrite" | "fputs" => bp!(("stream", Req), ("data", Req), ("length", Null)),
        "fread" => bp!(("stream", Req), ("length", Req)),
        "fseek" => bp!(("stream", Req), ("offset", Req), ("whence", Int(0))),
        "ftell" | "fclose" | "feof" | "fgetc" | "fpassthru" | "rewind" | "fflush" | "fstat" => {
            bp!(("stream", Req))
        }
        "pclose" => bp!(("handle", Req)),
        "fgets" => bp!(("stream", Req), ("length", Null)),
        "fgetcsv" => bp!(
            ("stream", Req),
            ("length", Null),
            ("separator", Str(",")),
            ("enclosure", Str("\"")),
            ("escape", Str("\\"))
        ),
        "fputcsv" => bp!(
            ("stream", Req),
            ("fields", Req),
            ("separator", Str(",")),
            ("enclosure", Str("\"")),
            ("escape", Str("\\")),
            ("eol", Str("\n"))
        ),
        "fscanf" => bp!(("stream", Req), ("format", Req), ("vars", Var)),
        "sscanf" => bp!(("string", Req), ("format", Req), ("vars", Var)),
        "get_meta_tags" => bp!(("filename", Req), ("use_include_path", Bool(false))),
        "get_defined_functions" => bp!(("exclude_disabled", Bool(true))),
        "flock" => bp!(("stream", Req), ("operation", Req), ("would_block", Null)),
        "fopen" => bp!(
            ("filename", Req),
            ("mode", Req),
            ("use_include_path", Bool(false)),
            ("context", Null)
        ),
        "ftruncate" => bp!(("stream", Req), ("size", Req)),
        "popen" => bp!(("command", Req), ("mode", Req)),
        "unlink" => bp!(("filename", Req), ("context", Null)),
        "rename" => bp!(("from", Req), ("to", Req), ("context", Null)),
        "copy" => bp!(("from", Req), ("to", Req), ("context", Null)),
        "mkdir" => bp!(
            ("directory", Req),
            ("permissions", Int(0o777)),
            ("recursive", Bool(false)),
            ("context", Null)
        ),
        "rmdir" => bp!(("directory", Req), ("context", Null)),
        "umask" => bp!(("mask", Null)),
        "chmod" => bp!(("filename", Req), ("permissions", Req)),
        "chown" => bp!(("filename", Req), ("user", Req)),
        "chgrp" => bp!(("filename", Req), ("group", Req)),
        "touch" => bp!(("filename", Req), ("mtime", Null), ("atime", Null)),
        "symlink" | "link" => bp!(("target", Req), ("link", Req)),
        "linkinfo" | "readlink" => bp!(("path", Req)),
        "stat" | "lstat" | "fileatime" | "filectime" | "filemtime" | "filesize" | "filetype"
        | "fileperms" | "fileinode" | "fileowner" | "filegroup" => {
            bp!(("filename", Req))
        }
        "is_file" | "is_dir" | "is_link" | "is_readable" | "is_writable" | "is_writeable"
        | "is_executable" | "file_exists" | "is_uploaded_file" => {
            bp!(("filename", Req))
        }
        "basename" => bp!(("path", Req), ("suffix", Str(""))),
        "dirname" | "pathinfo_dirname" => bp!(("path", Req), ("levels", Int(1))),
        "pathinfo" => bp!(("path", Req), ("flags", Int(15))),
        "realpath" => bp!(("path", Req)),
        "glob" => bp!(("pattern", Req), ("flags", Int(0))),
        "scandir" => bp!(
            ("directory", Req),
            ("sorting_order", Int(0)),
            ("context", Null)
        ),
        "file" => bp!(("filename", Req), ("flags", Int(0)), ("context", Null)),
        "readfile" => bp!(
            ("filename", Req),
            ("use_include_path", Bool(false)),
            ("context", Null)
        ),
        "parse_ini_file" => bp!(
            ("filename", Req),
            ("process_sections", Bool(false)),
            ("scanner_mode", Int(0))
        ),
        "parse_ini_string" => bp!(
            ("ini_string", Req),
            ("process_sections", Bool(false)),
            ("scanner_mode", Int(0))
        ),
        "fnmatch" => bp!(("pattern", Req), ("filename", Req), ("flags", Int(0))),
        "disk_free_space" | "disk_total_space" | "diskfreespace" => {
            bp!(("directory", Req))
        }
        "tempnam" => bp!(("directory", Req), ("prefix", Req)),
        "opendir" => bp!(("directory", Req), ("context", Null)),
        "closedir" | "readdir" => bp!(("dir_handle", Null)),
        "chdir" => bp!(("directory", Req)),
        "clearstatcache" => bp!(("clear_realpath_cache", Bool(false)), ("filename", Str(""))),
        "move_uploaded_file" => bp!(("from", Req), ("to", Req)),
        "stream_get_contents" => bp!(("stream", Req), ("length", Null), ("offset", Int(-1))),
        "stream_get_meta_data" => bp!(("stream", Req)),
        "stream_copy_to_stream" => bp!(
            ("from", Req),
            ("to", Req),
            ("length", Null),
            ("offset", Int(0))
        ),
        "stream_context_create" => bp!(("options", Null), ("params", Null)),
        "stream_context_get_default" => bp!(("options", Null)),
        "stream_context_get_options" => bp!(("stream_or_context", Req)),
        "stream_context_set_option" => bp!(
            ("context", Req),
            ("wrapper_or_options", Req),
            ("option_name", Null),
            ("value", Unk)
        ),
        "stream_filter_prepend" | "stream_filter_append" => bp!(
            ("stream", Req),
            ("filter_name", Req),
            ("mode", Int(0)),
            ("params", Unk)
        ),
        "stream_filter_remove" => bp!(("stream_filter", Req)),
        "stream_bucket_new" => bp!(("stream", Req), ("buffer", Req)),
        "stream_bucket_append" | "stream_bucket_prepend" => {
            bp!(("brigade", Req), ("bucket", Req))
        }
        "stream_bucket_make_writeable" => bp!(("brigade", Req)),
        "headers_sent" => bp!(("filename", Null), ("line", Null)),
        // Oracle arginfo for internal functions whose reflection
        // signature was previously unknown (reported via the (0,0)
        // catch-all).
        "abs" => bp!(("num", Req)),
        "addcslashes" => bp!(("string", Req), ("characters", Req)),
        "addslashes" => bp!(("string", Req)),
        "array_chunk" => {
            bp!(
                ("array", Req),
                ("length", Req),
                ("preserve_keys", Bool(false))
            )
        }
        "array_count_values" | "array_key_first" | "array_key_last" => {
            bp!(("array", Req))
        }
        "base64_encode" | "bin2hex" | "hex2bin" | "quotemeta" | "rawurlencode" | "rawurldecode"
        | "serialize" | "soundex" | "stripcslashes" | "stripslashes" | "urldecode"
        | "urlencode" => bp!(("string", Req)),
        "base64_decode" => bp!(("string", Req), ("strict", Bool(false))),
        "chunk_split" => bp!(
            ("string", Req),
            ("length", Int(76)),
            ("separator", Str("\r\n"))
        ),
        "class_exists" => bp!(("class", Req), ("autoload", Bool(true))),
        "class_parents" | "class_implements" | "class_uses" => {
            bp!(("object_or_class", Req), ("autoload", Bool(true)))
        }
        "connection_aborted"
        | "connection_status"
        | "error_clear_last"
        | "error_get_last"
        | "func_get_args"
        | "func_num_args"
        | "gc_collect_cycles"
        | "gc_disable"
        | "gc_enable"
        | "gc_mem_caches"
        | "gc_status"
        | "get_called_class"
        | "get_declared_classes"
        | "get_declared_interfaces"
        | "get_declared_traits"
        | "get_include_path"
        | "getmypid"
        | "getrandmax"
        | "lcg_value"
        | "mt_getrandmax"
        | "memory_reset_peak_usage"
        | "ob_clean"
        | "ob_end_clean"
        | "ob_end_flush"
        | "ob_flush"
        | "ob_get_clean"
        | "ob_get_contents"
        | "ob_get_flush"
        | "ob_get_length"
        | "ob_get_level"
        | "ob_list_handlers"
        | "output_reset_rewrite_vars"
        | "preg_last_error"
        | "preg_last_error_msg"
        | "restore_error_handler"
        | "restore_exception_handler"
        | "sys_get_temp_dir" => bp!(),
        "count_chars" => bp!(("string", Req), ("mode", Int(0))),
        "crc32" => bp!(("string", Req)),
        "dechex" => bp!(("num", Req)),
        "debug_backtrace" => bp!(("options", Int(1)), ("limit", Int(0))),
        "debug_print_backtrace" => bp!(("options", Int(0)), ("limit", Int(0))),
        "debug_zval_dump" => bp!(("value", Req), ("values", Var)),
        "register_shutdown_function" | "register_tick_function" => {
            bp!(("callback", Req), ("args", Var))
        }
        "enum_exists" => bp!(("enum", Req), ("autoload", Bool(true))),
        "exit" | "die" => bp!(("status", Int(0))),
        "extension_loaded" | "get_extension_funcs" => bp!(("extension", Req)),
        "extract" => bp!(("array", Req), ("flags", Int(0)), ("prefix", Str(""))),
        "func_get_arg" => bp!(("position", Req)),
        "function_exists" => bp!(("function", Req)),
        // get_class's $object is optional (the no-arg form reads the
        // calling scope, deprecated since 8.0); get_class_methods takes
        // object|string as $object_or_class.
        "get_class" => bp!(("object", Null)),
        "get_class_methods" => bp!(("object_or_class", Req)),
        "get_object_vars" => bp!(("object", Req)),
        "get_class_vars" => bp!(("class", Req)),
        "get_debug_type" | "gettype" | "is_array" | "is_bool" | "is_countable" | "is_double"
        | "is_float" | "is_int" | "is_integer" | "is_iterable" | "is_long" | "is_null"
        | "is_numeric" | "is_object" | "is_resource" | "is_scalar" | "is_string" => {
            bp!(("value", Req))
        }
        "get_loaded_extensions" => bp!(("zend_extensions", Bool(false))),
        "get_parent_class" => bp!(("object_or_class", Req)),
        "get_resource_id" | "get_resource_type" => bp!(("resource", Req)),
        "hash" => bp!(
            ("algo", Req),
            ("data", Req),
            ("binary", Bool(false)),
            ("options", Arr)
        ),
        "ignore_user_abort" => bp!(("enable", Null)),
        "ini_parse_quantity" => bp!(("shorthand", Req)),
        "ini_restore" => bp!(("option", Req)),
        "interface_exists" => bp!(("interface", Req), ("autoload", Bool(true))),
        "is_a" => bp!(
            ("object_or_class", Req),
            ("class", Req),
            ("allow_string", Bool(false))
        ),
        "is_callable" => bp!(
            ("value", Req),
            ("syntax_only", Bool(false)),
            ("callable_name", Null)
        ),
        "is_subclass_of" => bp!(
            ("object_or_class", Req),
            ("class", Req),
            ("allow_string", Bool(true))
        ),
        "json_validate" => bp!(("json", Req), ("depth", Int(512)), ("flags", Int(0))),
        "levenshtein" => bp!(
            ("string1", Req),
            ("string2", Req),
            ("insertion_cost", Int(1)),
            ("replacement_cost", Int(1)),
            ("deletion_cost", Int(1))
        ),
        "mb_check_encoding" => bp!(("value", Null), ("encoding", Null)),
        "mb_convert_case" => bp!(("string", Req), ("mode", Req), ("encoding", Null)),
        "mb_convert_encoding" => {
            bp!(
                ("string", Req),
                ("to_encoding", Req),
                ("from_encoding", Null)
            )
        }
        "mb_detect_encoding" => {
            bp!(
                ("string", Req),
                ("encodings", Null),
                ("strict", Bool(false))
            )
        }
        "mb_str_split" => bp!(("string", Req), ("length", Int(1)), ("encoding", Null)),
        "mb_stripos" | "mb_strpos" | "mb_strripos" | "mb_strrpos" => bp!(
            ("haystack", Req),
            ("needle", Req),
            ("offset", Int(0)),
            ("encoding", Null)
        ),
        "mb_stristr" | "mb_strrchr" | "mb_strrichr" | "mb_strstr" => bp!(
            ("haystack", Req),
            ("needle", Req),
            ("before_needle", Bool(false)),
            ("encoding", Null)
        ),
        "mb_strlen" | "mb_strtolower" | "mb_strtoupper" => {
            bp!(("string", Req), ("encoding", Null))
        }
        "mb_substr" => bp!(
            ("string", Req),
            ("start", Req),
            ("length", Null),
            ("encoding", Null)
        ),
        "mb_substr_count" => bp!(("haystack", Req), ("needle", Req), ("encoding", Null)),
        "memory_get_peak_usage" | "memory_get_usage" => bp!(("real_usage", Bool(false))),
        "metaphone" => bp!(("string", Req), ("max_phonemes", Int(0))),
        "method_exists" => bp!(("object_or_class", Req), ("method", Req)),
        "mt_rand" | "rand" => bp!(("min", OptReq), ("max", OptReq)),
        "ob_get_status" => bp!(("full_status", Bool(false))),
        "ob_implicit_flush" => bp!(("enable", Bool(true))),
        "ob_start" => bp!(
            ("callback", Null),
            ("chunk_size", Int(0)),
            ("flags", Int(112))
        ),
        "output_add_rewrite_var" => bp!(("name", Req), ("value", Req)),
        "pow" => bp!(("num", Req), ("exponent", Req)),
        "print_r" => bp!(("value", Req), ("return", Bool(false))),
        "property_exists" => bp!(("object_or_class", Req), ("property", Req)),
        "putenv" => bp!(("assignment", Req)),
        "random_bytes" => bp!(("length", Req)),
        "random_int" => bp!(("min", Req), ("max", Req)),
        "set_error_handler" => bp!(("callback", Req), ("error_levels", Int(30719))),
        "set_exception_handler" => bp!(("callback", Req)),
        "set_include_path" => bp!(("include_path", Req)),
        "set_time_limit" => bp!(("seconds", Req)),
        "settype" => bp!(("var", Req), ("type", Req)),
        "similar_text" => bp!(("string1", Req), ("string2", Req), ("percent", Null)),
        "str_split" => bp!(("string", Req), ("length", Int(1))),
        "str_word_count" => {
            bp!(("string", Req), ("format", Int(0)), ("characters", Null))
        }
        "strchr" | "stristr" | "strstr" => bp!(
            ("haystack", Req),
            ("needle", Req),
            ("before_needle", Bool(false))
        ),
        "strip_tags" => bp!(("string", Req), ("allowed_tags", Null)),
        "strtr" => bp!(("string", Req), ("from", Req), ("to", Null)),
        "substr_count" => bp!(
            ("haystack", Req),
            ("needle", Req),
            ("offset", Int(0)),
            ("length", Null)
        ),
        "time_nanosleep" => bp!(("seconds", Req), ("nanoseconds", Req)),
        "time_sleep_until" => bp!(("timestamp", Req)),
        "trait_exists" => bp!(("trait", Req), ("autoload", Bool(true))),
        "trigger_error" | "user_error" => bp!(("message", Req), ("error_level", Int(1024))),
        "uniqid" => bp!(("prefix", Str("")), ("more_entropy", Bool(false))),
        "unserialize" => bp!(("data", Req), ("options", Arr)),
        "unregister_tick_function" => bp!(("callback", Req)),
        "var_dump" => bp!(("value", Req), ("values", Var)),
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
        "defined" => &[("constant_name", "string")],
        "fwrite" | "fputs" => &[
            ("stream", "resource"),
            ("data", "string"),
            ("length", "?int"),
        ],
        "constant" => &[("name", "string")],
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
        "substr_replace" => &[
            ("string", "array|string"),
            ("replace", "string"),
            ("offset", "array|int"),
            ("length", "array|int|null"),
        ],
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
        "trim" | "ltrim" | "rtrim" | "chop" => &[("string", "string"), ("characters", "string")],
        "str_split" => &[("string", "string"), ("length", "int")],
        "str_replace" | "str_ireplace" => &[
            ("search", "array|string"),
            ("replace", "array|string"),
            ("subject", "array|string"),
        ],
        "explode" => &[
            ("separator", "string"),
            ("string", "string"),
            ("limit", "int"),
        ],
        "implode" | "join" => &[("separator", "array|string"), ("array", "?array")],
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
        "count" | "sizeof" => &[("value", "Countable|array"), ("mode", "int")],
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
