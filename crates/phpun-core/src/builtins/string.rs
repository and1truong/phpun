//! String builtins — PHP string ops are byte ops on `Rc<[u8]>`.

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
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
        _ => return Ok(None),
    }))
}

// ----- helpers -----

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

pub(in crate::builtins) fn php_substr(s: &[u8], start: i64, len: Option<i64>) -> Option<Vec<u8>> {
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
