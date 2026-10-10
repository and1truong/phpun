//! json_encode/json_decode and their error reporting.

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
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
        _ => return Ok(None),
    }))
}

// ----- helpers -----

fn json_encode(it: &mut Interp, v: &Value, flags: i64) -> Result<String, ()> {
    let mut out = String::new();
    json_enc(it, v, flags, &mut Vec::new(), &mut out)?;
    Ok(out)
}

// Preserve the existing child-error null fallback without per-child strings.
fn json_child(it: &mut Interp, v: &Value, flags: i64, seen: &mut Vec<usize>, out: &mut String) {
    let start = out.len();
    if json_enc(it, v, flags, seen, out).is_err() {
        out.truncate(start);
        out.push_str("null");
    }
}

fn json_enc(
    it: &mut Interp,
    v: &Value,
    flags: i64,
    seen: &mut Vec<usize>,
    out: &mut String,
) -> Result<(), ()> {
    use std::fmt::Write;
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => write!(out, "{i}").unwrap(),
        Value::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                write!(out, "{}", *f as i64).unwrap();
            } else {
                out.push_str(&crate::value::format_float(*f));
            }
        }
        Value::Str(s) => json_str(&crate::value::lossy(s), flags, out),
        Value::Array(a) => {
            let a = a.borrow();
            let is_list = a
                .entries
                .iter()
                .enumerate()
                .all(|(i, (k, _))| matches!(k, ArrKey::Int(x) if *x == i as i64));
            out.push(if is_list { '[' } else { '{' });
            for (i, (k, c)) in a.entries.iter().enumerate() {
                if i != 0 {
                    out.push(',');
                }
                if !is_list {
                    json_key(k, flags, out);
                    out.push(':');
                }
                json_child(it, &c.borrow(), flags, seen, out);
            }
            out.push(if is_list { ']' } else { '}' });
        }
        Value::Object(o) => {
            let key = Rc::as_ptr(o) as usize;
            if !seen.contains(&key)
                && it
                    .find_method_in(&o.borrow().class, "jsonserialize")
                    .is_some()
            {
                seen.push(key);
                let v = it
                    .method_invoke(o.clone(), "jsonSerialize", crate::interp::CallArgs::empty())
                    .unwrap_or(Value::Null);
                return json_enc(it, &v, flags, seen, out);
            }
            let ao_arr = if matches!(
                o.borrow().internal,
                Some(crate::value::ObjectInternal::ArrayIter { .. })
            ) {
                Some(it.ao_arr(o))
            } else {
                None
            };
            let dtp = if ao_arr.is_none() {
                crate::builtins::datetime::dt_public_props(&o.borrow())
            } else {
                None
            };
            out.push('{');
            if let Some(arr) = ao_arr {
                for (i, (k, c)) in arr.borrow().iter().enumerate() {
                    if i != 0 {
                        out.push(',');
                    }
                    json_key(k, flags, out);
                    out.push(':');
                    json_child(it, &c.borrow(), flags, seen, out);
                }
            } else if let Some(dtp) = dtp {
                for (i, (k, v)) in dtp.iter().enumerate() {
                    if i != 0 {
                        out.push(',');
                    }
                    json_str(k, flags, out);
                    out.push(':');
                    json_child(it, v, flags, seen, out);
                }
            } else {
                let entries = it.object_serial_entries(o);
                let mut first = true;
                for (name, slot, decl) in entries {
                    if !decl
                        .as_ref()
                        .map(|(p, _)| p.visibility == crate::ast::Visibility::Public)
                        .unwrap_or(true)
                    {
                        continue;
                    }
                    let name = match crate::value::int_prop_index(&name) {
                        Some(i) => i.to_string(),
                        None => name,
                    };
                    let v = match &decl {
                        Some((p, dcls)) => it.serial_entry_value(o, p, dcls, &slot),
                        None => o.borrow().props.get(&slot).map(|c| c.borrow().clone()),
                    };
                    if let Some(v) = v {
                        if !first {
                            out.push(',');
                        }
                        first = false;
                        json_str(&name, flags, out);
                        out.push(':');
                        // Raw object fields historically use a fresh serializer scope.
                        json_child(it, &v, flags, &mut Vec::new(), out);
                    }
                }
            }
            out.push('}');
        }
        _ => out.push_str("null"),
    }
    Ok(())
}

fn json_key(key: &ArrKey, flags: i64, out: &mut String) {
    match key {
        ArrKey::Str(s) => json_str(s, flags, out),
        _ => json_str(&key_str(key), flags, out),
    }
}

fn json_str(s: &str, flags: i64, out: &mut String) {
    use std::fmt::Write;
    const HEX_TAG: i64 = 1;
    const HEX_AMP: i64 = 2;
    const HEX_APOS: i64 = 4;
    const HEX_QUOT: i64 = 8;
    const UNESCAPED_SLASHES: i64 = 64;
    const UNESCAPED_UNICODE: i64 = 256;
    // HEX-flag escapes use %04X; default unicode escapes %04x (PHP quirk).
    out.push('"');
    for c in s.chars() {
        let u = c as u32;
        match c {
            '"' if flags & HEX_QUOT != 0 => write!(out, "\\u{:04X}", u).unwrap(),
            '"' => out.push_str("\\\""),
            '\'' if flags & HEX_APOS != 0 => write!(out, "\\u{:04X}", u).unwrap(),
            '<' | '>' if flags & HEX_TAG != 0 => write!(out, "\\u{:04X}", u).unwrap(),
            '&' if flags & HEX_AMP != 0 => write!(out, "\\u{:04X}", u).unwrap(),
            '\\' => out.push_str("\\\\"),
            '/' if flags & UNESCAPED_SLASHES == 0 => out.push_str("\\/"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            _ if u < 0x20 => write!(out, "\\u{:04x}", u).unwrap(),
            _ if u > 0x7f && flags & UNESCAPED_UNICODE == 0 => {
                if u > 0xffff {
                    let x = u - 0x10000;
                    write!(
                        out,
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (x >> 10),
                        0xdc00 + (x & 0x3ff)
                    )
                    .unwrap();
                } else {
                    write!(out, "\\u{:04x}", u).unwrap();
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
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
                if b[*pos] == b'\\' {
                    if *pos + 1 == b.len() {
                        return Err(());
                    }
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
                } else if b[*pos].is_ascii() {
                    // Scan ASCII runs once instead of validating/copying each byte.
                    let start = *pos;
                    while *pos < b.len() && b[*pos].is_ascii() && !matches!(b[*pos], b'"' | b'\\') {
                        *pos += 1;
                    }
                    s.push_str(std::str::from_utf8(&b[start..*pos]).unwrap());
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
