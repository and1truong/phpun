//! Array builtins: count/search/sort/splice and the internal array pointer.

use super::*;

/// `zend_argument_type_error` for array params: internal fns
/// ZPP-check the declared `array` type even when the arg bound by
/// reference (`sort($undef)` binds null, then fails ZPP).
fn zpp_gate(it: &mut Interp, name: &str, args: &[Cell]) -> Option<PhpError> {
    let is_arr = |c: &Cell| matches!(&*c.borrow(), Value::Array(_));
    let type_err = |n: usize, pname: Option<&str>, ty: &str, v: &Value| {
        PhpError::uncaught(
            "TypeError",
            format!(
                "{}(): Argument #{}{} must be of type {}, {} given",
                name,
                n,
                pname.map(|p| format!(" (${})", p)).unwrap_or_default(),
                ty,
                zval_word(v)
            ),
            0,
        )
    };
    match name {
        // `array $array` — by-ref family and value params alike.
        "sort"
        | "rsort"
        | "asort"
        | "arsort"
        | "ksort"
        | "krsort"
        | "usort"
        | "uasort"
        | "uksort"
        | "natsort"
        | "natcasesort"
        | "shuffle"
        | "array_pop"
        | "array_shift"
        | "array_push"
        | "array_unshift"
        | "array_keys"
        | "array_reverse"
        | "extract"
        | "array_column"
        | "array_slice"
        | "array_sum"
        | "array_product"
        | "array_filter"
        | "array_flip"
        | "array_values"
        | "array_unique"
        | "array_pad"
        | "array_count_values"
        | "array_is_list"
        | "array_chunk"
        | "array_diff"
        | "array_udiff"
        | "array_diff_assoc"
        | "array_diff_key"
        | "array_intersect"
        | "array_uintersect"
        | "array_intersect_assoc"
        | "array_intersect_key" => {
            if let Some(c) = args.first() {
                if !is_arr(c) {
                    return Some(type_err(1, Some("array"), "array", &c.borrow()));
                }
            }
        }
        // `array &$array` with a required second param — missing-args
        // wins over the arg0 type check (array_splice/array_walk).
        "array_splice" => {
            if args.len() >= 2 {
                if let Some(c) = args.first() {
                    if !is_arr(c) {
                        return Some(type_err(1, Some("array"), "array", &c.borrow()));
                    }
                }
            }
        }
        // `array|object` — pointer-movement and walk params; zend's
        // error text still says `array`.
        "reset"
        | "end"
        | "next"
        | "prev"
        | "current"
        | "pos"
        | "array_walk"
        | "array_walk_recursive" => {
            if args.len()
                < if matches!(name, "array_walk" | "array_walk_recursive") {
                    2
                } else {
                    1
                }
            {
                return None;
            }
            if let Some(c) = args.first() {
                if !is_arr(c) && !matches!(&*c.borrow(), Value::Object(_)) {
                    return Some(type_err(1, Some("array"), "array", &c.borrow()));
                }
            }
        }
        "count" | "sizeof" => {
            if let Some(c) = args.first() {
                let ok = match &*c.borrow() {
                    Value::Array(_) => true,
                    Value::Object(o) => it.obj_is_a(o, "Countable"),
                    _ => false,
                };
                if !ok {
                    return Some(type_err(1, Some("value"), "Countable|array", &c.borrow()));
                }
            }
        }
        // `array ...$arrays` — variadic args report without a name.
        "array_merge" | "array_merge_recursive" | "array_replace" | "array_replace_recursive" => {
            for (i, c) in args.iter().enumerate() {
                if !is_arr(c) {
                    return Some(type_err(i + 1, None, "array", &c.borrow()));
                }
            }
        }
        "in_array" | "array_search" => {
            if let Some(c) = args.get(1) {
                if !is_arr(c) {
                    return Some(type_err(2, Some("haystack"), "array", &c.borrow()));
                }
            }
        }
        "array_key_exists" | "key_exists" => {
            if let Some(c) = args.get(1) {
                if !is_arr(c) {
                    return Some(type_err(2, Some("array"), "array", &c.borrow()));
                }
            }
        }
        _ => {}
    }
    None
}

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    if let Some(e) = zpp_gate(it, name, args) {
        return Err(e);
    }
    Ok(Some(match name {
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
                        crate::value::clear_cmp_depth_err();
                        // zend compares (search_value, entry) — the
                        // search value is the protected LEFT operand.
                        let hit = if strict {
                            crate::value::identical(sv, &ev)
                        } else {
                            crate::value::compare(sv, &ev) == std::cmp::Ordering::Equal
                        };
                        if crate::value::cmp_depth_err() {
                            return depth_err();
                        }
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
                Value::Array(a) => {
                    let mut found = false;
                    for (_, c) in a.borrow().iter() {
                        let v = c.borrow();
                        crate::value::clear_cmp_depth_err();
                        // zend's _php_search_array compares
                        // (needle, entry) — the needle is the
                        // protected LEFT operand.
                        found = if strict {
                            crate::value::identical(&needle, &v)
                        } else {
                            compare(&needle, &v) == std::cmp::Ordering::Equal
                        };
                        if crate::value::cmp_depth_err() {
                            return depth_err();
                        }
                        if found {
                            break;
                        }
                    }
                    Value::Bool(found)
                }
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
                        crate::value::clear_cmp_depth_err();
                        // zend compares (needle, entry) — needle LEFT.
                        let hit = if strict {
                            crate::value::identical(&needle, &v)
                        } else {
                            compare(&needle, &v) == std::cmp::Ordering::Equal
                        };
                        if crate::value::cmp_depth_err() {
                            return depth_err();
                        }
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
        "array_replace_recursive" => {
            fn rec(base: &mut PhpArray, over: &PhpArray) {
                for (k, c) in over.iter() {
                    let v = c.borrow().clone();
                    let sub = base.get(k).and_then(|b| match b {
                        Value::Array(a) => Some(a.borrow().clone()),
                        _ => None,
                    });
                    match (&v, sub) {
                        (Value::Array(oa), Some(mut sub_arr)) => {
                            rec(&mut sub_arr, &oa.borrow());
                            base.set(k.clone(), Value::Array(Rc::new(RefCell::new(sub_arr))));
                        }
                        (Value::Array(oa), None) => {
                            let mut fresh = PhpArray::new();
                            rec(&mut fresh, &oa.borrow());
                            base.set(k.clone(), Value::Array(Rc::new(RefCell::new(fresh))));
                        }
                        _ => base.set(k.clone(), v),
                    }
                }
            }
            let mut out = match args.first().map(|a| a.borrow().clone()) {
                Some(Value::Array(a)) => a.borrow().clone(),
                _ => PhpArray::new(),
            };
            for a in args.iter().skip(1) {
                if let Value::Array(m) = &*a.borrow() {
                    rec(&mut out, &m.borrow());
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
                arr.next = 0;
                let (head, rest) = tail.split_at(off as usize);
                let (cut, tail2) = rest.split_at((len as usize).min(rest.len()));
                for (k, c) in cut {
                    removed.push(c.borrow().clone());
                    let _ = k;
                }
                // PHP renumbers every integer key in the result (string
                // keys are kept); replacement values always append.
                let put = |arr: &mut PhpArray, k: &ArrKey, c: &Cell| match k {
                    ArrKey::Str(s) => arr.set_cell(ArrKey::Str(s.clone()), c.clone()),
                    _ => arr.push_cell(c.clone()),
                };
                for (k, c) in head {
                    put(&mut arr, k, c);
                }
                if let Some(repl) = args.get(3) {
                    let rv = repl.borrow().clone();
                    match &rv {
                        Value::Array(r) => {
                            for (_, c) in r.borrow().iter() {
                                arr.push(c.borrow().clone());
                            }
                        }
                        // Non-array replacement is `(array)`-cast — an
                        // object yields its prop values, everything else
                        // becomes `[0 => $v]` (bug52193).
                        Value::Object(o) => {
                            let ob = o.borrow();
                            for n in &ob.prop_order {
                                if let Some(c) = ob.props.get(n) {
                                    arr.push(c.borrow().clone());
                                }
                            }
                        }
                        Value::Null => {}
                        _ => arr.push(rv.clone()),
                    }
                }
                for (k, c) in tail2 {
                    put(&mut arr, k, c);
                }
                // Splice renumbers integer keys (bug52193).
                let mut i = 0i64;
                for (k, _) in arr.entries.iter_mut() {
                    if matches!(k, ArrKey::Int(_)) {
                        *k = ArrKey::Int(i);
                        i += 1;
                    }
                }
                arr.next = i;
                arr.iter_pos = 0;
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
                        // Zend drops nNextFreeElement to the popped key
                        // when it was the top int slot (holes keep it).
                        if let ArrKey::Int(k) = arr.entries[f].0 {
                            if k == arr.next - 1 {
                                arr.next = k;
                            }
                        }
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
        "array_diff" => array_diff(it, args)?,
        "array_diff_assoc" | "array_intersect_assoc" => {
            array_assoc_match(it, name, args, name == "array_diff_assoc")?
        }
        "array_diff_key" => {
            if let Some(e) = need_arrays(name, args) {
                return Err(e);
            }
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
        "array_intersect" => array_intersect(it, args)?,
        "array_intersect_key" => {
            if let Some(e) = need_arrays(name, args) {
                return Err(e);
            }
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
                    // Single-array calls preserve keys (composer's
                    // autoload_files.php fileIdentifiers rely on it);
                    // multi-array zips renumber — PHP semantics.
                    for (k, c) in a.borrow().iter() {
                        let v = it.call_value(
                            &cb,
                            crate::interp::CallArgs::positional(vec![cell(c.borrow().clone())]),
                        )?;
                        out.set(k.clone(), v);
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
            // zend passes userdata to the callback ONLY when the caller
            // supplied it — otherwise the cb gets (value, key) exactly
            // (bug24658's `typehint(1, 1)` trace frame).
            let extra = if args.len() > 2 {
                Some(arg(args, 2))
            } else {
                None
            };
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
                    let mut cb_args = vec![cell(v), cell(Value::str(plain))];
                    if let Some(e) = &extra {
                        cb_args.push(cell(e.clone()));
                    }
                    it.call_value(&cb, crate::interp::CallArgs::positional(cb_args))?;
                }
                if walked {
                    return Ok(Some(Value::Bool(true)));
                }
            }
            if let Some(rc) = it.arr_mut(&args[0]) {
                let cells: Vec<(ArrKey, Cell)> = rc.borrow().iter().cloned().collect();
                for (k, c) in cells {
                    let mut cb_args = vec![
                        c.clone(),
                        cell(match k {
                            ArrKey::Int(i) => Value::Int(i),
                            ArrKey::Str(s) => Value::str(s.to_string()),
                            ArrKey::Tomb => Value::Null,
                        }),
                    ];
                    if let Some(e) = &extra {
                        cb_args.push(cell(e.clone()));
                    }
                    it.call_value(&cb, crate::interp::CallArgs::positional(cb_args))?;
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
            sort_array(it, &args[0], name, args.get(1))?;
            Value::Bool(true)
        }
        "array_multisort" => {
            if args.is_empty() {
                return err(
                    "ArgumentCountError",
                    "array_multisort() expects at least 1 argument, 0 given",
                );
            }
            // Columns are arrays; scalar Int args are flags on the last
            // column (one order flag + one type flag each).
            struct Col {
                arr: Rc<RefCell<PhpArray>>,
                desc: bool,
                flag: u8,
            }
            let mut cols: Vec<Col> = Vec::new();
            let mut order_set = false;
            let mut type_set = false;
            for (i, a) in args.iter().enumerate() {
                let n = i + 1;
                match &*a.borrow() {
                    Value::Array(rc) => {
                        cols.push(Col {
                            arr: rc.clone(),
                            desc: false,
                            flag: 0,
                        });
                        order_set = false;
                        type_set = false;
                    }
                    Value::Int(flag) => {
                        if cols.is_empty() {
                            let msg = if n == 1 {
                                "array_multisort(): Argument #1 ($array) must be an array or a sort flag that has not already been specified".to_string()
                            } else {
                                format!("array_multisort(): Argument #{} must be an array or a sort flag that has not already been specified", n)
                            };
                            return err("TypeError", msg);
                        }
                        let base = flag & !8; // strip SORT_FLAG_CASE
                        let is_order = matches!(base, 3 | 4);
                        let is_type = *flag == 8 || matches!(base, 0 | 1 | 2 | 5 | 6);
                        if !is_order && !is_type {
                            return err(
                                "ValueError",
                                format!(
                                    "array_multisort(): Argument #{} must be a valid sort flag",
                                    n
                                ),
                            );
                        }
                        if is_order {
                            if order_set {
                                return err(
                                    "TypeError",
                                    format!("array_multisort(): Argument #{} must be an array or a sort flag that has not already been specified", n),
                                );
                            }
                            cols.last_mut().unwrap().desc = base == 3;
                            order_set = true;
                        } else {
                            if type_set {
                                return err(
                                    "TypeError",
                                    format!("array_multisort(): Argument #{} must be an array or a sort flag that has not already been specified", n),
                                );
                            }
                            cols.last_mut().unwrap().flag = *flag as u8;
                            type_set = true;
                        }
                    }
                    _ => {
                        let msg = if n == 1 {
                            "array_multisort(): Argument #1 ($array) must be an array or a sort flag".to_string()
                        } else {
                            format!(
                                "array_multisort(): Argument #{} must be an array or a sort flag",
                                n
                            )
                        };
                        return err("TypeError", msg);
                    }
                }
            }
            // Snapshot all columns' rows and check sizes.
            let mut col_entries: Vec<Vec<(ArrKey, Cell)>> = Vec::new();
            let mut n_rows = 0usize;
            for (i, col) in cols.iter().enumerate() {
                let entries = col.arr.borrow().entries.clone();
                if i == 0 {
                    n_rows = entries.len();
                } else if entries.len() != n_rows {
                    return err("ValueError", "Array sizes are inconsistent");
                }
                col_entries.push(entries);
            }
            let mut pending: Deferred = None;
            let mut perm: Vec<usize> = (0..n_rows).collect();
            perm.sort_by(|&x, &y| {
                for (ci, col) in cols.iter().enumerate() {
                    let va = col_entries[ci][x].1.borrow();
                    let vb = col_entries[ci][y].1.borrow();
                    let c = data_cmp(it, &va, &vb, col.flag as i64, &mut pending);
                    let c = if col.desc { c.reverse() } else { c };
                    if c != std::cmp::Ordering::Equal {
                        return c;
                    }
                }
                std::cmp::Ordering::Equal
            });
            if let Some(e) = deferred_err(it, pending) {
                return Err(e);
            }
            // Write each column back: numeric keys renumber, others preserved.
            for (ci, col) in cols.iter().enumerate() {
                let mut arr = col.arr.borrow_mut();
                let entries = &col_entries[ci];
                arr.entries.clear();
                arr.next = 0;
                for (k, &pi) in perm.iter().enumerate() {
                    let (key, val) = &entries[pi];
                    let new_key = match key {
                        ArrKey::Int(_) => ArrKey::Int(k as i64),
                        other => other.clone(),
                    };
                    if let ArrKey::Int(x) = new_key {
                        arr.next = arr.next.max(x + 1);
                    }
                    arr.entries.push((new_key, val.clone()));
                }
                arr.iter_pos = arr.entries.len();
            }
            Value::Bool(true)
        }
        "compact" => {
            // Missing names warn 'Undefined variable $x' and are
            // skipped; defined-but-null names land in the result.
            // zend walks args left-to-right, expanding array elements
            // in place; a self-referential array hits zend's hash
            // recursion guard — catchable Error 'Recursion detected'.
            let mut out = PhpArray::new();
            let mut active = std::collections::HashSet::new();
            for c in args {
                compact_one(it, &c.borrow().clone(), &mut out, &mut active)?;
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
                .ptr_entry()
                .map(|(_, c)| c.borrow().clone())
                .unwrap_or(Value::Bool(false)),
            _ => Value::Bool(false),
        },
        "end" => match arg(args, 0) {
            Value::Array(a) => {
                let mut b = a.borrow_mut();
                b.iter_pos = b.entries.len();
                b.ptr_retreat();
                b.ptr_entry()
                    .map(|(_, c)| c.borrow().clone())
                    .unwrap_or(Value::Bool(false))
            }
            _ => Value::Bool(false),
        },
        "reset" => match arg(args, 0) {
            Value::Array(a) => {
                let mut b = a.borrow_mut();
                b.iter_pos = 0;
                b.ptr_entry()
                    .map(|(_, c)| c.borrow().clone())
                    .unwrap_or(Value::Bool(false))
            }
            _ => Value::Bool(false),
        },
        "key" => match arg(args, 0) {
            Value::Array(a) => a
                .borrow()
                .ptr_entry()
                .map(|(k, _)| match k {
                    ArrKey::Int(i) => Value::Int(*i),
                    ArrKey::Str(s) => Value::str(s.to_string()),
                    ArrKey::Tomb => Value::Null,
                })
                .unwrap_or(Value::Null),
            _ => Value::Null,
        },
        "next" => match arg(args, 0) {
            Value::Array(a) => {
                let mut b = a.borrow_mut();
                b.ptr_advance();
                b.ptr_entry()
                    .map(|(_, c)| c.borrow().clone())
                    .unwrap_or(Value::Bool(false))
            }
            _ => Value::Bool(false),
        },
        "prev" => match arg(args, 0) {
            Value::Array(a) => {
                let mut b = a.borrow_mut();
                b.ptr_retreat();
                b.ptr_entry()
                    .map(|(_, c)| c.borrow().clone())
                    .unwrap_or(Value::Bool(false))
            }
            _ => Value::Bool(false),
        },
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
        "array_key_exists_slow" => Value::Null,
        _ => return Ok(None),
    }))
}

// ----- helpers -----

/// zend_hash_sort compacts tombstones and dups the array for user
/// sorts — either way the compare loop sees the live entries only.
fn sort_snapshot(entries: &[(ArrKey, Cell)]) -> Vec<crate::value::SortElem> {
    entries
        .iter()
        .filter(|(k, _)| !matches!(k, ArrKey::Tomb))
        .cloned()
        .enumerate()
        .map(|(i, (k, c))| (i as u32, k, c))
        .collect()
}

/// `php_get_data_compare_func`: `sort_type & ~SORT_FLAG_CASE` picks the
/// comparator — SORT_NUMERIC is a THREEWAY on zval_get_double,
/// SORT_STRING/SORT_LOCALE_STRING strcmp (+8 → strcasecmp) on
/// zval_get_tmp_string (deferred conversion errors land in `pending`),
/// SORT_NATURAL strnatcmp (+8 → strnatcasecmp), anything else is
/// SORT_REGULAR's zend_compare.
fn data_cmp(
    it: &mut Interp,
    x: &Value,
    y: &Value,
    flag: i64,
    pending: &mut Deferred,
) -> std::cmp::Ordering {
    let ci = flag & 8 != 0;
    match flag & !8 {
        1 => crate::value::num_cmp(x.to_float(), y.to_float()),
        2 | 5 => {
            let xs = ztmp_str(it, x, pending);
            let ys = ztmp_str(it, y, pending);
            if ci {
                fold_case(&xs).cmp(&fold_case(&ys))
            } else {
                xs.cmp(&ys)
            }
        }
        6 => {
            let xs = ztmp_str(it, x, pending);
            let ys = ztmp_str(it, y, pending);
            natcmp(&xs, &ys, ci)
        }
        _ => compare(x, y),
    }
}

/// `php_get_key_compare_func`: SORT_NUMERIC parses leading-numerics
/// (`zend_strtod` — "2a" → 2.0, "x" → 0.0); string-ish flags compare
/// the key's printed form; anything else is `php_array_key_compare` —
/// int×int ±1, str×str `zendi_smart_strcmp`, mixed zend_compare.
fn key_cmp(x: &ArrKey, y: &ArrKey, flag: i64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ci = flag & 8 != 0;
    match flag & !8 {
        1 => crate::value::num_cmp(key_strtod(x), key_strtod(y)),
        2 | 5 => {
            let xs = key_bytes(x);
            let ys = key_bytes(y);
            if ci {
                fold_case(&xs).cmp(&fold_case(&ys))
            } else {
                xs.cmp(&ys)
            }
        }
        6 => natcmp(&key_bytes(x), &key_bytes(y), ci),
        _ => match (x, y) {
            // Unique keys → zend never reports int keys equal.
            (ArrKey::Int(a), ArrKey::Int(b)) => {
                if a > b {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (ArrKey::Str(a), ArrKey::Str(b)) => {
                crate::value::smart_strcmp(a.as_bytes(), b.as_bytes())
            }
            _ => compare(
                &crate::interp::util::key_value(x),
                &crate::interp::util::key_value(y),
            ),
        },
    }
}

fn key_bytes(k: &ArrKey) -> Vec<u8> {
    match k {
        ArrKey::Int(i) => i.to_string().into_bytes(),
        ArrKey::Str(s) => s.as_bytes().to_vec(),
        ArrKey::Tomb => Vec::new(),
    }
}

/// zend_strtod on the key's printed form — leading-parse only.
fn key_strtod(k: &ArrKey) -> f64 {
    match k {
        ArrKey::Int(i) => *i as f64,
        ArrKey::Str(s) => match crate::value::numeric(s.as_bytes()) {
            crate::value::Numeric::Int(i) => i as f64,
            crate::value::Numeric::Float(f) | crate::value::Numeric::Leading(f, _) => f,
            crate::value::Numeric::NonNumeric => 0.0,
        },
        ArrKey::Tomb => 0.0,
    }
}

/// The zend_sort driver every flag-taking sort goes through (global
/// builtins AND the SPL ArrayObject/ArrayIterator methods — zend's SPL
/// hands its storage HashTable straight to the same functions). Takes a
/// pre-cloned entry list — no borrow may be held while user code
/// (`__toString`, error handlers) runs inside a comparator — and
/// returns the sorted entries with the depth-err flag so the caller
/// writes back wherever its storage resolves and throws "Nesting level
/// too deep" only after the array has been permuted like zend's.
pub(crate) fn zend_sort_flags(
    it: &mut Interp,
    entries: &[(ArrKey, Cell)],
    flag: i64,
    desc: bool,
    by_key: bool,
) -> (Vec<(ArrKey, Cell)>, bool, Option<PhpError>) {
    crate::value::clear_cmp_depth_err();
    let mut pending: Deferred = None;
    let mut deep = false;
    let mut v = sort_snapshot(entries);
    crate::value::zend_sort(&mut v, &mut |x, y| {
        let r = if by_key {
            key_cmp(&x.1, &y.1, flag)
        } else {
            data_cmp(it, &x.2.borrow(), &y.2.borrow(), flag, &mut pending)
        };
        deep |= crate::value::cmp_depth_err();
        // Reverse sorts in zend are `inner(a, b) * -1` — result negation
        // (a cyclic UNCOMPARABLE keeps sorting as "less"), not a swap
        // of operands.
        let r = if desc { r.reverse() } else { r };
        if r != std::cmp::Ordering::Equal {
            r
        } else {
            // RETURN_STABLE_SORT: ties fall back on insertion position.
            x.0.cmp(&y.0)
        }
    });
    // zend sorts ht->arData in place: a mid-sort conversion Error
    // leaves the partially-permuted table visible after the throw.
    let conv_err = deferred_err(it, pending);
    (
        v.into_iter().map(|(_, k, c)| (k, c)).collect(),
        deep,
        conv_err,
    )
}

/// `php_usort` family: zend dups the array so the callback sees the
/// pre-sort contents (a callback's own writes to the argument are
/// discarded when the sorted copy replaces it — snapshot, don't take).
/// A bool retval deprecates once per sort; `false` retries the
/// comparison with swapped operands and negates the result. A thrown
/// error keeps the sort running (retval UNDEF → 0 → equal); the caller
/// still writes the sorted entries back, then the error propagates.
pub(crate) fn zend_sort_user(
    it: &mut Interp,
    entries: &[(ArrKey, Cell)],
    cb: &Value,
    by_key: bool,
    fname: &str,
) -> (Vec<(ArrKey, Cell)>, Option<PhpError>) {
    crate::value::clear_cmp_depth_err();
    let mut cb_err: Option<PhpError> = None;
    let mut dep_thrown = false;
    let mut v = sort_snapshot(entries);
    let call = |it: &mut Interp, x: &crate::value::SortElem, y: &crate::value::SortElem| {
        // zend passes the bucket zvals BY VALUE — a `&$k` param warns
        // "must be passed by reference, value given" and binds a copy,
        // so callback writes can never reach the sorted storage.
        let args = if by_key {
            crate::interp::CallArgs::positional(vec![
                cell(crate::interp::util::key_value(&x.1)),
                cell(crate::interp::util::key_value(&y.1)),
            ])
        } else {
            crate::interp::CallArgs::positional(vec![
                cell(x.2.borrow().clone()),
                cell(y.2.borrow().clone()),
            ])
        };
        let mut args = args;
        args.nonref_cells = vec![0, 1];
        it.call_value(cb, args)
    };
    crate::value::zend_sort(&mut v, &mut |x, y| {
        // zend keeps the comparator running after an exception but the
        // call short-circuits (retval UNDEF → 0) — the callback body
        // does NOT execute again once it threw.
        if cb_err.is_some() {
            return x.0.cmp(&y.0);
        }
        let r = match call(it, x, y) {
            Err(e) => {
                if cb_err.is_none() {
                    cb_err = Some(e);
                }
                std::cmp::Ordering::Equal
            }
            Ok(Value::Bool(true)) => {
                if !dep_thrown {
                    dep_thrown = true;
                    let r = it.deprecated_pub(&format!(
                        "{}(): Returning bool from comparison function is deprecated, return an integer less than, equal to, or greater than zero",
                        fname
                    ));
                    if let Err(e) = r {
                        if cb_err.is_none() {
                            cb_err = Some(e);
                        }
                    }
                }
                // php_get_long(true) → 1 → NORMALIZE → greater.
                std::cmp::Ordering::Greater
            }
            Ok(Value::Bool(false)) => {
                if !dep_thrown {
                    dep_thrown = true;
                    let r = it.deprecated_pub(&format!(
                        "{}(): Returning bool from comparison function is deprecated, return an integer less than, equal to, or greater than zero",
                        fname
                    ));
                    if let Err(e) = r {
                        if cb_err.is_none() {
                            cb_err = Some(e);
                        }
                    }
                }
                // zend retries the swapped pair and NEGATES the result.
                match call(it, y, x) {
                    Err(e) => {
                        if cb_err.is_none() {
                            cb_err = Some(e);
                        }
                        std::cmp::Ordering::Equal
                    }
                    Ok(r) => match r.to_int() {
                        i if i > 0 => std::cmp::Ordering::Less,
                        i if i < 0 => std::cmp::Ordering::Greater,
                        _ => std::cmp::Ordering::Equal,
                    },
                }
            }
            // php_get_long on the retval, ZEND_NORMALIZE_BOOL.
            Ok(r) => match r.to_int() {
                i if i > 0 => std::cmp::Ordering::Greater,
                i if i < 0 => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Equal,
            },
        };
        if r != std::cmp::Ordering::Equal {
            r
        } else {
            x.0.cmp(&y.0)
        }
    });
    let sorted: Vec<(ArrKey, Cell)> = v.into_iter().map(|(_, k, c)| (k, c)).collect();
    // zend replaces the arg's array with the sorted dup BEFORE the
    // pending callback error propagates — the write-back is the
    // caller's job on either outcome.
    (sorted, cb_err)
}

fn need_callback_arg(it: &mut Interp, name: &str, cb: &Value) -> PhpError {
    let detail = it.zpp_callback_detail(cb);
    PhpError::uncaught(
        "TypeError",
        format!(
            "{}(): Argument #2 ($callback) must be a valid callback, {}",
            name, detail
        ),
        it.cur_line,
    )
}

fn sort_array(
    it: &mut Interp,
    cell: &Cell,
    name: &str,
    cb_arg: Option<&Cell>,
) -> Result<(), PhpError> {
    let renumber = matches!(name, "sort" | "rsort" | "usort" | "shuffle");
    match name {
        "sort" | "rsort" | "asort" | "arsort" | "ksort" | "krsort" | "natsort" | "natcasesort" => {
            let Some(arr) = it.arr_mut(cell) else {
                return Ok(());
            };
            let src = arr.borrow().entries.clone();
            let (flag, desc, by_key) = match name {
                "natsort" => (6, false, false),
                "natcasesort" => (6 | 8, false, false),
                _ => (
                    cb_arg.map(|c| c.borrow().to_int()).unwrap_or(0),
                    matches!(name, "rsort" | "arsort" | "krsort"),
                    matches!(name, "ksort" | "krsort"),
                ),
            };
            let (sorted, deep, conv_err) = zend_sort_flags(it, &src, flag, desc, by_key);
            // Write back through a fresh resolve — a `__toString` mid-sort
            // may have COW-split the arg cell to another table. zend's
            // in-place arData sort means the partial permutation sticks
            // even when a conversion Error aborts it.
            if let Some(rc) = it.arr_mut(cell) {
                rc.borrow_mut().entries = sorted;
            }
            if deep {
                return depth_err();
            }
            if let Some(e) = conv_err {
                return Err(e);
            }
        }
        "usort" | "uasort" | "uksort" => {
            if let Some(cbc) = cb_arg {
                let cb = cbc.borrow().clone();
                if !it.is_callable_value(&cb) {
                    return Err(need_callback_arg(it, name, &cb));
                }
                let Some(arr) = it.arr_mut(cell) else {
                    return Ok(());
                };
                let (src, next) = {
                    let a = arr.borrow();
                    (a.entries.clone(), a.next)
                };
                let (sorted, cb_err) = zend_sort_user(it, &src, &cb, name == "uksort", name);
                // zend assigns the sorted dup into the arg zval —
                // whatever the callback wrote mid-sort is discarded.
                let mut out = PhpArray::new();
                out.entries = sorted;
                out.next = next;
                *cell.borrow_mut() = Value::Array(Rc::new(RefCell::new(out)));
                if let Some(e) = cb_err {
                    return Err(e);
                }
            }
        }
        "shuffle" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let mut x = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(1);
            if let Some(arr) = it.arr_mut(cell) {
                let mut a = arr.borrow_mut();
                for i in (1..a.entries.len()).rev() {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let j = (x as usize) % (i + 1);
                    a.entries.swap(i, j);
                }
            }
        }
        _ => {}
    }
    if let Some(arr) = it.arr_mut(cell) {
        let mut a = arr.borrow_mut();
        if renumber {
            // renumber=1 rewrites every key — string keys are
            // destroyed, not canonicalized.
            let mut i = 0;
            for (k, _) in a.entries.iter_mut() {
                *k = ArrKey::Int(i);
                i += 1;
            }
            a.next = i;
        }
        a.iter_pos = 0;
    }
    Ok(())
}

/// `compact` element walk: strings look up scope vars (missing →
/// 'Undefined variable' warning), arrays expand in place.
/// `active` tracks arrays mid-expansion — a self-referential element
/// hits zend's hash-recursion guard ('Recursion detected' Error).
fn compact_one(
    it: &mut Interp,
    v: &Value,
    out: &mut PhpArray,
    active: &mut std::collections::HashSet<usize>,
) -> Result<(), PhpError> {
    match v {
        Value::Str(s) => {
            let n = crate::value::lossy(s);
            match it.lookup_var(&n) {
                Some(val) => out.set(ArrKey::Str(n.into_owned().into()), val),
                None => {
                    it.warn_pub(&format!("compact(): Undefined variable ${}", n))?;
                }
            }
        }
        Value::Array(a) => {
            let id = Rc::as_ptr(a) as usize;
            if !active.insert(id) {
                let e = it.exception("Error", "Recursion detected");
                return Err(it.throw_value(e));
            }
            let entries: Vec<Value> = a
                .borrow()
                .entries
                .iter()
                .map(|(_, c)| c.borrow().clone())
                .collect();
            for e in &entries {
                compact_one(it, e, out, active)?;
            }
            active.remove(&id);
        }
        _ => {}
    }
    Ok(())
}

fn is_varname(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn num_val(x: f64, orig: Value, _step: f64) -> Value {
    // int-preserve: when the range bound is int and value is integral → int
    if matches!(orig, Value::Int(_)) && x.fract() == 0.0 && x.abs() < 9e15 {
        Value::Int(x as i64)
    } else {
        Value::Float(x)
    }
}

fn fold_case(b: &[u8]) -> Vec<u8> {
    b.iter().map(|c| c.to_ascii_uppercase()).collect()
}

/// Port of PHP's strnatcmp_ex (ext/standard/strnatcmp.c).
pub(crate) fn natcmp(a: &[u8], b: &[u8], ci: bool) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    fn digit(c: u8) -> bool {
        c.is_ascii_digit()
    }
    fn space(c: u8) -> bool {
        c.is_ascii_whitespace()
    }
    fn compare_right(
        a: &[u8],
        mut i: usize,
        b: &[u8],
        mut j: usize,
    ) -> (std::cmp::Ordering, usize, usize) {
        let mut bias = Equal;
        loop {
            let da = i < a.len() && digit(a[i]);
            let db = j < b.len() && digit(b[j]);
            match (da, db) {
                (false, false) => return (bias, i, j),
                (false, true) => return (Less, i, j),
                (true, false) => return (Greater, i, j),
                (true, true) => {
                    if bias == Equal {
                        if a[i] < b[j] {
                            bias = Less;
                        } else if a[i] > b[j] {
                            bias = Greater;
                        }
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
    }
    fn compare_left(
        a: &[u8],
        mut i: usize,
        b: &[u8],
        mut j: usize,
    ) -> (std::cmp::Ordering, usize, usize) {
        loop {
            let da = i < a.len() && digit(a[i]);
            let db = j < b.len() && digit(b[j]);
            match (da, db) {
                (false, false) => return (Equal, i, j),
                (false, true) => return (Less, i, j),
                (true, false) => return (Greater, i, j),
                (true, true) => {
                    if a[i] != b[j] {
                        return (a[i].cmp(&b[j]), i, j);
                    }
                    i += 1;
                    j += 1;
                }
            }
        }
    }
    if a.is_empty() || b.is_empty() {
        return a.len().cmp(&b.len());
    }
    let (mut i, mut j) = (0usize, 0usize);
    // skip over leading zeros
    while a[i] == b'0' && i + 1 < a.len() && digit(a[i + 1]) {
        i += 1;
    }
    while b[j] == b'0' && j + 1 < b.len() && digit(b[j + 1]) {
        j += 1;
    }
    let (mut ca, mut cb) = (a[i], b[j]);
    loop {
        while i < a.len() && space(ca) {
            i += 1;
            ca = a[i];
        }
        while j < b.len() && space(cb) {
            j += 1;
            cb = b[j];
        }
        if digit(ca) && digit(cb) {
            let (r, ni, nj) = if ca == b'0' || cb == b'0' {
                compare_left(a, i, b, j)
            } else {
                compare_right(a, i, b, j)
            };
            if r != Equal {
                return r;
            }
            i = ni;
            j = nj;
            if i >= a.len() && j >= b.len() {
                return Equal;
            } else if i >= a.len() {
                return Less;
            } else if j >= b.len() {
                return Greater;
            }
            ca = a[i];
            cb = b[j];
        }
        let (xa, xb) = if ci {
            (ca.to_ascii_uppercase(), cb.to_ascii_uppercase())
        } else {
            (ca, cb)
        };
        match xa.cmp(&xb) {
            Equal => {}
            r => return r,
        }
        i += 1;
        j += 1;
        if i >= a.len() && j >= b.len() {
            return Equal;
        } else if i >= a.len() {
            return Less;
        } else if j >= b.len() {
            return Greater;
        }
        ca = a[i];
        cb = b[j];
    }
}

/// `zend_argument_type_error` for the array family: arg #1 names
/// `$array`, the variadic rest is nameless.
fn need_array_arg(name: &str, n: usize, v: &Value) -> PhpError {
    if n == 1 {
        PhpError::uncaught(
            "TypeError",
            format!(
                "{}(): Argument #1 ($array) must be of type array, {} given",
                name,
                zval_word(v)
            ),
            0,
        )
    } else {
        PhpError::uncaught(
            "TypeError",
            format!(
                "{}(): Argument #{} must be of type array, {} given",
                name,
                n,
                zval_word(v)
            ),
            0,
        )
    }
}

/// `Z_PARAM_VARIADIC('+')` arity: at least one argument.
fn need_args(name: &str, args: &[Cell]) -> Result<(), PhpError> {
    if args.is_empty() {
        return Err(PhpError::uncaught(
            "ArgumentCountError",
            format!("{}() expects at least 1 argument, 0 given", name),
            0,
        ));
    }
    Ok(())
}

/// Check every arg is an array, in order (zend validates args[i] lazily
/// while each list is built — but for the simple key-only walks it is
/// just the in-order TypeError).
fn need_arrays(name: &str, args: &[Cell]) -> Option<PhpError> {
    if let Err(e) = need_args(name, args) {
        return Some(e);
    }
    for (i, a) in args.iter().enumerate() {
        if !matches!(&*a.borrow(), Value::Array(_)) {
            return Some(need_array_arg(name, i + 1, &a.borrow()));
        }
    }
    None
}

/// A conversion error deferred the way `zval_get_tmp_string` defers it
/// in zend: the cast Error stays pending in EG(exception), execution
/// continues with "", and the FIRST exception propagates when the C
/// function returns. Later failed casts see EG(exception) already set
/// and stay silent.
enum DeferredErr {
    /// The throwable object a failed cast left pending.
    Exc(Value),
    /// A non-throwable conversion error.
    Raw(PhpError),
}

type Deferred = Option<DeferredErr>;

/// `zval_get_tmp_string` emulation for the diff/intersect family.
fn ztmp_str(it: &mut Interp, v: &Value, pending: &mut Deferred) -> Vec<u8> {
    match v {
        Value::Str(s) => s.to_vec(),
        _ => match it.try_conv_bytes(v) {
            Ok(b) => b,
            Err(e) => {
                if e.kind == crate::error::ErrorKind::Throw {
                    match it.take_pending_exception() {
                        Some(x) => {
                            if pending.is_none() {
                                *pending = Some(DeferredErr::Exc(x));
                            }
                        }
                        None => {
                            if pending.is_none() {
                                *pending = Some(DeferredErr::Raw(e));
                            }
                        }
                    }
                } else if pending.is_none() {
                    *pending = Some(DeferredErr::Raw(e));
                }
                Vec::new()
            }
        },
    }
}

/// `string_compare_function`: binary strcmp over tmp strings.
fn zstr_cmp(it: &mut Interp, a: &Value, b: &Value, pending: &mut Deferred) -> std::cmp::Ordering {
    let sa = ztmp_str(it, a, pending);
    let sb = ztmp_str(it, b, pending);
    sa.cmp(&sb)
}

/// Propagate a deferred conversion error as the builtin's result —
/// re-arms `pending_exception` so `err_flow` hands the saved object to
/// the unwinder (later casts may have overwritten it in the meantime).
fn deferred_err(it: &mut Interp, pending: Deferred) -> Option<PhpError> {
    match pending? {
        DeferredErr::Exc(v) => Some(it.throw_value(v)),
        DeferredErr::Raw(e) => Some(e),
    }
}

/// A deferred error takes precedence over a later type error (zend
/// can't throw the second exception while one is pending).
fn deferred_or<T>(it: &mut Interp, pending: Deferred, e: PhpError) -> Result<T, PhpError> {
    match deferred_err(it, pending) {
        Some(p) => Err(p),
        None => Err(e),
    }
}

/// 8.5's hash-based `array_diff` (php-src @614b22a+): elements compare
/// by tmp string. arg#1 checks first; a 1-element arg0 casts itself
/// once then scans each arg lazily until a hit; otherwise all
/// args[1..] elements populate an exclude set first, then arg0 scans.
fn array_diff(it: &mut Interp, args: &[Cell]) -> Result<Value, PhpError> {
    need_args("array_diff", args)?;
    let a0 = arg(args, 0);
    let Value::Array(arr) = &a0 else {
        return Err(need_array_arg("array_diff", 1, &a0));
    };
    let elems: Vec<(ArrKey, Cell)> = arr
        .borrow()
        .iter()
        .map(|(k, c)| (k.clone(), c.clone()))
        .collect();
    let mut pending: Deferred = None;
    if elems.is_empty() {
        for (i, o) in args[1..].iter().enumerate() {
            if !matches!(&*o.borrow(), Value::Array(_)) {
                return deferred_or(
                    it,
                    pending,
                    need_array_arg("array_diff", i + 2, &o.borrow()),
                );
            }
        }
        return Ok(Value::Array(Rc::new(RefCell::new(PhpArray::new()))));
    }
    if elems.len() == 1 {
        let search = ztmp_str(it, &elems[0].1.borrow(), &mut pending);
        let mut found = false;
        for (i, o) in args[1..].iter().enumerate() {
            let ob = o.borrow();
            let Value::Array(oa) = &*ob else {
                return deferred_or(it, pending, need_array_arg("array_diff", i + 2, &ob));
            };
            if !found {
                for (_, oc) in oa.borrow().iter() {
                    let s = ztmp_str(it, &oc.borrow(), &mut pending);
                    if s == search {
                        found = true;
                        break;
                    }
                }
            }
        }
        if let Some(e) = deferred_err(it, pending) {
            return Err(e);
        }
        return Ok(if found {
            Value::Array(Rc::new(RefCell::new(PhpArray::new())))
        } else {
            a0
        });
    }
    let mut num = 0usize;
    for (i, o) in args[1..].iter().enumerate() {
        match &*o.borrow() {
            Value::Array(oa) => num += oa.borrow().iter().count(),
            v => {
                return deferred_or(it, pending, need_array_arg("array_diff", i + 2, v));
            }
        }
    }
    if num == 0 {
        return Ok(a0);
    }
    if num >= 0x4000_0000 {
        return err(
            "Error",
            "The total number of elements must be lower than 1073741824",
        );
    }
    let mut exclude: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    for o in &args[1..] {
        if let Value::Array(oa) = &*o.borrow() {
            for (_, oc) in oa.borrow().iter() {
                exclude.insert(ztmp_str(it, &oc.borrow(), &mut pending));
            }
        }
    }
    let mut out = PhpArray::new();
    for (k, c) in &elems {
        let s = ztmp_str(it, &c.borrow(), &mut pending);
        if !exclude.contains(&s) {
            out.set(k.clone(), c.borrow().clone());
        }
    }
    if let Some(e) = deferred_err(it, pending) {
        return Err(e);
    }
    Ok(Value::Array(Rc::new(RefCell::new(out))))
}

/// `php_array_intersect` INTERSECT_NORMAL: sort each arg's bucket list
/// by tmp string, merge-walk, keep arg0 entries found in every other.
fn array_intersect(it: &mut Interp, args: &[Cell]) -> Result<Value, PhpError> {
    need_args("array_intersect", args)?;
    let mut pending: Deferred = None;
    let mut lists: Vec<Vec<(ArrKey, Value)>> = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        let ab = a.borrow();
        let Value::Array(arr) = &*ab else {
            return deferred_or(it, pending, need_array_arg("array_intersect", i + 1, &ab));
        };
        let mut list: Vec<(ArrKey, Value)> = arr
            .borrow()
            .iter()
            .map(|(k, c)| (k.clone(), c.borrow().clone()))
            .collect();
        if list.len() > 1 {
            crate::value::zend_sort(&mut list, &mut |x, y| {
                zstr_cmp(it, &x.1, &y.1, &mut pending)
            });
        }
        lists.push(list);
    }
    let argc = lists.len();
    let mut ptrs = vec![0usize; argc];
    let mut deleted: Vec<ArrKey> = Vec::new();
    let mut c;
    'out: while ptrs[0] < lists[0].len() {
        c = std::cmp::Ordering::Equal;
        let mut i = 1;
        while i < argc {
            while ptrs[i] < lists[i].len() {
                c = zstr_cmp(it, &lists[0][ptrs[0]].1, &lists[i][ptrs[i]].1, &mut pending);
                if c != std::cmp::Ordering::Greater {
                    break;
                }
                ptrs[i] += 1;
            }
            if ptrs[i] >= lists[i].len() {
                // arg i exhausted → nothing left of arg0 can match it:
                // delete the rest and stop.
                while ptrs[0] < lists[0].len() {
                    deleted.push(lists[0][ptrs[0]].0.clone());
                    ptrs[0] += 1;
                }
                break 'out;
            }
            if c != std::cmp::Ordering::Equal {
                break;
            }
            ptrs[i] += 1;
            i += 1;
        }
        if c != std::cmp::Ordering::Equal {
            // delete arg0 entries while they stay below ptrs[i]
            loop {
                deleted.push(lists[0][ptrs[0]].0.clone());
                ptrs[0] += 1;
                if ptrs[0] >= lists[0].len() {
                    break 'out;
                }
                if zstr_cmp(it, &lists[0][ptrs[0]].1, &lists[i][ptrs[i]].1, &mut pending)
                    != std::cmp::Ordering::Less
                {
                    break;
                }
            }
        } else {
            // kept — skip same-valued run (compare order matches zend)
            loop {
                ptrs[0] += 1;
                if ptrs[0] >= lists[0].len() {
                    break 'out;
                }
                if zstr_cmp(
                    it,
                    &lists[0][ptrs[0] - 1].1,
                    &lists[0][ptrs[0]].1,
                    &mut pending,
                ) != std::cmp::Ordering::Equal
                {
                    break;
                }
            }
        }
    }
    let mut out = PhpArray::new();
    if let Value::Array(a) = arg(args, 0) {
        for (k, cell_v) in a.borrow().iter() {
            if !deleted.contains(k) {
                out.set(k.clone(), cell_v.borrow().clone());
            }
        }
    }
    if let Some(e) = deferred_err(it, pending) {
        return Err(e);
    }
    Ok(Value::Array(Rc::new(RefCell::new(out))))
}

/// `php_array_diff_key` / `php_array_intersect_key` with internal data
/// compare (`zval_compare` = string cmp): diff keeps arg0 entries whose
/// key is absent from every other array OR whose value differs;
/// intersect keeps entries whose key is in every other array with an
/// equal tmp string.
fn array_assoc_match(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
    diff: bool,
) -> Result<Value, PhpError> {
    if let Some(e) = need_arrays(name, args) {
        return Err(e);
    }
    let Value::Array(arr) = arg(args, 0) else {
        unreachable!()
    };
    let mut pending: Deferred = None;
    let mut out = PhpArray::new();
    'entry: for (k, c) in arr.borrow().iter() {
        let v = c.borrow();
        for o in args[1..].iter() {
            let ob = o.borrow();
            let oc = match &*ob {
                Value::Array(oa) => oa.borrow().get_cell(k),
                _ => unreachable!(),
            };
            match oc {
                Some(oc) => {
                    let equal =
                        zstr_cmp(it, &v, &oc.borrow(), &mut pending) == std::cmp::Ordering::Equal;
                    // diff drops on a same-key equal value; intersect
                    // drops on a same-key unequal one.
                    if equal == diff {
                        continue 'entry;
                    }
                }
                None => {
                    if !diff {
                        continue 'entry;
                    }
                }
            }
        }
        out.set(k.clone(), v.clone());
    }
    if let Some(e) = deferred_err(it, pending) {
        return Err(e);
    }
    Ok(Value::Array(Rc::new(RefCell::new(out))))
}
