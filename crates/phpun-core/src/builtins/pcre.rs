//! preg_* builtins on the PCRE2 engine (crate::pcre).

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
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
        "preg_jit" => Value::Bool(false),
        _ => return Ok(None),
    }))
}

// ----- helpers -----

/// Byte-faithful arg conversion — PHP strings are byte arrays.
/// PCRE2 match/scan error code → PHP preg_last_error() code.
pub(in crate::builtins) fn preg_rc_err(rc: i32) -> i64 {
    match rc {
        -47 => 2, // PCRE2_ERROR_MATCHLIMIT
        -53 => 3, // PCRE2_ERROR_DEPTHLIMIT
        // UTF-8 subject/pattern errors (UTF8_ERR1..21 and
        // UTF16-range) and BadNewline → PREG_BAD_UTF8_ERROR.
        x if (-56..=-36).contains(&x) || (-200..=-169).contains(&x) => 4,
        _ => 1,
    }
}

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
pub(in crate::builtins) struct PhpCap {
    pub(in crate::builtins) spans: Vec<Option<(usize, usize)>>,
    mark: Option<String>,
}

/// Translate a PHP `/pat/flags` regex to a `PhpRe`. Err is the full
/// warning text PHP emits (`Compilation failed: ...`, `Unknown
/// modifier 'x'`, delimiter problems) — callers prefix `fname(): `.
pub(in crate::builtins) fn php_regex(pat: &[u8]) -> Result<PhpRe, String> {
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

/// A PHP pattern compiled by PCRE2 — PHP's own engine.
pub(in crate::builtins) enum PhpRe {
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
    pub(in crate::builtins) fn caps(&self, s: &[u8], it: &Interp) -> (Vec<PhpCap>, i32) {
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
