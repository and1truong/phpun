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
        "strlen" => Value::Int(arg(args, 0).to_php_string().len() as i64),
        "mb_strlen" => Value::Int(arg(args, 0).to_php_string().chars().count() as i64),
        "strtoupper" | "mb_strtoupper" => Value::str(arg_str(it, args, 0).to_uppercase()),
        "strtolower" | "mb_strtolower" => Value::str(arg_str(it, args, 0).to_lowercase()),
        "ucfirst" => {
            let s = arg_str(it, args, 0);
            let mut c = s.chars();
            Value::str(match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => s,
            })
        }
        "lcfirst" => {
            let s = arg_str(it, args, 0);
            let mut c = s.chars();
            Value::str(match c.next() {
                Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
                None => s,
            })
        }
        "ucwords" => {
            let s = arg_str(it, args, 0);
            let mut out = String::new();
            let mut cap = true;
            for ch in s.chars() {
                if cap && ch.is_alphabetic() {
                    out.extend(ch.to_uppercase());
                    cap = false;
                } else {
                    cap = ch == ' ';
                    out.push(ch);
                }
            }
            Value::str(out)
        }
        "str_repeat" => {
            let s = arg_str(it, args, 0);
            let n = arg(args, 1).to_int().max(0) as usize;
            Value::str(s.repeat(n))
        }
        "strrev" => Value::str(arg_str(it, args, 0).chars().rev().collect::<String>()),
        "str_pad" => {
            let s = arg_str(it, args, 0);
            let len = arg(args, 1).to_int() as usize;
            let pad = if args.len() > 2 {
                arg_str(it, args, 2)
            } else {
                " ".into()
            };
            let ty = arg(args, 3).to_int(); // STR_PAD_RIGHT=1 default
            Value::str(str_pad(&s, len, &pad, ty))
        }
        "str_split" | "mb_str_split" => {
            let s = arg_str(it, args, 0);
            let n = arg(args, 1).to_int().max(1) as usize;
            let mut a = PhpArray::new();
            for chunk in s.as_bytes().chunks(n) {
                a.push(Value::str(String::from_utf8_lossy(chunk).into_owned()));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "str_replace" => {
            let find = arg(args, 0);
            let repl = arg(args, 1);
            let subj = arg(args, 2);
            Value::str(str_replace(&find, &repl, &subj))
        }
        "str_ireplace" => {
            let find = arg(args, 0);
            let repl = arg(args, 1);
            let subj = arg(args, 2);
            Value::str(str_replace_i(&find, &repl, &subj))
        }
        "substr" | "mb_substr" => {
            let s = arg_str(it, args, 0);
            let start = arg(args, 1).to_int();
            let len = if args.len() > 2 {
                Some(arg(args, 2).to_int())
            } else {
                None
            };
            match php_substr(&s, start, len) {
                Some(x) => Value::str(x),
                None => Value::Bool(false),
            }
        }
        "substr_count" => {
            let s = arg_str(it, args, 0);
            let n = arg_str(it, args, 1);
            Value::Int(if n.is_empty() {
                0
            } else {
                s.matches(&n).count()
            } as i64)
        }
        "substr_replace" => {
            let s = arg_str(it, args, 0);
            let r = arg_str(it, args, 1);
            let start = arg(args, 2).to_int();
            let len = if args.len() > 3 {
                Some(arg(args, 3).to_int())
            } else {
                None
            };
            Value::str(substr_replace(&s, &r, start, len))
        }
        "strpos" | "stripos" => {
            let hay = arg_str(it, args, 0);
            let needle = arg_str(it, args, 1);
            let off = arg(args, 2).to_int().max(0) as usize;
            let (hay, needle) = if name == "stripos" {
                (hay.to_lowercase(), needle.to_lowercase())
            } else {
                (hay, needle)
            };
            match hay
                .get(off..)
                .and_then(|h| h.find(&needle))
                .map(|p| p + off)
            {
                Some(p) => Value::Int(p as i64),
                None => Value::Bool(false),
            }
        }
        "strrpos" | "strripos" => {
            let hay = arg_str(it, args, 0);
            let needle = arg_str(it, args, 1);
            let (hay, needle) = if name == "strripos" {
                (hay.to_lowercase(), needle.to_lowercase())
            } else {
                (hay, needle)
            };
            match hay.rfind(&needle) {
                Some(p) => Value::Int(p as i64),
                None => Value::Bool(false),
            }
        }
        "strstr" | "strchr" | "stristr" => {
            let hay = arg_str(it, args, 0);
            let needle = arg_str(it, args, 1);
            let before = arg(args, 2).is_truthy();
            let (hay, needle) = if name == "stristr" {
                (hay.to_lowercase(), needle.to_lowercase())
            } else {
                (hay, needle)
            };
            match hay.find(&needle) {
                Some(p) => {
                    let orig = arg_str(it, args, 0);
                    if before {
                        Value::str(orig[..p].to_string())
                    } else {
                        Value::str(orig[p..].to_string())
                    }
                }
                None => Value::Bool(false),
            }
        }
        "str_contains" => {
            let hay = arg_str(it, args, 0);
            let needle = arg_str(it, args, 1);
            Value::Bool(hay.contains(&needle))
        }
        "str_starts_with" => {
            let hay = arg_str(it, args, 0);
            let needle = arg_str(it, args, 1);
            Value::Bool(hay.starts_with(&needle))
        }
        "str_ends_with" => {
            let hay = arg_str(it, args, 0);
            let needle = arg_str(it, args, 1);
            Value::Bool(hay.ends_with(&needle))
        }
        "trim" => {
            let s = arg_str(it, args, 0);
            let chars = if args.len() > 1 {
                arg_str(it, args, 1)
            } else {
                " \n\r\t\x0b\0".into()
            };
            Value::str(trim_set(&s, &chars, true, true))
        }
        "ltrim" => {
            let s = arg_str(it, args, 0);
            let chars = if args.len() > 1 {
                arg_str(it, args, 1)
            } else {
                " \n\r\t\x0b\0".into()
            };
            Value::str(trim_set(&s, &chars, true, false))
        }
        "rtrim" | "chop" => {
            let s = arg_str(it, args, 0);
            let chars = if args.len() > 1 {
                arg_str(it, args, 1)
            } else {
                " \n\r\t\x0b\0".into()
            };
            Value::str(trim_set(&s, &chars, false, true))
        }
        "explode" => {
            let sep = arg_str(it, args, 0);
            let s = arg_str(it, args, 1);
            let limit = arg(args, 2).to_int();
            let mut a = PhpArray::new();
            if sep.is_empty() {
                return err(
                    "ValueError",
                    "explode(): Argument #1 ($separator) cannot be empty",
                );
            }
            let parts: Vec<String> = if limit > 0 {
                let mut v: Vec<String> = s
                    .split(&sep)
                    .take((limit - 1) as usize)
                    .map(|x| x.to_string())
                    .collect();
                let consumed: usize = v.iter().map(|x| x.len() + sep.len()).sum();
                let rest = s
                    .get(consumed.saturating_sub(sep.len()).min(s.len())..)
                    .unwrap_or("");
                // simpler: re-split correctly
                let all: Vec<&str> = s.split(&sep).collect();
                if all.len() as i64 > limit {
                    let mut head: Vec<String> = all[..limit as usize - 1]
                        .iter()
                        .map(|x| x.to_string())
                        .collect();
                    head.push(all[limit as usize - 1..].join(&sep));
                    v = head;
                    let _ = rest;
                }
                v
            } else {
                s.split(&sep).map(|x| x.to_string()).collect()
            };
            for p in parts {
                a.push(Value::str(p));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "implode" | "join" => {
            let (sep, arr) = if args.len() == 1 {
                (String::new(), arg(args, 0))
            } else {
                (arg(args, 0).to_php_string(), arg(args, 1))
            };
            match arr {
                Value::Array(a) => {
                    let parts: Vec<String> = a
                        .borrow()
                        .entries
                        .iter()
                        .map(|(_, c)| c.borrow().to_php_string())
                        .collect();
                    Value::str(parts.join(&sep))
                }
                _ => Value::str(""),
            }
        }
        "nl2br" => Value::str(arg_str(it, args, 0).replace('\n', "<br />\n")),
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
            let s = arg_str(it, args, 0);
            let mut out = String::new();
            for c in s.chars() {
                if ".\\+*?[^]$()=!<>|:-#{}".contains(c) {
                    out.push('\\');
                }
                out.push(c);
            }
            Value::str(out)
        }
        "addslashes" => {
            let s = arg_str(it, args, 0);
            let mut out = String::new();
            for c in s.chars() {
                match c {
                    '\'' | '"' | '\\' | '\0' => {
                        out.push('\\');
                        out.push(if c == '\0' { '0' } else { c });
                    }
                    _ => out.push(c),
                }
            }
            Value::str(out)
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
        "stripslashes" => {
            let s = arg_str(it, args, 0);
            let mut out = String::new();
            let mut it2 = s.chars();
            while let Some(c) = it2.next() {
                if c == '\\' {
                    match it2.next() {
                        Some('0') => out.push('\0'),
                        Some(other) => out.push(other),
                        None => out.push('\\'),
                    }
                } else {
                    out.push(c);
                }
            }
            Value::str(out)
        }
        "htmlspecialchars" | "htmlentities" => {
            let s = arg_str(it, args, 0);
            Value::str(
                s.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
                    .replace('\'', "&#039;"),
            )
        }
        "htmlspecialchars_decode" | "html_entity_decode" => {
            let s = arg_str(it, args, 0);
            Value::str(
                s.replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&quot;", "\"")
                    .replace("&#039;", "'")
                    .replace("&apos;", "'")
                    .replace("&amp;", "&"),
            )
        }
        "strip_tags" => {
            let s = arg_str(it, args, 0);
            let mut out = String::new();
            let mut in_tag = false;
            for c in s.chars() {
                match c {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    _ if !in_tag => out.push(c),
                    _ => {}
                }
            }
            Value::str(out)
        }
        "ord" => Value::Int(
            arg_str(it, args, 0)
                .as_bytes()
                .first()
                .copied()
                .unwrap_or(0) as i64,
        ),
        "chr" => Value::str(
            String::from_utf8_lossy(&[(arg(args, 0).to_int() & 0xff) as u8]).into_owned(),
        ),
        "bin2hex" => Value::str(hex_encode(arg_str(it, args, 0).as_bytes())),
        "hex2bin" => {
            let s = arg_str(it, args, 0);
            match hex_decode(&s) {
                Some(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
                None => Value::Bool(false),
            }
        }
        "str_rot13" => Value::str(rot13(&arg_str(it, args, 0))),
        "count_chars" => {
            let s = arg_str(it, args, 0);
            let mode = arg(args, 1).to_int();
            match mode {
                0 => {
                    let mut a = PhpArray::new();
                    let mut counts = [0i64; 256];
                    for b in s.bytes() {
                        counts[b as usize] += 1;
                    }
                    for (i, c) in counts.iter().enumerate() {
                        a.set(ArrKey::Int(i as i64), Value::Int(*c));
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
                _ => {
                    let mut counts = [0i64; 256];
                    for b in s.bytes() {
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
            let s = arg_str(it, args, 0);
            let len = if args.len() > 1 {
                arg(args, 1).to_int() as usize
            } else {
                76
            };
            let end = if args.len() > 2 {
                arg_str(it, args, 2)
            } else {
                "\r\n".into()
            };
            let mut out = String::new();
            for c in s.as_bytes().chunks(len.max(1)) {
                out.push_str(&String::from_utf8_lossy(c));
                out.push_str(&end);
            }
            Value::str(out)
        }
        "strtr" => {
            let s = arg_str(it, args, 0);
            match arg(args, 1) {
                Value::Array(m) => {
                    // longest keys first
                    let mut pairs: Vec<(String, String)> = m
                        .borrow()
                        .entries
                        .iter()
                        .map(|(k, c)| (key_str(k), c.borrow().to_php_string()))
                        .collect();
                    pairs.sort_by_key(|p| std::cmp::Reverse(p.0.len()));
                    Value::str(strtr_map(&s, &pairs))
                }
                from => {
                    let to = arg_str(it, args, 2);
                    Value::str(strtr_chars(&s, &from.to_php_string(), &to))
                }
            }
        }
        "wordwrap" => Value::str(arg_str(it, args, 0)), // minimal passthrough
        "sprintf_js" | "vsprintf_js" => Value::Null,

        // ----- arrays -----
        "count" | "sizeof" => match arg(args, 0) {
            Value::Array(a) => Value::Int(a.borrow().len() as i64),
            Value::Object(o) if it.obj_is_a(&o, "Countable") => {
                it.method_invoke(o.clone(), "count", vec![])?
            }
            Value::Null => Value::Int(0),
            v => {
                let _ = v;
                Value::Int(1)
            }
        },
        "array_keys" => match arg(args, 0) {
            Value::Array(a) => {
                let mut out = PhpArray::new();
                for (k, _) in a.borrow().iter() {
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
            if let Value::Array(rc) = &mut *args[0].borrow_mut() {
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
            if let Value::Array(rc) = &mut *args[0].borrow_mut() {
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
            if let Value::Array(rc) = &mut *args[0].borrow_mut() {
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
            if let Value::Array(rc) = &mut *args[0].borrow_mut() {
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
            if let Value::Array(rc) = &mut *args[0].borrow_mut() {
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
                            let r = it.call_value(cb, vec![cell(v.clone())])?;
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
                        let v = it.call_value(&cb, vec![cell(c.borrow().clone())])?;
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
                    let v = it.call_value(&cb, call_args)?;
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
                    acc = it.call_value(&cb, vec![cell(acc), cell(c.borrow().clone())])?;
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
                        vec![cell(v), cell(Value::str(plain)), cell(extra.clone())],
                    )?;
                }
                if walked {
                    return Ok(Some(Value::Bool(true)));
                }
            }
            if let Value::Array(rc) = &mut *args[0].borrow_mut() {
                let cells: Vec<(ArrKey, Cell)> = rc.borrow().iter().cloned().collect();
                for (k, c) in cells {
                    it.call_value(
                        &cb,
                        vec![
                            c.clone(),
                            cell(match k {
                                ArrKey::Int(i) => Value::Int(i),
                                ArrKey::Str(s) => Value::str(s.to_string()),
                                ArrKey::Tomb => Value::Null,
                            }),
                            cell(extra.clone()),
                        ],
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
                    if a.len() == 1 && b.len() == 1 && !a.as_bytes()[0].is_ascii_digit() =>
                {
                    let (mut c, end) = (a.as_bytes()[0] as i64, b.as_bytes()[0] as i64);
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
            if let Value::Array(rc) = &mut *args[0].borrow_mut() {
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
                    let v = it.lookup_var(s).unwrap_or(Value::Null);
                    out.set(ArrKey::Str(s.clone()), v);
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
            Value::Str(s) => it.functions.contains_key(&s.to_lowercase()),
            Value::Array(a) => a.borrow().entries.len() == 2,
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
            if n.contains("::") {
                let cls = n.split_once("::").map(|(c, _)| c).unwrap_or_default();
                return err("Error", format!("Class \"{}\" not found", cls));
            }
            match it.const_get(&n) {
                Some(v) => v,
                None => return err("Error", format!("Undefined constant {}", n)),
            }
        }
        "function_exists" => {
            let n = arg_str(it, args, 0).trim_start_matches('\\').to_lowercase();
            Value::Bool(it.functions.contains_key(&n) || is_builtin(&n))
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
            Value::Str(cn) => match it.lookup_class(&cn) {
                Some(c) => Value::Bool(
                    c.find_method(&arg_str(it, args, 1).to_lowercase())
                        .is_some(),
                ),
                None => Value::Bool(false),
            },
            _ => Value::Bool(false),
        },
        "property_exists" => match arg(args, 0) {
            Value::Object(o) => Value::Bool(o.borrow().props.contains_key(&arg_str(it, args, 1))),
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
            Value::Str(cn) => match it.lookup_class(&cn) {
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
            Value::Str(cn) => match it.lookup_class(&cn) {
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
                Value::Str(cn) => it.lookup_class(&cn),
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
            let call_args: Vec<Cell> = if name.ends_with("_array") {
                match arg(args, 1) {
                    Value::Array(a) => a.borrow().iter().map(|(_, c)| c.clone()).collect(),
                    _ => vec![],
                }
            } else {
                args[1.min(args.len())..].to_vec()
            };
            it.call_value(&cb, call_args)?
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
            Some(b) => Value::str(b.clone()),
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
        "serialize" => Value::str(serialize(&arg(args, 0))),
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
            match json_encode(it, &v) {
                Ok(s) => Value::str(s),
                Err(_) => Value::Bool(false),
            }
        }
        "json_decode" => {
            let s = arg_str(it, args, 0);
            let assoc = arg(args, 1).is_truthy();
            match json_decode(it, &s, assoc) {
                Ok(v) => v,
                Err(_) => Value::Null,
            }
        }
        "json_validate" => {
            let s = arg_str(it, args, 0);
            Value::Bool(json_decode(it, &s, true).is_ok())
        }

        // ----- hashing -----
        "md5" => Value::str(md5_hex(arg_str(it, args, 0).as_bytes())),
        "sha1" => Value::str(sha1_hex(arg_str(it, args, 0).as_bytes())),
        "crc32" => Value::Int(crc32(arg_str(it, args, 0).as_bytes()) as i64),
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
        "base64_encode" => Value::str(base64_encode(arg_str(it, args, 0).as_bytes())),
        "base64_decode" => match base64_decode(&arg_str(it, args, 0)) {
            Some(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
            None => Value::Bool(false),
        },
        "urlencode" => Value::str(urlencode(&arg_str(it, args, 0), false)),
        "rawurlencode" => Value::str(urlencode(&arg_str(it, args, 0), true)),
        "urldecode" => Value::str(urldecode(&arg_str(it, args, 0), false)),
        "rawurldecode" => Value::str(urldecode(&arg_str(it, args, 0), true)),
        "http_build_query" => {
            let mut parts = Vec::new();
            if let Value::Array(a) = arg(args, 0) {
                for (k, c) in a.borrow().iter() {
                    let ks = key_str(k);
                    let vs = c.borrow().to_php_string();
                    parts.push(format!("{}={}", urlencode(&ks, true), urlencode(&vs, true)));
                }
            }
            Value::str(parts.join("&"))
        }
        "parse_str" => {
            let s = arg_str(it, args, 0);
            let mut out = PhpArray::new();
            for pair in s.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    let k = urldecode(k, true);
                    let v = urldecode(v, false);
                    out.set(to_key(&Value::str(k)), Value::str(v));
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
        | "preg_split"
        | "preg_grep"
        | "preg_quote"
        | "preg_last_error" => preg_dispatch(it, name, args)?,

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
        "opendir" | "readdir" | "closedir" | "rewinddir" => Value::Null,
        "fopen" => {
            let path = arg_str(it, args, 0);
            let mode = arg_str(it, args, 1);
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
                return err("AssertionError", "assert(false)");
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
        "iterator_to_array" | "iterator_count" | "iterator_apply" => match arg(args, 0) {
            Value::Array(a) => {
                let mut out = PhpArray::new();
                for (k, c) in a.borrow().iter() {
                    out.set(k.clone(), c.borrow().clone());
                }
                Value::Array(Rc::new(RefCell::new(out)))
            }
            _ => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        },
        "closure_from_callable" | "closure::fromcallable" => arg(args, 0),
        "get_called_class" => Value::str(""),
        "spl_autoload_register" => {
            if let Some(v) = args.first() {
                it.autoload_fns.push(v.borrow().clone());
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
            it.run_autoload(&n);
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
        "mb_convert_case" => Value::str(arg_str(it, args, 0)),
        "mb_strtolower_nc" => Value::Null,
        "get_include_path" | "set_include_path" | "restore_include_path" => {
            Value::str(".:/home/linuxbrew/.linuxbrew/share/pear")
        }
        "token_get_all" | "token_name" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "highlight_string" | "highlight_file" | "php_strip_whitespace" => Value::Bool(true),
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
                urlencode(&arg_str(it, args, 1), true)
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
fn builtin_sig(n: &str) -> Option<Vec<(String, bool)>> {
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
            | "mb_convert_case"
            | "mb_strlen"
            | "mb_strtolower_nc"
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
    let _ = zval;
    let r = if is_ref { "&" } else { "" };
    match v {
        Value::Null => it.emit(&format!("{}{}NULL\n", pad, r)),
        Value::Bool(b) => it.emit(&format!("{}{}bool({})\n", pad, r, b)),
        Value::Int(i) => it.emit(&format!("{}{}int({})\n", pad, r, i)),
        Value::Float(f) => {
            let prec = it.ini_int("serialize_precision", -1);
            it.emit(&format!(
                "{}{}float({})\n",
                pad,
                r,
                crate::value::format_float_prec(*f, prec)
            ))
        }
        Value::Str(s) => it.emit(&format!("{}{}string({}) \"{}\"\n", pad, r, s.len(), s)),
        Value::Array(a) => {
            let a = a.borrow();
            it.emit(&format!("{}{}array({}) {{\n", pad, r, a.len()));
            for (k, c) in a.iter() {
                match k {
                    ArrKey::Int(i) => it.emit(&format!("{}  [{}]=>\n", pad, i)),
                    ArrKey::Str(s) => it.emit(&format!("{}  [\"{}\"]=>\n", pad, s)),
                    ArrKey::Tomb => continue,
                }
                var_dump(it, &c.borrow(), indent + 1, zval, Rc::strong_count(c) > 1);
            }
            it.emit(&format!("{}}}\n", pad));
        }
        Value::Object(o) => {
            let ob = o.borrow();
            // Count live props only — unset() tombstones prop_order slots.
            let live = ob
                .prop_order
                .iter()
                .filter(|n| ob.props.contains_key(*n))
                .count();
            it.emit(&format!(
                "{}object({})#{} ({}) {{\n",
                pad,
                ob.class.name(),
                ob.id,
                live
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
                    var_dump(it, &c.borrow(), indent + 1, zval, Rc::strong_count(c) > 1);
                }
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
        Value::Str(s) => format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")),
        Value::Array(a) => {
            let a = a.borrow();
            let mut s = String::from("array (\n");
            for (k, c) in a.iter() {
                s.push_str("  ");
                s.push_str(&match k {
                    ArrKey::Int(i) => i.to_string(),
                    ArrKey::Str(st) => {
                        format!("'{}'", st.replace('\\', "\\\\").replace('\'', "\\'"))
                    }
                    ArrKey::Tomb => continue,
                });
                s.push_str(" => ");
                s.push_str(&var_export(it, &c.borrow()));
                s.push_str(",\n");
            }
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
                    s.push_str(&var_export(it, &v));
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

fn trim_set(s: &str, chars: &str, left: bool, right: bool) -> String {
    let in_set = |c: char| {
        chars.contains(c)
            || chars.contains("..") && {
                // charlist range "a..z"
                let bytes = chars.as_bytes();
                let mut hit = false;
                for w in bytes.windows(4) {
                    if w[1] == b'.' && w[2] == b'.' && c >= w[0] as char && c <= w[3] as char {
                        hit = true;
                    }
                }
                hit
            }
    };
    let start = if left {
        s.char_indices()
            .find(|(_, c)| !in_set(*c))
            .map(|(i, _)| i)
            .unwrap_or(s.len())
    } else {
        0
    };
    let end = if right {
        s.char_indices()
            .rev()
            .find(|(_, c)| !in_set(*c))
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0)
    } else {
        s.len()
    };
    if end < start {
        String::new()
    } else {
        s[start..end].to_string()
    }
}

fn str_replace(find: &Value, repl: &Value, subj: &Value) -> String {
    let finds: Vec<String> = match find {
        Value::Array(a) => a
            .borrow()
            .entries
            .iter()
            .map(|(_, c)| c.borrow().to_php_string())
            .collect(),
        v => vec![v.to_php_string()],
    };
    let repls: Vec<String> = match repl {
        Value::Array(a) => a
            .borrow()
            .entries
            .iter()
            .map(|(_, c)| c.borrow().to_php_string())
            .collect(),
        v => vec![v.to_php_string()],
    };
    match subj {
        Value::Str(s) => {
            let mut out = s.to_string();
            for (i, f) in finds.iter().enumerate() {
                if f.is_empty() {
                    continue;
                }
                let r = repls
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| repls.last().cloned().unwrap_or_default());
                out = out.replace(f.as_str(), &r);
            }
            out
        }
        v => v.to_php_string(),
    }
}

fn str_replace_i(find: &Value, repl: &Value, subj: &Value) -> String {
    let finds: Vec<String> = match find {
        Value::Array(a) => a
            .borrow()
            .entries
            .iter()
            .map(|(_, c)| c.borrow().to_php_string())
            .collect(),
        v => vec![v.to_php_string()],
    };
    let mut out = subj.to_php_string();
    for (i, f) in finds.iter().enumerate() {
        if f.is_empty() {
            continue;
        }
        let r = match repl {
            Value::Array(a) => a
                .borrow()
                .entries
                .get(i)
                .map(|(_, c)| c.borrow().to_php_string())
                .unwrap_or_default(),
            v => v.to_php_string(),
        };
        // case-insensitive replace
        let mut res = String::new();
        let fl = f.to_lowercase();
        let mut rest = out.as_str();
        loop {
            let pos = rest.to_lowercase().find(&fl);
            match pos {
                Some(p) => {
                    res.push_str(&rest[..p]);
                    res.push_str(&r);
                    rest = &rest[p + f.len()..];
                }
                None => {
                    res.push_str(rest);
                    break;
                }
            }
        }
        out = res;
    }
    out
}

fn php_substr(s: &str, start: i64, len: Option<i64>) -> Option<String> {
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
    Some(s[start as usize..(start + len) as usize].to_string())
}

fn substr_replace(s: &str, r: &str, start: i64, len: Option<i64>) -> String {
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
    format!("{}{}{}", &s[..start as usize], r, &s[end as usize..])
}

fn str_pad(s: &str, len: usize, pad: &str, ty: i64) -> String {
    if s.len() >= len || pad.is_empty() {
        return s.to_string();
    }
    let need = len - s.len();
    let mk = |n: usize| -> String { pad.repeat(n / pad.len() + 1)[..n].to_string() };
    match ty {
        0 => format!("{}{}", s, mk(need)), // STR_PAD_RIGHT
        1 => format!("{}{}", mk(need), s), // STR_PAD_LEFT
        2 => {
            // STR_PAD_BOTH
            let l = need / 2;
            let r = need - l;
            format!("{}{}{}", mk(l), s, mk(r))
        }
        _ => format!("{}{}", s, mk(need)),
    }
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

fn strtr_map(s: &str, pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let mut matched = false;
        for (from, to) in pairs {
            if !from.is_empty() && s[i..].starts_with(from.as_str()) {
                out.push_str(to);
                i += from.len();
                matched = true;
                break;
            }
        }
        if !matched {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn strtr_chars(s: &str, from: &str, to: &str) -> String {
    let fb: Vec<char> = from.chars().collect();
    let tb: Vec<char> = to.chars().collect();
    s.chars()
        .map(|c| {
            fb.iter()
                .position(|&f| f == c)
                .and_then(|i| tb.get(i).copied())
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
                        let r = it.call_value(&cb, args)?;
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

fn serialize(v: &Value) -> String {
    match v {
        Value::Null => "N;".into(),
        Value::Bool(b) => format!("b:{};", *b as i32),
        Value::Int(i) => format!("i:{};", i),
        Value::Float(f) => format!("d:{};", crate::value::format_float_repr(*f)),
        Value::Str(s) => format!("s:{}:\"{}\";", s.len(), s),
        Value::Array(a) => {
            let a = a.borrow();
            let mut s = format!("a:{}:{{", a.len());
            for (k, c) in a.iter() {
                s.push_str(&serialize(&match k {
                    ArrKey::Int(i) => Value::Int(*i),
                    ArrKey::Str(st) => Value::str(st.to_string()),
                    ArrKey::Tomb => Value::Null,
                }));
                s.push_str(&serialize(&c.borrow()));
            }
            s.push('}');
            s
        }
        Value::Object(o) => {
            let ob = o.borrow();
            let mut body = String::new();
            let mut n = 0;
            for name in &ob.prop_order {
                if let Some(c) = ob.props.get(name) {
                    body.push_str(&serialize(&Value::str(name.clone())));
                    body.push_str(&serialize(&c.borrow()));
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
                    .strip_prefix('\0')
                    .and_then(|r| r.split('\0').nth(1))
                    .unwrap_or(ks.as_ref());
                // Virtual hooked props have no backing to fill — zend
                // aborts the whole unserialize, reporting the offset
                // right after the property name (unserialize.phpt).
                if it.unserial_prop_virtual(&obj, plain) {
                    let _ = it.warn_pub(&format!(
                        "unserialize(): Cannot unserialize value for virtual property {}::${}",
                        cname, plain
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
                let key = ks.to_string();
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

fn json_encode(_it: &mut Interp, v: &Value) -> Result<String, ()> {
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
        Value::Str(s) => json_str(s),
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
                    .map(|(_, c)| json_encode(_it, &c.borrow()).unwrap_or("null".into()))
                    .collect();
                format!("[{}]", parts.join(","))
            } else {
                let parts: Vec<String> = a
                    .entries
                    .iter()
                    .map(|(k, c)| {
                        format!(
                            "{}:{}",
                            json_str(&key_str(k)),
                            json_encode(_it, &c.borrow()).unwrap_or("null".into())
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
                    .method_invoke(o.clone(), "jsonSerialize", Vec::new())
                    .unwrap_or(Value::Null);
                return json_encode(_it, &v);
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
                        json_str(&out),
                        json_encode(_it, &v).unwrap_or("null".into())
                    ));
                }
            }
            format!("{{{}}}", parts.join(","))
        }
        _ => "null".into(),
    })
}

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
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
                    }))
                });
            }
            loop {
                json_ws(b, pos);
                let k = match json_value(it, b, pos, true)? {
                    Value::Str(s) => s.to_string(),
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

pub(crate) fn urlencode(s: &str, raw: bool) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            b'~' if raw => out.push('~'),
            b' ' if !raw => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

pub(crate) fn urldecode(s: &str, raw: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' if !raw => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() + 1 => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..(i + 3).min(b.len())], 16) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------- regex ----------

fn preg_dispatch(it: &mut Interp, name: &str, args: &[Cell]) -> Result<Value, PhpError> {
    match name {
        "preg_quote" => {
            let s = arg_str(it, args, 0);
            let mut out = String::new();
            for c in s.chars() {
                if ".\\+*?[^]$(){}=!<>|:-#/".contains(c) {
                    out.push('\\');
                }
                out.push(c);
            }
            Ok(Value::str(out))
        }
        "preg_last_error" => Ok(Value::Int(0)),
        "preg_match" | "preg_match_all" => {
            let pat = arg_str(it, args, 0);
            let subj = arg_str(it, args, 1);
            let re = match php_regex(&pat) {
                Some(r) => r,
                None => {
                    it.warn_pub(&format!("preg_match(): Invalid regex '{}'", pat))?;
                    return Ok(Value::Bool(false));
                }
            };
            let all = name == "preg_match_all";
            let flags = arg(args, 3).to_int();
            let offset = arg(args, 4).to_int().max(0) as usize;
            let hay = subj.get(offset..).unwrap_or("");
            let mut matches_arr = PhpArray::new();
            let mut count = 0i64;
            let caps = re.caps(hay);
            // A capture group -> PHP value honoring OFFSET_CAPTURE and
            // UNMATCHED_AS_NULL; offsets are absolute on the subject.
            let entry = |span: Option<(usize, usize)>| -> Value {
                match span {
                    Some((a, b)) if flags & 256 != 0 => {
                        let mut pair = PhpArray::new();
                        pair.push(hay.get(a..b).map(Value::str).unwrap_or(Value::str("")));
                        pair.push(Value::Int((a + offset) as i64));
                        Value::Array(Rc::new(RefCell::new(pair)))
                    }
                    Some((a, b)) => hay.get(a..b).map(Value::str).unwrap_or(Value::str("")),
                    None if flags & 512 != 0 => Value::Null,
                    None => Value::str(""),
                }
            };
            if all {
                let ngroups = re.captures_len();
                if flags & 2 != 0 {
                    // PREG_SET_ORDER: one row per match.
                    for cap in &caps {
                        count += 1;
                        let mut row = PhpArray::new();
                        for g in 0..cap.spans.len().max(ngroups) {
                            let span = cap.spans.get(g).copied().flatten();
                            // PHP omits unmatched groups entirely unless
                            // PREG_UNMATCHED_AS_NULL asked for nulls.
                            if span.is_none() && flags & 512 == 0 {
                                continue;
                            }
                            let v = entry(span);
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
                        for c in &caps {
                            marks.push(
                                c.mark
                                    .as_ref()
                                    .map(|m| Value::str(m.clone()))
                                    .unwrap_or(Value::Bool(false)),
                            );
                        }
                        matches_arr.set(
                            ArrKey::Str("MARK".into()),
                            Value::Array(Rc::new(RefCell::new(marks))),
                        );
                    }
                }
            } else if let Some(cap) = caps.into_iter().next() {
                count = 1;
                for g in 0..cap.spans.len() {
                    let span = cap.spans.get(g).copied().flatten();
                    if span.is_none() && flags & 512 == 0 {
                        continue;
                    }
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
        "preg_replace" | "preg_replace_callback" => {
            let pat = arg(args, 0);
            let pats: Vec<String> = match &pat {
                Value::Array(a) => a
                    .borrow()
                    .entries
                    .iter()
                    .map(|(_, c)| c.borrow().to_php_string())
                    .collect(),
                v => vec![v.to_php_string()],
            };
            let subj = arg(args, 2).to_php_string();
            let mut result = subj;
            let limit = arg(args, 3).to_int();
            for p in pats {
                let re = match php_regex(&p) {
                    Some(r) => r,
                    None => continue,
                };
                result = if name == "preg_replace_callback" {
                    let cb = arg(args, 1);
                    let mut out = String::new();
                    let mut last = 0;
                    for (n, cap) in re.caps(&result).into_iter().enumerate() {
                        let n = n as i64;
                        if limit > 0 && n >= limit {
                            break;
                        }
                        let Some(Some((ms, me))) = cap.spans.first() else {
                            continue;
                        };
                        out.push_str(result.get(last..*ms).unwrap_or(""));
                        let mut group_arr = PhpArray::new();
                        for g in 0..cap.spans.len() {
                            group_arr.push(
                                cap.spans
                                    .get(g)
                                    .copied()
                                    .flatten()
                                    .and_then(|(a, b)| result.get(a..b))
                                    .map(Value::str)
                                    .unwrap_or(Value::str("")),
                            );
                        }
                        let r = it.call_value(
                            &cb,
                            vec![cell(Value::Array(Rc::new(RefCell::new(group_arr))))],
                        )?;
                        out.push_str(&r.to_php_string());
                        last = *me;
                        let _ = n;
                    }
                    out.push_str(&result[last..]);
                    out
                } else {
                    let repl = arg(args, 1).to_php_string();
                    re.replace_all(&result, &mut |caps: &PhpCap| {
                        let mut out = repl.clone();
                        for g in (0..caps.spans.len()).rev() {
                            let m = caps
                                .spans
                                .get(g)
                                .copied()
                                .flatten()
                                .and_then(|(a, b)| result.get(a..b))
                                .unwrap_or("");
                            out = out.replace(&format!("${}", g), m);
                            out = out.replace(&format!("\\{}", g), m);
                        }
                        out
                    })
                };
            }
            Ok(Value::str(result))
        }
        "preg_split" => {
            let pat = arg_str(it, args, 0);
            let subj = arg_str(it, args, 1);
            let flags = arg(args, 3).to_int();
            let limit = arg(args, 2).to_int();
            let re = match php_regex(&pat) {
                Some(r) => r,
                None => return Ok(Value::Bool(false)),
            };
            let mut out = PhpArray::new();
            let mut last = 0usize;
            for cap in re.caps(&subj) {
                let Some(Some((a, b))) = cap.spans.first() else {
                    continue;
                };
                // limit reached: emit the rest as one piece and stop
                if limit > 0 && out.entries.len() as i64 >= limit - 1 {
                    out.push(Value::str(subj.get(last..).unwrap_or("")));
                    return Ok(Value::Array(Rc::new(RefCell::new(out))));
                }
                let piece = subj.get(last..*a).unwrap_or("");
                if flags & 1 == 0 || !piece.is_empty() {
                    if flags & 4 != 0 {
                        let mut pair = PhpArray::new();
                        pair.push(Value::str(piece));
                        pair.push(Value::Int(last as i64));
                        out.push(Value::Array(Rc::new(RefCell::new(pair))));
                    } else {
                        out.push(Value::str(piece));
                    }
                }
                if flags & 2 != 0 {
                    for (ga, gb) in cap.spans.iter().skip(1).flatten() {
                        if flags & 1 == 0 || ga != gb {
                            let g = subj.get(*ga..*gb).unwrap_or("");
                            if flags & 4 != 0 {
                                let mut pair = PhpArray::new();
                                pair.push(Value::str(g));
                                pair.push(Value::Int(*ga as i64));
                                out.push(Value::Array(Rc::new(RefCell::new(pair))));
                            } else {
                                out.push(Value::str(g));
                            }
                        }
                    }
                }
                last = *b;
            }
            let tail = subj.get(last..).unwrap_or("");
            if flags & 1 == 0 || !tail.is_empty() {
                if flags & 4 != 0 {
                    let mut pair = PhpArray::new();
                    pair.push(Value::str(tail));
                    pair.push(Value::Int(last as i64));
                    out.push(Value::Array(Rc::new(RefCell::new(pair))));
                } else {
                    out.push(Value::str(tail));
                }
            }
            Ok(Value::Array(Rc::new(RefCell::new(out))))
        }
        "preg_grep" => {
            let pat = arg_str(it, args, 0);
            let flags = arg(args, 2).to_int();
            let re = match php_regex(&pat) {
                Some(r) => r,
                None => return Ok(Value::Bool(false)),
            };
            let mut out = PhpArray::new();
            if let Value::Array(a) = arg(args, 1) {
                for (k, c) in a.borrow().iter() {
                    let v = c.borrow().to_php_string();
                    if re.is_match(&v) != (flags & 1 != 0) {
                        out.set(k.clone(), Value::str(v));
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

/// A PHP pattern compiled for one of the two engines we carry: `regex`
/// (pure Rust) for the common syntax subset, or PCRE2 — PHP's own
/// engine — for anything it can't express (backtracking verbs like
/// `(*SKIP)(*F)`, recursion `(?-n)`, lookbehind, etc.).
enum PhpRe {
    Re(regex::Regex),
    Pcre(crate::pcre::PcreRe),
}

impl PhpRe {
    fn captures_len(&self) -> usize {
        match self {
            PhpRe::Re(r) => r.captures_len(),
            PhpRe::Pcre(r) => r.captures_len(),
        }
    }
    fn group_name(&self, g: usize) -> Option<String> {
        match self {
            PhpRe::Re(r) => r.capture_names().nth(g).flatten().map(|s| s.to_string()),
            PhpRe::Pcre(r) => r.group_name(g),
        }
    }
    /// All matches in order, normalized to group byte spans.
    fn caps(&self, s: &str) -> Vec<PhpCap> {
        match self {
            PhpRe::Re(r) => r
                .captures_iter(s)
                .map(|c| PhpCap {
                    spans: (0..c.len())
                        .map(|g| c.get(g).map(|m| (m.start(), m.end())))
                        .collect(),
                    mark: None,
                })
                .collect(),
            PhpRe::Pcre(r) => r
                .match_all(s.as_bytes())
                .into_iter()
                .map(|m| PhpCap {
                    spans: m.spans,
                    mark: m.mark,
                })
                .collect(),
        }
    }
    fn is_match(&self, s: &str) -> bool {
        match self {
            PhpRe::Re(r) => r.is_match(s),
            PhpRe::Pcre(r) => !r.match_all(s.as_bytes()).is_empty(),
        }
    }
    fn replace_all(&self, s: &str, repl: &mut dyn FnMut(&PhpCap) -> String) -> String {
        let mut out = String::new();
        let mut last = 0usize;
        for c in self.caps(s) {
            if let Some(Some((a, b))) = c.spans.first() {
                out.push_str(s.get(last..*a).unwrap_or(""));
                out.push_str(&repl(&c));
                last = *b;
            }
        }
        out.push_str(&s[last..]);
        out
    }
}

/// Translate a PHP `/pat/flags` regex to a `PhpRe`.
fn php_regex(pat: &str) -> Option<PhpRe> {
    let b = pat.as_bytes();
    if b.len() < 2 {
        return None;
    }
    let delim = b[0] as char;
    let end = pat.rfind(delim)?;
    if end == 0 {
        return None;
    }
    let body = &pat[1..end];
    let flags = &pat[end + 1..];
    let mut wrapped = String::new();
    for f in flags.chars() {
        match f {
            'i' => wrapped.push_str("(?i)"),
            'm' => wrapped.push_str("(?m)"),
            's' => wrapped.push_str("(?s)"),
            'x' => wrapped.push_str("(?x)"),
            'u' | 'U' | 'D' | 'S' | 'A' | 'J' => {}
            _ => {}
        }
    }
    // PHP compiles patterns with PCRE2; use it for anything the `regex`
    // crate can't express rather than trying to emulate backtracking.
    let pcre_only = [
        "(*", "\\K", "\\G", "(?<", "(?R", "(?-", "(?+", "(?|", "(?'", "(?P>", "(?#",
    ];
    let mut has_backref = false;
    let bb = body.as_bytes();
    for i in 0..bb.len().saturating_sub(1) {
        if bb[i] == b'\\' && bb[i + 1].is_ascii_digit() {
            has_backref = true;
            break;
        }
    }
    if has_backref || pcre_only.iter().any(|t| body.contains(t)) {
        let src = format!("{}{}", wrapped, body);
        return crate::pcre::compile(&src).map(PhpRe::Pcre);
    }
    wrapped.push_str(body);
    regex::Regex::new(&wrapped)
        .ok()
        .map(PhpRe::Re)
        .or_else(|| crate::pcre::compile(&wrapped).map(PhpRe::Pcre))
}

// ---------- filesystem ----------

fn read_stream(path: &str) -> Result<Vec<u8>, std::io::Error> {
    if path.starts_with("php://stdin") {
        return Ok(Vec::new());
    }
    std::fs::read(path)
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

fn write_resource(_it: &mut Interp, c: Option<&Cell>, data: &str) -> Result<(), PhpError> {
    use std::io::{Seek, Write};
    match c.map(|c| c.borrow().clone()) {
        Some(Value::Resource(r)) => {
            let mut rb = r.borrow_mut();
            match &mut *rb {
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
