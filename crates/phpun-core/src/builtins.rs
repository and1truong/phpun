//! Builtin function table. `call` returns Ok(Some(v)) when `name` is a
//! builtin, Ok(None) when it isn't (the interpreter then tries userland).

use crate::error::PhpError;
use crate::interp::Interp;
use crate::value::{
    compare, numeric, to_key, ArrKey, Cell, Numeric, PhpArray, PhpObject, PhpResource, Value,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

fn cell(v: Value) -> Cell {
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

/// Byte-faithful arg conversion — PHP strings are byte arrays.
/// PCRE2 match/scan error code → PHP preg_last_error() code.
fn preg_rc_err(rc: i32) -> i64 {
    match rc {
        -47 => 2, // PCRE2_ERROR_MATCHLIMIT
        -53 => 3, // PCRE2_ERROR_DEPTHLIMIT
        // UTF-8 subject/pattern errors (UTF8_ERR1..21 and
        // UTF16-range) and BadNewline → PREG_BAD_UTF8_ERROR.
        x if (-56..=-36).contains(&x) || (-200..=-169).contains(&x) => 4,
        _ => 1,
    }
}

fn arg_bs(it: &mut Interp, args: &[Cell], i: usize) -> Vec<u8> {
    it.to_bytes_of(&arg(args, i))
}

// ----- byte-string helpers (PHP string fns are byte operations) -----

/// First index of `needle` in `hay[from..]` (PHP string search = bytes).
// HTML 4.01 named entities used by `htmlentities` (the ENT_HTML401
// table PHP ships): Latin-1 supplement, symbols/math, Greek, misc.
// Codepoints without a named entity fall back to `&#N;`.
static HTML_ENTITIES: &[(u32, &str)] = &[
    (0xA0, "nbsp"),
    (0xA1, "iexcl"),
    (0xA2, "cent"),
    (0xA3, "pound"),
    (0xA4, "curren"),
    (0xA5, "yen"),
    (0xA6, "brvbar"),
    (0xA7, "sect"),
    (0xA8, "uml"),
    (0xA9, "copy"),
    (0xAA, "ordf"),
    (0xAB, "laquo"),
    (0xAC, "not"),
    (0xAD, "shy"),
    (0xAE, "reg"),
    (0xAF, "macr"),
    (0xB0, "deg"),
    (0xB1, "plusmn"),
    (0xB2, "sup2"),
    (0xB3, "sup3"),
    (0xB4, "acute"),
    (0xB5, "micro"),
    (0xB6, "para"),
    (0xB7, "middot"),
    (0xB8, "cedil"),
    (0xB9, "sup1"),
    (0xBA, "ordm"),
    (0xBB, "raquo"),
    (0xBC, "frac14"),
    (0xBD, "frac12"),
    (0xBE, "frac34"),
    (0xBF, "iquest"),
    (0xC0, "Agrave"),
    (0xC1, "Aacute"),
    (0xC2, "Acirc"),
    (0xC3, "Atilde"),
    (0xC4, "Auml"),
    (0xC5, "Aring"),
    (0xC6, "AElig"),
    (0xC7, "Ccedil"),
    (0xC8, "Egrave"),
    (0xC9, "Eacute"),
    (0xCA, "Ecirc"),
    (0xCB, "Euml"),
    (0xCC, "Igrave"),
    (0xCD, "Iacute"),
    (0xCE, "Icirc"),
    (0xCF, "Iuml"),
    (0xD0, "ETH"),
    (0xD1, "Ntilde"),
    (0xD2, "Ograve"),
    (0xD3, "Oacute"),
    (0xD4, "Ocirc"),
    (0xD5, "Otilde"),
    (0xD6, "Ouml"),
    (0xD7, "times"),
    (0xD8, "Oslash"),
    (0xD9, "Ugrave"),
    (0xDA, "Uacute"),
    (0xDB, "Ucirc"),
    (0xDC, "Uuml"),
    (0xDD, "Yacute"),
    (0xDE, "THORN"),
    (0xDF, "szlig"),
    (0xE0, "agrave"),
    (0xE1, "aacute"),
    (0xE2, "acirc"),
    (0xE3, "atilde"),
    (0xE4, "auml"),
    (0xE5, "aring"),
    (0xE6, "aelig"),
    (0xE7, "ccedil"),
    (0xE8, "egrave"),
    (0xE9, "eacute"),
    (0xEA, "ecirc"),
    (0xEB, "euml"),
    (0xEC, "igrave"),
    (0xED, "iacute"),
    (0xEE, "icirc"),
    (0xEF, "iuml"),
    (0xF0, "eth"),
    (0xF1, "ntilde"),
    (0xF2, "ograve"),
    (0xF3, "oacute"),
    (0xF4, "ocirc"),
    (0xF5, "otilde"),
    (0xF6, "ouml"),
    (0xF7, "divide"),
    (0xF8, "oslash"),
    (0xF9, "ugrave"),
    (0xFA, "uacute"),
    (0xFB, "ucirc"),
    (0xFC, "uuml"),
    (0xFD, "yacute"),
    (0xFE, "thorn"),
    (0xFF, "yuml"),
    (0x152, "OElig"),
    (0x153, "oelig"),
    (0x160, "Scaron"),
    (0x161, "scaron"),
    (0x178, "Yuml"),
    (0x192, "fnof"),
    (0x2C6, "circ"),
    (0x2DC, "tilde"),
    (0x391, "Alpha"),
    (0x392, "Beta"),
    (0x393, "Gamma"),
    (0x394, "Delta"),
    (0x395, "Epsilon"),
    (0x396, "Zeta"),
    (0x397, "Eta"),
    (0x398, "Theta"),
    (0x399, "Iota"),
    (0x39A, "Kappa"),
    (0x39B, "Lambda"),
    (0x39C, "Mu"),
    (0x39D, "Nu"),
    (0x39E, "Xi"),
    (0x39F, "Omicron"),
    (0x3A0, "Pi"),
    (0x3A1, "Rho"),
    (0x3A3, "Sigma"),
    (0x3A4, "Tau"),
    (0x3A5, "Upsilon"),
    (0x3A6, "Phi"),
    (0x3A7, "Chi"),
    (0x3A8, "Psi"),
    (0x3A9, "Omega"),
    (0x3B1, "alpha"),
    (0x3B2, "beta"),
    (0x3B3, "gamma"),
    (0x3B4, "delta"),
    (0x3B5, "epsilon"),
    (0x3B6, "zeta"),
    (0x3B7, "eta"),
    (0x3B8, "theta"),
    (0x3B9, "iota"),
    (0x3BA, "kappa"),
    (0x3BB, "lambda"),
    (0x3BC, "mu"),
    (0x3BD, "nu"),
    (0x3BE, "xi"),
    (0x3BF, "omicron"),
    (0x3C0, "pi"),
    (0x3C1, "rho"),
    (0x3C2, "sigmaf"),
    (0x3C3, "sigma"),
    (0x3C4, "tau"),
    (0x3C5, "upsilon"),
    (0x3C6, "phi"),
    (0x3C7, "chi"),
    (0x3C8, "psi"),
    (0x3C9, "omega"),
    (0x3D1, "thetasym"),
    (0x3D2, "upsih"),
    (0x3D6, "piv"),
    (0x2002, "ensp"),
    (0x2003, "emsp"),
    (0x2009, "thinsp"),
    (0x200C, "zwnj"),
    (0x200D, "zwj"),
    (0x200E, "lrm"),
    (0x200F, "rlm"),
    (0x2013, "ndash"),
    (0x2014, "mdash"),
    (0x2018, "lsquo"),
    (0x2019, "rsquo"),
    (0x201A, "sbquo"),
    (0x201C, "ldquo"),
    (0x201D, "rdquo"),
    (0x201E, "bdquo"),
    (0x2020, "dagger"),
    (0x2021, "Dagger"),
    (0x2022, "bull"),
    (0x2026, "hellip"),
    (0x2030, "permil"),
    (0x2032, "prime"),
    (0x2033, "Prime"),
    (0x2039, "lsaquo"),
    (0x203A, "rsaquo"),
    (0x203E, "oline"),
    (0x2044, "frasl"),
    (0x20AC, "euro"),
    (0x2111, "image"),
    (0x2118, "weierp"),
    (0x211C, "real"),
    (0x2122, "trade"),
    (0x2135, "alefsym"),
    (0x2190, "larr"),
    (0x2191, "uarr"),
    (0x2192, "rarr"),
    (0x2193, "darr"),
    (0x2194, "harr"),
    (0x21B5, "crarr"),
    (0x21D0, "lArr"),
    (0x21D1, "uArr"),
    (0x21D2, "rArr"),
    (0x21D3, "dArr"),
    (0x21D4, "hArr"),
    (0x2200, "forall"),
    (0x2202, "part"),
    (0x2203, "exist"),
    (0x2205, "empty"),
    (0x2207, "nabla"),
    (0x2208, "isin"),
    (0x2209, "notin"),
    (0x220B, "ni"),
    (0x220F, "prod"),
    (0x2211, "sum"),
    (0x2212, "minus"),
    (0x2217, "lowast"),
    (0x221A, "radic"),
    (0x221D, "prop"),
    (0x221E, "infin"),
    (0x2220, "ang"),
    (0x2227, "and"),
    (0x2228, "or"),
    (0x2229, "cap"),
    (0x222A, "cup"),
    (0x222B, "int"),
    (0x2234, "there4"),
    (0x223C, "sim"),
    (0x2245, "cong"),
    (0x2248, "asymp"),
    (0x2260, "ne"),
    (0x2261, "equiv"),
    (0x2264, "le"),
    (0x2265, "ge"),
    (0x2282, "sub"),
    (0x2283, "sup"),
    (0x2284, "nsub"),
    (0x2286, "sube"),
    (0x2287, "supe"),
    (0x2295, "oplus"),
    (0x2297, "otimes"),
    (0x22A5, "perp"),
    (0x22C5, "sdot"),
    (0x2308, "lceil"),
    (0x2309, "rceil"),
    (0x230A, "lfloor"),
    (0x230B, "rfloor"),
    (0x2329, "lang"),
    (0x232A, "rang"),
    (0x25CA, "loz"),
    (0x2660, "spades"),
    (0x2663, "clubs"),
    (0x2665, "hearts"),
    (0x2666, "diams"),
];

fn html_entity(cp: u32) -> Option<&'static str> {
    HTML_ENTITIES
        .binary_search_by_key(&cp, |e| e.0)
        .ok()
        .map(|i| HTML_ENTITIES[i].1)
}

/// Decode `s` from `from` into codepoints and re-encode as `to`.
/// Only the latin-1 family, ASCII, and UTF-8 are real encodings; every
/// other name is treated as UTF-8.
fn mb_recode(s: &[u8], to: &str, from: &str) -> Vec<u8> {
    let to = to.to_ascii_uppercase();
    let from = from.to_ascii_uppercase();
    let is_latin = |e: &str| matches!(e, "ISO-8859-1" | "ISO8859-1" | "LATIN1" | "WINDOWS-1252");
    let cps: Vec<u32> = if is_latin(&from) {
        s.iter().map(|b| *b as u32).collect()
    } else {
        String::from_utf8_lossy(s)
            .chars()
            .map(|c| c as u32)
            .collect()
    };
    let mut out: Vec<u8> = Vec::new();
    if is_latin(&to) {
        for cp in cps {
            out.push(if cp <= 0xFF { cp as u8 } else { b'?' });
        }
    } else if to == "ASCII" || to == "US-ASCII" {
        for cp in cps {
            out.push(if cp < 0x80 { cp as u8 } else { b'?' });
        }
    } else {
        for cp in cps {
            let c = char::from_u32(cp).unwrap_or('\u{FFFD}');
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    out
}

/// mb_convert_variables' per-var conversion: strings recode, arrays
/// recurse, everything else passes through.
fn mb_cv(v: &Value, to: &str, from: &str) -> Value {
    match v {
        Value::Str(s) => Value::bytes(mb_recode(s, to, from)),
        Value::Array(a) => {
            let mut out = PhpArray::new();
            for (k, c) in a.borrow().iter() {
                let nv = mb_cv(&c.borrow(), to, from);
                out.set(k.clone(), nv);
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        _ => v.clone(),
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

fn bfind_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
        .map(|p| p + from)
}

fn brfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).rposition(|w| w == needle)
}

fn brfind_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len())
        .rposition(|w| w.eq_ignore_ascii_case(needle))
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

fn breplace_ci(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while let Some(p) = bfind_ci(hay, from, i) {
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

/// Length of the UTF-8 sequence starting at `b[i]`, None if invalid.
fn utf8_char_len(b: &[u8], i: usize) -> Option<usize> {
    let c = *b.get(i)?;
    let len = match c {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    if i + len > b.len() {
        return None;
    }
    let seq = &b[i..i + len];
    if seq[1..].iter().all(|x| x & 0xC0 == 0x80) {
        // reject overlongs/surrogates roughly: decode check
        if std::str::from_utf8(seq).is_ok() {
            return Some(len);
        }
    }
    None
}

fn err<T>(cls: &'static str, msg: impl Into<String>) -> Result<T, PhpError> {
    Err(PhpError::uncaught(cls, msg, 0))
}

pub fn call(it: &mut Interp, name: &str, args: &[Cell]) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        // ----- output/debug -----
        "var_dump" => {
            for a in args {
                var_dump(it, &a.borrow(), 0, false, false);
            }
            Value::Null
        }
        "debug_zval_dump" => {
            for a in args {
                var_dump(it, &a.borrow(), 0, true, false);
            }
            Value::Null
        }
        "print_r" => {
            let v = arg(args, 0);
            let ret = arg(args, 1).is_truthy();
            let s = print_r(it, &v, 0);
            if ret {
                Value::str(s)
            } else {
                it.emit(&s);
                // print_r echoes a trailing newline only for arrays/objects.
                if matches!(v, Value::Array(_) | Value::Object(_)) {
                    it.emit("\n");
                }
                Value::Bool(true)
            }
        }
        "var_export" => {
            let v = arg(args, 0);
            let ret = arg(args, 1).is_truthy();
            let s = var_export(it, &v);
            if ret {
                Value::str(s)
            } else {
                it.emit(&s);
                Value::Null
            }
        }
        "printf" => {
            let s = sprintf(it, args)?;
            it.emit(&s);
            Value::Int(s.len() as i64)
        }
        "sprintf" => Value::str(sprintf(it, args)?),
        "vsprintf" => {
            let fmt = arg_str(it, args, 0);
            let list = match arg(args, 1) {
                Value::Array(a) => a
                    .borrow()
                    .entries
                    .iter()
                    .map(|(_, c)| c.clone())
                    .collect::<Vec<_>>(),
                _ => vec![],
            };
            Value::str(sprintf_args(it, &fmt, &list)?)
        }
        "vprintf" => {
            let fmt = arg_str(it, args, 0);
            let list = match arg(args, 1) {
                Value::Array(a) => a
                    .borrow()
                    .entries
                    .iter()
                    .map(|(_, c)| c.clone())
                    .collect::<Vec<_>>(),
                _ => vec![],
            };
            let s = sprintf_args(it, &fmt, &list)?;
            it.emit(&s);
            Value::Int(s.len() as i64)
        }
        "fprintf" => {
            // fprintf($fh, fmt, ...) — write to resource
            let fmt = arg_str(it, args, 1);
            let s = sprintf_args(it, &fmt, &args[2.min(args.len())..])?;
            write_resource(it, args.first(), &s)?;
            Value::Int(s.len() as i64)
        }
        "number_format" => {
            let n = arg(args, 0).to_float();
            let dec = arg(args, 1).to_int() as usize;
            let dp = arg_str(it, args, 2);
            let dp = if args.len() > 2 { dp.as_str() } else { "." };
            let ts = if args.len() > 3 {
                arg_str(it, args, 3)
            } else {
                ",".into()
            };
            let ts = if args.len() > 3 { ts.as_str() } else { "," };
            Value::str(number_format(n, dec, dp, ts))
        }

        // ----- strings -----
        "strlen" => Value::Int(arg(args, 0).to_php_bytes().len() as i64),
        "mb_strlen" => Value::Int(arg(args, 0).to_php_string().chars().count() as i64),
        // PHP strtoupper/strtolower are ASCII-only byte maps.
        "strtoupper" => Value::bytes({
            let mut s = arg_bs(it, args, 0);
            s.make_ascii_uppercase();
            s
        }),
        "strtolower" => Value::bytes({
            let mut s = arg_bs(it, args, 0);
            s.make_ascii_lowercase();
            s
        }),
        "mb_strtoupper" => Value::str(arg_str(it, args, 0).to_uppercase()),
        "mb_strtolower" => Value::str(arg_str(it, args, 0).to_lowercase()),
        "ucfirst" => {
            let mut s = arg_bs(it, args, 0);
            if let Some(f) = s.first_mut() {
                f.make_ascii_uppercase();
            }
            Value::bytes(s)
        }
        "lcfirst" => {
            let mut s = arg_bs(it, args, 0);
            if let Some(f) = s.first_mut() {
                f.make_ascii_lowercase();
            }
            Value::bytes(s)
        }
        "ucwords" => {
            // ucwords(string, separators = " \t\r\n\f\v") — cap after
            // any byte in the separator set (PHP 8 signature).
            let mut s = arg_bs(it, args, 0);
            let seps = args
                .get(1)
                .map(|c| arg_bs(it, std::slice::from_ref(c), 0))
                .unwrap_or_else(|| b" \t\r\n\x0B\x0C".to_vec());
            let mut cap = true;
            for ch in s.iter_mut() {
                if seps.contains(ch) {
                    cap = true;
                } else if cap && ch.is_ascii_alphabetic() {
                    ch.make_ascii_uppercase();
                    cap = false;
                } else if cap {
                    cap = false;
                }
            }
            Value::bytes(s)
        }
        "str_repeat" => {
            let s = arg_bs(it, args, 0);
            let n = arg(args, 1).to_int().max(0) as usize;
            Value::bytes(s.repeat(n))
        }
        "strrev" => Value::bytes({
            let mut s = arg_bs(it, args, 0);
            s.reverse();
            s
        }),
        "str_pad" => {
            let s = arg_bs(it, args, 0);
            let len = arg(args, 1).to_int() as usize;
            let pad = if args.len() > 2 {
                arg_bs(it, args, 2)
            } else {
                b" ".to_vec()
            };
            let ty = if args.len() > 3 {
                arg(args, 3).to_int()
            } else {
                1
            };
            Value::bytes(str_pad(&s, len, &pad, ty))
        }
        "str_split" => {
            let s = arg_bs(it, args, 0);
            let n = arg(args, 1).to_int().max(1) as usize;
            let mut a = PhpArray::new();
            for chunk in s.chunks(n) {
                a.push(Value::bytes(chunk.to_vec()));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "mb_str_split" => {
            let s = arg_str(it, args, 0);
            let n = arg(args, 1).to_int().max(1) as usize;
            let mut a = PhpArray::new();
            let chars: Vec<char> = s.chars().collect();
            for chunk in chars.chunks(n) {
                a.push(Value::str(chunk.iter().collect::<String>()));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "str_replace" => {
            let find = arg(args, 0);
            let repl = arg(args, 1);
            let subj = arg(args, 2);
            Value::bytes(str_replace(&find, &repl, &subj, false))
        }
        "str_ireplace" => {
            let find = arg(args, 0);
            let repl = arg(args, 1);
            let subj = arg(args, 2);
            Value::bytes(str_replace(&find, &repl, &subj, true))
        }
        "substr" => {
            let s = arg_bs(it, args, 0);
            let start = arg(args, 1).to_int();
            let len = if args.len() > 2 {
                Some(arg(args, 2).to_int())
            } else {
                None
            };
            match php_substr(&s, start, len) {
                Some(x) => Value::bytes(x),
                None => Value::Bool(false),
            }
        }
        "mb_substr" => {
            let s = arg_str(it, args, 0);
            let start = arg(args, 1).to_int();
            let len = if args.len() > 2 {
                Some(arg(args, 2).to_int())
            } else {
                None
            };
            match mb_substr(&s, start, len) {
                Some(x) => Value::str(x),
                None => Value::Bool(false),
            }
        }
        "mb_strpos" | "mb_stripos" | "mb_strrpos" | "mb_strripos" => {
            let ci = matches!(name, "mb_stripos" | "mb_strripos");
            let rev = matches!(name, "mb_strrpos" | "mb_strripos");
            let mut h = mb_chars(&arg_bs(it, args, 0));
            let mut n = mb_chars(&arg_bs(it, args, 1));
            if ci {
                h = mb_ci(&h);
                n = mb_ci(&n);
            }
            let off = arg(args, 2).to_int().max(0) as usize;
            let found = if rev {
                char_rfind(&h, &n, off)
            } else {
                char_find(&h, &n, off.min(h.len()))
            };
            match found {
                Some(i) => Value::Int(i as i64),
                None => Value::Bool(false),
            }
        }
        "mb_strstr" | "mb_stristr" | "mb_strrchr" | "mb_strrichr" => {
            let ci = matches!(name, "mb_stristr" | "mb_strrichr");
            let last = matches!(name, "mb_strrchr" | "mb_strrichr");
            let h = mb_chars(&arg_bs(it, args, 0));
            // strrchr uses only the first character of the needle.
            let mut n = mb_chars(&arg_bs(it, args, 1));
            if last {
                n.truncate(1);
            }
            let before = arg(args, 2).is_truthy();
            let (hh, nn) = if ci { (mb_ci(&h), mb_ci(&n)) } else { (h, n) };
            let found = if last {
                char_rfind(&hh, &nn, 0)
            } else {
                char_find(&hh, &nn, 0)
            };
            match found {
                Some(i) => Value::str(if before {
                    mb_join(&hh[..i])
                } else {
                    mb_join(&hh[i..])
                }),
                None => Value::Bool(false),
            }
        }
        "mb_substr_count" => {
            let h = mb_chars(&arg_bs(it, args, 0));
            let n = mb_chars(&arg_bs(it, args, 1));
            if n.is_empty() {
                return err(
                    "ValueError",
                    "mb_substr_count(): Argument #2 ($needle) cannot be empty",
                );
            }
            let mut count = 0i64;
            let mut i = 0usize;
            while i + n.len() <= h.len() {
                if h[i..i + n.len()] == n[..] {
                    count += 1;
                    i += n.len();
                } else {
                    i += 1;
                }
            }
            Value::Int(count)
        }
        "mb_ucfirst" | "mb_lcfirst" => {
            let mut c = mb_chars(&arg_bs(it, args, 0));
            if let Some(f) = c.first_mut() {
                let mapped: Vec<char> = if name == "mb_ucfirst" {
                    f.to_uppercase().collect()
                } else {
                    f.to_lowercase().collect()
                };
                c.splice(..1, mapped);
            }
            Value::str(mb_join(&c))
        }
        "mb_trim" | "mb_ltrim" | "mb_rtrim" => {
            let c = mb_chars(&arg_bs(it, args, 0));
            let set: Vec<char> = if args.len() > 1 {
                mb_chars(&arg_bs(it, args, 1))
            } else {
                vec![' ', '\t', '\n', '\r', '\0', '\x0B']
            };
            let (mut a, mut b) = (0usize, c.len());
            if name != "mb_rtrim" {
                while a < b && set.contains(&c[a]) {
                    a += 1;
                }
            }
            if name != "mb_ltrim" {
                while b > a && set.contains(&c[b - 1]) {
                    b -= 1;
                }
            }
            Value::str(mb_join(&c[a..b]))
        }
        "mb_str_pad" => {
            let c = mb_chars(&arg_bs(it, args, 0));
            let len = arg(args, 1).to_int().max(0) as usize;
            let pad = mb_chars(&arg_bs(it, args, 2));
            let typ = arg(args, 3).to_int();
            let pad = if pad.is_empty() { vec![' '] } else { pad };
            let mut out: Vec<char> = Vec::new();
            if c.len() < len {
                let need = len - c.len();
                let (l, r) = match typ {
                    0 => (need, 0),                   // STR_PAD_LEFT
                    2 => (need / 2, need - need / 2), // STR_PAD_BOTH
                    _ => (0, need),                   // STR_PAD_RIGHT
                };
                for i in 0..l {
                    out.push(pad[i % pad.len()]);
                }
                out.extend_from_slice(&c);
                for i in 0..r {
                    out.push(pad[i % pad.len()]);
                }
            } else {
                out = c;
            }
            Value::str(mb_join(&out))
        }
        "mb_strcut" => {
            // Byte offsets like substr, but the end snaps back to the
            // previous character boundary (never splits mid-char).
            let s = arg_bs(it, args, 0);
            let start = arg(args, 1).to_int();
            let len = if args.len() > 2 {
                Some(arg(args, 2).to_int())
            } else {
                None
            };
            match php_substr(&s, start, len) {
                Some(mut x) => {
                    while !x.is_empty() && !utf8_boundary(&s, start.max(0) as usize + x.len()) {
                        x.pop();
                    }
                    Value::bytes(x)
                }
                None => Value::Bool(false),
            }
        }
        "mb_scrub" => Value::str(arg_str(it, args, 0)),
        "mb_ord" => {
            let c = mb_chars(&arg_bs(it, args, 0));
            Value::Int(c.first().map(|c| *c as i64).unwrap_or(0))
        }
        "mb_chr" => {
            let cp = arg(args, 0).to_int();
            match char::from_u32(cp as u32) {
                Some(c) => Value::str(c.to_string()),
                None => Value::Bool(false),
            }
        }
        "mb_convert_case" => {
            let s = arg_str(it, args, 0);
            let mode = arg(args, 1).to_int();
            Value::str(match mode {
                0 => s.to_uppercase(), // MB_CASE_UPPER
                2 => {
                    // MB_CASE_TITLE: uppercase chars that follow whitespace.
                    let mut out = String::with_capacity(s.len());
                    let mut ws = true;
                    for ch in s.chars() {
                        if ch.is_whitespace() {
                            ws = true;
                            out.push(ch);
                        } else if ws {
                            ws = false;
                            out.extend(ch.to_uppercase());
                        } else {
                            out.push(ch);
                        }
                    }
                    out
                }
                _ => s.to_lowercase(), // 1 = LOWER, 3 = FOLD
            })
        }
        "mb_detect_encoding" => {
            if let Value::Array(_) = arg(args, 0) {
                return err(
                    "TypeError",
                    "mb_detect_encoding(): Argument #1 ($string) must be of type string, array given",
                );
            }
            let s = arg_bs(it, args, 0);
            let mut encs: Vec<String> = Vec::new();
            if let Value::Array(a) = arg(args, 1) {
                for (_, c) in a.borrow().iter() {
                    encs.push(c.borrow().to_php_string());
                }
            } else if args.len() > 1 {
                let v = arg(args, 1).to_php_string();
                if !v.is_empty() {
                    encs.push(v);
                }
            }
            if encs.is_empty() {
                encs.push("UTF-8".into());
            }
            let valid_utf8 = std::str::from_utf8(&s).is_ok();
            let ascii = s.iter().all(|b| *b < 0x80);
            let mut found = None;
            for e in encs {
                let e = e.to_ascii_uppercase();
                let ok = match e.as_str() {
                    "UTF-8" | "UTF8" => valid_utf8,
                    "ASCII" => ascii,
                    // Latin-1 accepts any byte sequence.
                    "ISO-8859-1" | "ISO8859-1" | "LATIN1" | "WINDOWS-1252" => true,
                    _ => false,
                };
                if ok {
                    found = Some(e);
                    break;
                }
            }
            match found {
                Some(e) => Value::str(e),
                None => Value::Bool(false),
            }
        }
        "mb_check_encoding" => {
            if let Value::Array(_) = arg(args, 0) {
                return err(
                    "TypeError",
                    "mb_check_encoding(): Argument #1 ($string) must be of type string, array given",
                );
            }
            let s = arg_bs(it, args, 0);
            let enc = arg(args, 1).to_php_string().to_ascii_uppercase();
            Value::Bool(match enc.as_str() {
                "" | "UTF-8" | "UTF8" => std::str::from_utf8(&s).is_ok(),
                "ASCII" => s.iter().all(|b| *b < 0x80),
                "ISO-8859-1" | "ISO8859-1" | "LATIN1" | "WINDOWS-1252" => true,
                _ => true,
            })
        }
        "mb_convert_encoding" => {
            if let Value::Array(_) = arg(args, 0) {
                return err(
                    "TypeError",
                    "mb_convert_encoding(): Argument #1 ($string) must be of type string, array given",
                );
            }
            let s = arg_bs(it, args, 0);
            let to = arg(args, 1).to_php_string();
            let from = if args.len() > 2 {
                arg(args, 2).to_php_string()
            } else {
                "UTF-8".to_string()
            };
            Value::bytes(mb_recode(&s, &to, &from))
        }
        "mb_convert_variables" => {
            // mb_convert_variables($to, $from, &$var, ...) — converts
            // each var in place (nested arrays too), returns the source
            // encoding it detected.
            let to = arg(args, 0).to_php_string();
            let from_v = arg(args, 1);
            let from = match &from_v {
                Value::Array(a) => a
                    .borrow()
                    .iter()
                    .map(|(_, c)| c.borrow().to_php_string())
                    .collect::<Vec<_>>()
                    .join(","),
                _ => from_v.to_php_string(),
            };
            for c in args.iter().skip(2) {
                let nv = mb_cv(&c.borrow(), &to, &from);
                *c.borrow_mut() = nv;
            }
            Value::str(from)
        }
        "mb_split" => {
            let pat = arg_bs(it, args, 0);
            let subj = arg_bs(it, args, 1);
            // mb_* regexes are delimiter-free oniguruma patterns — the
            // whole argument is the body; empty = match-everywhere.
            let re = match crate::pcre::compile(&pat, 0, 0).map(PhpRe::Pcre) {
                Ok(r) => r,
                Err(e) => {
                    it.warn_pub(&format!("mb_split(): {}", e))?;
                    return Ok(Some(Value::Bool(false)));
                }
            };
            let (caps, rc) = re.caps(&subj, it);
            if rc != 0 {
                it.last_preg_error = preg_rc_err(rc);
                return Ok(Some(Value::Bool(false)));
            }
            let mut a = PhpArray::new();
            let mut last = 0usize;
            for cap in &caps {
                if let Some(Some((x, y))) = cap.spans.first() {
                    a.push(Value::bytes(subj[last..*x].to_vec()));
                    last = *y;
                }
            }
            a.push(Value::bytes(subj[last..].to_vec()));
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "mb_internal_encoding" => Value::str("UTF-8"),
        "mb_regex_encoding" => Value::str("UTF-8"),
        "mb_http_output" => Value::str("pass"),
        "mb_http_input" => Value::str("pass"),
        "mb_language" => Value::str("neutral"),
        "mb_substitute_character" => Value::Int(63),
        "mb_list_encodings" => {
            let mut a = PhpArray::new();
            for e in [
                "7bit",
                "8bit",
                "ASCII",
                "ISO-8859-1",
                "UTF-8",
                "UTF-16",
                "UTF-32",
            ] {
                a.push(Value::str(e));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "mb_detect_order" => {
            let mut a = PhpArray::new();
            a.push(Value::str("ASCII"));
            a.push(Value::str("UTF-8"));
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "mb_encoding_aliases" => {
            let e = arg(args, 0).to_php_string().to_ascii_uppercase();
            let mut a = PhpArray::new();
            match e.as_str() {
                "UTF-8" => a.push(Value::str("utf8")),
                "ISO-8859-1" => {
                    a.push(Value::str("ISO8859-1"));
                    a.push(Value::str("latin1"));
                }
                _ => {}
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "substr_count" => {
            let s = arg_bs(it, args, 0);
            let n = arg_bs(it, args, 1);
            Value::Int(if n.is_empty() {
                0
            } else {
                let mut c = 0;
                let mut i = 0;
                while let Some(p) = bfind(&s, &n, i) {
                    c += 1;
                    i = p + n.len();
                }
                c
            } as i64)
        }
        "substr_replace" => {
            let s = arg_bs(it, args, 0);
            let r = arg_bs(it, args, 1);
            let start = arg(args, 2).to_int();
            let len = if args.len() > 3 {
                Some(arg(args, 3).to_int())
            } else {
                None
            };
            Value::bytes(substr_replace(&s, &r, start, len))
        }
        "strpos" | "stripos" => {
            let hay = arg_bs(it, args, 0);
            let needle = arg_bs(it, args, 1);
            let off = arg(args, 2).to_int().max(0) as usize;
            let found = if name == "stripos" {
                bfind_ci(&hay, &needle, off)
            } else {
                bfind(&hay, &needle, off)
            };
            match found {
                Some(p) => Value::Int(p as i64),
                None => Value::Bool(false),
            }
        }
        "strrpos" | "strripos" => {
            let hay = arg_bs(it, args, 0);
            let needle = arg_bs(it, args, 1);
            let found = if name == "strripos" {
                brfind_ci(&hay, &needle)
            } else {
                brfind(&hay, &needle)
            };
            match found {
                Some(p) => Value::Int(p as i64),
                None => Value::Bool(false),
            }
        }
        "strstr" | "strchr" | "stristr" => {
            let hay = arg_bs(it, args, 0);
            let needle = arg_bs(it, args, 1);
            let before = arg(args, 2).is_truthy();
            let found = if name == "stristr" {
                bfind_ci(&hay, &needle, 0)
            } else {
                bfind(&hay, &needle, 0)
            };
            match found {
                Some(p) => {
                    if before {
                        Value::bytes(hay[..p].to_vec())
                    } else {
                        Value::bytes(hay[p..].to_vec())
                    }
                }
                None => Value::Bool(false),
            }
        }
        "str_contains" => {
            let hay = arg_bs(it, args, 0);
            let needle = arg_bs(it, args, 1);
            Value::Bool(needle.is_empty() || bfind(&hay, &needle, 0).is_some())
        }
        "strcmp" | "strcasecmp" | "strncmp" | "strncasecmp" => {
            // Binary-safe comparison: PHP compares bytes and returns
            // the sign, not a specific magnitude.
            let a = arg_bs(it, args, 0);
            let b = arg_bs(it, args, 1);
            let (a, b) = if name.contains("case") {
                (
                    a.iter().map(|c| c.to_ascii_lowercase()).collect::<Vec<_>>(),
                    b.iter().map(|c| c.to_ascii_lowercase()).collect::<Vec<_>>(),
                )
            } else {
                (a, b)
            };
            let (a, b) = if name.contains('n') {
                let n = arg(args, 2).to_int().max(0) as usize;
                (a[..a.len().min(n)].to_vec(), b[..b.len().min(n)].to_vec())
            } else {
                (a, b)
            };
            Value::Int(match a.cmp(&b) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            })
        }
        "strspn" | "strcspn" => {
            // Byte-based: subject slice is (offset, length) of $str;
            // count the leading run of mask-present (strspn) or
            // mask-absent (strcspn) bytes.
            let s = arg_bs(it, args, 0);
            let mask = arg_bs(it, args, 1);
            let len = s.len() as i64;
            let off = arg(args, 2).to_int();
            let start = if off < 0 {
                (len + off).max(0)
            } else {
                off.min(len)
            } as usize;
            let mut end = len as usize;
            if let Some(l) = args.get(3) {
                let l = l.borrow().to_int();
                end = if l < 0 {
                    (len + l).max(start as i64) as usize
                } else {
                    (start + l as usize).min(end)
                };
            }
            let mut member = [false; 256];
            for &b in &mask {
                member[b as usize] = true;
            }
            let want = name == "strspn";
            let mut n = 0usize;
            for &b in &s[start..end.max(start)] {
                if member[b as usize] != want {
                    break;
                }
                n += 1;
            }
            Value::Int(n as i64)
        }
        "str_starts_with" => {
            let hay = arg_bs(it, args, 0);
            let needle = arg_bs(it, args, 1);
            Value::Bool(hay.starts_with(&needle))
        }
        "str_ends_with" => {
            let hay = arg_bs(it, args, 0);
            let needle = arg_bs(it, args, 1);
            Value::Bool(hay.ends_with(&needle))
        }
        "trim" => {
            let s = arg_bs(it, args, 0);
            let chars = if args.len() > 1 {
                arg_bs(it, args, 1)
            } else {
                b" \n\r\t\x0b\0".to_vec()
            };
            Value::bytes(trim_set(&s, &chars, true, true))
        }
        "ltrim" => {
            let s = arg_bs(it, args, 0);
            let chars = if args.len() > 1 {
                arg_bs(it, args, 1)
            } else {
                b" \n\r\t\x0b\0".to_vec()
            };
            Value::bytes(trim_set(&s, &chars, true, false))
        }
        "rtrim" | "chop" => {
            if matches!(arg(args, 0), Value::Null) {
                // Non-nullable internal param receiving null (bug43201).
                it.deprecated_pub(&format!(
                    "{}(): Passing null to parameter #1 ($string) of type string is deprecated",
                    name
                ))?;
            }
            let s = arg_bs(it, args, 0);
            let chars = if args.len() > 1 {
                arg_bs(it, args, 1)
            } else {
                b" \n\r\t\x0b\0".to_vec()
            };
            Value::bytes(trim_set(&s, &chars, false, true))
        }
        "explode" => {
            let sep = arg_bs(it, args, 0);
            let s = arg_bs(it, args, 1);
            let limit = arg(args, 2).to_int();
            let mut a = PhpArray::new();
            if sep.is_empty() {
                return err(
                    "ValueError",
                    "explode(): Argument #1 ($separator) cannot be empty",
                );
            }
            // split at each byte-level sep occurrence, honoring limit
            let mut parts: Vec<Vec<u8>> = Vec::new();
            let mut i = 0usize;
            loop {
                let stop = limit > 0 && parts.len() as i64 + 1 >= limit;
                match if stop { None } else { bfind(&s, &sep, i) } {
                    Some(p) => {
                        parts.push(s[i..p].to_vec());
                        i = p + sep.len();
                    }
                    None => {
                        parts.push(s[i..].to_vec());
                        break;
                    }
                }
            }
            if limit < 0 {
                // PHP: negative limit drops the last |limit| pieces
                let drop = (-limit) as usize;
                parts.truncate(parts.len().saturating_sub(drop));
            }
            for p in parts {
                a.push(Value::bytes(p));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "implode" | "join" => {
            let (sep, arr) = if args.len() == 1 {
                (Vec::new(), arg(args, 0))
            } else {
                (arg(args, 0).to_php_bytes(), arg(args, 1))
            };
            match arr {
                Value::Array(a) => {
                    let parts: Vec<Vec<u8>> = a
                        .borrow()
                        .entries
                        .iter()
                        .map(|(_, c)| c.borrow().to_php_bytes())
                        .collect();
                    let mut out = Vec::new();
                    for (i, p) in parts.iter().enumerate() {
                        if i > 0 {
                            out.extend_from_slice(&sep);
                        }
                        out.extend_from_slice(p);
                    }
                    Value::bytes(out)
                }
                _ => Value::str(""),
            }
        }
        "nl2br" => Value::bytes(breplace(&arg_bs(it, args, 0), b"\n", b"<br />\n")),
        "str_word_count" => {
            let s = arg_str(it, args, 0);
            Value::Int(s.split_whitespace().count() as i64)
        }
        "levenshtein" => {
            let a = arg_str(it, args, 0);
            let b = arg_str(it, args, 1);
            Value::Int(levenshtein(&a, &b) as i64)
        }
        "similar_text" => {
            let a = arg_str(it, args, 0);
            let b = arg_str(it, args, 1);
            let (n, _) = similar_text(&a, &b);
            Value::Int(n as i64)
        }
        "soundex" => Value::str(soundex(&arg_str(it, args, 0))),
        "quotemeta" => {
            let s = arg_bs(it, args, 0);
            let mut out = Vec::with_capacity(s.len());
            for &c in &s {
                if b".\\+*?[^]$()=!<>|:-#{}".contains(&c) {
                    out.push(b'\\');
                }
                out.push(c);
            }
            Value::bytes(out)
        }
        "addslashes" => {
            let s = arg_bs(it, args, 0);
            let mut out = Vec::with_capacity(s.len());
            for &c in &s {
                match c {
                    b'\'' | b'"' | b'\\' | 0 => {
                        out.push(b'\\');
                        out.push(if c == 0 { b'0' } else { c });
                    }
                    _ => out.push(c),
                }
            }
            Value::bytes(out)
        }
        "addcslashes" => {
            // Escapes chars in the charlist (with `a..z` ranges) as
            // C-escapes or \ooo octal (heredoc_nowdoc/bug79934).
            let s = arg_str(it, args, 0);
            let list = arg_str(it, args, 1);
            let lb = list.as_bytes();
            let mut set = [false; 256];
            let mut i = 0;
            while i < lb.len() {
                if i + 3 < lb.len() && lb[i + 1] == b'.' && lb[i + 2] == b'.' {
                    let (lo, hi) = (lb[i], lb[i + 3]);
                    if lo <= hi {
                        for c in lo..=hi {
                            set[c as usize] = true;
                        }
                    }
                    i += 4;
                } else {
                    set[lb[i] as usize] = true;
                    i += 1;
                }
            }
            let mut out = String::new();
            for c in s.chars() {
                let cp = c as u32;
                if cp < 256 && set[cp as usize] {
                    match c {
                        '\0' => out.push_str("\\0"),
                        '\x07' => out.push_str("\\a"),
                        '\x08' => out.push_str("\\b"),
                        '\t' => out.push_str("\\t"),
                        '\n' => out.push_str("\\n"),
                        '\x0b' => out.push_str("\\v"),
                        '\x0c' => out.push_str("\\f"),
                        '\r' => out.push_str("\\r"),
                        _ if (0x20..0x7f).contains(&cp) => out.push_str(&format!("\\{}", c)),
                        _ => out.push_str(&format!("\\{:03o}", cp)),
                    }
                } else {
                    out.push(c);
                }
            }
            Value::str(out)
        }
        "stripslashes" | "stripcslashes" => {
            let s = arg_bs(it, args, 0);
            let mut out = Vec::with_capacity(s.len());
            let mut i = 0;
            while i < s.len() {
                if s[i] == b'\\' {
                    i += 1;
                    match s.get(i) {
                        Some(b'0') => {
                            out.push(0);
                            i += 1;
                        }
                        Some(&c) => {
                            out.push(c);
                            i += 1;
                        }
                        None => out.push(b'\\'),
                    }
                } else {
                    out.push(s[i]);
                    i += 1;
                }
            }
            Value::bytes(out)
        }

        "htmlspecialchars" | "htmlentities" => {
            let s = arg_bs(it, args, 0);
            // `double_encode: false` skips existing entities (bug80096).
            let double = args.len() < 4 || arg(args, 3).is_truthy();
            let mut out: Vec<u8> = Vec::with_capacity(s.len());
            let mut i = 0;
            while i < s.len() {
                if s[i] == b'&' {
                    if !double {
                        if let Some(semi) = s[i + 1..]
                            .iter()
                            .position(|&b| b == b';')
                            .map(|o| i + 1 + o)
                        {
                            let ent = &s[i + 1..semi];
                            if ent.starts_with(b"#")
                                || ent.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'#')
                                    && !ent.is_empty()
                            {
                                out.extend_from_slice(&s[i..=semi]);
                                i = semi + 1;
                                continue;
                            }
                        }
                    }
                    out.extend_from_slice(b"&amp;");
                } else {
                    match s[i] {
                        b'<' => out.extend_from_slice(b"&lt;"),
                        b'>' => out.extend_from_slice(b"&gt;"),
                        b'"' => out.extend_from_slice(b"&quot;"),
                        b'\'' => out.extend_from_slice(b"&#039;"),
                        _ => {
                            // Valid UTF-8 seq copies whole; invalid →
                            // U+FFFD per byte (PHP ENT_SUBSTITUTE default).
                            match utf8_char_len(&s, i) {
                                Some(l) => {
                                    // `htmlentities` maps every non-ASCII
                                    // codepoint to a named entity (or a
                                    // numeric ref); `htmlspecialchars`
                                    // passes it through verbatim.
                                    if name == "htmlentities" && l > 1 {
                                        let cp = std::str::from_utf8(&s[i..i + l])
                                            .ok()
                                            .and_then(|c| c.chars().next())
                                            .map(|c| c as u32)
                                            .unwrap_or(0xFFFD);
                                        match html_entity(cp) {
                                            Some(e) => {
                                                out.extend_from_slice(b"&");
                                                out.extend_from_slice(e.as_bytes());
                                                out.extend_from_slice(b";");
                                            }
                                            None => out
                                                .extend_from_slice(format!("&#{};", cp).as_bytes()),
                                        }
                                    } else {
                                        out.extend_from_slice(&s[i..i + l]);
                                    }
                                    i += l;
                                    continue;
                                }
                                None => out.extend_from_slice(b"\xef\xbf\xbd"),
                            }
                        }
                    }
                }
                i += 1;
            }
            Value::bytes(out)
        }
        "htmlspecialchars_decode" | "html_entity_decode" => {
            let s = arg_bs(it, args, 0);
            Value::bytes(breplace(
                &breplace(
                    &breplace(
                        &breplace(
                            &breplace(&breplace(&s, b"&lt;", b"<"), b"&gt;", b">"),
                            b"&quot;",
                            b"\"",
                        ),
                        b"&#039;",
                        b"'",
                    ),
                    b"&apos;",
                    b"'",
                ),
                b"&amp;",
                b"&",
            ))
        }
        "strip_tags" => {
            let s = arg_bs(it, args, 0);
            let mut out = Vec::with_capacity(s.len());
            let mut in_tag = false;
            for &c in &s {
                match c {
                    b'<' => in_tag = true,
                    b'>' => in_tag = false,
                    _ if !in_tag => out.push(c),
                    _ => {}
                }
            }
            Value::bytes(out)
        }
        "ord" => Value::Int(arg(args, 0).to_php_bytes().first().copied().unwrap_or(0) as i64),
        "chr" => Value::bytes(vec![(arg(args, 0).to_int() & 0xff) as u8]),
        "bin2hex" => Value::str(hex_encode(&arg_bs(it, args, 0))),
        "hex2bin" => {
            let s = arg_str(it, args, 0);
            match hex_decode(&s) {
                Some(b) => Value::bytes(b),
                None => Value::Bool(false),
            }
        }
        "str_rot13" => Value::str(rot13(&arg_str(it, args, 0))),
        "count_chars" => {
            let s = arg_bs(it, args, 0);
            let mode = arg(args, 1).to_int();
            match mode {
                0 => {
                    let mut a = PhpArray::new();
                    let mut counts = [0i64; 256];
                    for &b in &s {
                        counts[b as usize] += 1;
                    }
                    for (i, c) in counts.iter().enumerate() {
                        a.set(ArrKey::Int(i as i64), Value::Int(*c));
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
                _ => {
                    let mut counts = [0i64; 256];
                    for &b in &s {
                        counts[b as usize] += 1;
                    }
                    let mut a = PhpArray::new();
                    for (i, c) in counts.iter().enumerate() {
                        if *c > 0 {
                            a.set(ArrKey::Int(i as i64), Value::Int(*c));
                        }
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
            }
        }
        "chunk_split" => {
            let s = arg_bs(it, args, 0);
            let len = if args.len() > 1 {
                arg(args, 1).to_int() as usize
            } else {
                76
            };
            let end = if args.len() > 2 {
                arg_bs(it, args, 2)
            } else {
                b"\r\n".to_vec()
            };
            let mut out = Vec::new();
            for c in s.chunks(len.max(1)) {
                out.extend_from_slice(c);
                out.extend_from_slice(&end);
            }
            Value::bytes(out)
        }
        "strtr" => {
            let s = arg_bs(it, args, 0);
            match arg(args, 1) {
                Value::Array(m) => {
                    // longest keys first
                    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = m
                        .borrow()
                        .entries
                        .iter()
                        .map(|(k, c)| {
                            (
                                match k {
                                    ArrKey::Int(i) => i.to_string().into_bytes(),
                                    ArrKey::Str(st) => st.as_bytes().to_vec(),
                                    ArrKey::Tomb => Vec::new(),
                                },
                                c.borrow().to_php_bytes(),
                            )
                        })
                        .collect();
                    pairs.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
                    Value::bytes(strtr_map(&s, &pairs))
                }
                from => {
                    let to = arg_bs(it, args, 2);
                    Value::bytes(strtr_chars(&s, &from.to_php_bytes(), &to))
                }
            }
        }
        "wordwrap" => Value::str(arg_str(it, args, 0)), // minimal passthrough
        "sprintf_js" | "vsprintf_js" => Value::Null,

        // ----- arrays -----
        "count" | "sizeof" => match arg(args, 0) {
            Value::Array(a) => Value::Int(a.borrow().len() as i64),
            Value::Object(o) if it.obj_is_a(&o, "Countable") => {
                it.method_invoke(o.clone(), "count", crate::interp::CallArgs::empty())?
            }
            Value::Null => Value::Int(0),
            v => {
                let _ = v;
                Value::Int(1)
            }
        },
        "array_keys" => match arg(args, 0) {
            Value::Array(a) => {
                let search = args.get(1).map(|c| c.borrow().clone());
                let strict = arg(args, 2).is_truthy();
                let mut out = PhpArray::new();
                for (k, c) in a.borrow().iter() {
                    if let Some(sv) = &search {
                        let ev = c.borrow();
                        let hit = if strict {
                            crate::value::identical(&ev, sv)
                        } else {
                            crate::value::compare(&ev, sv) == std::cmp::Ordering::Equal
                        };
                        if !hit {
                            continue;
                        }
                    }
                    out.push(match k {
                        ArrKey::Int(i) => Value::Int(*i),
                        ArrKey::Str(s) => Value::str(s.to_string()),
                        ArrKey::Tomb => continue,
                    });
                }
                Value::Array(Rc::new(RefCell::new(out)))
            }
            _ => Value::Null,
        },
        "array_values" => match arg(args, 0) {
            Value::Array(a) => {
                let mut out = PhpArray::new();
                for (_, c) in a.borrow().iter() {
                    out.push(c.borrow().clone());
                }
                Value::Array(Rc::new(RefCell::new(out)))
            }
            _ => Value::Null,
        },
        "array_key_exists" | "key_exists" => {
            let k = to_key(&arg(args, 0));
            match arg(args, 1) {
                Value::Array(a) => Value::Bool(a.borrow().get_cell(&k).is_some()),
                _ => Value::Bool(false),
            }
        }
        "in_array" => {
            let needle = arg(args, 0);
            let strict = arg(args, 2).is_truthy();
            match arg(args, 1) {
                Value::Array(a) => Value::Bool(a.borrow().iter().any(|(_, c)| {
                    let v = c.borrow();
                    if strict {
                        crate::value::identical(&v, &needle)
                    } else {
                        compare(&v, &needle) == std::cmp::Ordering::Equal
                    }
                })),
                _ => Value::Bool(false),
            }
        }
        "array_search" => {
            let needle = arg(args, 0);
            let strict = arg(args, 2).is_truthy();
            match arg(args, 1) {
                Value::Array(a) => {
                    for (k, c) in a.borrow().iter() {
                        let v = c.borrow().clone();
                        let hit = if strict {
                            crate::value::identical(&v, &needle)
                        } else {
                            compare(&v, &needle) == std::cmp::Ordering::Equal
                        };
                        if hit {
                            return Ok(Some(match k {
                                ArrKey::Int(i) => Value::Int(*i),
                                ArrKey::Str(s) => Value::str(s.to_string()),
                                ArrKey::Tomb => continue,
                            }));
                        }
                    }
                    Value::Bool(false)
                }
                _ => Value::Bool(false),
            }
        }
        "filter_var" => {
            const REQUIRE_ARRAY: i64 = 16777216;
            const REQUIRE_SCALAR: i64 = 33554432;
            const FORCE_ARRAY: i64 = 67108864;
            const NULL_ON_FAILURE: i64 = 134217728;
            let value = arg(args, 0);
            let filter = arg(args, 1).to_int();
            let mut flags = 0i64;
            let mut inner_opts = Value::Null;
            match arg(args, 2) {
                Value::Int(f) => flags = f,
                Value::Array(a) => {
                    let b = a.borrow();
                    if let Some(c) = b.get_cell(&to_key(&Value::str("flags"))) {
                        flags = c.borrow().to_int();
                    }
                    if let Some(c) = b.get_cell(&to_key(&Value::str("options"))) {
                        inner_opts = c.borrow().clone();
                    }
                }
                _ => {}
            }
            let fail = if flags & NULL_ON_FAILURE != 0 {
                Value::Null
            } else {
                Value::Bool(false)
            };
            let one = |it: &mut Interp, v: &Value| -> Result<Value, PhpError> {
                filter_var_one(it, v, filter, &inner_opts, flags)
            };
            if flags & REQUIRE_ARRAY != 0 {
                match &value {
                    Value::Array(a) => {
                        let mut out = PhpArray::new();
                        for (_, c) in a.borrow().iter() {
                            let v = c.borrow().clone();
                            out.push(one(it, &v)?);
                        }
                        Value::Array(Rc::new(RefCell::new(out)))
                    }
                    _ => fail,
                }
            } else if matches!(value, Value::Array(_) | Value::Object(_))
                && flags & REQUIRE_SCALAR != 0
            {
                fail
            } else if matches!(value, Value::Array(_)) {
                // Arrays only validate under REQUIRE_ARRAY.
                fail
            } else {
                let r = one(it, &value)?;
                if flags & FORCE_ARRAY != 0 {
                    let mut out = PhpArray::new();
                    out.push(r);
                    Value::Array(Rc::new(RefCell::new(out)))
                } else {
                    r
                }
            }
        }
        "array_merge" | "array_merge_recursive" => {
            let mut out = PhpArray::new();
            for a in args {
                if let Value::Array(m) = &*a.borrow() {
                    for (k, c) in m.borrow().iter() {
                        match k {
                            ArrKey::Int(_) => out.push(c.borrow().clone()),
                            _ => out.set(k.clone(), c.borrow().clone()),
                        }
                    }
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_replace" => {
            let mut out = PhpArray::new();
            for a in args {
                if let Value::Array(m) = &*a.borrow() {
                    for (k, c) in m.borrow().iter() {
                        out.set(k.clone(), c.borrow().clone());
                    }
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_combine" => {
            let keys = arg(args, 0);
            let vals = arg(args, 1);
            let mut out = PhpArray::new();
            if let (Value::Array(k), Value::Array(v)) = (keys, vals) {
                let kb = k.borrow();
                let vb = v.borrow();
                for (i, (kk, _)) in kb.iter().enumerate() {
                    let vv = vb
                        .entries
                        .get(i)
                        .map(|(_, c)| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    out.set(kk.clone(), vv);
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_fill" => {
            let start = arg(args, 0).to_int();
            let n = arg(args, 1).to_int();
            let v = arg(args, 2);
            let mut out = PhpArray::new();
            for i in 0..n.max(0) {
                out.set(ArrKey::Int(start + i), v.clone());
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_fill_keys" => {
            let mut out = PhpArray::new();
            let v = arg(args, 1);
            if let Value::Array(keys) = arg(args, 0) {
                for (_, c) in keys.borrow().iter() {
                    out.set(to_key(&c.borrow()), v.clone());
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_slice" => {
            let preserve = arg(args, 3).is_truthy();
            match arg(args, 0) {
                Value::Array(a) => {
                    let b = a.borrow();
                    let n = b.entries.len() as i64;
                    let off = arg(args, 1).to_int();
                    let off = if off < 0 {
                        (n + off).max(0)
                    } else {
                        off.min(n)
                    };
                    let len = if args.len() > 2 && !matches!(arg(args, 2), Value::Null) {
                        let l = arg(args, 2).to_int();
                        if l < 0 {
                            (n - off + l).max(0)
                        } else {
                            l.min(n - off)
                        }
                    } else {
                        n - off
                    };
                    let mut out = PhpArray::new();
                    for i in off..off + len {
                        let (k, c) = &b.entries[i as usize];
                        if preserve || matches!(k, ArrKey::Str(_)) {
                            out.set(k.clone(), c.borrow().clone());
                        } else {
                            out.push(c.borrow().clone());
                        }
                    }
                    Value::Array(Rc::new(RefCell::new(out)))
                }
                _ => Value::Null,
            }
        }
        "array_splice" => {
            // array_splice(&$a, $off, $len, $repl)
            let mut removed = PhpArray::new();
            if let Some(rc) = it.arr_mut(&args[0]) {
                let mut arr = rc.borrow_mut();
                let n = arr.entries.len() as i64;
                let off = arg(args, 1).to_int();
                let off = if off < 0 {
                    (n + off).max(0)
                } else {
                    off.min(n)
                };
                let len = if args.len() > 2 {
                    let l = arg(args, 2).to_int();
                    if l < 0 {
                        (n - off + l).max(0)
                    } else {
                        l.min(n - off)
                    }
                } else {
                    n - off
                };
                let tail: Vec<(ArrKey, Cell)> = std::mem::take(&mut arr.entries);
                let (head, rest) = tail.split_at(off as usize);
                let (cut, tail2) = rest.split_at((len as usize).min(rest.len()));
                for (k, c) in cut {
                    removed.push(c.borrow().clone());
                    let _ = k;
                }
                arr.entries = head.to_vec();
                if let Some(repl) = args.get(3) {
                    if let Value::Array(r) = &*repl.borrow() {
                        for (_, c) in r.borrow().iter() {
                            arr.push(c.borrow().clone());
                        }
                    }
                }
                for (k, c) in tail2 {
                    let _ = k;
                    arr.push(c.borrow().clone());
                }
            }
            Value::Array(Rc::new(RefCell::new(removed)))
        }
        "array_push" => {
            if let Some(rc) = it.arr_mut(&args[0]) {
                let mut arr = rc.borrow_mut();
                for a in &args[1..] {
                    arr.push(a.borrow().clone());
                }
                let n = arr.len() as i64;
                return Ok(Some(Value::Int(n)));
            }
            Value::Null
        }
        "array_pop" => {
            if let Some(rc) = it.arr_mut(&args[0]) {
                let mut arr = rc.borrow_mut();
                // Tombstone the last live bucket — a live foreach anchored
                // on it still finds it and ends instead of restarting
                // (foreachLoop.009/.013).
                let last = arr
                    .entries
                    .iter()
                    .rposition(|(k, _)| !matches!(k, ArrKey::Tomb));
                match last {
                    Some(f) => {
                        let c = arr.entries[f].1.clone();
                        arr.entries[f].0 = ArrKey::Tomb;
                        return Ok(Some(c.borrow().clone()));
                    }
                    None => return Ok(Some(Value::Null)),
                }
            }
            Value::Null
        }
        "array_shift" => {
            if let Some(rc) = it.arr_mut(&args[0]) {
                let mut arr = rc.borrow_mut();
                // Shift the first LIVE element: tombstone its bucket (a live
                // foreach keeps positions — foreachLoop.013) and renumber
                // integer keys over the remaining live elements.
                let first = arr
                    .entries
                    .iter()
                    .position(|(k, _)| !matches!(k, ArrKey::Tomb));
                match first {
                    Some(f) => {
                        let c = arr.entries[f].1.clone();
                        arr.entries[f].0 = ArrKey::Tomb;
                        let mut ni = 0i64;
                        for (k, _) in arr.entries.iter_mut() {
                            if let ArrKey::Int(i) = k {
                                *i = ni;
                                ni += 1;
                            }
                        }
                        arr.next = ni;
                        return Ok(Some(c.borrow().clone()));
                    }
                    None => return Ok(Some(Value::Null)),
                }
            }
            Value::Null
        }
        "array_unshift" => {
            if let Some(rc) = it.arr_mut(&args[0]) {
                let mut arr = rc.borrow_mut();
                // Renumber existing int keys up by arg count.
                let add = args.len() - 1;
                for (k, _) in arr.entries.iter_mut() {
                    if let ArrKey::Int(i) = k {
                        *i += add as i64;
                    }
                }
                arr.next += add as i64;
                let mut new_entries: Vec<(ArrKey, Cell)> = Vec::new();
                for (i, a) in args[1..].iter().enumerate() {
                    new_entries.push((ArrKey::Int(i as i64), cell(a.borrow().clone())));
                }
                new_entries.append(&mut arr.entries);
                arr.entries = new_entries;
                return Ok(Some(Value::Int(arr.len() as i64)));
            }
            Value::Null
        }
        "array_reverse" => match arg(args, 0) {
            Value::Array(a) => {
                let preserve = arg(args, 1).is_truthy();
                let mut out = PhpArray::new();
                for (k, c) in a.borrow().iter().rev() {
                    if preserve || matches!(k, ArrKey::Str(_)) {
                        out.set(k.clone(), c.borrow().clone());
                    } else {
                        out.push(c.borrow().clone());
                    }
                }
                Value::Array(Rc::new(RefCell::new(out)))
            }
            _ => Value::Null,
        },
        "array_unique" => match arg(args, 0) {
            Value::Array(a) => {
                let mut seen: Vec<String> = Vec::new();
                let mut out = PhpArray::new();
                for (k, c) in a.borrow().iter() {
                    let v = c.borrow().to_php_string();
                    if !seen.contains(&v) {
                        seen.push(v);
                        out.set(k.clone(), c.borrow().clone());
                    }
                }
                Value::Array(Rc::new(RefCell::new(out)))
            }
            _ => Value::Null,
        },
        "array_flip" => match arg(args, 0) {
            Value::Array(a) => {
                let mut out = PhpArray::new();
                for (k, c) in a.borrow().iter() {
                    let v = c.borrow().clone();
                    out.set(
                        to_key(&v),
                        match k {
                            ArrKey::Int(i) => Value::Int(*i),
                            ArrKey::Str(s) => Value::str(s.to_string()),
                            ArrKey::Tomb => continue,
                        },
                    );
                }
                Value::Array(Rc::new(RefCell::new(out)))
            }
            _ => Value::Null,
        },
        "array_sum" => match arg(args, 0) {
            Value::Array(a) => {
                let mut is_f = false;
                let mut i: i64 = 0;
                let mut f: f64 = 0.0;
                for (_, c) in a.borrow().iter() {
                    match &*c.borrow() {
                        Value::Int(x) => i += x,
                        v => {
                            is_f = true;
                            f += v.to_float();
                        }
                    }
                }
                if is_f {
                    Value::Float(f + i as f64)
                } else {
                    Value::Int(i)
                }
            }
            _ => Value::Int(0),
        },
        "array_product" => match arg(args, 0) {
            Value::Array(a) => {
                let mut is_f = false;
                let mut i: i64 = 1;
                let mut f: f64 = 1.0;
                for (_, c) in a.borrow().iter() {
                    match &*c.borrow() {
                        Value::Int(x) => i *= x,
                        v => {
                            is_f = true;
                            f *= v.to_float();
                        }
                    }
                }
                if is_f {
                    Value::Float(f * i as f64)
                } else {
                    Value::Int(i)
                }
            }
            _ => Value::Int(1),
        },
        "array_count_values" => match arg(args, 0) {
            Value::Array(a) => {
                let mut out = PhpArray::new();
                for (_, c) in a.borrow().iter() {
                    let k = to_key(&c.borrow());
                    let cur = out.get(&k).unwrap_or(Value::Int(0)).to_int();
                    out.set(k, Value::Int(cur + 1));
                }
                Value::Array(Rc::new(RefCell::new(out)))
            }
            _ => Value::Null,
        },
        "array_diff" => {
            let mut out = PhpArray::new();
            if let Value::Array(a) = arg(args, 0) {
                'outer: for (k, c) in a.borrow().iter() {
                    let v = c.borrow().clone();
                    for other in &args[1..] {
                        if let Value::Array(o) = &*other.borrow() {
                            for (_, oc) in o.borrow().iter() {
                                if compare(&v, &oc.borrow()) == std::cmp::Ordering::Equal {
                                    continue 'outer;
                                }
                            }
                        }
                    }
                    out.set(k.clone(), v);
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_diff_key" => {
            let mut out = PhpArray::new();
            if let Value::Array(a) = arg(args, 0) {
                'outer: for (k, c) in a.borrow().iter() {
                    for other in &args[1..] {
                        if let Value::Array(o) = &*other.borrow() {
                            if o.borrow().get_cell(k).is_some() {
                                continue 'outer;
                            }
                        }
                    }
                    out.set(k.clone(), c.borrow().clone());
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_intersect" => {
            let mut out = PhpArray::new();
            if let Value::Array(a) = arg(args, 0) {
                'outer: for (k, c) in a.borrow().iter() {
                    let v = c.borrow().clone();
                    for other in &args[1..] {
                        if let Value::Array(o) = &*other.borrow() {
                            let mut found = false;
                            for (_, oc) in o.borrow().iter() {
                                if compare(&v, &oc.borrow()) == std::cmp::Ordering::Equal {
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                continue 'outer;
                            }
                        }
                    }
                    out.set(k.clone(), v);
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_intersect_key" => {
            let mut out = PhpArray::new();
            if let Value::Array(a) = arg(args, 0) {
                'outer: for (k, c) in a.borrow().iter() {
                    for other in &args[1..] {
                        if let Value::Array(o) = &*other.borrow() {
                            if o.borrow().get_cell(k).is_none() {
                                continue 'outer;
                            }
                        }
                    }
                    out.set(k.clone(), c.borrow().clone());
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_filter" => {
            let mut out = PhpArray::new();
            let cb = args.get(1).map(|c| c.borrow().clone());
            if let Value::Array(a) = arg(args, 0) {
                for (k, c) in a.borrow().iter() {
                    let v = c.borrow().clone();
                    let keep = match &cb {
                        Some(cb) => {
                            let r = it.call_value(
                                cb,
                                crate::interp::CallArgs::positional(vec![cell(v.clone())]),
                            )?;
                            r.is_truthy()
                        }
                        None => v.is_truthy(),
                    };
                    if keep {
                        out.set(k.clone(), v);
                    }
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_map" => {
            let cb = arg(args, 0);
            let mut out = PhpArray::new();
            if args.len() == 2 {
                if let Value::Array(a) = arg(args, 1) {
                    for (_, c) in a.borrow().iter() {
                        let v = it.call_value(
                            &cb,
                            crate::interp::CallArgs::positional(vec![cell(c.borrow().clone())]),
                        )?;
                        out.push(v);
                    }
                }
            } else {
                // multiple arrays → zip
                let mut arrays = Vec::new();
                for a in &args[1..] {
                    if let Value::Array(arr) = &*a.borrow() {
                        arrays.push(
                            arr.borrow()
                                .entries
                                .iter()
                                .map(|(_, c)| c.borrow().clone())
                                .collect::<Vec<_>>(),
                        );
                    }
                }
                let n = arrays.iter().map(|a| a.len()).max().unwrap_or(0);
                for i in 0..n {
                    let call_args: Vec<Cell> = arrays
                        .iter()
                        .map(|a| cell(a.get(i).cloned().unwrap_or(Value::Null)))
                        .collect();
                    let v = it.call_value(&cb, crate::interp::CallArgs::positional(call_args))?;
                    out.push(v);
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_map_assoc" | "array_map_key" => Value::Null,
        "array_reduce" => {
            let cb = arg(args, 1);
            let mut acc = arg(args, 2);
            if let Value::Array(a) = arg(args, 0) {
                for (_, c) in a.borrow().iter() {
                    acc = it.call_value(
                        &cb,
                        crate::interp::CallArgs::positional(vec![
                            cell(acc),
                            cell(c.borrow().clone()),
                        ]),
                    )?;
                }
            }
            acc
        }
        "array_walk" => {
            let cb = arg(args, 1);
            let extra = arg(args, 2);
            // array_walk on an object iterates its property entries
            // (gh18268: hooked props yield their serialized value).
            let obj = match &*args[0].borrow() {
                Value::Object(o) => Some(o.clone()),
                _ => None,
            };
            if let Some(o) = obj {
                let mut walked = false;
                for (n, slot, decl) in it.object_serial_entries(&o) {
                    walked = true;
                    let v = match &decl {
                        Some((p, dcls)) => it
                            .serial_entry_value(&o, p, dcls, &slot)
                            .unwrap_or(Value::Null),
                        None => o
                            .borrow()
                            .props
                            .get(&slot)
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Null),
                    };
                    let plain = n
                        .trim_start_matches('\0')
                        .split('\0')
                        .next_back()
                        .unwrap_or(&n)
                        .to_string();
                    it.call_value(
                        &cb,
                        crate::interp::CallArgs::positional(vec![
                            cell(v),
                            cell(Value::str(plain)),
                            cell(extra.clone()),
                        ]),
                    )?;
                }
                if walked {
                    return Ok(Some(Value::Bool(true)));
                }
            }
            if let Some(rc) = it.arr_mut(&args[0]) {
                let cells: Vec<(ArrKey, Cell)> = rc.borrow().iter().cloned().collect();
                for (k, c) in cells {
                    it.call_value(
                        &cb,
                        crate::interp::CallArgs::positional(vec![
                            c.clone(),
                            cell(match k {
                                ArrKey::Int(i) => Value::Int(i),
                                ArrKey::Str(s) => Value::str(s.to_string()),
                                ArrKey::Tomb => Value::Null,
                            }),
                            cell(extra.clone()),
                        ]),
                    )?;
                }
            }
            Value::Bool(true)
        }
        "array_column" => {
            let mut out = PhpArray::new();
            if let Value::Array(a) = arg(args, 0) {
                let col = arg(args, 1);
                let colkey = to_key(&col);
                let idx = arg(args, 2);
                let has_idx = !matches!(idx, Value::Null);
                for (_, c) in a.borrow().iter() {
                    if let Value::Array(row) = &*c.borrow() {
                        let row = row.borrow();
                        if let Some(v) = row.get(&colkey) {
                            if has_idx {
                                let k = row.get(&to_key(&idx)).unwrap_or(Value::Null);
                                out.set(to_key(&k), v);
                            } else {
                                out.push(v);
                            }
                        }
                    }
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_pad" => {
            let n = arg(args, 1).to_int();
            let v = arg(args, 2);
            let mut out = PhpArray::new();
            if let Value::Array(a) = arg(args, 0) {
                for (k, c) in a.borrow().iter() {
                    out.set(k.clone(), c.borrow().clone());
                }
                while (out.len() as i64) < n.abs() {
                    if n > 0 {
                        out.push(v.clone());
                    } else {
                        out.entries.insert(0, (ArrKey::Int(0), cell(v.clone())));
                        let mut i = 0;
                        for (k, _) in out.entries.iter_mut() {
                            if let ArrKey::Int(x) = k {
                                *x = i;
                                i += 1;
                            }
                        }
                    }
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_is_list" => match arg(args, 0) {
            Value::Array(a) => {
                let b = a.borrow();
                Value::Bool(
                    b.entries
                        .iter()
                        .enumerate()
                        .all(|(i, (k, _))| matches!(k, ArrKey::Int(x) if *x == i as i64)),
                )
            }
            _ => Value::Bool(false),
        },
        "array_first" | "array_key_first" => match arg(args, 0) {
            Value::Array(a) => a
                .borrow()
                .iter()
                .next()
                .map(|(k, _)| match k {
                    ArrKey::Int(i) => Value::Int(*i),
                    ArrKey::Str(s) => Value::str(s.to_string()),
                    ArrKey::Tomb => Value::Null,
                })
                .unwrap_or(Value::Null),
            _ => Value::Null,
        },
        "array_key_last" => match arg(args, 0) {
            Value::Array(a) => a
                .borrow()
                .iter()
                .next_back()
                .map(|(k, _)| match k {
                    ArrKey::Int(i) => Value::Int(*i),
                    ArrKey::Str(s) => Value::str(s.to_string()),
                    ArrKey::Tomb => Value::Null,
                })
                .unwrap_or(Value::Null),
            _ => Value::Null,
        },
        "array_rand" => match arg(args, 0) {
            Value::Array(a) => {
                let b = a.borrow();
                let first = b
                    .iter()
                    .next()
                    .map(|(k, _)| match k {
                        ArrKey::Int(i) => Value::Int(*i),
                        ArrKey::Str(s) => Value::str(s.to_string()),
                        ArrKey::Tomb => Value::Null,
                    })
                    .unwrap_or(Value::Null);
                first
            }
            _ => Value::Null,
        },
        "range" => {
            let lo = arg(args, 0);
            let hi = arg(args, 1);
            let step = arg(args, 2).to_float();
            let step = if step == 0.0 { 1.0 } else { step.abs() };
            let mut out = PhpArray::new();
            match (&lo, &hi) {
                (Value::Str(a), Value::Str(b))
                    if a.len() == 1 && b.len() == 1 && !a[0].is_ascii_digit() =>
                {
                    let (mut c, end) = (a[0] as i64, b[0] as i64);
                    if c <= end {
                        while c <= end {
                            out.push(Value::str((c as u8 as char).to_string()));
                            c += step as i64;
                        }
                    } else {
                        while c >= end {
                            out.push(Value::str((c as u8 as char).to_string()));
                            c -= step as i64;
                        }
                    }
                }
                _ => {
                    let (mut x, y) = (lo.to_float(), hi.to_float());
                    if x <= y {
                        while x <= y {
                            out.push(num_val(x, lo.clone(), step));
                            x += step;
                        }
                    } else {
                        while x >= y {
                            out.push(num_val(x, lo.clone(), step));
                            x -= step;
                        }
                    }
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "sort" | "rsort" | "asort" | "arsort" | "ksort" | "krsort" | "usort" | "uasort"
        | "uksort" | "natsort" | "natcasesort" | "shuffle" => {
            if let Some(rc) = it.arr_mut(&args[0]) {
                let mut arr = rc.borrow_mut();
                sort_array(it, &mut arr, name, args.get(1))?;
            }
            Value::Bool(true)
        }
        "array_multisort" => Value::Bool(true), // TODO
        "compact" => {
            let mut out = PhpArray::new();
            for a in args {
                if let Value::Str(s) = &*a.borrow() {
                    let v = it
                        .lookup_var(&crate::value::lossy(&s))
                        .unwrap_or(Value::Null);
                    out.set(ArrKey::Str(crate::value::lossy(&s).into_owned().into()), v);
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "extract" => {
            if let Value::Array(a) = arg(args, 0) {
                for (k, c) in a.borrow().iter() {
                    if let ArrKey::Str(s) = k {
                        if is_varname(s) {
                            it.var_name_set(s, c.borrow().clone());
                        }
                    }
                }
            }
            Value::Int(0)
        }
        "current" | "pos" => match arg(args, 0) {
            Value::Array(a) => a
                .borrow()
                .iter()
                .next()
                .map(|(_, c)| c.borrow().clone())
                .unwrap_or(Value::Bool(false)),
            _ => Value::Bool(false),
        },
        "end" => match arg(args, 0) {
            Value::Array(a) => a
                .borrow()
                .iter()
                .next_back()
                .map(|(_, c)| c.borrow().clone())
                .unwrap_or(Value::Bool(false)),
            _ => Value::Bool(false),
        },
        "reset" => match arg(args, 0) {
            Value::Array(a) => a
                .borrow()
                .iter()
                .next()
                .map(|(_, c)| c.borrow().clone())
                .unwrap_or(Value::Bool(false)),
            _ => Value::Bool(false),
        },
        "key" => match arg(args, 0) {
            Value::Array(a) => a
                .borrow()
                .iter()
                .next()
                .map(|(k, _)| match k {
                    ArrKey::Int(i) => Value::Int(*i),
                    ArrKey::Str(s) => Value::str(s.to_string()),
                    ArrKey::Tomb => Value::Null,
                })
                .unwrap_or(Value::Null),
            _ => Value::Null,
        },
        "next" | "prev" => Value::Bool(false), // no internal pointer yet
        "each" => Value::Bool(false),

        // ----- math -----
        "abs" => match arg(args, 0) {
            Value::Int(i) => Value::Int(i.saturating_abs()),
            v => Value::Float(v.to_float().abs()),
        },
        "intdiv" => {
            let (va, vb) = (arg(args, 0), arg(args, 1));
            let a = it.coerce_int_pub(&va);
            let b = it.coerce_int_pub(&vb);
            if b == 0 {
                return err("DivisionByZeroError", "Division by zero");
            }
            match a.checked_div(b) {
                Some(q) => Value::Int(q),
                None => {
                    return err(
                        "ArithmeticError",
                        "Division of PHP_INT_MIN by -1 is not an integer",
                    )
                }
            }
        }
        "divmod" => Value::Null,
        "max" | "min" => {
            let mut vals: Vec<Value> = Vec::new();
            for a in args {
                let v = a.borrow().clone();
                if let Value::Array(arr) = &v {
                    for (_, c) in arr.borrow().iter() {
                        vals.push(c.borrow().clone());
                    }
                } else {
                    vals.push(v);
                }
            }
            let mut best = vals.first().cloned().unwrap_or(Value::Null);
            for v in &vals[1.min(vals.len())..] {
                let ord = compare(v, &best);
                if (name == "max" && ord == std::cmp::Ordering::Greater)
                    || (name == "min" && ord == std::cmp::Ordering::Less)
                {
                    best = v.clone();
                }
            }
            best
        }
        "round" => {
            let v = arg(args, 0).to_float();
            let p = arg(args, 1).to_int();
            let m = 10f64.powi(p as i32);
            let _mode = arg(args, 2).to_int();
            let r = v * m;
            let rounded = if r >= 0.0 {
                (r + 0.5).floor()
            } else {
                (r - 0.5).ceil()
            };
            Value::Float(rounded / m)
        }
        "floor" => Value::Float(arg(args, 0).to_float().floor()),
        "ceil" => Value::Float(arg(args, 0).to_float().ceil()),
        "fmod" => Value::Float(arg(args, 0).to_float() % arg(args, 1).to_float()),
        "fdiv" => {
            let d = arg(args, 1).to_float();
            if d == 0.0 {
                let n = arg(args, 0).to_float();
                Value::Float(if n > 0.0 {
                    f64::INFINITY
                } else if n < 0.0 {
                    f64::NEG_INFINITY
                } else {
                    f64::NAN
                })
            } else {
                Value::Float(arg(args, 0).to_float() / d)
            }
        }
        "pow" => {
            let b = arg(args, 0);
            let e = arg(args, 1);
            match (&b, &e) {
                (Value::Int(bi), Value::Int(ei)) if *ei >= 0 => match bi.checked_pow(*ei as u32) {
                    Some(v) => Value::Int(v),
                    None => Value::Float(b.to_float().powf(e.to_float())),
                },
                _ => Value::Float(b.to_float().powf(e.to_float())),
            }
        }
        "sqrt" => Value::Float(arg(args, 0).to_float().sqrt()),
        "exp" => Value::Float(arg(args, 0).to_float().exp()),
        "log" | "ln" => {
            let v = arg(args, 0).to_float();
            if args.len() > 1 {
                Value::Float(v.ln() / arg(args, 1).to_float().ln())
            } else {
                Value::Float(v.ln())
            }
        }
        "log10" => Value::Float(arg(args, 0).to_float().log10()),
        "log2" => Value::Float(arg(args, 0).to_float().log2()),
        "sin" => Value::Float(arg(args, 0).to_float().sin()),
        "cos" => Value::Float(arg(args, 0).to_float().cos()),
        "tan" => Value::Float(arg(args, 0).to_float().tan()),
        "asin" => Value::Float(arg(args, 0).to_float().asin()),
        "acos" => Value::Float(arg(args, 0).to_float().acos()),
        "atan" => Value::Float(arg(args, 0).to_float().atan()),
        "atan2" => Value::Float(arg(args, 0).to_float().atan2(arg(args, 1).to_float())),
        "sinh" => Value::Float(arg(args, 0).to_float().sinh()),
        "cosh" => Value::Float(arg(args, 0).to_float().cosh()),
        "tanh" => Value::Float(arg(args, 0).to_float().tanh()),
        "pi" => Value::Float(std::f64::consts::PI),
        "deg2rad" => Value::Float(arg(args, 0).to_float() * std::f64::consts::PI / 180.0),
        "rad2deg" => Value::Float(arg(args, 0).to_float() * 180.0 / std::f64::consts::PI),
        "hypot" => Value::Float(arg(args, 0).to_float().hypot(arg(args, 1).to_float())),
        "rand" | "mt_rand" | "random_int" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let lo = arg(args, 0).to_int();
            let hi = arg(args, 1).to_int();
            let seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
                .unwrap_or(0x9e3779b9);
            let r = xorshift(seed);
            Value::Int(if hi >= lo {
                lo + (r % ((hi - lo + 1) as u64).max(1)) as i64
            } else {
                lo
            })
        }
        "mt_srand" | "srand" => Value::Null,
        "mt_getrandmax" | "getrandmax" => Value::Int(2147483647),
        "lcg_value" => Value::Float(0.5),
        "decbin" => Value::str(format!("{:b}", arg(args, 0).to_int())),
        "dechex" => Value::str(format!("{:x}", arg(args, 0).to_int())),
        "decoct" => Value::str(format!("{:o}", arg(args, 0).to_int())),
        "bindec" => Value::Int(i64::from_str_radix(&arg_str(it, args, 0), 2).unwrap_or(0)),
        "hexdec" => {
            let s = arg_str(it, args, 0);
            match i64::from_str_radix(s.trim(), 16) {
                Ok(v) => Value::Int(v),
                Err(_) => match u64::from_str_radix(s.trim(), 16) {
                    Ok(v) => Value::Float(v as f64),
                    Err(_) => Value::Int(0),
                },
            }
        }
        "octdec" => Value::Int(i64::from_str_radix(&arg_str(it, args, 0), 8).unwrap_or(0)),
        "base_convert" => {
            let s = arg_str(it, args, 0);
            let from = arg(args, 1).to_int() as u32;
            let to = arg(args, 2).to_int() as u32;
            match i128::from_str_radix(&s.to_lowercase(), from) {
                Ok(v) => Value::str(base_conv(v, to)),
                Err(_) => Value::Bool(false),
            }
        }

        // ----- type introspection -----
        "gettype" => Value::str(arg(args, 0).gettype()),
        "get_debug_type" => Value::str(
            match arg(args, 0) {
                Value::Null => "null",
                Value::Bool(_) => "bool",
                Value::Int(_) => "int",
                Value::Float(_) => "float",
                Value::Str(_) => "string",
                Value::Array(_) => "array",
                Value::Object(o) => {
                    return Ok(Some(Value::str(o.borrow().class.name().to_string())))
                }
                Value::Callable(_) => "Closure",
                Value::Resource(_) => "resource",
            }
            .to_string(),
        ),
        "settype" => {
            let t = arg_str(it, args, 1);
            if let Some(c) = args.first() {
                let nv = cast_to(&c.borrow(), &t);
                *c.borrow_mut() = nv;
            }
            Value::Bool(true)
        }
        "intval" | "ip2long" => Value::Int(arg(args, 0).to_int()),
        "floatval" | "doubleval" => Value::Float(arg(args, 0).to_float()),
        "strval" => Value::str(it.to_string_of(&arg(args, 0))),
        "boolval" => Value::Bool(arg(args, 0).is_truthy()),
        "is_int" | "is_integer" | "is_long" => Value::Bool(matches!(arg(args, 0), Value::Int(_))),
        "is_float" | "is_double" | "is_real" => {
            Value::Bool(matches!(arg(args, 0), Value::Float(_)))
        }
        "is_string" => Value::Bool(matches!(arg(args, 0), Value::Str(_))),
        "is_bool" => Value::Bool(matches!(arg(args, 0), Value::Bool(_))),
        "is_null" => Value::Bool(matches!(arg(args, 0), Value::Null)),
        "is_array" => Value::Bool(matches!(arg(args, 0), Value::Array(_))),
        "is_object" => Value::Bool(matches!(
            arg(args, 0),
            Value::Object(_) | Value::Callable(_)
        )),
        "is_numeric" => match arg(args, 0) {
            Value::Int(_) | Value::Float(_) => Value::Bool(true),
            Value::Str(s) => Value::Bool(!matches!(numeric(&s), Numeric::NonNumeric)),
            _ => Value::Bool(false),
        },
        "is_scalar" => Value::Bool(matches!(
            arg(args, 0),
            Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(_)
        )),
        "is_callable" => Value::Bool(match arg(args, 0) {
            Value::Callable(_) => true,
            Value::Str(s) => {
                it.functions
                    .contains_key(&crate::value::lossy(&s).to_lowercase())
                    || is_builtin(&crate::value::lossy(&s).to_lowercase())
            }
            Value::Array(a) => {
                let e: Vec<Value> = a
                    .borrow()
                    .entries
                    .iter()
                    .map(|x| x.1.borrow().clone())
                    .collect();
                if e.len() != 2 {
                    false
                } else {
                    let mname = match &e[1] {
                        Value::Str(s) => crate::value::lossy(s).to_string(),
                        _ => String::new(),
                    };
                    it.is_callable_arr(&e[0], &mname)
                }
            }
            _ => false,
        }),
        "is_iterable" => Value::Bool(matches!(arg(args, 0), Value::Array(_))),
        "is_countable" => Value::Bool(matches!(arg(args, 0), Value::Array(_))),
        "is_resource" => Value::Bool(matches!(arg(args, 0), Value::Resource(_))),
        "is_nan" => Value::Bool(matches!(arg(args, 0), Value::Float(f) if f.is_nan())),
        "is_finite" => Value::Bool(matches!(arg(args, 0), Value::Float(f) if f.is_finite())),
        "is_infinite" => Value::Bool(matches!(arg(args, 0), Value::Float(f) if f.is_infinite())),

        // ----- constants / functions -----
        "define" => {
            let n = arg_str(it, args, 0);
            if n.contains("::") {
                return err(
                    "ValueError",
                    "define(): Argument #1 ($constant_name) cannot be a class constant",
                );
            }
            let v = arg(args, 1);
            it.define_const(&n, v);
            Value::Bool(true)
        }
        "defined" => {
            let n = arg_str(it, args, 0);
            Value::Bool(it.const_defined(&n))
        }
        "constant" => {
            let n = arg_str(it, args, 0);
            if let Some((cls, cn)) = n.split_once("::") {
                return it.class_const_named(cls, cn).map(Some);
            }
            match it.const_get(&n) {
                Some(v) => v,
                None => return err("Error", format!("Undefined constant {}", n)),
            }
        }
        "class_alias" => {
            let name = arg_str(it, args, 0);
            let alias = arg_str(it, args, 1);
            Value::Bool(it.class_alias(&name, &alias)?)
        }
        "function_exists" => {
            let n = arg_str(it, args, 0).trim_start_matches('\\').to_lowercase();
            Value::Bool(
                it.functions.contains_key(&n) || is_builtin(&n) || builtin_params(&n).is_some(),
            )
        }
        "class_exists" => {
            let n = arg_str(it, args, 0);
            Value::Bool(it.lookup_class(&n).is_some())
        }
        "interface_exists" => {
            let n = arg_str(it, args, 0);
            Value::Bool(
                it.interfaces
                    .contains_key(&n.trim_start_matches('\\').to_lowercase()),
            )
        }
        "trait_exists" => {
            let n = arg_str(it, args, 0);
            Value::Bool(
                it.traits
                    .contains_key(&n.trim_start_matches('\\').to_lowercase()),
            )
        }
        "enum_exists" => Value::Bool(false),
        "method_exists" => match arg(args, 0) {
            Value::Object(o) => {
                let m = arg_str(it, args, 1).to_lowercase();
                Value::Bool(o.borrow().class.find_method(&m).is_some())
            }
            Value::Str(cn) => match it.lookup_class(&crate::value::lossy(&cn)) {
                Some(c) => Value::Bool(
                    c.find_method(&arg_str(it, args, 1).to_lowercase())
                        .is_some(),
                ),
                None => Value::Bool(false),
            },
            _ => Value::Bool(false),
        },
        "property_exists" => match arg(args, 0) {
            Value::Object(o) => {
                let n = arg_str(it, args, 1);
                Value::Bool(
                    o.borrow().props.contains_key(&n) || it.class_has_prop(&o.borrow().class, &n),
                )
            }
            Value::Str(cn) => {
                let n = arg_str(it, args, 1);
                match it.lookup_class(&crate::value::lossy(&cn)) {
                    Some(c) => Value::Bool(it.class_has_prop(&c, &n)),
                    None => Value::Bool(false),
                }
            }
            _ => Value::Bool(false),
        },
        "get_class" => match arg(args, 0) {
            Value::Object(o) => Value::str(o.borrow().class.name().to_string()),
            _ => Value::Bool(false),
        },
        "get_parent_class" => match arg(args, 0) {
            Value::Object(o) => match &o.borrow().class.decl.parent {
                Some(p) => Value::str(p.clone()),
                None => Value::Bool(false),
            },
            Value::Str(cn) => match it.lookup_class(&crate::value::lossy(&cn)) {
                Some(c) => match &c.decl.parent {
                    Some(p) => Value::str(p.clone()),
                    None => Value::Bool(false),
                },
                None => Value::Bool(false),
            },
            _ => Value::Bool(false),
        },
        "get_object_vars" | "get_mangled_object_vars" => match arg(args, 0) {
            Value::Object(o) => {
                let mut a = PhpArray::new();
                if name == "get_mangled_object_vars" {
                    // Raw slots with mangled keys — no hooks
                    // (property_hooks/dump).
                    let ob = o.borrow();
                    for n in &ob.prop_order {
                        if let Some(c) = ob.props.get(n) {
                            a.set(ArrKey::Str(n.clone().into()), c.borrow().clone());
                        }
                    }
                } else {
                    // Scope-visible decl entries; hooked props run `get`,
                    // write-only and uninitialized props are skipped.
                    let scope = it.caller_scope_name();
                    let entries = it.object_serial_entries(&o);
                    for (out, slot, decl) in entries {
                        let ok = match &decl {
                            None => true, // dynamic props are public
                            Some((p, dcls)) => match p.visibility {
                                crate::ast::Visibility::Public => true,
                                crate::ast::Visibility::Protected => {
                                    let oc = o.borrow().class.name().to_string();
                                    scope.as_ref().is_some_and(|sc| {
                                        it.obj_is_a_str(sc, &oc) || it.obj_is_a_str(&oc, sc)
                                    })
                                }
                                crate::ast::Visibility::Private => {
                                    scope.as_ref() == Some(&dcls.name().to_string())
                                }
                            },
                        };
                        if !ok {
                            continue;
                        }
                        let v = match &decl {
                            Some((p, dcls)) => it.serial_entry_value(&o, p, dcls, &slot),
                            None => o.borrow().props.get(&slot).map(|c| c.borrow().clone()),
                        };
                        if let Some(v) = v {
                            a.set(ArrKey::Str(out.into()), v);
                        }
                    }
                }
                Value::Array(Rc::new(RefCell::new(a)))
            }
            _ => Value::Null,
        },
        "get_class_methods" => match arg(args, 0) {
            Value::Object(o) => {
                let mut a = PhpArray::new();
                for m in &o.borrow().class.decl.methods {
                    a.push(Value::str(m.decl.name.clone()));
                }
                Value::Array(Rc::new(RefCell::new(a)))
            }
            Value::Str(cn) => match it.lookup_class(&crate::value::lossy(&cn)) {
                Some(c) => {
                    let mut a = PhpArray::new();
                    for m in &c.decl.methods {
                        a.push(Value::str(m.decl.name.clone()));
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
                None => Value::Bool(false),
            },
            _ => Value::Null,
        },
        "get_class_vars" => {
            let cls = match arg(args, 0) {
                Value::Object(o) => Some(o.borrow().class.clone()),
                Value::Str(cn) => it.lookup_class(&crate::value::lossy(&cn)),
                _ => None,
            };
            match cls {
                Some(c) => {
                    let mut a = PhpArray::new();
                    for (n, v) in it.class_default_props(&c) {
                        a.set(ArrKey::Str(n.into()), v);
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
                None => Value::Bool(false),
            }
        }
        "get_declared_classes" => {
            let mut a = PhpArray::new();
            for n in it.declared_names(crate::ast::ClassKind::Class) {
                a.push(Value::str(n));
            }
            for n in it.declared_names(crate::ast::ClassKind::Enum) {
                a.push(Value::str(n));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "get_declared_interfaces" => {
            let mut a = PhpArray::new();
            for n in it.declared_names(crate::ast::ClassKind::Interface) {
                a.push(Value::str(n));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "get_declared_traits" => {
            let mut a = PhpArray::new();
            for n in it.declared_names(crate::ast::ClassKind::Trait) {
                a.push(Value::str(n));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "debug_print_backtrace" => {
            it.emit(&it.format_backtrace());
            Value::Null
        }
        "debug_backtrace" => {
            let mut arr = PhpArray::new();
            for fr in it.backtrace() {
                let mut f = PhpArray::new();
                if fr.file != "[internal function]" {
                    f.set(ArrKey::Str("file".into()), Value::str(fr.file.clone()));
                    f.set(ArrKey::Str("line".into()), Value::Int(fr.line as i64));
                }
                f.set(
                    ArrKey::Str("function".into()),
                    Value::str(fr.function.clone()),
                );
                if let Some(c) = &fr.class {
                    f.set(ArrKey::Str("class".into()), Value::str(c.clone()));
                    f.set(ArrKey::Str("type".into()), Value::str(fr.ty.clone()));
                }
                let mut a = PhpArray::new();
                for av in &fr.args {
                    a.push(av.borrow().clone());
                }
                // Variadic-collected named args keep their string keys
                // (named_params/backtrace: `x`/`y` after the positionals).
                for (n, av) in &fr.named_args {
                    a.set(ArrKey::Str(n.clone().into()), av.borrow().clone());
                }
                f.set(
                    ArrKey::Str("args".into()),
                    Value::Array(Rc::new(RefCell::new(a))),
                );
                arr.push(Value::Array(Rc::new(RefCell::new(f))));
            }
            Value::Array(Rc::new(RefCell::new(arr)))
        }
        "is_a" => match arg(args, 0) {
            Value::Object(o) => {
                let n = arg_str(it, args, 1);
                Value::Bool(it.obj_is_a(&o, &n))
            }
            _ => Value::Bool(false),
        },
        "is_subclass_of" => match arg(args, 0) {
            Value::Object(o) => {
                let n = arg_str(it, args, 1);
                let cls = o.borrow().class.clone();
                Value::Bool(
                    cls.decl
                        .parent
                        .as_ref()
                        .map(|p| it.obj_is_a_str(p, &n))
                        .unwrap_or(false),
                )
            }
            _ => Value::Bool(false),
        },
        "class_implements" | "class_uses" | "class_parents" => {
            Value::Array(Rc::new(RefCell::new(PhpArray::new())))
        }
        "spl_object_id" | "spl_object_hash" => match arg(args, 0) {
            Value::Object(o) => {
                if name == "spl_object_id" {
                    Value::Int(o.borrow().id as i64)
                } else {
                    Value::str(format!("{:032x}", o.borrow().id))
                }
            }
            _ => Value::Null,
        },
        "func_get_args" | "func_num_args" | "func_get_arg" => {
            if !it.in_call() {
                let msg = match name {
                    "func_num_args" => "func_num_args() must be called from a function context",
                    _ => &format!("{}() cannot be called from the global scope", name),
                };
                return Err(PhpError::uncaught("Error", msg, it.cur_line));
            }
            let fa = it.frame_args();
            match name {
                "func_num_args" => Value::Int(fa.len() as i64),
                "func_get_arg" => {
                    let i = arg(args, 0).to_int();
                    if i < 0 {
                        return Err(PhpError::uncaught(
                            "Error",
                            "func_get_arg(): Argument #1 ($position) must be greater than or equal to 0",
                            it.cur_line,
                        ));
                    }
                    match fa.get(i as usize) {
                        Some(c) => c.borrow().clone(),
                        None => {
                            return Err(PhpError::uncaught(
                                "Error",
                                "func_get_arg(): Argument #1 ($position) must be less than the number of the arguments passed to the currently executed function",
                                it.cur_line,
                            ));
                        }
                    }
                }
                _ => {
                    let mut a = PhpArray::new();
                    for c in fa {
                        a.push(c.borrow().clone());
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
            }
        }
        "call_user_func"
        | "call_user_func_array"
        | "forward_static_call"
        | "forward_static_call_array" => {
            let cb = arg(args, 0);
            // `*_array` unpacks the args array: string keys become named
            // args (a later int key is the positional-after-named Error).
            if name.ends_with("_array") {
                let mut ca = crate::interp::CallArgs::empty();
                let mut seen_str = false;
                if let Value::Array(a) = arg(args, 1) {
                    for (k, c) in a.borrow().iter() {
                        match k {
                            ArrKey::Str(s) => {
                                seen_str = true;
                                ca.named.push((s.to_string(), c.clone(), true, false));
                            }
                            _ if seen_str => {
                                return err::<Option<Value>>(
                                    "Error",
                                    "Cannot use positional argument after named argument",
                                );
                            }
                            _ => ca.cells.push(c.clone()),
                        }
                    }
                }
                it.call_value(&cb, ca)?
            } else {
                it.call_value(
                    &cb,
                    crate::interp::CallArgs::positional(args[1.min(args.len())..].to_vec()),
                )?
            }
        }
        "register_shutdown_function" => {
            let f = arg(args, 0);
            let rest: Vec<Cell> = args[1.min(args.len())..].to_vec();
            it.register_shutdown(f, rest);
            Value::Null
        }
        "set_error_handler" => {
            let prev = it.error_handler().unwrap_or(Value::Null);
            it.set_error_handler(if matches!(arg(args, 0), Value::Null) {
                None
            } else {
                Some(arg(args, 0))
            });
            prev
        }
        "set_exception_handler" => {
            let prev = it.exception_handler().unwrap_or(Value::Null);
            it.set_exception_handler(if matches!(arg(args, 0), Value::Null) {
                None
            } else {
                Some(arg(args, 0))
            });
            prev
        }
        "restore_error_handler" | "restore_exception_handler" => Value::Bool(true),
        "trigger_error" | "user_error" => {
            let msg = arg_str(it, args, 0);
            // E_USER_WARNING=512 / E_USER_NOTICE=1024 / E_USER_DEPRECATED=
            // 16384 select the diagnostic label + errno seen by the handler
            // (error_2_exception_001, bug21094). Default is E_USER_NOTICE.
            let level = args.get(1).map(|c| c.borrow().to_int()).unwrap_or(1024);
            it.emit_diag_pub(level, &msg)?;
            Value::Bool(true)
        }
        "error_reporting" => {
            let lv = args.first().map(|c| c.borrow().to_int());
            Value::Int(it.error_reporting(lv))
        }
        "ini_set" => {
            // Stores into the INI table and returns the previous
            // value (false when unset) — memory_limit, html_errors,
            // docref_* etc. all read back through ini_get (bug45392).
            let k = arg_str(it, args, 0);
            let prev = it.ini.get(&k).cloned();
            let v = arg_str(it, args, 1);
            // Shrinking the limit under current usage refuses with a
            // warning and leaves the old value (bug45392).
            if k == "memory_limit" {
                it.ini.insert(k.clone(), v);
                let lim = it.ini_bytes(&k);
                if lim > 0 && (it.mem_used as i64) > lim {
                    let _ = it
                        .ini
                        .insert(k.clone(), prev.clone().unwrap_or_else(|| "-1".into()));
                    it.warn_pub(&format!(
                        "Failed to set memory limit to {} bytes (Current memory usage is {} bytes)",
                        lim, it.mem_used
                    ))?;
                    return Ok(Some(match prev {
                        Some(p) => Value::str(p),
                        None => Value::Bool(false),
                    }));
                }
            } else {
                it.ini.insert(k, v);
            }
            match prev {
                Some(p) => Value::str(p),
                None => Value::Bool(false),
            }
        }
        "ini_get" => {
            let k = arg_str(it, args, 0);
            match it.ini.get(&k) {
                Some(v) => Value::str(v.clone()),
                None => Value::Bool(false),
            }
        }
        "ini_get_all" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "ini_restore" => Value::Null,
        "ini_parse_quantity" => Value::Int(arg(args, 0).to_int()),
        "error_get_last" | "error_clear_last" => Value::Null,
        "set_time_limit" => {
            // Restarts the seconds counter (045).
            it.set_deadline(arg(args, 0).to_int());
            Value::Bool(true)
        }
        "ignore_user_abort" => Value::Int(0),
        "register_tick_function" | "unregister_tick_function" => Value::Bool(true),

        // ----- output buffering -----
        "ob_start" => {
            let h = args.first().map(|c| c.borrow().clone());
            it.ob_push(h);
            Value::Bool(true)
        }
        "ob_end_clean" => {
            it.ob_end_clean()?;
            Value::Bool(true)
        }
        "ob_end_flush" => {
            it.ob_end_flush()?;
            Value::Bool(true)
        }
        "ob_get_clean" => it.ob_get_clean(),
        "ob_get_flush" => it.ob_get_flush()?,
        "ob_get_contents" => match it.ob_top() {
            Some(b) => Value::bytes(b.clone()),
            None => Value::Bool(false),
        },
        "ob_get_length" => match it.ob_top() {
            Some(b) => Value::Int(b.len() as i64),
            None => Value::Bool(false),
        },
        "ob_get_level" => Value::Int(it.ob_len() as i64),
        "ob_clean" => {
            it.ob_clean()?;
            Value::Bool(true)
        }
        "ob_flush" | "flush" => {
            it.ob_flush()?;
            Value::Null
        }
        "ob_implicit_flush" | "ob_list_handlers" => Value::Null,
        "ob_get_status" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "output_reset_rewrite_vars" => Value::Bool(true),

        // ----- serialization -----
        "serialize" => Value::str(serialize(it, &arg(args, 0))),
        "unserialize" => {
            let s = arg_str(it, args, 0);
            let mut pos = 0;
            match unserialize(it, &s, &mut pos) {
                Ok(v) => v,
                Err(_) => Value::Bool(false),
            }
        }
        "json_encode" => {
            let v = arg(args, 0);
            let flags = arg(args, 1).to_int();
            match json_encode(it, &v, flags) {
                Ok(s) => {
                    it.last_json_error = 0;
                    Value::str(s)
                }
                Err(_) => {
                    it.last_json_error = 8;
                    Value::Bool(false)
                }
            }
        }
        "json_decode" => {
            let s = arg_str(it, args, 0);
            let mut assoc = arg(args, 1).is_truthy();
            let flags = arg(args, 3).to_int();
            if flags & 1 != 0 {
                assoc = true; // JSON_OBJECT_AS_ARRAY
            }
            match json_decode(it, &s, assoc) {
                Ok(v) => {
                    it.last_json_error = 0;
                    v
                }
                Err(_) => {
                    it.last_json_error = 4;
                    if flags & 4194304 != 0 {
                        return Err(PhpError::uncaught("JsonException", "Syntax error", 0));
                    }
                    Value::Null
                }
            }
        }
        "json_last_error" => Value::Int(it.last_json_error),
        "json_last_error_msg" => Value::str(
            match it.last_json_error {
                0 => "No error",
                1 => "Maximum stack depth exceeded",
                2 => "State mismatch (invalid or malformed JSON)",
                3 => "Control character error, possibly incorrectly encoded",
                4 => "Syntax error",
                5 => "Malformed UTF-8 characters, possibly incorrectly encoded",
                7 => "Inf and NaN cannot be JSON encoded",
                8 => "Unsupported type",
                _ => "Unknown error",
            }
            .to_string(),
        ),
        "json_validate" => {
            let s = arg_str(it, args, 0);
            Value::Bool(json_decode(it, &s, true).is_ok())
        }

        // ----- hashing -----
        "md5" => Value::str(md5_hex(&arg_bs(it, args, 0))),
        "sha1" => Value::str(sha1_hex(&arg_bs(it, args, 0))),
        "crc32" => Value::Int(crc32(&arg_bs(it, args, 0)) as i64),
        "hash" => {
            let algo = arg_str(it, args, 0).to_lowercase();
            let data = arg_str(it, args, 1);
            match algo.as_str() {
                "md5" => Value::str(md5_hex(data.as_bytes())),
                "sha1" => Value::str(sha1_hex(data.as_bytes())),
                "crc32" | "crc32b" => Value::str(format!("{:08x}", crc32(data.as_bytes()))),
                _ => Value::Bool(false),
            }
        }
        "hash_equals" => Value::Bool(arg_str(it, args, 0) == arg_str(it, args, 1)),
        "crc32_combine" => Value::Int(0),

        // ----- encoding -----
        "base64_encode" => Value::str(base64_encode(&arg_bs(it, args, 0))),
        "base64_decode" => match base64_decode(&arg_str(it, args, 0)) {
            Some(b) => Value::bytes(b),
            None => Value::Bool(false),
        },
        "urlencode" => Value::str(
            String::from_utf8(urlencode(&arg_bs(it, args, 0), false)).unwrap_or_default(),
        ),
        "rawurlencode" => {
            Value::str(String::from_utf8(urlencode(&arg_bs(it, args, 0), true)).unwrap_or_default())
        }
        "urldecode" => Value::bytes(urldecode(&arg_bs(it, args, 0), false)),
        "rawurldecode" => Value::bytes(urldecode(&arg_bs(it, args, 0), true)),
        "http_build_query" => {
            let mut parts = Vec::new();
            if let Value::Array(a) = arg(args, 0) {
                for (k, c) in a.borrow().iter() {
                    let ks = key_str(k).into_bytes();
                    let vs = c.borrow().to_php_bytes();
                    parts.push(format!(
                        "{}={}",
                        String::from_utf8(urlencode(&ks, true)).unwrap_or_default(),
                        String::from_utf8(urlencode(&vs, true)).unwrap_or_default()
                    ));
                }
            }
            Value::str(parts.join("&"))
        }
        "parse_str" => {
            let s = arg_str(it, args, 0);
            let mut out = PhpArray::new();
            for pair in s.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    let k = urldecode(k.as_bytes(), true);
                    let v = urldecode(v.as_bytes(), false);
                    out.set(to_key(&Value::bytes(k)), Value::bytes(v));
                }
            }
            if let Some(c) = args.get(1) {
                *c.borrow_mut() = Value::Array(Rc::new(RefCell::new(out)));
            }
            Value::Null
        }

        // ----- regex (subset — PCRE-ish via regex crate) -----
        "preg_match"
        | "preg_match_all"
        | "preg_replace"
        | "preg_replace_callback"
        | "preg_replace_callback_array"
        | "preg_filter"
        | "preg_split"
        | "preg_grep"
        | "preg_quote"
        | "preg_last_error"
        | "preg_last_error_msg" => preg_dispatch(it, name, args)?,

        // ----- filesystem/process -----
        "file_exists" => Value::Bool(std::path::Path::new(&arg_str(it, args, 0)).exists()),
        "is_file" => Value::Bool(std::path::Path::new(&arg_str(it, args, 0)).is_file()),
        "is_dir" => Value::Bool(std::path::Path::new(&arg_str(it, args, 0)).is_dir()),
        "is_link" => Value::Bool(
            std::path::Path::new(&arg_str(it, args, 0))
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false),
        ),
        "is_readable" => Value::Bool(std::path::Path::new(&arg_str(it, args, 0)).exists()),
        "is_writable" | "is_writeable" => {
            Value::Bool(std::path::Path::new(&arg_str(it, args, 0)).exists())
        }
        "is_executable" => Value::Bool(std::path::Path::new(&arg_str(it, args, 0)).exists()),
        "filesize" => match std::fs::metadata(arg_str(it, args, 0)) {
            Ok(m) => Value::Int(m.len() as i64),
            Err(_) => Value::Bool(false),
        },
        "filemtime" | "fileatime" | "filectime" => match std::fs::metadata(arg_str(it, args, 0)) {
            Ok(m) => match m.modified() {
                Ok(t) => Value::Int(
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0),
                ),
                Err(_) => Value::Bool(false),
            },
            Err(_) => Value::Bool(false),
        },
        "fileperms" => {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::metadata(arg_str(it, args, 0)) {
                Ok(m) => Value::Int(m.permissions().mode() as i64),
                Err(_) => Value::Bool(false),
            }
        }
        "file_get_contents" => {
            let path = arg_str(it, args, 0);
            if path == "php://input" {
                Value::str(String::from_utf8_lossy(&it.php_input).into_owned())
            } else {
                match read_stream(&path) {
                    Ok(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
                    Err(e) => {
                        it.warn_pub(&format!(
                            "file_get_contents({}): Failed to open stream: {}",
                            path, e
                        ))?;
                        Value::Bool(false)
                    }
                }
            }
        }
        "file_put_contents" => {
            let path = arg_str(it, args, 0);
            let data = arg(args, 1).to_php_string();
            let append = arg(args, 2).to_int() & 8 != 0; // FILE_APPEND
            let r = if append {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .and_then(|mut f| f.write_all(data.as_bytes()))
            } else {
                std::fs::write(&path, data.as_bytes())
            };
            match r {
                Ok(_) => Value::Int(data.len() as i64),
                Err(e) => {
                    it.warn_pub(&format!(
                        "file_put_contents({}): Failed to open stream: {}",
                        path, e
                    ))?;
                    Value::Bool(false)
                }
            }
        }
        "unlink" => match std::fs::remove_file(arg_str(it, args, 0)) {
            Ok(_) => Value::Bool(true),
            Err(_) => Value::Bool(false),
        },
        "rename" => {
            Value::Bool(std::fs::rename(arg_str(it, args, 0), arg_str(it, args, 1)).is_ok())
        }
        "copy" => Value::Bool(std::fs::copy(arg_str(it, args, 0), arg_str(it, args, 1)).is_ok()),
        "mkdir" => Value::Bool(std::fs::create_dir_all(arg_str(it, args, 0)).is_ok()),
        "rmdir" => Value::Bool(std::fs::remove_dir(arg_str(it, args, 0)).is_ok()),
        "basename" => {
            let p = arg_str(it, args, 0);
            let suffix = arg_str(it, args, 1);
            let b = std::path::Path::new(&p)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            Value::str(b.strip_suffix(&suffix).map(|s| s.to_string()).unwrap_or(b))
        }
        "dirname" | "pathinfo_dirname" => {
            let p = arg_str(it, args, 0);
            Value::str(
                std::path::Path::new(&p)
                    .parent()
                    .map(|d| {
                        let s = d.display().to_string();
                        if s.is_empty() {
                            ".".into()
                        } else {
                            s
                        }
                    })
                    .unwrap_or_else(|| ".".into()),
            )
        }
        "realpath" => match std::fs::canonicalize(arg_str(it, args, 0)) {
            Ok(p) => Value::str(p.display().to_string()),
            Err(_) => Value::Bool(false),
        },
        "pathinfo" => {
            let p = arg_str(it, args, 0);
            let path = std::path::Path::new(&p);
            let mut out = PhpArray::new();
            let dir = path
                .parent()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|| ".".into());
            out.set(ArrKey::Str("dirname".into()), Value::str(dir));
            let file = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.set(ArrKey::Str("basename".into()), Value::str(file.clone()));
            let stem = if ext.is_empty() {
                file.clone()
            } else {
                file.strip_suffix(&format!(".{}", ext))
                    .unwrap_or(&file)
                    .to_string()
            };
            if !ext.is_empty() {
                out.set(ArrKey::Str("extension".into()), Value::str(ext));
            }
            out.set(ArrKey::Str("filename".into()), Value::str(stem));
            Value::Array(Rc::new(RefCell::new(out)))
        }
        // scheme://user:pass@host:port/path?query#fragment — component arg
        // selects one part (PHP_URL_*), -1 returns the present parts.
        "parse_url" => {
            let url = arg_str(it, args, 0);
            let comp = arg(args, 1).to_int();
            let (rest, fragment) = match url.split_once('#') {
                Some((a, b)) => (a.to_string(), Some(b.to_string())),
                None => (url.clone(), None),
            };
            let (rest, query) = match rest.split_once('?') {
                Some((a, b)) => (a.to_string(), Some(b.to_string())),
                None => (rest, None),
            };
            let (scheme, rest) = match rest.split_once("://") {
                Some((s, r)) => (Some(s.to_lowercase()), r.to_string()),
                None => (None, rest),
            };
            let (authority, path) = if scheme.is_some() || rest.starts_with("//") {
                let rest = rest.trim_start_matches('/');
                match rest.split_once('/') {
                    Some((a, p)) => (Some(a.to_string()), format!("/{}", p)),
                    None => (
                        if rest.is_empty() {
                            None
                        } else {
                            Some(rest.to_string())
                        },
                        String::new(),
                    ),
                }
            } else {
                (None, rest)
            };
            let (user, pass, host, port) = match &authority {
                Some(auth) => {
                    let (up, hp) = match auth.split_once('@') {
                        Some((u, h)) => (Some(u), h),
                        None => (None, auth.as_str()),
                    };
                    let (user, pass) = up
                        .map(|u| match u.split_once(':') {
                            Some((a, b)) => (Some(a.to_string()), Some(b.to_string())),
                            None => (Some(u.to_string()), None),
                        })
                        .unwrap_or((None, None));
                    let (host, port) = match hp.rsplit_once(':') {
                        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
                            (h.to_string(), p.parse::<i64>().ok())
                        }
                        _ => (hp.to_string(), None),
                    };
                    (user, pass, Some(host), port)
                }
                None => (None, None, None, None),
            };
            let part = |i: i64| -> Option<Value> {
                match i {
                    0 => scheme.clone().map(Value::str),
                    1 => host.clone().map(Value::str),
                    2 => port.map(Value::Int),
                    3 => user.clone().map(Value::str),
                    4 => pass.clone().map(Value::str),
                    5 => {
                        if path.is_empty() {
                            None
                        } else {
                            Some(Value::str(path.clone()))
                        }
                    }
                    6 => query.clone().map(Value::str),
                    7 => fragment.clone().map(Value::str),
                    _ => None,
                }
            };
            if comp >= 0 {
                return Ok(Some(part(comp).unwrap_or(Value::Null)));
            }
            let mut out = PhpArray::new();
            for (i, key) in [
                (0, "scheme"),
                (1, "host"),
                (2, "port"),
                (3, "user"),
                (4, "pass"),
                (5, "path"),
                (6, "query"),
                (7, "fragment"),
            ] {
                if let Some(v) = part(i) {
                    out.set(ArrKey::Str(key.into()), v);
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "tempnam" => {
            let dir = arg_str(it, args, 0);
            let prefix = arg_str(it, args, 1);
            let name = format!("{}/{}{}", dir, prefix, std::process::id());
            let _ = std::fs::File::create(&name);
            Value::str(name)
        }
        "tmpfile" => {
            let name = std::env::temp_dir().join(format!("phpun-{}", std::process::id()));
            match std::fs::File::create(&name) {
                Ok(f) => {
                    let id = it.next_res_id();
                    Value::Resource(Rc::new(RefCell::new(PhpResource::File {
                        id,
                        file: f,
                        read: true,
                        write: true,
                        pos: 0,
                        eof: false,
                    })))
                }
                Err(_) => Value::Bool(false),
            }
        }
        "sys_get_temp_dir" => Value::str(std::env::temp_dir().display().to_string()),
        "getcwd" => match std::env::current_dir() {
            Ok(d) => Value::str(d.display().to_string()),
            Err(_) => Value::Bool(false),
        },
        "chdir" => Value::Bool(std::env::set_current_dir(arg_str(it, args, 0)).is_ok()),
        "glob" => {
            let pat = arg_str(it, args, 0);
            let mut out = PhpArray::new();
            // minimal glob: only '*' and '?' in filename segments
            let dir = std::path::Path::new(&pat)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let fname = std::path::Path::new(&pat)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let re = glob_to_regex(&fname);
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if re.is_match(&n) {
                        let p = e.path();
                        let s = if std::path::Path::new(&pat).is_absolute() {
                            p.display().to_string()
                        } else {
                            let ds = dir.display().to_string();
                            if ds == "." {
                                n.clone()
                            } else {
                                format!("{}/{}", ds, n)
                            }
                        };
                        out.push(Value::str(s));
                    }
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "scandir" => {
            let mut out = PhpArray::new();
            if let Ok(rd) = std::fs::read_dir(arg_str(it, args, 0)) {
                out.push(Value::str("."));
                out.push(Value::str(".."));
                for e in rd.flatten() {
                    out.push(Value::str(e.file_name().to_string_lossy().into_owned()));
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "is_uploaded_file" => {
            let p = arg_str(it, args, 0);
            Value::Bool(it.uploads.iter().any(|u| u.display().to_string() == p))
        }
        "move_uploaded_file" => {
            let from = arg_str(it, args, 0);
            let to = arg_str(it, args, 1);
            if !it.uploads.iter().any(|u| u.display().to_string() == from) {
                it.warn_pub(&format!(
                    "move_uploaded_file({}): Unable to move: not an uploaded file",
                    from
                ))?;
                Value::Bool(false)
            } else {
                match std::fs::rename(&from, &to).or_else(|_| {
                    std::fs::copy(&from, &to)
                        .map(|_| ())
                        .and_then(|_| std::fs::remove_file(&from))
                }) {
                    Ok(_) => {
                        it.uploads.retain(|u| u.display().to_string() != from);
                        Value::Bool(true)
                    }
                    Err(e) => {
                        it.warn_pub(&format!(
                            "move_uploaded_file(): Unable to move '{}' to '{}': {}",
                            from, to, e
                        ))?;
                        Value::Bool(false)
                    }
                }
            }
        }
        "opendir" | "readdir" | "closedir" | "rewinddir" => Value::Null,
        "fopen" => {
            let path = arg_str(it, args, 0);
            let mode = arg_str(it, args, 1);
            if path == "php://input" {
                let id = it.next_res_id();
                Value::Resource(Rc::new(RefCell::new(PhpResource::Input {
                    id,
                    body: it.php_input.clone(),
                    pos: 0,
                })))
            } else if let Some(body) = parse_data_uri(&path) {
                // `data:[mediatype][;base64],payload` — a memory stream
                // (scalar_* tests fopen a data: URL for test values).
                let id = it.next_res_id();
                Value::Resource(Rc::new(RefCell::new(PhpResource::Input {
                    id,
                    body: Rc::new(body),
                    pos: 0,
                })))
            } else if let Some(which) = match path.as_str() {
                "php://stdin" => Some(0u8),
                "php://stdout" => Some(1u8),
                "php://stderr" => Some(2u8),
                _ => None,
            } {
                let id = it.next_res_id();
                Value::Resource(Rc::new(RefCell::new(PhpResource::Stdio { id, which })))
            } else {
                match fopen(&path, &mode) {
                    Ok(f) => {
                        let id = it.next_res_id();
                        let (r, w) = mode_flags(&mode);
                        Value::Resource(Rc::new(RefCell::new(PhpResource::File {
                            id,
                            file: f,
                            read: r,
                            write: w,
                            pos: 0,
                            eof: false,
                        })))
                    }
                    Err(e) => {
                        it.warn_pub(&format!("fopen({}): Failed to open stream: {}", path, e))?;
                        Value::Bool(false)
                    }
                }
            }
        }
        "fclose" => {
            if let Some(c) = args.first() {
                *c.borrow_mut() = Value::Null;
            }
            Value::Bool(true)
        }
        "fwrite" | "fputs" => {
            let data = arg(args, 1).to_php_string();
            match write_resource(it, args.first(), &data) {
                Ok(_) => Value::Int(data.len() as i64),
                Err(_) => Value::Bool(false),
            }
        }
        "fread" => {
            let n = arg(args, 1).to_int().max(0) as usize;
            match read_resource(args.first(), n) {
                Ok(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
                Err(_) => Value::Bool(false),
            }
        }
        "fgets" => match read_line_resource(args.first()) {
            Ok(b) => {
                if b.is_empty() {
                    Value::Bool(false)
                } else {
                    Value::str(String::from_utf8_lossy(&b).into_owned())
                }
            }
            Err(_) => Value::Bool(false),
        },
        "fgetc" => match read_resource(args.first(), 1) {
            Ok(b) if b.is_empty() => Value::Bool(false),
            Ok(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
            Err(_) => Value::Bool(false),
        },
        "feof" => match args.first() {
            Some(c) => match &*c.borrow() {
                Value::Resource(r) => match &*r.borrow() {
                    PhpResource::File { eof, .. } => Value::Bool(*eof),
                    _ => Value::Bool(true),
                },
                _ => Value::Bool(true),
            },
            None => Value::Bool(true),
        },
        "fseek" => {
            if let Some(c) = args.first() {
                if let Value::Resource(r) = &*c.borrow() {
                    if let PhpResource::File { pos, eof, .. } = &mut *r.borrow_mut() {
                        *pos = arg(args, 1).to_int().max(0) as u64;
                        *eof = false;
                    }
                }
            }
            Value::Int(0)
        }
        "ftell" => match args.first() {
            Some(c) => match &*c.borrow() {
                Value::Resource(r) => match &*r.borrow() {
                    PhpResource::File { pos, .. } => Value::Int(*pos as i64),
                    _ => Value::Int(0),
                },
                _ => Value::Int(0),
            },
            None => Value::Int(0),
        },
        "rewind" => {
            if let Some(c) = args.first() {
                if let Value::Resource(r) = &*c.borrow() {
                    if let PhpResource::File { pos, eof, .. } = &mut *r.borrow_mut() {
                        *pos = 0;
                        *eof = false;
                    }
                }
            }
            Value::Bool(true)
        }
        "ftruncate" => Value::Bool(true),
        "fflush" => Value::Bool(true),
        "flock" => Value::Bool(true),
        "fpassthru" => {
            let mut out = Vec::new();
            loop {
                match read_resource(args.first(), 8192) {
                    Ok(b) if b.is_empty() => break,
                    Ok(b) => out.extend_from_slice(&b),
                    Err(_) => break,
                }
            }
            let s = String::from_utf8_lossy(&out);
            it.emit(&s);
            Value::Int(out.len() as i64)
        }
        "fgetcsv" => Value::Bool(false), // TODO
        "file" => {
            let path = arg_str(it, args, 0);
            match std::fs::read_to_string(&path) {
                Ok(s) => {
                    let mut a = PhpArray::new();
                    for l in s.split_inclusive('\n') {
                        a.push(Value::str(l.to_string()));
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
                Err(_) => {
                    it.warn_pub(&format!("file({}): Failed to open stream", path))?;
                    Value::Bool(false)
                }
            }
        }
        "readfile" => {
            let path = arg_str(it, args, 0);
            if path == "php://input" {
                let body = it.php_input.clone();
                it.emit(&String::from_utf8_lossy(&body));
                Value::Int(body.len() as i64)
            } else {
                match std::fs::read(&path) {
                    Ok(b) => {
                        let s = String::from_utf8_lossy(&b);
                        it.emit(&s);
                        Value::Int(b.len() as i64)
                    }
                    Err(_) => {
                        it.warn_pub(&format!("readfile({}): Failed to open stream", path))?;
                        Value::Bool(false)
                    }
                }
            }
        }
        "parse_ini_file" | "parse_ini_string" => {
            let s = if name == "parse_ini_file" {
                std::fs::read_to_string(arg_str(it, args, 0)).unwrap_or_default()
            } else {
                arg_str(it, args, 0)
            };
            let mut out = PhpArray::new();
            let mut section: Option<String> = None;
            for line in s.lines() {
                let l = line.trim();
                if l.is_empty() || l.starts_with(';') || l.starts_with('#') {
                    continue;
                }
                if l.starts_with('[') && l.ends_with(']') {
                    section = Some(l[1..l.len() - 1].to_string());
                    continue;
                }
                if let Some((k, v)) = l.split_once('=') {
                    let k = k.trim().to_string();
                    let v = v.trim().trim_matches('"').to_string();
                    let _key = match &section {
                        Some(s) => format!("{}.{}", s, k),
                        None => k.clone(),
                    };
                    out.set(to_key(&Value::str(k)), Value::str(v));
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "stat" | "lstat" | "clearstatcache" => Value::Bool(false),
        "umask" => Value::Int(0o022),
        "chmod" | "chown" | "chgrp" | "touch" => Value::Bool(true),
        "link" | "symlink" | "readlink" | "linkinfo" => Value::Bool(false),
        "disk_free_space" | "disk_total_space" => Value::Float(1e12),
        "fnmatch" => Value::Bool(false),

        // ----- env/process -----
        "getenv" => {
            let name = arg_str(it, args, 0);
            match it.getenv_pub(&name) {
                Some(v) => Value::str(v),
                None => Value::Bool(false),
            }
        }
        "putenv" => {
            let s = arg_str(it, args, 0);
            Value::Bool(it.putenv_pub(&s))
        }
        "php_sapi_name" => Value::str("cli"),
        "phpversion" | "phpversion_strict" => Value::str("8.5.11-phpun"),
        "php_uname" => Value::str("Linux"),
        "memory_get_usage" => Value::Int(2097152),
        "memory_get_peak_usage" => Value::Int(2097152),
        "memory_reset_peak_usage" => Value::Null,
        "zend_version" => Value::str("8.5.11-phpun"),
        "getmypid" => Value::Int(std::process::id() as i64),
        "getmyuid" | "getmygid" | "getmyinode" => Value::Int(1000),
        "get_current_user" => Value::str("ubuntu"),
        "get_cfg_var" | "get_magic_quotes_gpc" | "get_magic_quotes_runtime" => Value::Bool(false),
        "php_ini_loaded_file" | "php_ini_scanned_files" => Value::Bool(false),
        "php_check_syntax" => Value::Bool(true),
        "extension_loaded" => Value::Bool(matches!(
            arg_str(it, args, 0).to_lowercase().as_str(),
            "core"
                | "standard"
                | "spl"
                | "pcre"
                | "hash"
                | "json"
                | "ctype"
                | "random"
                | "date"
                | "reflection"
        )),
        "get_loaded_extensions" => {
            let mut a = PhpArray::new();
            for e in [
                "Core",
                "standard",
                "SPL",
                "pcre",
                "hash",
                "json",
                "ctype",
                "random",
                "date",
                "Reflection",
            ] {
                a.push(Value::str(e));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "get_extension_funcs" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "dl" => Value::Bool(false),
        "assert" => {
            let v = arg(args, 0);
            if v.is_truthy() {
                Value::Bool(true)
            } else {
                // A `description` arg is the message; otherwise the call
                // renders as `assert(<args>)` (named_params/assert).
                let desc = arg(args, 1);
                let msg = if matches!(desc, Value::Null) || desc.to_php_string().is_empty() {
                    format!("assert({})", std::mem::take(&mut it.assert_src))
                } else {
                    desc.to_php_string()
                };
                return err("AssertionError", &msg);
            }
        }
        "assert_options" => Value::Bool(true),
        "setlocale" => {
            // No real locale switching — echo the first locale string.
            let loc = arg_str(it, args, 1);
            if loc.is_empty() {
                Value::str("C")
            } else {
                Value::str(loc)
            }
        }
        "cli_set_process_title" | "cli_get_process_title" => Value::Bool(true),
        "sleep" => {
            let n = arg(args, 0).to_int().clamp(0, 60);
            std::thread::sleep(std::time::Duration::from_secs(n as u64));
            Value::Int(0)
        }
        "usleep" => {
            let n = arg(args, 0).to_int().clamp(0, 60_000_000);
            std::thread::sleep(std::time::Duration::from_micros(n as u64));
            Value::Null
        }
        "time_nanosleep" => {
            let s = arg(args, 0).to_int().max(0) as u64;
            let ns = arg(args, 1).to_int().clamp(0, 999_999_999) as u64;
            std::thread::sleep(std::time::Duration::new(s.min(60), ns as u32));
            Value::Bool(true)
        }
        "time" | "time_sleep_until" => Value::Int(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        ),
        "microtime" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            if arg(args, 0).is_truthy() {
                Value::Float(now.as_secs() as f64 + now.subsec_micros() as f64 / 1e6)
            } else {
                Value::str(format!(
                    "{:.8} {}",
                    now.subsec_micros() as f64 / 1e6,
                    now.as_secs()
                ))
            }
        }
        "hrtime" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            if arg(args, 0).is_truthy() {
                let mut a = PhpArray::new();
                a.push(Value::Int(now.as_secs() as i64));
                a.push(Value::Int(now.subsec_nanos() as i64));
                Value::Array(Rc::new(RefCell::new(a)))
            } else {
                Value::Int(now.as_nanos() as i64)
            }
        }
        "uniqid" => Value::str(format!(
            "{:x}{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            std::process::id()
        )),
        "gc_collect_cycles" | "gc_enable" | "gc_disable" | "gc_mem_caches" => Value::Int(0),
        "gc_status" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "gc_enabled" => Value::Bool(false),
        "syslog" | "openlog" | "closelog" => Value::Bool(true),
        "array_change_key_case" => {
            let upper = arg(args, 1).to_int() == 1; // CASE_UPPER=1
            let mut out = PhpArray::new();
            if let Value::Array(a) = arg(args, 0) {
                for (k, c) in a.borrow().iter() {
                    let k2 = match k {
                        ArrKey::Str(s) => ArrKey::Str(
                            if upper {
                                s.to_uppercase()
                            } else {
                                s.to_lowercase()
                            }
                            .into(),
                        ),
                        _ => k.clone(),
                    };
                    out.set(k2, c.borrow().clone());
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "array_chunk" => {
            let size = arg(args, 1).to_int().max(1) as usize;
            let preserve = arg(args, 2).is_truthy();
            let mut out = PhpArray::new();
            if let Value::Array(a) = arg(args, 0) {
                let b = a.borrow();
                for chunk in b.entries.chunks(size) {
                    let mut c = PhpArray::new();
                    for (k, v) in chunk {
                        if preserve || matches!(k, ArrKey::Str(_)) {
                            c.set(k.clone(), v.borrow().clone());
                        } else {
                            c.push(v.borrow().clone());
                        }
                    }
                    out.push(Value::Array(Rc::new(RefCell::new(c))));
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "compact_obj" => Value::Null,
        "call_func" => Value::Null,
        "get_resource_type" | "get_resource_id" => match arg(args, 0) {
            Value::Resource(r) => {
                if name == "get_resource_id" {
                    Value::Int(r.borrow().id() as i64)
                } else {
                    Value::str("stream")
                }
            }
            _ => Value::Bool(false),
        },
        "stream_get_contents" => {
            let mut out = Vec::new();
            loop {
                match read_resource(args.first(), 8192) {
                    Ok(b) if b.is_empty() => break,
                    Ok(b) => out.extend_from_slice(&b),
                    Err(_) => break,
                }
            }
            Value::str(String::from_utf8_lossy(&out).into_owned())
        }
        "stream_context_create" | "stream_context_get_default" => {
            Value::Resource(Rc::new(RefCell::new(PhpResource::Other {
                id: it.next_res_id(),
                kind: "stream-context",
            })))
        }
        "stream_context_set_option" | "stream_context_get_options" => Value::Bool(true),
        "stream_wrapper_register" | "stream_wrapper_unregister" => Value::Bool(false),
        "stream_isatty" | "posix_isatty" => Value::Bool(false),
        "stream_set_timeout"
        | "stream_set_blocking"
        | "stream_set_read_buffer"
        | "stream_set_write_buffer"
        | "stream_set_chunk_size" => Value::Bool(true),
        "stream_get_meta_data" | "stream_get_filters" | "stream_get_wrappers" => {
            Value::Array(Rc::new(RefCell::new(PhpArray::new())))
        }
        "stream_filter_register" | "stream_filter_append" | "stream_filter_prepend" => {
            Value::Bool(false)
        }
        "fstat" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "fdopen" | "popen" | "pclose" => Value::Bool(false),
        "proc_open" | "proc_close" | "proc_get_status" | "proc_terminate" => Value::Bool(false),
        "shell_exec" | "exec" | "system" | "passthru" => Value::Null,
        "escapeshellarg" | "escapeshellcmd" => {
            let s = arg_str(it, args, 0);
            Value::str(format!("'{}'", s.replace('\'', "'\\''")))
        }
        "date_default_timezone_set" | "date_default_timezone_get" => {
            if name.ends_with("set") {
                Value::Bool(true)
            } else {
                Value::str("UTC")
            }
        }
        "date" | "gmdate" => {
            let fmt = arg_str(it, args, 0);
            let ts = if args.len() > 1 {
                arg(args, 1).to_int()
            } else {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            };
            Value::str(date_format(&fmt, ts))
        }
        "mktime" | "gmmktime" => {
            let h = arg(args, 0).to_int();
            let m = arg(args, 1).to_int();
            let s = arg(args, 2).to_int();
            let mo = if args.len() > 3 {
                arg(args, 3).to_int()
            } else {
                1
            };
            let d = if args.len() > 4 {
                arg(args, 4).to_int()
            } else {
                1
            };
            let y = if args.len() > 5 {
                arg(args, 5).to_int()
            } else {
                1970
            };
            Value::Int(days_from_civil(y, mo, d) * 86400 + h * 3600 + m * 60 + s)
        }
        "checkdate" => {
            let (m, d, y) = (
                arg(args, 0).to_int(),
                arg(args, 1).to_int(),
                arg(args, 2).to_int(),
            );
            Value::Bool((1..=12).contains(&m) && (1..=31).contains(&d) && (1..=32767).contains(&y))
        }
        "strtotime" => Value::Int(0), // TODO real parsing
        "date_parse" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "microtime_float" => Value::Float(0.0),
        "print" => {
            let s = it.to_string_of(&arg(args, 0));
            it.emit(&s);
            Value::Int(1)
        }

        // ----- misc -----
        "iterator_to_array" | "iterator_count" | "iterator_apply" => {
            let name_l = name.to_lowercase();
            match arg(args, 0) {
                Value::Array(a) => {
                    let mut out = PhpArray::new();
                    for (k, c) in a.borrow().iter() {
                        out.set(k.clone(), c.borrow().clone());
                    }
                    Value::Array(Rc::new(RefCell::new(out)))
                }
                Value::Object(o) => {
                    // Materialize via the Iterator protocol (Generator,
                    // IteratorAggregate, plain Iterator).
                    let items = it.yield_from_collect(&Value::Object(o))?;
                    if name_l == "iterator_count" {
                        return Ok(Some(Value::Int(items.len() as i64)));
                    }
                    let mut out = PhpArray::new();
                    // $preserve_keys (default true): duplicate int keys
                    // overwrite; false → append.
                    let preserve = args.get(1).map(|c| c.borrow().is_truthy()).unwrap_or(true);
                    for (k, v) in items {
                        let v = v.borrow().clone();
                        if preserve {
                            out.set(crate::value::to_key(&k), v);
                        } else {
                            out.push(v);
                        }
                    }
                    Value::Array(Rc::new(RefCell::new(out)))
                }
                _ => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
            }
        }
        "closure_from_callable" | "closure::fromcallable" => arg(args, 0),
        "get_called_class" => it.called_class_name(),
        "spl_autoload_register" => {
            if let Some(v) = args.first() {
                // ($callback, $throw, $prepend) — a truthy 3rd arg
                // prepends the loader (variance/loading_exception*).
                let prepend = args.get(2).map(|v| v.borrow().is_truthy()).unwrap_or(false);
                if prepend {
                    it.autoload_fns.insert(0, v.borrow().clone());
                } else {
                    it.autoload_fns.push(v.borrow().clone());
                }
            }
            Value::Bool(true)
        }
        "spl_autoload_unregister" => {
            // exact-value match — autoload lists are tiny in practice.
            if let Some(v) = args.first() {
                let target = v.borrow().clone();
                it.autoload_fns
                    .retain(|f| !crate::value::identical(f, &target));
            }
            Value::Bool(true)
        }
        "spl_autoload_functions" => {
            let mut a = PhpArray::new();
            for f in &it.autoload_fns {
                a.push(f.clone());
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "spl_autoload_call" => {
            let n = arg_str(it, args, 0);
            it.run_autoload(&n)?;
            Value::Bool(true)
        }
        "array_key_exists_slow" => Value::Null,
        "ctype_digit" => Value::Bool(
            !arg_str(it, args, 0).is_empty()
                && arg_str(it, args, 0).bytes().all(|b| b.is_ascii_digit()),
        ),
        "ctype_alpha" => Value::Bool(
            !arg_str(it, args, 0).is_empty()
                && arg_str(it, args, 0)
                    .bytes()
                    .all(|b| b.is_ascii_alphabetic()),
        ),
        "ctype_alnum" => Value::Bool(
            !arg_str(it, args, 0).is_empty()
                && arg_str(it, args, 0)
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric()),
        ),
        "ctype_space" => Value::Bool(
            !arg_str(it, args, 0).is_empty()
                && arg_str(it, args, 0)
                    .bytes()
                    .all(|b| b.is_ascii_whitespace()),
        ),
        "ctype_upper" => Value::Bool(
            !arg_str(it, args, 0).is_empty()
                && arg_str(it, args, 0).bytes().all(|b| b.is_ascii_uppercase()),
        ),
        "ctype_lower" => Value::Bool(
            !arg_str(it, args, 0).is_empty()
                && arg_str(it, args, 0).bytes().all(|b| b.is_ascii_lowercase()),
        ),
        "ctype_punct" | "ctype_graph" | "ctype_print" | "ctype_cntrl" | "ctype_xdigit" => {
            Value::Bool(true)
        }
        "mb_strtolower_nc" => Value::Null,
        "get_include_path" | "set_include_path" | "restore_include_path" => {
            Value::str(".:/home/linuxbrew/.linuxbrew/share/pear")
        }
        "token_get_all" | "token_name" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "highlight_string" => {
            let src = arg(args, 0).to_php_string();
            let h = crate::highlight::highlight_html(&src);
            if arg(args, 1).is_truthy() {
                Value::str(h)
            } else {
                it.emit(&h);
                Value::Bool(true)
            }
        }
        "highlight_file" | "show_source" => {
            let path = arg(args, 0).to_php_string();
            match std::fs::read(&path) {
                Ok(b) => {
                    let h = crate::highlight::highlight_html(&String::from_utf8_lossy(&b));
                    if arg(args, 1).is_truthy() {
                        Value::str(h)
                    } else {
                        it.emit(&h);
                        Value::Bool(true)
                    }
                }
                Err(_) => {
                    it.warn_pub(&format!(
                        "highlight_file({}): Failed to open stream: No such file or directory",
                        path
                    ))?;
                    it.warn_pub(&format!(
                        "highlight_file(): Failed opening '{}' for highlighting",
                        path
                    ))?;
                    Value::Bool(false)
                }
            }
        }
        "php_strip_whitespace" => {
            let path = arg(args, 0).to_php_string();
            match std::fs::read(&path) {
                Ok(b) => Value::str(crate::highlight::strip_whitespace(
                    &String::from_utf8_lossy(&b),
                )),
                Err(_) => Value::str(""),
            }
        }
        "pack" | "unpack" => Value::Bool(false), // TODO
        "header" => {
            let h = arg_str(it, args, 0);
            let replace = args.get(1).map(|c| c.borrow().is_truthy()).unwrap_or(true);
            let code = arg(args, 2).to_int();
            let lower = h.to_lowercase();
            if lower.starts_with("http/") {
                // Status-line form: header("HTTP/1.1 404 Not Found").
                if let Some(c) = h
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<i64>().ok())
                {
                    it.resp_code = c;
                }
            } else {
                let name = h.split(':').next().unwrap_or("").trim().to_lowercase();
                if replace && !name.is_empty() {
                    let prefix = format!("{}:", name);
                    it.out_headers
                        .retain(|x| !x.to_lowercase().starts_with(&prefix));
                }
                it.out_headers.push(h);
                if code > 0 {
                    it.resp_code = code;
                } else if name == "location" {
                    it.resp_code = 302;
                }
            }
            Value::Null
        }
        "headers_sent" => Value::Bool(false),
        "headers_list" => {
            let mut a = PhpArray::new();
            for h in &it.out_headers {
                a.push(Value::str(h.clone()));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "header_remove" => {
            if args.is_empty() {
                it.out_headers.clear();
            } else {
                let prefix = format!("{}:", arg_str(it, args, 0).to_lowercase());
                it.out_headers
                    .retain(|x| !x.to_lowercase().starts_with(&prefix));
            }
            Value::Null
        }
        "header_register_callback" => Value::Bool(false),
        "http_response_code" => {
            if args.is_empty() {
                Value::Int(it.resp_code)
            } else {
                let code = arg(args, 0).to_int();
                it.resp_code = code;
                Value::Int(code)
            }
        }
        "setcookie" | "setrawcookie" => {
            let cname = arg_str(it, args, 0);
            let cval = if name == "setcookie" {
                String::from_utf8(urlencode(&arg_bs(it, args, 1), true)).unwrap_or_default()
            } else {
                arg_str(it, args, 1)
            };
            let mut line = format!("Set-Cookie: {}={}", cname, cval);
            let opts = arg(args, 2);
            let (expires, path, domain, secure, httponly, samesite) = match &opts {
                Value::Array(a) => {
                    let a = a.borrow();
                    let get = |k: &str| {
                        a.entries
                            .iter()
                            .find(|(ek, _)| matches!(ek, ArrKey::Str(s) if s.as_ref() == k))
                            .map(|(_, c)| c.borrow().clone())
                    };
                    (
                        get("expires").map(|v| v.to_int()).unwrap_or(0),
                        get("path").map(|v| it.to_string_of(&v)).unwrap_or_default(),
                        get("domain")
                            .map(|v| it.to_string_of(&v))
                            .unwrap_or_default(),
                        get("secure").map(|v| v.is_truthy()).unwrap_or(false),
                        get("httponly").map(|v| v.is_truthy()).unwrap_or(false),
                        get("samesite")
                            .map(|v| it.to_string_of(&v))
                            .unwrap_or_default(),
                    )
                }
                v => (
                    v.to_int(),
                    arg_str(it, args, 3),
                    arg_str(it, args, 4),
                    arg(args, 5).is_truthy(),
                    arg(args, 6).is_truthy(),
                    String::new(),
                ),
            };
            if expires > 0 {
                line.push_str(&format!(
                    "; expires={}; Max-Age={}",
                    date_format("D, d M Y H:i:s", expires),
                    expires
                        - std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0)
                ));
            }
            if !path.is_empty() {
                line.push_str(&format!("; path={}", path));
            }
            if !domain.is_empty() {
                line.push_str(&format!("; domain={}", domain));
            }
            if secure {
                line.push_str("; secure");
            }
            if httponly {
                line.push_str("; HttpOnly");
            }
            if !samesite.is_empty() {
                line.push_str(&format!("; SameSite={}", samesite));
            }
            it.out_headers.push(line);
            Value::Bool(true)
        }
        "connection_status" | "connection_aborted" => Value::Int(0),
        "fastcgi_finish_request" => Value::Bool(true),
        "preg_jit" => Value::Bool(false),
        "date_sunrise" | "date_sunset" | "date_sun_info" => Value::Bool(false),
        "phpinfo" | "phpcredits" | "php_logo_guid" | "php_real_logo_guid" | "zend_logo_guid" => {
            Value::Null
        }
        "version_compare" => {
            let a = arg_str(it, args, 0);
            let b = arg_str(it, args, 1);
            let c = version_cmp(&a, &b);
            if args.len() > 2 {
                match arg_str(it, args, 2).as_str() {
                    "<" | "lt" => Value::Bool(c < 0),
                    "<=" | "le" => Value::Bool(c <= 0),
                    ">" | "gt" => Value::Bool(c > 0),
                    ">=" | "ge" => Value::Bool(c >= 0),
                    "==" | "=" | "eq" => Value::Bool(c == 0),
                    "!=" | "<>" | "ne" => Value::Bool(c != 0),
                    _ => Value::Null,
                }
            } else {
                Value::Int(c)
            }
        }
        "clone" => arg(args, 0),
        "assert_options_now" => Value::Null,
        "zend_test_func" | "zend_test_array_return" => Value::Null,
        "iterator_from_array" => Value::Null,
        "openssl_random_pseudo_bytes" | "random_bytes" => {
            let n = arg(args, 0).to_int().max(0) as usize;
            let mut b = vec![0u8; n];
            use std::time::{SystemTime, UNIX_EPOCH};
            let seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9e3779b97f4a7c15);
            let mut x = seed;
            for byte in b.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *byte = x as u8;
            }
            Value::str(String::from_utf8_lossy(&b).into_owned())
        }

        // eval of last resort
        _ => return Ok(None),
    }))
}

/// Parameter names/requiredness for a handful of builtins whose FCC
/// closures appear in var_dump output (Zend/tests/first_class_callable).
/// `(name, required)`.
pub(crate) fn builtin_sig(n: &str) -> Option<Vec<(String, bool)>> {
    let ps: &[(&str, bool)] = match n {
        "strlen" | "strrev" | "strtoupper" | "strtolower" | "md5" | "sha1" => &[("string", true)],
        "sprintf" => &[("format", true), ("args", false)],
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
        _ => return Some(Vec::new()),
    };
    Some(ps.iter().map(|(n, r)| (n.to_string(), *r)).collect())
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
            | "array_diff_key"
            | "array_fill"
            | "array_fill_keys"
            | "array_filter"
            | "array_flip"
            | "array_intersect"
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
            | "divmod"
            | "dl"
            | "each"
            | "end"
            | "enum_exists"
            | "error_reporting"
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
            | "pathinfo"
            | "php_check_syntax"
            | "php_sapi_name"
            | "php_uname"
            | "pi"
            | "pow"
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
            | "vprintf"
            | "vsprintf"
            | "wordwrap"
            | "zend_version"
    )
}

fn num_val(x: f64, orig: Value, _step: f64) -> Value {
    // int-preserve: when the range bound is int and value is integral → int
    if matches!(orig, Value::Int(_)) && x.fract() == 0.0 && x.abs() < 9e15 {
        Value::Int(x as i64)
    } else {
        Value::Float(x)
    }
}

fn xorshift(mut x: u64) -> u64 {
    if x == 0 {
        x = 0x9e3779b97f4a7c15;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

fn is_varname(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn key_str(k: &ArrKey) -> String {
    match k {
        ArrKey::Int(i) => i.to_string(),
        ArrKey::Str(s) => s.to_string(),
        ArrKey::Tomb => String::new(),
    }
}

// ---------- var_dump / print_r / var_export ----------

/// var_dump one zval; `is_ref` prints PHP's `&` prefix for reference cells.
fn var_dump(it: &mut Interp, v: &Value, indent: usize, zval: bool, is_ref: bool) {
    let pad = "  ".repeat(indent);
    // debug_zval_dump appends `refcount(N)` to every line/header.
    let rc = |n: usize| -> String {
        if zval {
            format!(" refcount({})", n)
        } else {
            String::new()
        }
    };
    let r = if is_ref { "&" } else { "" };
    match v {
        Value::Null => it.emit(&format!("{}{}NULL{}\n", pad, r, rc(1))),
        Value::Bool(b) => it.emit(&format!("{}{}bool({}){}\n", pad, r, b, rc(1))),
        Value::Int(i) => it.emit(&format!("{}{}int({}){}\n", pad, r, i, rc(1))),
        Value::Float(f) => {
            let prec = it.ini_int("serialize_precision", -1);
            it.emit(&format!(
                "{}{}float({}){}\n",
                pad,
                r,
                crate::value::format_float_prec(*f, prec),
                rc(1)
            ))
        }
        Value::Str(s) => it.emit_bytes(
            &[
                format!("{}{}string({}) \"", pad, r, s.len()).into_bytes(),
                s.to_vec(),
                format!("\"{}\n", rc(1)).into_bytes(),
            ]
            .concat(),
        ),
        Value::Array(a) => {
            let rcn = Rc::strong_count(a);
            let a = a.borrow();
            // zval: `array(2) refcount(1){` — plain: `array(2) {`.
            let tail = if zval { rc(rcn) } else { " ".to_string() };
            it.emit(&format!("{}{}array({}){}{{\n", pad, r, a.len(), tail));
            for (k, c) in a.iter() {
                match k {
                    ArrKey::Int(i) => it.emit(&format!("{}  [{}]=>\n", pad, i)),
                    ArrKey::Str(s) => it.emit(&format!("{}  [\"{}\"]=>\n", pad, s)),
                    ArrKey::Tomb => continue,
                }
                var_dump(
                    it,
                    &c.borrow(),
                    indent + 1,
                    zval,
                    // typed_slots pins a clone of bound cells — exclude it
                    // from the &-marker count (typed_properties_038).
                    Rc::strong_count(c)
                        > 1 + it.typed_slots.contains_key(&(Rc::as_ptr(c) as usize)) as usize,
                );
            }
            it.emit(&format!("{}}}\n", pad));
        }
        Value::Object(o) => {
            let ob = o.borrow();
            // Enum cases print `enum(E::Case1)` (single line).
            if ob.class.decl.kind == crate::ast::ClassKind::Enum {
                if let Some(nm) = ob.props.get("name") {
                    if let Value::Str(case) = &*nm.borrow() {
                        it.emit(&format!(
                            "{}enum({}::{})\n",
                            pad,
                            ob.class.name(),
                            crate::value::lossy(case)
                        ));
                        return;
                    }
                }
            }
            // Count live props only — unset() tombstones prop_order slots.
            let mut live = ob
                .prop_order
                .iter()
                .filter(|n| ob.props.contains_key(*n))
                .count();
            // Internal engine state Zend exposes in var_dump:
            // Generator's creating function and ArrayIterator's
            // private storage (iterable_001).
            let internal_props: Vec<(String, Value)> = match &ob.internal {
                Some(crate::value::ObjectInternal::Generator(st)) => {
                    let st = st.borrow();
                    let fname = match &st.setup {
                        // Methods dump as `C::test` (generator_return_
                        // containing_extra_types).
                        crate::value::GenSetup::Invoke {
                            decl, decl_class, ..
                        } => match decl_class {
                            Some(c) => format!("{}::{}", c.decl.name, decl.name),
                            None => decl.name.clone(),
                        },
                    };
                    vec![("\"function\"".to_string(), Value::str(&fname))]
                }
                Some(crate::value::ObjectInternal::ArrayIter { arr, .. }) => {
                    vec![(
                        "\"storage\":\"ArrayIterator\":private".to_string(),
                        Value::Array(arr.clone()),
                    )]
                }
                _ => Vec::new(),
            };
            live += internal_props.len();
            let tail = if zval {
                rc(Rc::strong_count(o))
            } else {
                " ".to_string()
            };
            it.emit(&format!(
                "{}object({})#{} ({}){}{{\n",
                pad,
                ob.class.name(),
                ob.id,
                live,
                tail
            ));
            for n in &ob.prop_order {
                // Reserved-but-cellless slots are uninitialized typed
                // props — zend prints `uninitialized(T)` (recursion).
                if !ob.props.contains_key(n) {
                    if let Some(pd) = it.decl_for_slot(o, n) {
                        if let Some(tys) = &pd.ty {
                            let ty = if tys.len() == 2 && tys.iter().any(|t| t == "null") {
                                format!("?{}", tys.iter().find(|t| *t != "null").unwrap())
                            } else {
                                tys.join("|")
                            };
                            let (vis, dcls) = it.prop_visibility(&ob.class, n);
                            let disp = n
                                .strip_prefix('\0')
                                .and_then(|r| r.split('\0').nth(1))
                                .unwrap_or(n.as_str());
                            let key = match vis {
                                crate::ast::Visibility::Private => {
                                    format!("\"{}\":\"{}\":private", disp, dcls)
                                }
                                crate::ast::Visibility::Protected => {
                                    format!("\"{}\":protected", disp)
                                }
                                crate::ast::Visibility::Public => {
                                    format!("\"{}\"", disp)
                                }
                            };
                            it.emit(&format!("{}  [{}]=>\n", pad, key));
                            it.emit(&format!("{}  uninitialized({})\n", pad, ty));
                        }
                    }
                    continue;
                }
                if let Some(c) = ob.props.get(n) {
                    let (vis, dcls) = it.prop_visibility(&ob.class, n);
                    // Mangled private keys "\0Cls\0name" display only `name`.
                    let disp = n
                        .strip_prefix('\0')
                        .and_then(|r| r.split('\0').nth(1))
                        .unwrap_or(n.as_str());
                    let key = match vis {
                        crate::ast::Visibility::Private => {
                            format!("\"{}\":\"{}\":private", disp, dcls)
                        }
                        crate::ast::Visibility::Protected => {
                            format!("\"{}\":protected", disp)
                        }
                        crate::ast::Visibility::Public => format!("\"{}\"", disp),
                    };
                    it.emit(&format!("{}  [{}]=>\n", pad, key));
                    var_dump(
                        it,
                        &c.borrow(),
                        indent + 1,
                        zval,
                        // typed_slots pins a clone of bound cells — exclude it
                        // from the &-marker count (typed_properties_038).
                        Rc::strong_count(c)
                            > 1 + it.typed_slots.contains_key(&(Rc::as_ptr(c) as usize)) as usize,
                    );
                }
            }
            for (k, v) in &internal_props {
                it.emit(&format!("{}  [{}]=>\n", pad, k));
                var_dump(it, v, indent + 1, zval, false);
            }
            it.emit(&format!("{}}}\n", pad));
        }
        Value::Callable(c) => {
            // Closure debug info (zend_closures.c): function/name key +
            // bound $this + file/line for literals + parameter map.
            let pad2 = format!("{}  ", pad);
            let pad3 = format!("{}    ", pad);
            let mut keys: Vec<(String, String)> = Vec::new();
            let mut this_obj = None;
            let params: Option<Vec<(String, bool)>> = match &c.kind {
                crate::value::CallableKind::Named(n) => {
                    keys.push(("function".into(), n.clone()));
                    it.functions
                        .get(&n.to_lowercase())
                        .map(|d| {
                            d.params
                                .iter()
                                .map(|p| (p.name.clone(), p.default.is_none() && !p.variadic))
                                .collect()
                        })
                        .or_else(|| builtin_sig(&n.to_lowercase()))
                }
                crate::value::CallableKind::Method { obj, class, name } => {
                    let cn = c
                        .scope_class
                        .as_ref()
                        .map(|sc| sc.name().to_string())
                        .or_else(|| {
                            obj.as_ref()
                                .map(|o| o.borrow().class.name().to_string())
                                .or_else(|| class.as_ref().map(|cl| cl.name().to_string()))
                        })
                        .unwrap_or_default();
                    keys.push(("function".into(), format!("{}::{}", cn, name)));
                    if let Some(o) = obj {
                        this_obj = Some(o.clone());
                    }
                    let cls = obj
                        .as_ref()
                        .map(|o| o.borrow().class.clone())
                        .or_else(|| class.clone());
                    cls.and_then(|cl| {
                        it.find_method_in(&cl, name).map(|(m, _)| {
                            m.decl
                                .params
                                .iter()
                                .map(|p| (p.name.clone(), p.default.is_none() && !p.variadic))
                                .collect()
                        })
                    })
                }
                crate::value::CallableKind::Closure(d) => {
                    keys.push(("name".into(), format!("{{closure:{}:{}}}", d.file, d.line)));
                    keys.push(("file".into(), d.file.clone()));
                    keys.push(("line".into(), String::new())); // int below
                    Some(
                        d.params
                            .iter()
                            .map(|p| (p.name.clone(), p.default.is_none() && !p.variadic))
                            .collect(),
                    )
                }
            };
            let has_params = params.as_ref().map(|p| !p.is_empty()).unwrap_or(false);
            let nfields = keys.len() + this_obj.is_some() as usize + has_params as usize;
            it.emit(&format!(
                "{}object(Closure)#{} ({}) {{\n",
                pad,
                c.id.get(),
                nfields
            ));
            let mut line_int = 0i64;
            if let crate::value::CallableKind::Closure(d) = &c.kind {
                line_int = d.line as i64;
            }
            for (k, v) in &keys {
                if k == "line" {
                    it.emit(&format!(
                        "{}[\"line\"]=>\n{}int({})\n",
                        pad2, pad2, line_int
                    ));
                } else {
                    it.emit(&format!(
                        "{}[\"{}\"]=>\n{}string({}) \"{}\"\n",
                        pad2,
                        k,
                        pad2,
                        v.len(),
                        v
                    ));
                }
            }
            if let Some(o) = &this_obj {
                it.emit(&format!("{}[\"this\"]=>\n", pad2));
                var_dump(it, &Value::Object(o.clone()), indent + 1, zval, false);
            }
            if let Some(ps) = &params {
                if has_params {
                    it.emit(&format!(
                        "{}[\"parameter\"]=>\n{}array({}) {{\n",
                        pad2,
                        pad2,
                        ps.len()
                    ));
                    for (pn, req) in ps {
                        let word = if *req { "<required>" } else { "<optional>" };
                        it.emit(&format!(
                            "{}[\"${}\"]=>\n{}string({}) \"{}\"\n",
                            pad3,
                            pn,
                            pad3,
                            word.len(),
                            word
                        ));
                    }
                    it.emit(&format!("{}}}\n", pad2));
                }
            }
            it.emit(&format!("{}}}\n", pad));
        }
        Value::Resource(r) => it.emit(&format!(
            "{}resource({}) of type (stream)\n",
            pad,
            r.borrow().id()
        )),
    }
}

fn print_r(_it: &mut Interp, v: &Value, indent: usize) -> String {
    match v {
        Value::Array(a) => {
            let a = a.borrow();
            let mut s = String::from("Array\n");
            s.push_str(&"    ".repeat(indent));
            s.push_str("(\n");
            for (k, c) in a.iter() {
                s.push_str(&"    ".repeat(indent + 1));
                s.push_str(&format!("[{}] => ", key_str(k)));
                let inner = print_r(_it, &c.borrow(), indent + 2);
                s.push_str(&inner);
                s.push('\n');
                if matches!(*c.borrow(), Value::Array(_) | Value::Object(_)) {
                    s.push('\n');
                }
            }
            s.push_str(&"    ".repeat(indent));
            s.push(')');
            s
        }
        Value::Object(o) => {
            let ob = o.borrow();
            let mut s = format!("{} Object\n", ob.class.name());
            s.push_str(&"    ".repeat(indent));
            s.push_str("(\n");
            for n in &ob.prop_order {
                if let Some(c) = ob.props.get(n) {
                    s.push_str(&"    ".repeat(indent + 1));
                    s.push_str(&format!("[{}] => ", n));
                    s.push_str(&print_r(_it, &c.borrow(), indent + 2));
                    s.push('\n');
                    if matches!(*c.borrow(), Value::Array(_) | Value::Object(_)) {
                        s.push('\n');
                    }
                }
            }
            s.push_str(&"    ".repeat(indent));
            s.push(')');
            s
        }
        Value::Float(f) => {
            let prec = _it.ini_int("precision", 14);
            crate::value::format_float_prec(*f, prec)
        }
        other => other.to_php_string(),
    }
}

fn var_export(it: &mut Interp, v: &Value) -> String {
    var_export_depth(it, v, 0)
}

fn var_export_depth(it: &mut Interp, v: &Value, depth: usize) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            let prec = it.ini_int("serialize_precision", -1);
            let s = crate::value::format_float_prec(*f, prec);
            // var_export always renders a decimal point: 0.0, 100.0.
            if s.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                format!("{}.0", s)
            } else {
                s
            }
        }
        Value::Str(s) => format!(
            "'{}'",
            crate::value::lossy(&s)
                .replace('\\', "\\\\")
                .replace('\'', "\\'")
        ),
        Value::Array(a) => {
            let a = a.borrow();
            let pad = "  ".repeat(depth + 1);
            let mut s = String::from("array (\n");
            for (k, c) in a.iter() {
                s.push_str(&pad);
                s.push_str(&match k {
                    ArrKey::Int(i) => i.to_string(),
                    ArrKey::Str(st) => {
                        format!("'{}'", st.replace('\\', "\\\\").replace('\'', "\\'"))
                    }
                    ArrKey::Tomb => continue,
                });
                s.push_str(" => ");
                // A nested array value renders on its own line at key depth
                // ('key' => \n  array (...)) — matches zend var_export.
                let inner = c.borrow();
                if matches!(&*inner, Value::Array(_)) {
                    s.push('\n');
                    s.push_str(&pad);
                }
                s.push_str(&var_export_depth(it, &inner, depth + 1));
                s.push_str(",\n");
            }
            s.push_str(&"  ".repeat(depth));
            s.push(')');
            s
        }
        Value::Object(o) => {
            // All decl entries (both private `changed`s), hooked props
            // via `get`, plain emitted names (property_hooks/dump).
            let mut s = format!("\\{}::__set_state(array(\n", o.borrow().class.name());
            for (out, slot, decl) in it.object_serial_entries(o) {
                let v = match &decl {
                    Some((p, dcls)) => it.serial_entry_value(o, p, dcls, &slot),
                    None => o.borrow().props.get(&slot).map(|c| c.borrow().clone()),
                };
                if let Some(v) = v {
                    s.push_str(&format!("   '{}' => ", out));
                    s.push_str(&var_export_depth(it, &v, depth + 1));
                    s.push_str(",\n");
                }
            }
            s.push_str("))");
            s
        }
        _ => "NULL".into(),
    }
}

// ---------- sprintf ----------

fn sprintf(it: &mut Interp, args: &[Cell]) -> Result<String, PhpError> {
    let fmt = arg_str(it, args, 0);
    sprintf_args(it, &fmt, &args[1.min(args.len())..])
}

fn sprintf_args(it: &mut Interp, fmt: &str, args: &[Cell]) -> Result<String, PhpError> {
    let mut out = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    let mut argi = 0usize;
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            break;
        }
        if chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        // argnum$
        let mut idx_override = None;
        let save = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i < chars.len() && chars[i] == '$' {
            idx_override = Some(
                chars[save..i]
                    .iter()
                    .collect::<String>()
                    .parse::<usize>()
                    .unwrap_or(1)
                    - 1,
            );
            i += 1;
        } else {
            i = save;
        }
        // flags
        let mut pad = ' ';
        let mut left = false;
        let mut plus = false;
        let mut alt = false;
        loop {
            match chars.get(i) {
                Some('0') => pad = '0',
                Some('-') => left = true,
                Some('+') => plus = true,
                Some(' ') => {}
                Some('#') => alt = true,
                Some('\'') => {
                    i += 1;
                    pad = chars.get(i).copied().unwrap_or(' ');
                }
                _ => break,
            }
            i += 1;
        }
        // width
        let mut width = String::new();
        while i < chars.len() && chars[i].is_ascii_digit() {
            width.push(chars[i]);
            i += 1;
        }
        let width: usize = width.parse().unwrap_or(0);
        // precision
        let mut prec: Option<usize> = None;
        if i < chars.len() && chars[i] == '.' {
            i += 1;
            let mut p = String::new();
            while i < chars.len() && chars[i].is_ascii_digit() {
                p.push(chars[i]);
                i += 1;
            }
            prec = Some(p.parse().unwrap_or(0));
        }
        if i >= chars.len() {
            break;
        }
        let spec = chars[i];
        i += 1;
        let ai = idx_override.unwrap_or(argi);
        argi = ai + 1;
        let v = args
            .get(ai)
            .map(|c| c.borrow().clone())
            .unwrap_or(Value::Null);
        let body = fmt_spec(it, spec, &v, prec, alt)?;
        let signed = plus && matches!(spec, 'd' | 'i' | 'e' | 'E' | 'f' | 'F' | 'g' | 'G');
        let body = if signed && !body.starts_with('-') {
            format!("+{}", body)
        } else {
            body
        };
        if body.len() < width {
            let fill = width - body.len();
            let mut padded = String::new();
            if left {
                padded.push_str(&body);
                padded.push_str(&" ".repeat(fill));
            } else if pad == '0' && body.starts_with('-') {
                padded.push('-');
                padded.push_str(&"0".repeat(fill));
                padded.push_str(&body[1..]);
            } else if pad == '0' && body.starts_with('+') {
                padded.push('+');
                padded.push_str(&"0".repeat(fill));
                padded.push_str(&body[1..]);
            } else {
                padded.push_str(&pad.to_string().repeat(fill));
                padded.push_str(&body);
            }
            out.push_str(&padded);
        } else {
            out.push_str(&body);
        }
    }
    Ok(out)
}

fn fmt_spec(
    it: &mut Interp,
    spec: char,
    v: &Value,
    prec: Option<usize>,
    alt: bool,
) -> Result<String, PhpError> {
    Ok(match spec {
        's' | 'S' => {
            let s = it.to_string_of(v);
            match prec {
                Some(p) => s.chars().take(p).collect(),
                None => s,
            }
        }
        'd' | 'i' | 'u' => v.to_int().to_string(),
        'b' => format!("{:b}", v.to_int()),
        'o' => format!("{:o}", v.to_int()),
        'x' => format!("{:x}", v.to_int()),
        'X' => format!("{:X}", v.to_int()),
        'c' => String::from_utf8_lossy(&[(v.to_int() & 0xff) as u8]).into_owned(),
        'e' => format!("{:.*e}", prec.unwrap_or(6), v.to_float()),
        'E' => format!("{:.*E}", prec.unwrap_or(6), v.to_float())
            .replace("e+", "E+")
            .replace("e-", "E-"),
        'f' | 'F' => format!("{:.*}", prec.unwrap_or(6), v.to_float()),
        'g' | 'G' => {
            let p = prec.unwrap_or(6).max(1);
            let s = crate::value::gcvt(v.to_float(), p);
            if spec == 'g' {
                s.to_lowercase()
            } else {
                s
            }
        }
        'h' | 'H' => crate::value::gcvt(v.to_float(), prec.unwrap_or(6).max(1)),
        'a' | 'A' => v.to_float().to_string(),
        'r' => v.to_php_string(),
        'v' => crate::value::format_float(v.to_float()),
        '%' => "%".into(),
        _ => {
            let _ = alt;
            format!("%{}", spec)
        }
    })
}

fn number_format(n: f64, dec: usize, dp: &str, ts: &str) -> String {
    let s = format!("{:.*}", dec, n.abs());
    let (int_part, frac) = match s.split_once('.') {
        Some((i, f)) => (i.to_string(), Some(f)),
        None => (s, None),
    };
    let mut grouped = String::new();
    let bytes = int_part.as_bytes();
    for (i, c) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            grouped.push_str(ts);
        }
        grouped.push(*c as char);
    }
    if n < 0.0 {
        grouped.insert(0, '-');
    }
    match frac {
        Some(f) => format!("{}{}{}", grouped, dp, f),
        None => grouped,
    }
}

// ---------- strings impl ----------

fn trim_set(s: &[u8], chars: &[u8], left: bool, right: bool) -> Vec<u8> {
    let in_set = |c: u8| {
        chars.contains(&c)
            || chars
                .windows(4)
                .any(|w| w[1] == b'.' && w[2] == b'.' && c >= w[0] && c <= w[3])
    };
    let start = if left {
        s.iter().position(|&c| !in_set(c)).unwrap_or(s.len())
    } else {
        0
    };
    let end = if right {
        s.iter()
            .rposition(|&c| !in_set(c))
            .map(|i| i + 1)
            .unwrap_or(0)
    } else {
        s.len()
    };
    if end < start {
        Vec::new()
    } else {
        s[start..end].to_vec()
    }
}

fn str_replace(find: &Value, repl: &Value, subj: &Value, ci: bool) -> Vec<u8> {
    let finds: Vec<Vec<u8>> = match find {
        Value::Array(a) => a
            .borrow()
            .entries
            .iter()
            .map(|(_, c)| c.borrow().to_php_bytes())
            .collect(),
        v => vec![v.to_php_bytes()],
    };
    let repls: Vec<Vec<u8>> = match repl {
        Value::Array(a) => a
            .borrow()
            .entries
            .iter()
            .map(|(_, c)| c.borrow().to_php_bytes())
            .collect(),
        v => vec![v.to_php_bytes()],
    };
    let mut out = subj.to_php_bytes();
    for (i, f) in finds.iter().enumerate() {
        if f.is_empty() {
            continue;
        }
        let r = repls
            .get(i)
            .cloned()
            .unwrap_or_else(|| repls.last().cloned().unwrap_or_default());
        out = if ci {
            breplace_ci(&out, f, &r)
        } else {
            breplace(&out, f, &r)
        };
    }
    out
}

fn php_substr(s: &[u8], start: i64, len: Option<i64>) -> Option<Vec<u8>> {
    let n = s.len() as i64;
    let start = if start < 0 {
        (n + start).max(0)
    } else {
        start.min(n)
    };
    let len = match len {
        Some(l) => {
            if l < 0 {
                (n - start + l).max(0)
            } else {
                l.min(n - start)
            }
        }
        None => n - start,
    };
    if len < 0 {
        return None;
    }
    Some(s[start as usize..(start + len) as usize].to_vec())
}

/// Lossily decode bytes to a char vector — the mb_* layer operates on
/// characters (UTF-8); invalid input collapses to U+FFFD like PHP's
/// substitute-character behavior.
fn mb_chars(s: &[u8]) -> Vec<char> {
    String::from_utf8_lossy(s).chars().collect()
}

fn char_find(h: &[char], n: &[char], from: usize) -> Option<usize> {
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    (from..=h.len() - n.len()).find(|&i| h[i..i + n.len()] == n[..])
}

fn char_rfind(h: &[char], n: &[char], from: usize) -> Option<usize> {
    if n.is_empty() || n.len() > h.len() {
        return None;
    }
    (0..=h.len() - n.len())
        .rev()
        .find(|&i| i >= from && h[i..i + n.len()] == n[..])
}

fn mb_ci(v: &[char]) -> Vec<char> {
    v.iter().flat_map(|c| c.to_lowercase()).collect()
}

fn mb_join(chars: &[char]) -> String {
    chars.iter().collect()
}

fn mb_substr(s: &str, start: i64, len: Option<i64>) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;
    let start = if start < 0 {
        (n + start).max(0)
    } else {
        start.min(n)
    };
    let len = match len {
        Some(l) => {
            if l < 0 {
                (n - start + l).max(0)
            } else {
                l.min(n - start)
            }
        }
        None => n - start,
    };
    if len < 0 {
        return None;
    }
    Some(
        chars[start as usize..(start + len) as usize]
            .iter()
            .collect(),
    )
}

fn substr_replace(s: &[u8], r: &[u8], start: i64, len: Option<i64>) -> Vec<u8> {
    let n = s.len() as i64;
    let start = if start < 0 {
        (n + start).max(0)
    } else {
        start.min(n)
    };
    let end = match len {
        Some(l) => {
            if l < 0 {
                (n + l).max(start)
            } else {
                (start + l).min(n)
            }
        }
        None => n,
    };
    let mut out = Vec::with_capacity(s.len() + r.len());
    out.extend_from_slice(&s[..start as usize]);
    out.extend_from_slice(r);
    out.extend_from_slice(&s[end as usize..]);
    out
}

fn str_pad(s: &[u8], len: usize, pad: &[u8], ty: i64) -> Vec<u8> {
    if s.len() >= len || pad.is_empty() {
        return s.to_vec();
    }
    let need = len - s.len();
    let mk = |n: usize| -> Vec<u8> { pad.repeat(n / pad.len() + 1)[..n].to_vec() };
    let mut out = Vec::new();
    match ty {
        0 => {
            out.extend_from_slice(&mk(need));
            out.extend_from_slice(s);
        }
        1 => {
            out.extend_from_slice(s);
            out.extend_from_slice(&mk(need));
        }
        2 => {
            let l = need / 2;
            let r = need - l;
            out.extend_from_slice(&mk(l));
            out.extend_from_slice(s);
            out.extend_from_slice(&mk(r));
        }
        _ => {
            out.extend_from_slice(s);
            out.extend_from_slice(&mk(need));
        }
    }
    out
}

fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn similar_text(a: &str, b: &str) -> (usize, f64) {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut best = 0usize;
    for i in 0..a.len() {
        for j in 0..b.len() {
            let mut k = 0;
            while i + k < a.len() && j + k < b.len() && a[i + k] == b[j + k] {
                k += 1;
            }
            if k > best {
                best = k;
            }
        }
    }
    (best, 0.0)
}

fn soundex(s: &str) -> String {
    let code = |c: char| -> Option<char> {
        match c.to_ascii_uppercase() {
            'B' | 'F' | 'P' | 'V' => Some('1'),
            'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => Some('2'),
            'D' | 'T' => Some('3'),
            'L' => Some('4'),
            'M' | 'N' => Some('5'),
            'R' => Some('6'),
            _ => None,
        }
    };
    let mut chars = s.chars().filter(|c| c.is_ascii_alphabetic());
    let first = match chars.next() {
        Some(c) => c.to_ascii_uppercase(),
        None => return String::new(),
    };
    let mut out = String::new();
    out.push(first);
    let mut prev = code(first);
    for c in chars {
        let cur = code(c);
        match cur {
            Some(d) => {
                if Some(d) != prev {
                    out.push(d);
                }
                prev = Some(d);
            }
            None => {
                prev = None;
            }
        }
    }
    out.truncate(4);
    while out.len() < 4 {
        out.push('0');
    }
    out
}

fn rot13(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='m' | 'A'..='M' => ((c as u8) + 13) as char,
            'n'..='z' | 'N'..='Z' => ((c as u8) - 13) as char,
            _ => c,
        })
        .collect()
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn strtr_map(s: &[u8], pairs: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let mut matched = false;
        for (from, to) in pairs {
            if !from.is_empty() && s[i..].starts_with(from) {
                out.extend_from_slice(to);
                i += from.len();
                matched = true;
                break;
            }
        }
        if !matched {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

fn strtr_chars(s: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    s.iter()
        .map(|&c| {
            from.iter()
                .position(|&f| f == c)
                .and_then(|i| to.get(i).copied())
                .unwrap_or(c)
        })
        .collect()
}

// ---------- arrays ----------

fn sort_array(
    it: &mut Interp,
    arr: &mut PhpArray,
    name: &str,
    cb_arg: Option<&Cell>,
) -> Result<(), PhpError> {
    match name {
        "sort" | "rsort" | "natsort" | "natcasesort" => {
            arr.entries
                .sort_by(|(_, a), (_, b)| compare(&a.borrow(), &b.borrow()));
            if name == "rsort" {
                arr.entries.reverse();
            }
            // renumber
            let mut i = 0;
            for (k, _) in arr.entries.iter_mut() {
                *k = ArrKey::Int(i);
                i += 1;
            }
            arr.next = i;
        }
        "asort" | "arsort" => {
            arr.entries
                .sort_by(|(_, a), (_, b)| compare(&a.borrow(), &b.borrow()));
            if name == "arsort" {
                arr.entries.reverse();
            }
        }
        "ksort" | "krsort" => {
            arr.entries.retain(|(k, _)| !matches!(k, ArrKey::Tomb));
            arr.entries.sort_by(|(a, _), (b, _)| match (a, b) {
                (ArrKey::Int(x), ArrKey::Int(y)) => x.cmp(y),
                (ArrKey::Str(x), ArrKey::Str(y)) => x.cmp(y),
                (ArrKey::Int(_), ArrKey::Str(_)) => std::cmp::Ordering::Less,
                (ArrKey::Str(_), ArrKey::Int(_)) => std::cmp::Ordering::Greater,
                _ => std::cmp::Ordering::Equal,
            });
            if name == "krsort" {
                arr.entries.reverse();
            }
        }
        "usort" | "uasort" | "uksort" => {
            if let Some(cbc) = cb_arg {
                let cb = cbc.borrow().clone();
                // insertion-sort-ish via comparisons through callback
                let mut sorted = arr.entries.clone();
                // simple bubble for callback correctness (test arrays are small)
                let mut swapped = true;
                while swapped {
                    swapped = false;
                    for i in 0..sorted.len().saturating_sub(1) {
                        let (ka, ca) = sorted[i].clone();
                        let (kb, cbb) = sorted[i + 1].clone();
                        let args = match name {
                            "uksort" => vec![
                                cell(match ka {
                                    ArrKey::Int(i) => Value::Int(i),
                                    ArrKey::Str(s) => Value::str(s.to_string()),
                                    ArrKey::Tomb => Value::Null,
                                }),
                                cell(match kb {
                                    ArrKey::Int(i) => Value::Int(i),
                                    ArrKey::Str(s) => Value::str(s.to_string()),
                                    ArrKey::Tomb => Value::Null,
                                }),
                            ],
                            _ => vec![ca.clone(), cbb.clone()],
                        };
                        let r = it.call_value(&cb, crate::interp::CallArgs::positional(args))?;
                        if r.to_int() > 0 {
                            sorted.swap(i, i + 1);
                            swapped = true;
                        }
                    }
                }
                arr.entries = sorted;
                if name == "usort" {
                    let mut i = 0;
                    for (k, _) in arr.entries.iter_mut() {
                        *k = ArrKey::Int(i);
                        i += 1;
                    }
                    arr.next = i;
                }
            }
        }
        "shuffle" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let mut x = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1);
            for i in (1..arr.entries.len()).rev() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let j = (x as usize) % (i + 1);
                arr.entries.swap(i, j);
            }
            let mut i = 0;
            for (k, _) in arr.entries.iter_mut() {
                *k = ArrKey::Int(i);
                i += 1;
            }
            arr.next = i;
        }
        _ => {}
    }
    Ok(())
}

// ---------- serialize/json ----------

fn serialize(it: &mut Interp, v: &Value) -> String {
    match v {
        Value::Null => "N;".into(),
        Value::Bool(b) => format!("b:{};", *b as i32),
        Value::Int(i) => format!("i:{};", i),
        Value::Float(f) => format!("d:{};", crate::value::format_float_repr(*f)),
        Value::Str(s) => format!("s:{}:\"{}\";", s.len(), crate::value::lossy(&s)),
        Value::Array(a) => {
            let a = a.borrow();
            let mut s = format!("a:{}:{{", a.len());
            for (k, c) in a.iter() {
                s.push_str(&serialize(
                    it,
                    &match k {
                        ArrKey::Int(i) => Value::Int(*i),
                        ArrKey::Str(st) => Value::str(st.to_string()),
                        ArrKey::Tomb => Value::Null,
                    },
                ));
                s.push_str(&serialize(it, &c.borrow()));
            }
            s.push('}');
            s
        }
        Value::Object(o) => {
            // Serializable implementors serialize as C:...{payload}
            // where the payload is whatever ->serialize() returns.
            if it.obj_implements(o, "serializable") {
                if let Ok(payload) =
                    it.method_invoke(o.clone(), "serialize", crate::interp::CallArgs::empty())
                {
                    let Value::Str(pb) = &payload else {
                        return "N;".into();
                    };
                    let p = crate::value::lossy(pb);
                    return format!(
                        "C:{}:\"{}\":{}:{{{}}}",
                        o.borrow().class.name().len(),
                        o.borrow().class.name(),
                        p.len(),
                        p
                    );
                }
            }
            let ob = o.borrow();
            let mut body = String::new();
            let mut n = 0;
            for name in &ob.prop_order {
                if let Some(c) = ob.props.get(name) {
                    body.push_str(&serialize(it, &Value::str(name.clone())));
                    body.push_str(&serialize(it, &c.borrow()));
                    n += 1;
                }
            }
            format!(
                "O:{}:\"{}\":{}:{{{}}}",
                ob.class.name().len(),
                ob.class.name(),
                n,
                body
            )
        }
        _ => "N;".into(),
    }
}

fn unserialize(it: &mut Interp, s: &str, pos: &mut usize) -> Result<Value, ()> {
    let b = s.as_bytes();
    let take_until = |pos: &mut usize, ch: u8| -> Result<String, ()> {
        let start = *pos;
        while *pos < b.len() && b[*pos] != ch {
            *pos += 1;
        }
        if *pos >= b.len() {
            return Err(());
        }
        let s = String::from_utf8_lossy(&b[start..*pos]).into_owned();
        *pos += 1;
        Ok(s)
    };
    match b.get(*pos) {
        Some(b'N') => {
            *pos += 2;
            Ok(Value::Null)
        }
        Some(b'b') => {
            *pos += 2;
            let n = take_until(pos, b';')?;
            Ok(Value::Bool(n == "1"))
        }
        Some(b'i') => {
            *pos += 2;
            let n = take_until(pos, b';')?;
            Ok(Value::Int(n.parse().map_err(|_| ())?))
        }
        Some(b'd') => {
            *pos += 2;
            let n = take_until(pos, b';')?;
            match n.as_str() {
                "NAN" => Ok(Value::Float(f64::NAN)),
                "INF" => Ok(Value::Float(f64::INFINITY)),
                "-INF" => Ok(Value::Float(f64::NEG_INFINITY)),
                _ => Ok(Value::Float(n.parse().map_err(|_| ())?)),
            }
        }
        Some(b's') => {
            *pos += 2;
            let len: usize = take_until(pos, b':')?.parse().map_err(|_| ())?;
            *pos += 1; // opening quote
            let st = String::from_utf8_lossy(&b[*pos..*pos + len]).into_owned();
            *pos += len + 2; // closing quote + ;
            Ok(Value::str(st))
        }
        Some(b'a') => {
            *pos += 2;
            let n: usize = take_until(pos, b':')?.parse().map_err(|_| ())?;
            *pos += 1; // {
            let mut arr = PhpArray::new();
            for _ in 0..n {
                let k = unserialize(it, s, pos)?;
                let v = unserialize(it, s, pos)?;
                arr.set(to_key(&k), v);
            }
            *pos += 1; // }
            Ok(Value::Array(Rc::new(RefCell::new(arr))))
        }
        Some(b'C') => {
            // C:<clen>:"<class>":<plen>:{<payload>} — a Serializable
            // payload; instantiate without ctor and call ->unserialize().
            *pos += 2;
            let clen: usize = take_until(pos, b':')?.parse().map_err(|_| ())?;
            *pos += 1; // opening quote
            if *pos + clen > b.len() {
                return Err(());
            }
            let cname = String::from_utf8_lossy(&b[*pos..*pos + clen]).into_owned();
            *pos += clen;
            *pos += 1; // closing quote
            *pos += 1; // :
            let plen: usize = take_until(pos, b':')?.parse().map_err(|_| ())?;
            *pos += 1; // {
            if *pos + plen > b.len() {
                return Err(());
            }
            let payload = String::from_utf8_lossy(&b[*pos..*pos + plen]).into_owned();
            *pos += plen;
            *pos += 1; // }
            let obj = match it.instantiate(&cname.to_lowercase(), &[]) {
                Ok(Value::Object(o)) => o,
                _ => return Err(()),
            };
            let _ = it.method_invoke(
                obj.clone(),
                "unserialize",
                crate::interp::CallArgs::positional(vec![cell(Value::str(payload))]),
            );
            Ok(Value::Object(obj))
        }
        Some(b'O') => {
            // O:<clen>:"<class>":<n>:{<pairs>}
            *pos += 2;
            let clen: usize = take_until(pos, b':')?.parse().map_err(|_| ())?;
            *pos += 1; // opening quote
            if *pos + clen > b.len() {
                return Err(());
            }
            let cname = String::from_utf8_lossy(&b[*pos..*pos + clen]).into_owned();
            *pos += clen;
            *pos += 1; // closing quote
            *pos += 1; // :
            let n: usize = take_until(pos, b':')?.parse().map_err(|_| ())?;
            *pos += 1; // {
            let obj = match it.instantiate(&cname.to_lowercase(), &[]) {
                Ok(Value::Object(o)) => o,
                _ => return Err(()),
            };
            for _ in 0..n {
                let k = unserialize(it, s, pos)?;
                let Value::Str(ks) = k else { return Err(()) };
                let plain = ks
                    .strip_prefix(&[0u8][..])
                    .and_then(|r| r.split(|b| *b == 0).nth(1))
                    .unwrap_or(ks.as_ref());
                // Virtual hooked props have no backing to fill — zend
                // aborts the whole unserialize, reporting the offset
                // right after the property name (unserialize.phpt).
                if it.unserial_prop_virtual(&obj, &crate::value::lossy(&plain)) {
                    let _ = it.warn_pub(&format!(
                        "unserialize(): Cannot unserialize value for virtual property {}::${}",
                        cname,
                        crate::value::lossy(&plain)
                    ));
                    let _ = it.warn_pub(&format!(
                        "unserialize(): Error at offset {} of {} bytes",
                        pos,
                        s.len()
                    ));
                    return Err(());
                }
                let v = unserialize(it, s, pos)?;
                let mut ob = obj.borrow_mut();
                let key = crate::value::lossy(&ks).into_owned();
                if !ob.prop_order.contains(&key) {
                    ob.prop_order.push(key.clone());
                }
                ob.props.insert(key, cell(v));
            }
            *pos += 1; // }
            Ok(Value::Object(obj))
        }
        _ => Err(()),
    }
}

fn json_encode(_it: &mut Interp, v: &Value, flags: i64) -> Result<String, ()> {
    Ok(match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}.0", *f as i64)
            } else {
                crate::value::format_float(*f)
            }
        }
        Value::Str(s) => json_str(&crate::value::lossy(&s), flags),
        Value::Array(a) => {
            let a = a.borrow();
            let is_list = a
                .entries
                .iter()
                .enumerate()
                .all(|(i, (k, _))| matches!(k, ArrKey::Int(x) if *x == i as i64));
            if is_list {
                let parts: Vec<String> = a
                    .entries
                    .iter()
                    .map(|(_, c)| json_encode(_it, &c.borrow(), flags).unwrap_or("null".into()))
                    .collect();
                format!("[{}]", parts.join(","))
            } else {
                let parts: Vec<String> = a
                    .entries
                    .iter()
                    .map(|(k, c)| {
                        format!(
                            "{}:{}",
                            json_str(&key_str(k), flags),
                            json_encode(_it, &c.borrow(), flags).unwrap_or("null".into())
                        )
                    })
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
        }
        Value::Object(o) => {
            // JsonSerializable::jsonSerialize() wins over the raw
            // public-property view (gh16725).
            if _it
                .find_method_in(&o.borrow().class, "jsonserialize")
                .is_some()
            {
                let v = _it
                    .method_invoke(o.clone(), "jsonSerialize", crate::interp::CallArgs::empty())
                    .unwrap_or(Value::Null);
                return json_encode(_it, &v, flags);
            }
            // Public props only; hooked props serialize their `get`
            // value (property_hooks/dump, oss-fuzz-382922236).
            let entries = _it.object_serial_entries(o);
            let mut parts: Vec<String> = Vec::new();
            for (out, slot, decl) in entries {
                let public = decl
                    .as_ref()
                    .map(|(p, _)| p.visibility == crate::ast::Visibility::Public)
                    .unwrap_or(true);
                if !public {
                    continue;
                }
                let v = match &decl {
                    Some((p, dcls)) => _it.serial_entry_value(o, p, dcls, &slot),
                    None => o.borrow().props.get(&slot).map(|c| c.borrow().clone()),
                };
                if let Some(v) = v {
                    parts.push(format!(
                        "{}:{}",
                        json_str(&out, flags),
                        json_encode(_it, &v, flags).unwrap_or("null".into())
                    ));
                }
            }
            format!("{{{}}}", parts.join(","))
        }
        _ => "null".into(),
    })
}

fn json_str(s: &str, flags: i64) -> String {
    const HEX_TAG: i64 = 1;
    const HEX_AMP: i64 = 2;
    const HEX_APOS: i64 = 4;
    const HEX_QUOT: i64 = 8;
    const UNESCAPED_SLASHES: i64 = 64;
    const UNESCAPED_UNICODE: i64 = 256;
    // HEX-flag escapes use %04X; default unicode escapes %04x (PHP quirk).
    let mut out = String::from("\"");
    for c in s.chars() {
        let u = c as u32;
        match c {
            '"' if flags & HEX_QUOT != 0 => out.push_str(&format!("\\u{:04X}", u)),
            '"' => out.push_str("\\\""),
            '\'' if flags & HEX_APOS != 0 => out.push_str(&format!("\\u{:04X}", u)),
            '<' | '>' if flags & HEX_TAG != 0 => out.push_str(&format!("\\u{:04X}", u)),
            '&' if flags & HEX_AMP != 0 => out.push_str(&format!("\\u{:04X}", u)),
            '\\' => out.push_str("\\\\"),
            '/' if flags & UNESCAPED_SLASHES == 0 => out.push_str("\\/"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            _ if u < 0x20 => out.push_str(&format!("\\u{:04x}", u)),
            _ if u > 0x7f && flags & UNESCAPED_UNICODE == 0 => {
                if u > 0xffff {
                    let x = u - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (x >> 10),
                        0xdc00 + (x & 0x3ff)
                    ));
                } else {
                    out.push_str(&format!("\\u{:04x}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_decode(it: &mut Interp, s: &str, assoc: bool) -> Result<Value, ()> {
    let b = s.as_bytes();
    let mut pos = 0;
    let v = json_value(it, b, &mut pos, assoc)?;
    Ok(v)
}

fn json_ws(b: &[u8], pos: &mut usize) {
    while *pos < b.len() && (b[*pos] as char).is_ascii_whitespace() {
        *pos += 1;
    }
}

fn json_value(it: &mut Interp, b: &[u8], pos: &mut usize, assoc: bool) -> Result<Value, ()> {
    json_ws(b, pos);
    match b.get(*pos) {
        Some(b'n') => {
            *pos += 4;
            Ok(Value::Null)
        }
        Some(b't') => {
            *pos += 4;
            Ok(Value::Bool(true))
        }
        Some(b'f') => {
            *pos += 5;
            Ok(Value::Bool(false))
        }
        Some(b'"') => {
            *pos += 1;
            let mut s = String::new();
            while *pos < b.len() && b[*pos] != b'"' {
                if b[*pos] == b'\\' && *pos + 1 < b.len() {
                    *pos += 1;
                    match b[*pos] {
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        b'r' => s.push('\r'),
                        b'b' => s.push('\u{8}'),
                        b'f' => s.push('\u{c}'),
                        b'u' if *pos + 4 < b.len() => {
                            let h = std::str::from_utf8(&b[*pos + 1..*pos + 5]).map_err(|_| ())?;
                            let cp = u32::from_str_radix(h, 16).map_err(|_| ())?;
                            s.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                            *pos += 4;
                        }
                        c => s.push(c as char),
                    }
                    *pos += 1;
                } else {
                    // UTF-8 pass-through
                    let ch_len = utf8_len(b[*pos]);
                    s.push_str(&String::from_utf8_lossy(&b[*pos..*pos + ch_len]));
                    *pos += ch_len;
                }
            }
            *pos += 1;
            Ok(Value::str(s))
        }
        Some(b'[') => {
            *pos += 1;
            let mut a = PhpArray::new();
            json_ws(b, pos);
            if b.get(*pos) == Some(&b']') {
                *pos += 1;
                return Ok(Value::Array(Rc::new(RefCell::new(a))));
            }
            loop {
                let v = json_value(it, b, pos, assoc)?;
                a.push(v);
                json_ws(b, pos);
                match b.get(*pos) {
                    Some(b',') => {
                        *pos += 1;
                    }
                    Some(b']') => {
                        *pos += 1;
                        break;
                    }
                    _ => return Err(()),
                }
            }
            Ok(Value::Array(Rc::new(RefCell::new(a))))
        }
        Some(b'{') => {
            *pos += 1;
            let mut a = PhpArray::new();
            json_ws(b, pos);
            if b.get(*pos) == Some(&b'}') {
                *pos += 1;
                return Ok(if assoc {
                    Value::Array(Rc::new(RefCell::new(a)))
                } else {
                    let Some(cls) = it.lookup_class("stdclass") else {
                        return Ok(Value::Null);
                    };
                    Value::Object(it.alloc_obj(PhpObject {
                        class: cls,
                        props: HashMap::new(),
                        prop_order: Vec::new(),
                        id: 0,
                        internal: None,
                        unset_props: std::collections::HashSet::new(),
                    }))
                });
            }
            loop {
                json_ws(b, pos);
                let k = match json_value(it, b, pos, true)? {
                    Value::Str(s) => crate::value::lossy(&s).into_owned(),
                    _ => return Err(()),
                };
                json_ws(b, pos);
                if b.get(*pos) != Some(&b':') {
                    return Err(());
                }
                *pos += 1;
                let v = json_value(it, b, pos, assoc)?;
                a.set(ArrKey::Str(k.into()), v);
                json_ws(b, pos);
                match b.get(*pos) {
                    Some(b',') => {
                        *pos += 1;
                    }
                    Some(b'}') => {
                        *pos += 1;
                        break;
                    }
                    _ => return Err(()),
                }
            }
            Ok(if assoc {
                Value::Array(Rc::new(RefCell::new(a)))
            } else {
                // non-assoc decodes to stdClass
                let mut props = HashMap::new();
                let mut order = Vec::new();
                for (k, c) in a.iter() {
                    let ks = match k {
                        ArrKey::Str(st) => st.to_string(),
                        ArrKey::Int(i) => i.to_string(),
                        ArrKey::Tomb => continue,
                    };
                    order.push(ks.clone());
                    props.insert(ks, c.clone());
                }
                let Some(cls) = it.lookup_class("stdclass") else {
                    return Ok(Value::Null);
                };
                Value::Object(it.alloc_obj(PhpObject {
                    class: cls,
                    props,
                    prop_order: order,
                    id: 0,
                    internal: None,
                    unset_props: std::collections::HashSet::new(),
                }))
            })
        }
        Some(&c) if c == b'-' || c.is_ascii_digit() => {
            let start = *pos;
            while *pos < b.len()
                && matches!(b[*pos], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
            {
                *pos += 1;
            }
            let text = std::str::from_utf8(&b[start..*pos]).map_err(|_| ())?;
            if let Ok(i) = text.parse::<i64>() {
                Ok(Value::Int(i))
            } else {
                Ok(Value::Float(text.parse().map_err(|_| ())?))
            }
        }
        _ => Err(()),
    }
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

// ---------- hashing ----------

fn md5_hex(data: &[u8]) -> String {
    format!("{:x}", md5::compute(data))
}

fn sha1_hex(data: &[u8]) -> String {
    use sha1::Digest;
    let mut h = sha1::Sha1::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

fn crc32(data: &[u8]) -> u32 {
    // IEEE CRC32
    let mut crc = 0xFFFFFFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB88320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let n = chunk.len();
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let v = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(v >> 18) as usize & 63] as char);
        out.push(T[(v >> 12) as usize & 63] as char);
        out.push(if n > 1 {
            T[(v >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if n > 2 {
            T[v as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => continue,
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

pub(crate) fn urlencode(s: &[u8], raw: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for &b in s {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b),
            b'~' if raw => out.push(b'~'),
            b' ' if !raw => out.push(b'+'),
            _ => out.extend_from_slice(format!("%{:02X}", b).as_bytes()),
        }
    }
    out
}

pub(crate) fn urldecode(s: &[u8], raw: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'+' if !raw => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < s.len() + 1 => {
                if let Ok(v) =
                    u8::from_str_radix(&crate::value::lossy(&s[i + 1..(i + 3).min(s.len())]), 16)
                {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(s[i]);
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

// ---------- regex ----------

/// arg0 as a pattern string, honoring __toString objects and raising
/// PHP's TypeError for composite patterns on string-only functions.
fn preg_pattern_str(it: &mut Interp, fname: &str, args: &[Cell]) -> Result<Vec<u8>, PhpError> {
    match arg(args, 0) {
        Value::Array(_) => err(
            "TypeError",
            format!(
                "{}(): Argument #1 ($pattern) must be of type string, array given",
                fname
            ),
        ),
        Value::Object(o) => {
            if o.borrow().class.decl.find_method("__tostring").is_some() {
                let r = it.method_invoke(
                    o.clone(),
                    "__toString",
                    crate::interp::CallArgs::positional(vec![]),
                )?;
                Ok(r.to_php_bytes())
            } else {
                err(
                    "TypeError",
                    format!(
                        "{}(): Argument #1 ($pattern) must be of type string, {} given",
                        fname,
                        o.borrow().class.name()
                    ),
                )
            }
        }
        v => Ok(v.to_php_bytes()),
    }
}

/// Array subject element to string — same rules as pattern elements.
fn subj_elem_str(it: &mut Interp, c: &Cell) -> Result<Vec<u8>, PhpError> {
    pat_elem_str(it, c)
}

/// Pattern element to string: Array warns, Object without __toString is Error.
fn pat_elem_str(it: &mut Interp, c: &Cell) -> Result<Vec<u8>, PhpError> {
    let v = c.borrow().clone();
    match &v {
        Value::Array(_) => {
            it.warn_pub("Array to string conversion")?;
            Ok(b"Array".to_vec())
        }
        Value::Object(o) => {
            if o.borrow().class.decl.find_method("__tostring").is_some() {
                let r = it.method_invoke(
                    o.clone(),
                    "__toString",
                    crate::interp::CallArgs::positional(vec![]),
                )?;
                Ok(r.to_php_bytes())
            } else {
                err(
                    "Error",
                    format!(
                        "Object of class {} could not be converted to string",
                        o.borrow().class.name()
                    ),
                )
            }
        }
        _ => Ok(v.to_php_bytes()),
    }
}

/// Is `v` callable-shaped enough for preg callback params?
fn preg_callable_ok(it: &Interp, v: &Value) -> bool {
    match v {
        Value::Callable(_) => true,
        Value::Str(s) => {
            let n = crate::value::lossy(&s);
            let n = n.trim_start_matches('\\');
            if n.contains("::") {
                true
            } else {
                it.functions.contains_key(&n.to_lowercase()) || is_builtin(&n.to_lowercase())
            }
        }
        Value::Array(a) => {
            let a = a.borrow();
            let o = a.get(&ArrKey::Int(0));
            let m = a.get(&ArrKey::Int(1)).map(|v| v.to_php_string());
            match (o, m) {
                (Some(Value::Object(ob)), Some(m)) => {
                    let d = &ob.borrow().class.decl;
                    d.find_method(&m.to_lowercase()).is_some() || d.find_method("__call").is_some()
                }
                (Some(Value::Str(cn)), Some(m)) => it
                    .lookup_class(&crate::value::lossy(&cn))
                    .map(|cl| cl.decl.find_method(&m.to_lowercase()).is_some())
                    .unwrap_or(false),
                _ => false,
            }
        }
        Value::Object(o) => {
            let d = &o.borrow().class.decl;
            d.find_method("__invoke").is_some() || d.find_method("__call").is_some()
        }
        _ => false,
    }
}

/// Does a `/pat/flags` pattern carry the `u` (UTF-8) modifier?
fn pat_is_utf(pat: &[u8]) -> bool {
    if pat.len() < 2 {
        return false;
    }
    let delim = pat[0];
    let close = match delim {
        b'(' => b')',
        b'{' => b'}',
        b'[' => b']',
        b'<' => b'>',
        _ => delim,
    };
    match pat.iter().rposition(|&c| c == close) {
        Some(end) if end > 0 => pat[end + 1..].contains(&b'u'),
        _ => false,
    }
}

fn preg_dispatch(it: &mut Interp, name: &str, args: &[Cell]) -> Result<Value, PhpError> {
    if !matches!(
        name,
        "preg_last_error" | "preg_last_error_msg" | "preg_quote"
    ) {
        it.last_preg_error = 0;
    }
    let regex_err = |it: &mut Interp, fname: &str, e: &String| -> Result<(), PhpError> {
        it.last_preg_error = 1;
        it.warn_pub(&format!("{}(): {}", fname, e))?;
        Ok(())
    };
    match name {
        "preg_quote" => {
            let s = arg_bs(it, args, 0);
            let extra = arg_bs(it, args, 1);
            let mut out = Vec::with_capacity(s.len());
            for &c in &s {
                if c == 0 {
                    out.extend_from_slice(b"\\000");
                } else {
                    if b".\\+*?[^]$(){}=!<>|:-#/".contains(&c) || extra.contains(&c) {
                        out.push(b'\\');
                    }
                    out.push(c);
                }
            }
            Ok(Value::bytes(out))
        }
        "preg_last_error" => Ok(Value::Int(it.last_preg_error)),
        "preg_last_error_msg" => Ok(Value::str(
            match it.last_preg_error {
                0 => "No error",
                1 => "Internal error",
                2 => "Backtrack limit exhausted",
                3 => "Recursion limit exhausted",
                4 => "Malformed UTF-8 characters, possibly incorrectly encoded",
                5 => "The offset did not correspond to the beginning of a valid UTF-8 code point",
                6 => "JIT stack limit exhausted",
                _ => "Unknown error",
            }
            .to_string(),
        )),
        "preg_match" | "preg_match_all" => {
            let pat = preg_pattern_str(it, name, args)?;
            let subj = arg_bs(it, args, 1);
            let re = match php_regex(&pat) {
                Ok(r) => r,
                Err(e) => {
                    regex_err(it, name, &e)?;
                    return Ok(Value::Bool(false));
                }
            };
            let all = name == "preg_match_all";
            let flags = arg(args, 3).to_int();
            let valid: i64 = if all { 1 | 2 | 256 | 512 } else { 256 | 512 };
            if flags & !valid != 0 {
                return err(
                    "ValueError",
                    format!("{}(): Argument #4 ($flags) must be a PREG_* constant", name),
                );
            }
            let off = arg(args, 4).to_int();
            // PHP (GH-16189): only INT_MIN is rejected — every other
            // negative offset is len-relative and clamps to 0.
            if off == i64::MIN {
                return err(
                    "ValueError",
                    format!(
                        "{}(): Argument #5 ($offset) must be greater than {}",
                        name,
                        i64::MIN
                    ),
                );
            }
            let offset = if off < 0 {
                (subj.len() as i64 + off).max(0) as usize
            } else {
                off as usize
            };
            if offset > subj.len() {
                return err(
                    "ValueError",
                    format!(
                        "{}(): Argument #5 ($offset) must be contained in subject",
                        name
                    ),
                );
            }
            if pat_is_utf(&pat) && !utf8_boundary(&subj, offset) {
                it.last_preg_error = 5;
                return Ok(Value::Bool(false));
            }
            let hay: &[u8] = subj.get(offset..).unwrap_or(&[]);
            let mut matches_arr = PhpArray::new();
            let mut count = 0i64;
            let (caps, rc) = re.caps(hay, it);
            if rc != 0 {
                it.last_preg_error = preg_rc_err(rc);
                return Ok(Value::Bool(false));
            }
            // A capture group -> PHP value honoring OFFSET_CAPTURE and
            // UNMATCHED_AS_NULL; offsets are absolute on the subject.
            let entry = |span: Option<(usize, usize)>| -> Value {
                match (span, flags & 256 != 0) {
                    (Some((a, b)), true) => {
                        let mut pair = PhpArray::new();
                        pair.push(
                            hay.get(a..b)
                                .map(|x| Value::bytes(x.to_vec()))
                                .unwrap_or(Value::str("")),
                        );
                        pair.push(Value::Int((a + offset) as i64));
                        Value::Array(Rc::new(RefCell::new(pair)))
                    }
                    (Some((a, b)), false) => hay
                        .get(a..b)
                        .map(|x| Value::bytes(x.to_vec()))
                        .unwrap_or(Value::str("")),
                    (None, true) => {
                        let mut pair = PhpArray::new();
                        pair.push(if flags & 512 != 0 {
                            Value::Null
                        } else {
                            Value::str("")
                        });
                        pair.push(Value::Int(-1));
                        Value::Array(Rc::new(RefCell::new(pair)))
                    }
                    (None, false) if flags & 512 != 0 => Value::Null,
                    (None, false) => Value::str(""),
                }
            };
            if all {
                let ngroups = re.captures_len();
                if flags & 2 != 0 {
                    // PREG_SET_ORDER: one row per match.
                    for cap in &caps {
                        count += 1;
                        let mut row = PhpArray::new();
                        // trailing unmatched groups are omitted; interior
                        // unmatched groups stay as "" / null.
                        let last = if flags & 512 != 0 {
                            ngroups - 1
                        } else {
                            cap.spans
                                .iter()
                                .rposition(|sp| sp.is_some())
                                .map(|i| i + 1)
                                .unwrap_or(0)
                        };
                        for g in 0..last.min(cap.spans.len().max(ngroups)) {
                            let span = cap.spans.get(g).copied().flatten();
                            let v = entry(span);
                            // named alias precedes its numeric key
                            if let Some(n) = re.group_name(g) {
                                row.set(ArrKey::Str(n.into()), v.clone());
                            }
                            row.set(ArrKey::Int(g as i64), v);
                        }
                        if let Some(m) = &cap.mark {
                            row.set(ArrKey::Str("MARK".into()), Value::str(m.clone()));
                        }
                        matches_arr.push(Value::Array(Rc::new(RefCell::new(row))));
                    }
                } else {
                    // PREG_PATTERN_ORDER (default): one column per group.
                    let mut groups: Vec<PhpArray> = (0..ngroups).map(|_| PhpArray::new()).collect();
                    for cap in &caps {
                        count += 1;
                        for (g, grp) in groups.iter_mut().enumerate().take(ngroups) {
                            grp.push(entry(cap.spans.get(g).copied().flatten()));
                        }
                    }
                    for (g, grp) in groups.into_iter().enumerate() {
                        if let Some(n) = re.group_name(g) {
                            matches_arr.set(
                                ArrKey::Str(n.into()),
                                Value::Array(Rc::new(RefCell::new(grp.clone()))),
                            );
                        }
                        matches_arr.push(Value::Array(Rc::new(RefCell::new(grp))));
                    }
                    if caps.iter().any(|c| c.mark.is_some()) {
                        let mut marks = PhpArray::new();
                        for (i, c) in caps.iter().enumerate() {
                            if let Some(m) = &c.mark {
                                marks.set(ArrKey::Int(i as i64), Value::str(m.clone()));
                            }
                        }
                        matches_arr.set(
                            ArrKey::Str("MARK".into()),
                            Value::Array(Rc::new(RefCell::new(marks))),
                        );
                    }
                }
            } else if let Some(cap) = caps.into_iter().next() {
                count = 1;
                let last = if flags & 512 != 0 {
                    cap.spans.len()
                } else {
                    cap.spans
                        .iter()
                        .rposition(|sp| sp.is_some())
                        .map(|i| i + 1)
                        .unwrap_or(0)
                };
                for g in 0..last.min(cap.spans.len()) {
                    let span = cap.spans.get(g).copied().flatten();
                    let v = entry(span);
                    if let Some(n) = re.group_name(g) {
                        matches_arr.set(ArrKey::Str(n.into()), v.clone());
                    }
                    matches_arr.set(ArrKey::Int(g as i64), v);
                }
                if let Some(m) = &cap.mark {
                    matches_arr.set(ArrKey::Str("MARK".into()), Value::str(m.clone()));
                }
            }
            if let Some(c) = args.get(2) {
                *c.borrow_mut() = Value::Array(Rc::new(RefCell::new(matches_arr)));
            }
            Ok(Value::Int(count))
        }
        "preg_replace"
        | "preg_replace_callback"
        | "preg_replace_callback_array"
        | "preg_filter" => {
            let cb_arr = name == "preg_replace_callback_array";
            let cb_family = name.contains("callback");
            let limit = arg(args, if cb_arr { 2 } else { 3 }).to_int();
            let flags = arg(
                args,
                if cb_family {
                    if cb_arr {
                        4
                    } else {
                        5
                    }
                } else {
                    5
                },
            )
            .to_int();
            // (pattern, callback-or-replacement) pairs
            let pairs: Vec<(Vec<u8>, Value)> = if name == "preg_replace_callback" {
                // arg #2 is the single callback (may itself be an array
                // like [$obj, 'method'] — not a replacement list)
                let cb = arg(args, 1);
                match arg(args, 0) {
                    Value::Array(a) => {
                        let mut ps = Vec::new();
                        for (_, c) in a.borrow().iter() {
                            ps.push((pat_elem_str(it, c)?, cb.clone()));
                        }
                        ps
                    }
                    v => vec![(v.to_php_bytes(), cb)],
                }
            } else if name == "preg_replace_callback_array" {
                match arg(args, 0) {
                    Value::Array(a) => {
                        if a.borrow()
                            .entries
                            .iter()
                            .any(|(k, _)| !matches!(k, ArrKey::Str(_)))
                        {
                            return err(
                                "TypeError",
                                "preg_replace_callback_array(): Argument #1 ($pattern) must contain only string patterns as keys",
                            );
                        }
                        a.borrow()
                            .entries
                            .iter()
                            .map(|(k, c)| (key_str(k).into_bytes(), c.borrow().clone()))
                            .collect()
                    }
                    _ => Vec::new(),
                }
            } else {
                if let Value::Object(o) = arg(args, 1) {
                    if matches!(name, "preg_replace" | "preg_filter")
                        && o.borrow().class.decl.find_method("__tostring").is_none()
                    {
                        return err(
                        "TypeError",
                        format!(
                            "{}(): Argument #2 ($replacement) must be of type array|string, {} given",
                            name,
                            o.borrow().class.name()
                        ),
                    );
                    }
                }
                let repl_scalar = !matches!(arg(args, if cb_arr { 0 } else { 1 }), Value::Array(_));
                let repls: Vec<Value> = match arg(args, if cb_arr { 0 } else { 1 }) {
                    Value::Array(a) => {
                        // array replacement requires an array pattern
                        if !matches!(arg(args, 0), Value::Array(_))
                            && matches!(name, "preg_replace" | "preg_filter")
                        {
                            return err(
                                "TypeError",
                                format!(
                                    "{}(): Argument #1 ($pattern) must be of type array when argument #2 ($replacement) is an array, string given",
                                    name
                                ),
                            );
                        }
                        a.borrow().iter().map(|(_, c)| c.borrow().clone()).collect()
                    }
                    v => vec![v],
                };
                match arg(args, 0) {
                    Value::Array(a) => {
                        let mut ps = Vec::new();
                        for (i, (_, c)) in a.borrow().iter().enumerate() {
                            // A scalar replacement broadcasts to every
                            // pattern; an array replacement is strictly
                            // positional (missing entries mean "").
                            ps.push((
                                pat_elem_str(it, c)?,
                                repls
                                    .get(i)
                                    .or(if repl_scalar { repls.first() } else { None })
                                    .cloned()
                                    .unwrap_or_else(|| Value::str("")),
                            ));
                        }
                        ps
                    }
                    v => vec![(
                        v.to_php_bytes(),
                        repls.into_iter().next().unwrap_or(Value::Null),
                    )],
                }
            };
            let subj_arg = if cb_arr { 1 } else { 2 };
            for (p, cb) in &pairs {
                if cb_family && !preg_callable_ok(it, cb) {
                    return err(
                        "TypeError",
                        format!(
                            "{}(): Argument #{} ($pattern) must contain only valid callbacks",
                            name, 1
                        ),
                    );
                }
                if let Value::Object(o) = arg(args, 0) {
                    if o.borrow().class.decl.find_method("__tostring").is_none() {
                        let tn = o.borrow().class.name().to_string();
                        let want = if name == "preg_replace" || name == "preg_filter" {
                            "array|string"
                        } else {
                            "string"
                        };
                        return err(
                            "TypeError",
                            format!(
                                "{}(): Argument #1 ($pattern) must be of type {}, {} given",
                                name, want, tn
                            ),
                        );
                    }
                }
                let _ = p;
            }
            if let Value::Object(o) = arg(args, subj_arg) {
                if o.borrow().class.decl.find_method("__tostring").is_none() {
                    return err(
                        "TypeError",
                        format!(
                            "{}(): Argument #{} ($subject) must be of type array|string, {} given",
                            name,
                            subj_arg + 1,
                            o.borrow().class.name()
                        ),
                    );
                }
            }
            let mut subjects: Vec<(ArrKey, Vec<u8>)> = Vec::new();
            match arg(args, subj_arg) {
                Value::Array(a) => {
                    for (k, c) in a.borrow().iter() {
                        subjects.push((k.clone(), subj_elem_str(it, c)?));
                    }
                }
                v => subjects.push((ArrKey::Int(0), v.to_php_bytes())),
            };
            let mut total = 0i64;
            let mut results: Vec<(ArrKey, Option<Vec<u8>>)> = Vec::new();
            for (k, subj) in subjects {
                let mut cur = subj;
                let mut matched = false;
                for (p, cb) in &pairs {
                    let re = match php_regex(p) {
                        Ok(r) => r,
                        Err(e) => {
                            regex_err(it, name, &e)?;
                            return Ok(Value::Null);
                        }
                    };
                    if name == "preg_replace_callback" || name == "preg_replace_callback_array" {
                        let mut out: Vec<u8> = Vec::new();
                        let mut last = 0usize;
                        let mut n = 0i64;
                        let (caps, rc) = re.caps(&cur, it);
                        if rc != 0 {
                            it.last_preg_error = preg_rc_err(rc);
                            return Ok(Value::Null);
                        }
                        for cap in &caps {
                            if limit > 0 && n >= limit {
                                break;
                            }
                            let Some(Some((ms, me))) = cap.spans.first() else {
                                continue;
                            };
                            n += 1;
                            matched = true;
                            out.extend_from_slice(cur.get(last..*ms).unwrap_or(&[]));
                            let mut group_arr = PhpArray::new();
                            let glast = if flags & 512 != 0 {
                                cap.spans.len()
                            } else {
                                cap.spans
                                    .iter()
                                    .rposition(|sp| sp.is_some())
                                    .map(|i| i + 1)
                                    .unwrap_or(0)
                            };
                            for g in 0..glast {
                                let span = cap.spans.get(g).copied().flatten();
                                let v = match (span, flags & 256 != 0) {
                                    (Some((a, b)), true) => {
                                        let mut pair = PhpArray::new();
                                        pair.push(
                                            cur.get(a..b)
                                                .map(|x| Value::bytes(x.to_vec()))
                                                .unwrap_or(Value::str("")),
                                        );
                                        pair.push(Value::Int(a as i64));
                                        Value::Array(Rc::new(RefCell::new(pair)))
                                    }
                                    (Some((a, b)), false) => cur
                                        .get(a..b)
                                        .map(|x| Value::bytes(x.to_vec()))
                                        .unwrap_or(Value::str("")),
                                    (None, true) => {
                                        let mut pair = PhpArray::new();
                                        pair.push(if flags & 512 != 0 {
                                            Value::Null
                                        } else {
                                            Value::str("")
                                        });
                                        pair.push(Value::Int(-1));
                                        Value::Array(Rc::new(RefCell::new(pair)))
                                    }
                                    (None, false) if flags & 512 != 0 => Value::Null,
                                    (None, false) => Value::str(""),
                                };
                                if let Some(n) = re.group_name(g) {
                                    group_arr.set(ArrKey::Str(n.into()), v.clone());
                                }
                                group_arr.push(v);
                            }
                            if let Some(m) = &cap.mark {
                                group_arr.set(ArrKey::Str("MARK".into()), Value::str(m.clone()));
                            }
                            let r = it.call_value(
                                cb,
                                crate::interp::CallArgs::positional(vec![cell(Value::Array(
                                    Rc::new(RefCell::new(group_arr)),
                                ))]),
                            )?;
                            out.extend_from_slice(&r.to_php_bytes());
                            last = *me;
                        }
                        out.extend_from_slice(&cur[last..]);
                        total += n;
                        cur = out;
                    } else {
                        let repl = cb.to_php_bytes();
                        let mut n = 0i64;
                        let src = cur.clone();
                        let mut out: Vec<u8> = Vec::new();
                        let mut last = 0usize;
                        let (caps, rc) = re.caps(&src, it);
                        if rc != 0 {
                            it.last_preg_error = preg_rc_err(rc);
                            return Ok(Value::Null);
                        }
                        for cap in &caps {
                            if limit > 0 && n >= limit {
                                break;
                            }
                            let Some(Some((a, b))) = cap.spans.first() else {
                                continue;
                            };
                            n += 1;
                            matched = true;
                            out.extend_from_slice(src.get(last..*a).unwrap_or(&[]));
                            let mut r = repl.clone();
                            for g in (0..cap.spans.len()).rev() {
                                let m: &[u8] = cap
                                    .spans
                                    .get(g)
                                    .copied()
                                    .flatten()
                                    .and_then(|(ga, gb)| src.get(ga..gb))
                                    .unwrap_or(&[]);
                                r = breplace(&r, format!("${{{}}}", g).as_bytes(), m);
                                r = breplace(&r, format!("${}", g).as_bytes(), m);
                                r = breplace(&r, format!("\\{}", g).as_bytes(), m);
                            }
                            // backrefs to groups that don't exist expand to ""
                            let mut cleaned: Vec<u8> = Vec::with_capacity(r.len());
                            let rb = r.as_slice();
                            let mut i = 0;
                            while i < rb.len() {
                                // `\\` in a replacement is ONE literal
                                // backslash (PHP's escape), not two.
                                if rb[i] == b'\\' && rb.get(i + 1) == Some(&b'\\') {
                                    cleaned.push(b'\\');
                                    i += 2;
                                    continue;
                                }
                                if rb[i] == b'$' || rb[i] == b'\\' {
                                    let (digits_len, end) = if rb[i] == b'$'
                                        && i + 1 < rb.len()
                                        && rb[i + 1] == b'{'
                                    {
                                        let mut e = i + 2;
                                        while e < rb.len() && rb[e].is_ascii_digit() {
                                            e += 1;
                                        }
                                        if e < rb.len() && rb[e] == b'}' && e > i + 2 {
                                            (e - (i + 2), e + 1)
                                        } else {
                                            (0, i + 1)
                                        }
                                    } else {
                                        let mut e = i + 1;
                                        while e < rb.len() && rb[e].is_ascii_digit() && e < i + 3 {
                                            e += 1;
                                        }
                                        (e - (i + 1), e)
                                    };
                                    if digits_len > 0 {
                                        i = end;
                                        continue;
                                    }
                                }
                                cleaned.push(rb[i]);
                                i += 1;
                            }
                            out.extend_from_slice(&cleaned);
                            last = *b;
                        }
                        out.extend_from_slice(&src[last..]);
                        total += n;
                        cur = out;
                    }
                }
                let keep = name != "preg_filter" || matched;
                results.push((k, if keep { Some(cur) } else { None }));
            }
            if let Some(c) = args.get(if cb_arr { 3 } else { 4 }) {
                *c.borrow_mut() = Value::Int(total);
            }
            if matches!(arg(args, subj_arg), Value::Array(_)) {
                let mut out = PhpArray::new();
                for (k, v) in results {
                    if let Some(v) = v {
                        out.set(k, Value::bytes(v));
                    }
                }
                // preg_filter on an all-miss array yields an empty array.
                Ok(Value::Array(Rc::new(RefCell::new(out))))
            } else {
                Ok(match results.into_iter().next() {
                    Some((_, Some(v))) => Value::bytes(v),
                    _ => Value::Null,
                })
            }
        }
        "preg_split" => {
            let pat = preg_pattern_str(it, name, args)?;
            let subj = arg_bs(it, args, 1);
            let flags = arg(args, 3).to_int();
            let limit = arg(args, 2).to_int();
            let re = match php_regex(&pat) {
                Ok(r) => r,
                Err(e) => {
                    regex_err(it, name, &e)?;
                    return Ok(Value::Bool(false));
                }
            };
            let mut out = PhpArray::new();
            let mut last = 0usize;
            let (caps, rc) = re.caps(&subj, it);
            if rc != 0 {
                it.last_preg_error = preg_rc_err(rc);
                return Ok(Value::Bool(false));
            }
            for cap in &caps {
                let Some(Some((a, b))) = cap.spans.first() else {
                    continue;
                };
                // limit reached: emit the rest as one piece and stop
                if limit > 0 && out.entries.len() as i64 >= limit - 1 {
                    out.push(Value::bytes(subj.get(last..).unwrap_or(&[]).to_vec()));
                    return Ok(Value::Array(Rc::new(RefCell::new(out))));
                }
                let piece = subj.get(last..*a).unwrap_or(&[]);
                if flags & 1 == 0 || !piece.is_empty() {
                    if flags & 4 != 0 {
                        let mut pair = PhpArray::new();
                        pair.push(Value::bytes(piece.to_vec()));
                        pair.push(Value::Int(last as i64));
                        out.push(Value::Array(Rc::new(RefCell::new(pair))));
                    } else {
                        out.push(Value::bytes(piece.to_vec()));
                    }
                }
                if flags & 2 != 0 {
                    for (ga, gb) in cap.spans.iter().skip(1).flatten() {
                        if flags & 1 == 0 || ga != gb {
                            let g = subj.get(*ga..*gb).unwrap_or(&[]);
                            if flags & 4 != 0 {
                                let mut pair = PhpArray::new();
                                pair.push(Value::bytes(g.to_vec()));
                                pair.push(Value::Int(*ga as i64));
                                out.push(Value::Array(Rc::new(RefCell::new(pair))));
                            } else {
                                out.push(Value::bytes(g.to_vec()));
                            }
                        }
                    }
                }
                last = *b;
            }
            let tail = subj.get(last..).unwrap_or(&[]);
            if flags & 1 == 0 || !tail.is_empty() {
                if flags & 4 != 0 {
                    let mut pair = PhpArray::new();
                    pair.push(Value::bytes(tail.to_vec()));
                    pair.push(Value::Int(last as i64));
                    out.push(Value::Array(Rc::new(RefCell::new(pair))));
                } else {
                    out.push(Value::bytes(tail.to_vec()));
                }
            }
            Ok(Value::Array(Rc::new(RefCell::new(out))))
        }
        "preg_grep" => {
            let pat = preg_pattern_str(it, name, args)?;
            let flags = arg(args, 2).to_int();
            let re = match php_regex(&pat) {
                Ok(r) => r,
                Err(e) => {
                    regex_err(it, name, &e)?;
                    return Ok(Value::Bool(false));
                }
            };
            let mut out = PhpArray::new();
            if let Value::Array(a) = arg(args, 1) {
                for (k, c) in a.borrow().iter() {
                    let v = c.borrow().clone();
                    let s: Vec<u8> = match &v {
                        Value::Array(_) => {
                            it.warn_pub("Array to string conversion")?;
                            b"Array".to_vec()
                        }
                        _ => v.to_php_bytes(),
                    };
                    let (caps, rc) = re.caps(&s, it);
                    if rc != 0 {
                        it.last_preg_error = preg_rc_err(rc);
                        return Ok(Value::Bool(false));
                    }
                    if !caps.is_empty() != (flags & 1 != 0) {
                        if Rc::strong_count(c) > 1 {
                            out.set_cell(k.clone(), c.clone());
                        } else {
                            out.set(k.clone(), v);
                        }
                    }
                }
            }
            Ok(Value::Array(Rc::new(RefCell::new(out))))
        }
        _ => Ok(Value::Null),
    }
}

/// Match-group byte spans; `spans[0]` is the whole match. `mark` is
/// the `(*MARK:x)` verb payload when one fired (PCRE2 only).
struct PhpCap {
    spans: Vec<Option<(usize, usize)>>,
    mark: Option<String>,
}

/// A PHP pattern compiled by PCRE2 — PHP's own engine.
enum PhpRe {
    Pcre(crate::pcre::PcreRe),
}

impl PhpRe {
    fn captures_len(&self) -> usize {
        match self {
            PhpRe::Pcre(r) => r.captures_len(),
        }
    }
    fn group_name(&self, g: usize) -> Option<String> {
        match self {
            PhpRe::Pcre(r) => r.group_name(g),
        }
    }
    /// All matches in order, normalized to group byte spans, plus the
    /// PCRE2 error code that stopped the scan (0 = clean).
    fn caps(&self, s: &[u8], it: &Interp) -> (Vec<PhpCap>, i32) {
        match self {
            PhpRe::Pcre(r) => {
                // PHP validates the subject under /u: a bad-UTF8 input
                // reports PREG_BAD_UTF8_ERROR (-36 = UTF8_ERR1 class).
                if r.utf8 && std::str::from_utf8(s).is_err() {
                    return (Vec::new(), -36);
                }
                let (v, e) = r.match_all(
                    s,
                    it.ini_int("pcre.backtrack_limit", 1_000_000).max(0) as u32,
                    it.ini_int("pcre.recursion_limit", 100_000).max(0) as u32,
                );
                (
                    v.into_iter()
                        .map(|m| PhpCap {
                            spans: m.spans,
                            mark: m.mark,
                        })
                        .collect(),
                    e,
                )
            }
        }
    }
}

/// Translate a PHP `/pat/flags` regex to a `PhpRe`. Err is the full
/// warning text PHP emits (`Compilation failed: ...`, `Unknown
/// modifier 'x'`, delimiter problems) — callers prefix `fname(): `.
fn php_regex(pat: &[u8]) -> Result<PhpRe, String> {
    // PHP skips leading whitespace before the delimiter; a pattern of
    // only whitespace is the same "Empty regular expression" error.
    let ws = pat.iter().take_while(|c| c.is_ascii_whitespace()).count();
    let b = &pat[ws..];
    if b.is_empty() {
        return Err("Empty regular expression".into());
    }
    let delim = b[0] as char;
    if delim.is_ascii_alphanumeric() || delim == '\\' || delim == '\0' {
        return Err("Delimiter must not be alphanumeric, backslash, or NUL byte".into());
    }
    let close = match delim {
        '(' => ')',
        '{' => '}',
        '[' => ']',
        '<' => '>',
        _ => delim,
    };
    if b.len() < 2 {
        return Err(if close != delim {
            format!("No ending matching delimiter '{}' found", close)
        } else {
            format!("No ending delimiter '{}' found", delim)
        });
    }
    let end = pat.iter().rposition(|&c| c == close as u8).ok_or_else(|| {
        if close != delim {
            format!("No ending matching delimiter '{}' found", close)
        } else {
            format!("No ending delimiter '{}' found", close)
        }
    })?;
    if end == 0 {
        return Err(if close != delim {
            format!("No ending matching delimiter '{}' found", close)
        } else {
            format!("No ending delimiter '{}' found", delim)
        });
    }
    let flags = &pat[end + 1..];
    let body = &pat[1..end];
    let mut wrapped = String::new();
    let mut anchor = false;
    let mut opts: u32 = 0;
    let mut extra_opts: u32 = 0;
    let mut need_pcre = false;
    for f in flags.iter().map(|&b| b as char) {
        match f {
            'i' => wrapped.push_str("(?i)"),
            'm' => wrapped.push_str("(?m)"),
            's' => wrapped.push_str("(?s)"),
            'x' => wrapped.push_str("(?x)"),
            'A' => anchor = true,
            'D' => {
                opts |= pcre2_sys::PCRE2_DOLLAR_ENDONLY;
                need_pcre = true;
            }
            'J' => {
                opts |= pcre2_sys::PCRE2_DUPNAMES;
                need_pcre = true;
            }
            'U' => {
                opts |= pcre2_sys::PCRE2_UNGREEDY;
                need_pcre = true;
            }
            'u' => {
                opts |= pcre2_sys::PCRE2_UTF
                    | pcre2_sys::PCRE2_UCP
                    // PHP compiles with MATCH_INVALID_UTF so bad-UTF8
                    // subjects report PREG_BAD_UTF8_ERROR (10.34+ default
                    // silently tolerates them otherwise).
                    | pcre2_sys::PCRE2_MATCH_INVALID_UTF;
                need_pcre = true;
            }
            'r' => {
                extra_opts |= pcre2_sys::PCRE2_EXTRA_CASELESS_RESTRICT;
                need_pcre = true;
            }
            'n' => {
                opts |= pcre2_sys::PCRE2_NO_AUTO_CAPTURE;
                need_pcre = true;
            }
            // S = study, X = extra strictness, whitespace tolerated.
            'S' | 'X' | ' ' | '\t' | '\n' | '\r' => {}
            _ => {
                return Err(if f == '\0' {
                    "NUL byte is not a valid modifier".into()
                } else {
                    format!("Unknown modifier '{}'", f)
                });
            }
        }
    }
    // PHP compiles patterns with PCRE2; use it for anything the `regex`
    // crate can't express rather than trying to emulate backtracking.
    let pcre_only = [
        "(*", "\\K", "\\G", "(?<", "(?R", "(?-", "(?+", "(?|", "(?", "(?#",
    ];
    // An empty *body* is legal (`//` matches the empty string); PHP only
    // rejects a zero-length pattern string, checked above.
    let mut has_backref = false;
    let bb = body;
    for i in 0..bb.len().saturating_sub(1) {
        if bb[i] == b'\\' && bb[i + 1].is_ascii_digit() {
            has_backref = true;
            break;
        }
    }
    let _ = (need_pcre, has_backref, pcre_only);
    let mut src: Vec<u8> = Vec::new();
    if anchor {
        src.extend_from_slice(b"\\A");
    }
    src.extend_from_slice(wrapped.as_bytes());
    src.extend_from_slice(body);
    crate::pcre::compile(&src, opts, extra_opts)
        .map(PhpRe::Pcre)
        .map_err(|e| format!("Compilation failed: {}", e))
}

// ---------- filesystem ----------

fn read_stream(path: &str) -> Result<Vec<u8>, std::io::Error> {
    if path.starts_with("php://stdin") {
        return Ok(Vec::new());
    }
    std::fs::read(path)
}

/// `data:[mediatype][;base64],payload` wrapper — returns the decoded
/// payload bytes, or None when the path isn't a data: URI.
/// `data:` and `data://` forms both work (scalar_* tests).
fn parse_data_uri(path: &str) -> Option<Vec<u8>> {
    let rest = path
        .strip_prefix("data:")
        .or_else(|| path.strip_prefix("data://"))?;
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let comma = rest.find(',')?;
    let (meta, payload) = (&rest[..comma], &rest[comma + 1..]);
    if meta.split(';').any(|m| m.eq_ignore_ascii_case("base64")) {
        base64_decode(payload)
    } else {
        Some(percent_decode(payload.as_bytes()))
    }
}

/// URL percent-decoding for data: URIs (`%41` -> 'A', '+' stays literal).
fn percent_decode(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = (b[i + 1] as char).to_digit(16);
            let l = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn mode_flags(mode: &str) -> (bool, bool) {
    let m = mode.chars().next().unwrap_or('r');
    let plus = mode.contains('+');
    match m {
        'r' => (true, plus),
        'w' | 'a' | 'x' | 'c' => (plus, true),
        _ => (true, true),
    }
}

fn fopen(path: &str, mode: &str) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    let m = mode.chars().next().unwrap_or('r');
    let plus = mode.contains('+');
    let mut o = OpenOptions::new();
    match m {
        'r' => {
            o.read(true);
            if plus {
                o.write(true);
            }
        }
        'w' => {
            o.write(true).create(true).truncate(true);
            if plus {
                o.read(true);
            }
        }
        'a' => {
            o.append(true).create(true);
            if plus {
                o.read(true);
            }
        }
        'x' => {
            o.write(true).create_new(true);
            if plus {
                o.read(true);
            }
        }
        'c' => {
            o.write(true).create(true);
            if plus {
                o.read(true);
            }
        }
        _ => {
            o.read(true);
        }
    }
    o.open(path)
}

fn write_resource(it: &mut Interp, c: Option<&Cell>, data: &str) -> Result<(), PhpError> {
    use std::io::{Seek, Write};
    match c.map(|c| c.borrow().clone()) {
        Some(Value::Resource(r)) => {
            let mut rb = r.borrow_mut();
            match &mut *rb {
                PhpResource::Stdio { which, .. } => match *which {
                    1 => {
                        it.out.extend_from_slice(data.as_bytes());
                        Ok(())
                    }
                    2 => {
                        it.err_buf.push_str(data);
                        Ok(())
                    }
                    _ => Err(PhpError::fatal("not writable", 0)),
                },
                PhpResource::File {
                    file, pos, write, ..
                } => {
                    if !*write {
                        return Err(PhpError::fatal("not writable", 0));
                    }
                    let _ = file.seek(std::io::SeekFrom::Start(*pos));
                    file.write_all(data.as_bytes())
                        .map_err(|e| PhpError::fatal(e.to_string(), 0))?;
                    *pos += data.len() as u64;
                    Ok(())
                }
                _ => Err(PhpError::fatal("bad resource", 0)),
            }
        }
        _ => Err(PhpError::fatal("not a resource", 0)),
    }
}

fn read_resource(c: Option<&Cell>, n: usize) -> Result<Vec<u8>, PhpError> {
    use std::io::{Read, Seek};
    match c.map(|c| c.borrow().clone()) {
        Some(Value::Resource(r)) => {
            let mut rb = r.borrow_mut();
            match &mut *rb {
                PhpResource::Stdio { .. } => Ok(Vec::new()),
                PhpResource::Input { body, pos, .. } => {
                    let avail = body.len().saturating_sub(*pos as usize);
                    let take = avail.min(n);
                    let out = body[*pos as usize..*pos as usize + take].to_vec();
                    *pos += take as u64;
                    Ok(out)
                }
                PhpResource::File {
                    file,
                    pos,
                    read,
                    eof,
                    ..
                } => {
                    if !*read || *eof {
                        return Ok(Vec::new());
                    }
                    let _ = file.seek(std::io::SeekFrom::Start(*pos));
                    let mut buf = vec![0u8; n];
                    match file.read(&mut buf) {
                        Ok(got) => {
                            buf.truncate(got);
                            *pos += got as u64;
                            if got < n {
                                *eof = true;
                            }
                            Ok(buf)
                        }
                        Err(e) => Err(PhpError::fatal(e.to_string(), 0)),
                    }
                }
                _ => Ok(Vec::new()),
            }
        }
        _ => Err(PhpError::fatal("not a resource", 0)),
    }
}

fn read_line_resource(c: Option<&Cell>) -> Result<Vec<u8>, PhpError> {
    use std::io::{Read, Seek};
    match c.map(|c| c.borrow().clone()) {
        Some(Value::Resource(r)) => {
            let mut rb = r.borrow_mut();
            match &mut *rb {
                PhpResource::Stdio { .. } => Ok(Vec::new()),
                PhpResource::Input { body, pos, .. } => {
                    let start = *pos as usize;
                    if start >= body.len() {
                        Ok(Vec::new())
                    } else {
                        let nl = body[start..]
                            .iter()
                            .position(|b| *b == b'\n')
                            .map(|o| start + o + 1)
                            .unwrap_or(body.len());
                        let out = body[start..nl].to_vec();
                        *pos = nl as u64;
                        Ok(out)
                    }
                }
                PhpResource::File {
                    file,
                    pos,
                    read,
                    eof,
                    ..
                } => {
                    if !*read || *eof {
                        return Ok(Vec::new());
                    }
                    let _ = file.seek(std::io::SeekFrom::Start(*pos));
                    let mut out = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        match file.read(&mut byte) {
                            Ok(0) => {
                                *eof = true;
                                break;
                            }
                            Ok(_) => {
                                out.push(byte[0]);
                                *pos += 1;
                                if byte[0] == b'\n' {
                                    break;
                                }
                            }
                            Err(e) => return Err(PhpError::fatal(e.to_string(), 0)),
                        }
                    }
                    Ok(out)
                }
                _ => Ok(Vec::new()),
            }
        }
        _ => Err(PhpError::fatal("not a resource", 0)),
    }
}

fn glob_to_regex(pat: &str) -> regex::Regex {
    let mut r = String::from("^");
    for c in pat.chars() {
        match c {
            '*' => r.push_str(".*"),
            '?' => r.push('.'),
            c => r.push_str(&regex::escape(&c.to_string())),
        }
    }
    r.push('$');
    regex::Regex::new(&r).unwrap_or_else(|_| regex::Regex::new("a^").unwrap())
}

fn base_conv(mut v: i128, to: u32) -> String {
    if v == 0 {
        return "0".into();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    let neg = v < 0;
    if neg {
        v = -v;
    }
    while v > 0 {
        out.push(digits[(v % to as i128) as usize] as char);
        v /= to as i128;
    }
    if neg {
        out.push('-');
    }
    out.iter().rev().collect()
}

fn version_cmp(a: &str, b: &str) -> i64 {
    let pa: Vec<i64> = a
        .split(['.', '-', '_'])
        .map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect();
    let pb: Vec<i64> = b
        .split(['.', '-', '_'])
        .map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect();
    for i in 0..pa.len().max(pb.len()) {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        if x != y {
            return if x < y { -1 } else { 1 };
        }
    }
    0
}

fn date_format(fmt: &str, ts: i64) -> String {
    // minimal strftime-ish for the common tokens
    let days = ts.div_euclid(86400);
    let secs = ts.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    let h = secs / 3600;
    let mi = (secs % 3600) / 60;
    let s = secs % 60;
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
            continue;
        }
        match c {
            'Y' => out.push_str(&format!("{:04}", y)),
            'y' => out.push_str(&format!("{:02}", y % 100)),
            'm' => out.push_str(&format!("{:02}", m)),
            'n' => out.push_str(&format!("{}", m)),
            'd' => out.push_str(&format!("{:02}", d)),
            'j' => out.push_str(&format!("{}", d)),
            'H' => out.push_str(&format!("{:02}", h)),
            'G' => out.push_str(&format!("{}", h)),
            'h' => out.push_str(&format!("{:02}", if h % 12 == 0 { 12 } else { h % 12 })),
            'g' => out.push_str(&format!("{}", if h % 12 == 0 { 12 } else { h % 12 })),
            'i' => out.push_str(&format!("{:02}", mi)),
            's' => out.push_str(&format!("{:02}", s)),
            'a' => out.push_str(if h < 12 { "am" } else { "pm" }),
            'A' => out.push_str(if h < 12 { "AM" } else { "PM" }),
            'D' => out.push_str(
                ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
                    [(days + 4).rem_euclid(7) as usize],
            ),
            'l' => out.push_str(
                [
                    "Sunday",
                    "Monday",
                    "Tuesday",
                    "Wednesday",
                    "Thursday",
                    "Friday",
                    "Saturday",
                ][(days + 4).rem_euclid(7) as usize],
            ),
            'M' => out.push_str(
                [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec",
                ][(m - 1) as usize],
            ),
            'F' => out.push_str(
                [
                    "January",
                    "February",
                    "March",
                    "April",
                    "May",
                    "June",
                    "July",
                    "August",
                    "September",
                    "October",
                    "November",
                    "December",
                ][(m - 1) as usize],
            ),
            'U' => out.push_str(&ts.to_string()),
            'e' | 'T' => out.push_str("UTC"),
            'O' => out.push_str("+0000"),
            'P' => out.push_str("+00:00"),
            'c' => out.push_str(&format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
                y, m, d, h, mi, s
            )),
            'r' => out.push_str(&format!(
                "{}, {:02} {} {:04} {:02}:{:02}:{:02} +0000",
                ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
                    [(days + 4).rem_euclid(7) as usize],
                d,
                [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec"
                ][(m - 1) as usize],
                y,
                h,
                mi,
                s
            )),
            'w' => out.push_str(&(days + 4).rem_euclid(7).to_string()),
            'z' => {
                let jan1 = days_from_civil(y, 1, 1);
                out.push_str(&(days - jan1).to_string());
            }
            't' => {
                let next = days_from_civil(
                    if m == 12 { y + 1 } else { y },
                    if m == 12 { 1 } else { m + 1 },
                    1,
                );
                let cur = days_from_civil(y, m, 1);
                out.push_str(&(next - cur).to_string());
            }
            'L' => out.push_str(if is_leap(y) { "1" } else { "0" }),
            'S' => out.push_str(match d % 10 {
                1 if d != 11 => "st",
                2 if d != 12 => "nd",
                3 if d != 13 => "rd",
                _ => "th",
            }),
            'N' => out.push_str(&((days + 3).rem_euclid(7) + 1).to_string()),
            'W' => {
                let jan1 = days_from_civil(y, 1, 1);
                let week = (days - jan1) / 7 + 1;
                out.push_str(&format!("{:02}", week));
            }
            c => out.push(c),
        }
    }
    out
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Howard Hinnant's civil calendar math.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn cast_to(v: &Value, t: &str) -> Value {
    match t {
        "int" | "integer" => Value::Int(v.to_int()),
        "float" | "double" | "real" => Value::Float(v.to_float()),
        "string" => Value::str(v.to_php_string()),
        "bool" | "boolean" => Value::Bool(v.is_truthy()),
        "null" | "unset" => Value::Null,
        "array" => match v {
            Value::Array(_) => v.clone(),
            Value::Null => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
            _ => {
                let mut a = PhpArray::new();
                a.push(v.clone());
                Value::Array(Rc::new(RefCell::new(a)))
            }
        },
        "object" => v.clone(),
        _ => v.clone(),
    }
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
        "substr" => bp!(("string", Req), ("start", Req), ("length", Null)),
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
        "sprintf" | "printf" | "vsprintf" | "fprintf" => {
            bp!(("format", Req), ("...", Var))
        }
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
        "getenv" => bp!(("name", Req), ("local_only", Bool(false))),
        "header" => bp!(
            ("header", Req),
            ("replace", Bool(true)),
            ("response_code", Int(0))
        ),
        "setcookie" => bp!(("...", Var)),
        _ => return None,
    })
}

/// Apply one filter_var() pass to a scalar. Returns the filtered value or
/// the failure sentinel (false; null under FILTER_NULL_ON_FAILURE — except
/// VALIDATE_BOOL whose failure polarity is reversed).
fn filter_var_one(
    it: &mut Interp,
    v: &Value,
    filter: i64,
    opts: &Value,
    flags: i64,
) -> Result<Value, PhpError> {
    const NULL_ON_FAILURE: i64 = 134217728;
    const IPV4: i64 = 1048576;
    const IPV6: i64 = 2097152;
    let fail = |_: bool| {
        if flags & NULL_ON_FAILURE != 0 {
            Value::Null
        } else {
            Value::Bool(false)
        }
    };
    let opt = |key: &str| -> Value {
        if let Value::Array(a) = opts {
            a.borrow()
                .get_cell(&to_key(&Value::str(key)))
                .map(|c| c.borrow().clone())
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        }
    };
    match filter {
        // FILTER_VALIDATE_INT
        257 => {
            if let Value::Int(_) = v {
                return Ok(v.clone());
            }
            let s = it.to_string_of(v);
            let t = s.trim();
            let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Ok(fail(false));
            }
            match t.parse::<i64>() {
                Ok(n) => {
                    let (min, max) = (opt("min_range"), opt("max_range"));
                    if !matches!(min, Value::Null) && n < min.to_int()
                        || !matches!(max, Value::Null) && n > max.to_int()
                    {
                        return Ok(fail(false));
                    }
                    Ok(Value::Int(n))
                }
                Err(_) => Ok(fail(false)),
            }
        }
        // FILTER_VALIDATE_BOOL
        258 => Ok(match v {
            Value::Bool(b) => Value::Bool(*b),
            _ => match it.to_string_of(v).to_lowercase().as_str() {
                "1" | "true" | "on" | "yes" => Value::Bool(true),
                "0" | "false" | "off" | "no" | "" => Value::Bool(false),
                _ => fail(true),
            },
        }),
        // FILTER_VALIDATE_FLOAT
        259 => {
            if let Value::Float(f) = v {
                return Ok(Value::Float(*f));
            }
            let s = it.to_string_of(v);
            match s.trim().replace(',', "").parse::<f64>() {
                Ok(f) => Ok(Value::Float(f)),
                Err(_) => Ok(fail(false)),
            }
        }
        // FILTER_VALIDATE_REGEXP
        272 => {
            let pat = it.to_string_of(&opt("regexp"));
            let subject = it.to_string_of(v);
            match call(
                it,
                "preg_match",
                &[cell(Value::str(pat)), cell(Value::str(subject))],
            )? {
                Some(Value::Int(1)) => Ok(v.clone()),
                _ => Ok(fail(false)),
            }
        }
        // FILTER_VALIDATE_URL
        273 => {
            let s = it.to_string_of(v);
            let ok = s
                .find("://")
                .map(|i| {
                    let scheme = &s[..i];
                    !scheme.is_empty()
                        && scheme.bytes().all(|b| {
                            b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.'
                        })
                        && !s[i + 3..].is_empty()
                        && !s.bytes().any(|b| b <= b' ')
                })
                .unwrap_or(false);
            Ok(if ok { v.clone() } else { fail(false) })
        }
        // FILTER_VALIDATE_EMAIL
        274 => {
            let s = it.to_string_of(v);
            Ok(if valid_email(&s) {
                v.clone()
            } else {
                fail(false)
            })
        }
        // FILTER_VALIDATE_IP
        275 => {
            let s = it.to_string_of(v);
            let ok = if flags & IPV4 != 0 {
                valid_ipv4(&s)
            } else if flags & IPV6 != 0 {
                valid_ipv6(&s)
            } else {
                valid_ipv4(&s) || valid_ipv6(&s)
            };
            Ok(if ok { v.clone() } else { fail(false) })
        }
        // FILTER_VALIDATE_DOMAIN
        277 => {
            let s = it.to_string_of(v);
            Ok(if valid_domain(&s) {
                v.clone()
            } else {
                fail(false)
            })
        }
        // FILTER_DEFAULT
        516 | 0 => Ok(v.clone()),
        // FILTER_CALLBACK
        1024 => {
            let cb = opt("options");
            it.call_value(
                &cb,
                crate::interp::CallArgs::positional(vec![cell(v.clone())]),
            )
        }
        _ => Ok(fail(false)),
    }
}

fn valid_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            (1..=3).contains(&p.len())
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
                && p.parse::<u32>().map(|n| n <= 255).unwrap_or(false)
        })
}

fn ipv6_groups(half: &str) -> Option<usize> {
    if half.is_empty() {
        return Some(0);
    }
    let mut n = 0usize;
    let segs: Vec<&str> = half.split(':').collect();
    for (i, g) in segs.iter().enumerate() {
        if i + 1 == segs.len() && g.contains('.') {
            if !valid_ipv4(g) {
                return None;
            }
            n += 2;
        } else if g.is_empty() || g.len() > 4 || !g.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        } else {
            n += 1;
        }
    }
    Some(n)
}

fn valid_ipv6(s: &str) -> bool {
    let parts: Vec<&str> = s.split("::").collect();
    match parts.as_slice() {
        [one] => ipv6_groups(one) == Some(8),
        [l, r] => match (ipv6_groups(l), ipv6_groups(r)) {
            (Some(a), Some(b)) => a + b < 8,
            _ => false,
        },
        _ => false,
    }
}

fn valid_domain(s: &str) -> bool {
    if s.len() > 253 {
        return false;
    }
    s.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

fn valid_email(s: &str) -> bool {
    let (local, domain) = match s.split_once('@') {
        Some(p) if !p.1.contains('@') => p,
        _ => return false,
    };
    if local.is_empty() || local.len() > 64 || !valid_domain(domain) {
        return false;
    }
    local
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-/=?^_`{|}~.".contains(&b))
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
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
        "array_merge" | "array_replace" | "array_merge_recursive" => {
            &[("array", "array"), ("arrays", "array")]
        }
        "array_reverse" => &[("array", "array"), ("preserve_keys", "bool")],
        "array_fill" => &[("start_index", "int"), ("count", "int"), ("value", "mixed")],
        "array_fill_keys" => &[("keys", "array"), ("value", "mixed")],
        "array_keys" | "array_values" => &[("array", "array")],
        "array_flip" | "array_unique" | "array_rand" => &[("array", "array")],
        "str_word_count" | "similar_text" => &[("string", "string")],
        "ucwords" | "lcwords" => &[("string", "string"), ("separators", "string")],
        "sprintf" | "printf" | "vsprintf" | "vprintf" => &[("format", "string")],
        "number_format" => &[("num", "float"), ("decimals", "int")],
        "preg_match" | "preg_match_all" => &[("pattern", "string"), ("subject", "string")],
        "preg_replace" | "preg_filter" | "preg_replace_callback" => &[
            ("pattern", "string|array"),
            ("replacement", "string|array|callable"),
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
