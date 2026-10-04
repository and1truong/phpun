//! filter_var validation/sanitization.

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
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
        _ => return Ok(None),
    }))
}

// ----- helpers -----

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
