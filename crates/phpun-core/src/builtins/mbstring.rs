//! mbstring builtins — charset/UTF-8-aware string ops.

use super::pcre::{preg_rc_err, PhpRe};
use super::string::php_substr;
use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        "mb_strlen" => Value::Int(arg(args, 0).to_php_string().chars().count() as i64),
        "mb_strtoupper" => Value::str(arg_str(it, args, 0).to_uppercase()),
        "mb_strtolower" => Value::str(arg_str(it, args, 0).to_lowercase()),
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
        "mb_strtolower_nc" => Value::Null,
        _ => return Ok(None),
    }))
}

// ----- helpers -----

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
