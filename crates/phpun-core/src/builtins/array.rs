//! Array builtins: count/search/sort/splice and the internal array pointer.

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
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
        "array_diff_assoc" => {
            let mut out = PhpArray::new();
            if let Value::Array(a) = arg(args, 0) {
                'outer: for (k, c) in a.borrow().iter() {
                    let v = c.borrow().clone();
                    for other in &args[1..] {
                        if let Value::Array(o) = &*other.borrow() {
                            if let Some(oc) = o.borrow().get_cell(k) {
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
        "array_intersect_assoc" => {
            let mut out = PhpArray::new();
            if let Value::Array(a) = arg(args, 0) {
                'outer: for (k, c) in a.borrow().iter() {
                    let v = c.borrow().clone();
                    for other in &args[1..] {
                        if let Value::Array(o) = &*other.borrow() {
                            let hit = o.borrow().get_cell(k).is_some_and(|oc| {
                                compare(&v, &oc.borrow()) == std::cmp::Ordering::Equal
                            });
                            if !hit {
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
            let mut perm: Vec<usize> = (0..n_rows).collect();
            perm.sort_by(|&x, &y| {
                for (ci, col) in cols.iter().enumerate() {
                    let va = col_entries[ci][x].1.borrow();
                    let vb = col_entries[ci][y].1.borrow();
                    let c = ms_cmp(&va, &vb, col.flag);
                    let c = if col.desc { c.reverse() } else { c };
                    if c != std::cmp::Ordering::Equal {
                        return c;
                    }
                }
                std::cmp::Ordering::Equal
            });
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
        "each" => match arg(args, 0) {
            Value::Array(a) => {
                let mut b = a.borrow_mut();
                match b.ptr_entry() {
                    Some((k, c)) => {
                        let mut r = PhpArray::new();
                        let kv = match k {
                            ArrKey::Int(i) => Value::Int(*i),
                            ArrKey::Str(s) => Value::str(s.to_string()),
                            ArrKey::Tomb => Value::Null,
                        };
                        let vv = c.borrow().clone();
                        r.set(ArrKey::Int(1), vv.clone());
                        r.set(ArrKey::Str("value".into()), vv);
                        r.set(ArrKey::Int(0), kv.clone());
                        r.set(ArrKey::Str("key".into()), kv);
                        b.ptr_advance();
                        Value::Array(Rc::new(RefCell::new(r)))
                    }
                    None => Value::Bool(false),
                }
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

fn sort_array(
    it: &mut Interp,
    arr: &mut PhpArray,
    name: &str,
    cb_arg: Option<&Cell>,
) -> Result<(), PhpError> {
    match name {
        "sort" | "rsort" => {
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
        // natsort/natcasesort compare naturally and keep keys (like asort).
        "natsort" | "natcasesort" => {
            let ci = name == "natcasesort";
            arr.entries.sort_by(|(_, a), (_, b)| {
                natcmp(&a.borrow().to_php_bytes(), &b.borrow().to_php_bytes(), ci)
            });
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
    arr.iter_pos = 0;
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

/// Column comparator for array_multisort: flag = sort-type|sort-flag-case.
fn ms_cmp(a: &Value, b: &Value, flag: u8) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let base = flag & !8;
    let ci = flag & 8 != 0;
    match base {
        1 => a
            .to_float()
            .partial_cmp(&b.to_float())
            .unwrap_or(Ordering::Equal),
        2 | 5 => {
            // SORT_STRING / SORT_LOCALE_STRING (C locale → plain bytes)
            let x = a.to_php_bytes();
            let y = b.to_php_bytes();
            if ci {
                fold_case(&x).cmp(&fold_case(&y))
            } else {
                x.cmp(&y)
            }
        }
        6 => natcmp(&a.to_php_bytes(), &b.to_php_bytes(), ci),
        _ => compare(a, b), // SORT_REGULAR (and bare SORT_FLAG_CASE)
    }
}

fn fold_case(b: &[u8]) -> Vec<u8> {
    b.iter().map(|c| c.to_ascii_uppercase()).collect()
}

/// Port of PHP's strnatcmp_ex (ext/standard/strnatcmp.c).
fn natcmp(a: &[u8], b: &[u8], ci: bool) -> std::cmp::Ordering {
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
