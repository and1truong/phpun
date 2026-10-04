//! URL builtins: urlencode family, http_build_query, parse_str/parse_url.

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
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
        _ => return Ok(None),
    }))
}

// ----- helpers -----

pub(in crate::builtins) fn urlencode(s: &[u8], raw: bool) -> Vec<u8> {
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
