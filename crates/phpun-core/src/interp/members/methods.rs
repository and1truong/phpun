//! Method dispatch + magic: method calls, `__call`/`__get` magic
//! routing, visibility errors and prototypes, throwable plumbing,
//! native `ArrayIterator`/callable dispatch helpers.

use super::*;

/// Parsed legacy spl `serialize()` payload: (flags, storage, props).
type AoUnserData = (i64, Rc<RefCell<PhpArray>>, Rc<RefCell<PhpArray>>);

impl<'a> Interp<'a> {
    /// Native bodies for the ArrayIterator/ArrayObject stubs.
    /// Iteration state lives in the `ArrayIter` object internal;
    /// unknown methods return None so the generic dispatch can report
    /// `Call to undefined method`.
    pub(in crate::interp) fn array_iter_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        args: &CallArgs,
    ) -> Result<Option<Value>, PhpError> {
        // Internal calls still get a backtrace frame — zend renders
        // `ArrayObject->unserialize('O:11:"ArrayObje...')` in uncaught
        // traces (arg repr truncates at 15 chars via trace_arg).
        self.call_trace.push(TraceFrame {
            file: self.diag_file(),
            line: self.cur_line as u32,
            function: name.to_string(),
            class: Some(obj.borrow().class.name().to_string()),
            ty: "->".into(),
            args: args.cells.clone(),
            named_args: args
                .named
                .iter()
                .map(|(n, c, ..)| (n.clone(), c.clone()))
                .collect(),
            internal: true,
        });
        let r = self.array_iter_body(obj, name, args);
        self.call_trace.pop();
        r
    }

    fn array_iter_body(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        args: &CallArgs,
    ) -> Result<Option<Value>, PhpError> {
        let lname = name.to_lowercase();
        // Declaring class Zend reports in deprecation/ctor messages —
        // the family base, not a userland subclass.
        let is_ao = self.obj_is_a(obj, "arrayobject");
        let family = if is_ao {
            "ArrayObject"
        } else {
            "ArrayIterator"
        };
        let canonical = match lname.as_str() {
            "exchangearray" => "exchangeArray",
            "getarraycopy" => "getArrayCopy",
            "getflags" => "getFlags",
            "setflags" => "setFlags",
            "getiterator" => "getIterator",
            "getiteratorclass" => "getIteratorClass",
            "setiteratorclass" => "setIteratorClass",
            "offsetget" => "offsetGet",
            "offsetexists" => "offsetExists",
            "offsetset" => "offsetSet",
            "offsetunset" => "offsetUnset",
            "__serialize" => "__serialize",
            "__unserialize" => "__unserialize",
            "__debuginfo" => "__debugInfo",
            "haschildren" => "hasChildren",
            "getchildren" => "getChildren",
            other => other,
        };
        // ArrayObject-only methods stay undefined on ArrayIterator.
        if !is_ao
            && matches!(
                lname.as_str(),
                "getiterator" | "exchangearray" | "getiteratorclass" | "setiteratorclass"
            )
        {
            return Ok(None);
        }
        // (min, max) arg counts mirroring the Zend stubs.
        let (amin, amax): (usize, usize) = match lname.as_str() {
            "__construct" if is_ao => (0, 3),
            "__construct" => (0, 2),
            "offsetset" => (2, 2),
            "offsetget" | "offsetexists" | "offsetunset" | "append" | "seek" | "setflags"
            | "unserialize" | "__unserialize" | "exchangearray" | "uasort" | "uksort"
            | "setiteratorclass" => (1, 1),
            "asort" | "ksort" => (0, 1),
            "rewind" | "valid" | "current" | "key" | "next" | "count" | "getarraycopy"
            | "getflags" | "natsort" | "natcasesort" | "serialize" | "__serialize"
            | "getiterator" | "getiteratorclass" | "__debuginfo" | "haschildren"
            | "getchildren" => (0, 0),
            _ => return Ok(None),
        };
        let given = args.cells.len();
        if given < amin || given > amax {
            let (word, n) = if amin == amax {
                ("exactly", amin)
            } else if given > amax {
                ("at most", amax)
            } else {
                ("at least", amin)
            };
            let e = self.spl_throw(
                "ArgumentCountError",
                format!(
                    "{}::{}() expects {} {} argument{}, {} given",
                    family,
                    canonical,
                    word,
                    n,
                    if n == 1 { "" } else { "s" },
                    given
                ),
            );
            return self.fail(e);
        }
        if lname == "__construct" {
            // zpp order: arg1 type, arg2 int, arg3 iterator class —
            // then the storage assignment (object deprecation inside).
            let first = args.cells.first().map(|c| c.borrow().clone());
            if let Some(v) = &first {
                if !matches!(v, Value::Array(_) | Value::Object(_)) {
                    let tn = self.zval_type_name(v);
                    let e = self.spl_throw(
                        "TypeError",
                        format!(
                            "{}::{}(): Argument #1 ($array) must be of type array, {} given",
                            family, canonical, tn
                        ),
                    );
                    return self.fail(e);
                }
            }
            let mut flags = 0i64;
            if let Some(f) = args.cells.get(1) {
                let fv = f.borrow().clone();
                match self.spl_int_arg(&fv, family, canonical, 2, "flags") {
                    Ok(i) => flags = i,
                    Err(e) => return self.fail(e),
                }
            }
            let mut iterator_class = None;
            if is_ao {
                if let Some(ic) = args.cells.get(2) {
                    let icv = ic.borrow().clone();
                    match self.ao_iterator_class(&icv, family, canonical, 3) {
                        Ok(n) => iterator_class = Some(n),
                        Err(e) => return self.fail(e),
                    }
                }
            }
            let backing = match &first {
                Some(v) => Some(self.ao_backing(v, family, "__construct")?),
                None => None,
            };
            let src_obj = match &first {
                Some(Value::Object(o)) => Some(o.clone()),
                _ => None,
            };
            // No explicit flags arg → an spl-array source's flags carry
            // over (zend spl_array_object_new_ex); array/plain inputs
            // default to 0.
            if args.cells.len() < 2 {
                if let Some((_, Some(sf))) = &backing {
                    flags = *sf;
                }
            }
            let mut ob = obj.borrow_mut();
            ob.internal = Some(ObjectInternal::ArrayIter {
                arr: backing.map(|(a, _)| a).unwrap_or_default(),
                pos: 0,
                flags,
                iterator_class,
                src: src_obj,
            });
            return Ok(Some(Value::Null));
        }
        // Everything else needs storage — zend lazily creates it on
        // first access (newInstanceWithoutConstructor).
        let (arr, pos, flags) = self.ao_state(obj);
        let v = match lname.as_str() {
            "rewind" => {
                self.ao_set_pos(obj, 0);
                Value::Null
            }
            "valid" => Value::Bool(pos < arr.borrow().len()),
            "current" => arr
                .borrow()
                .iter()
                .nth(pos)
                .map(|(_, c)| c.borrow().clone())
                .unwrap_or(Value::Bool(false)),
            "key" => arr
                .borrow()
                .iter()
                .nth(pos)
                .map(|(k, _)| key_value(k))
                .unwrap_or(Value::Null),
            "next" => {
                self.ao_set_pos(obj, pos + 1);
                Value::Null
            }
            "seek" => {
                let i = args
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let i = match self.spl_int_arg(&i, family, canonical, 1, "offset") {
                    Ok(i) => i,
                    Err(e) => return self.fail(e),
                };
                let len = arr.borrow().len() as i64;
                if i < 0 || i >= len.max(1) && !(i == 0 && len == 0) {
                    let e = self.spl_throw(
                        "OutOfBoundsException",
                        format!("Seek position {} is out of range", i),
                    );
                    return self.fail(e);
                }
                self.ao_set_pos(obj, i as usize);
                Value::Null
            }
            "count" => Value::Int(arr.borrow().len() as i64),
            "append" => {
                let v = args
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                arr.borrow_mut().push(v);
                Value::Null
            }
            "exchangearray" => {
                let v = args.cells.first().unwrap().borrow().clone();
                if !matches!(v, Value::Array(_) | Value::Object(_)) {
                    let tn = self.zval_type_name(&v);
                    let e = self.spl_throw(
                        "TypeError",
                        format!(
                            "{}::{}(): Argument #1 ($array) must be of type array, {} given",
                            family, canonical, tn
                        ),
                    );
                    return self.fail(e);
                }
                let (new, src_flags) = self.ao_backing(&v, family, canonical)?;
                let old = {
                    let mut ob = obj.borrow_mut();
                    match &mut ob.internal {
                        Some(ObjectInternal::ArrayIter {
                            arr: slot,
                            flags: f,
                            ..
                        }) => {
                            // An spl-array source carries its flags over
                            // (zend USE_OTHER); plain inputs keep ours.
                            if let Some(sf) = src_flags {
                                *f = sf;
                            }
                            std::mem::replace(slot, new)
                        }
                        _ => unreachable!(),
                    }
                };
                // zend returns a copy of the old hash — bound refs stay.
                let old = old.borrow();
                Value::Array(Rc::new(RefCell::new(ao_copy(&old))))
            }
            "getarraycopy" => Value::Array(Rc::new(RefCell::new(ao_copy(&arr.borrow())))),
            "offsetget" => {
                let k = args
                    .cells
                    .first()
                    .map(|c| to_key(&c.borrow()))
                    .unwrap_or(ArrKey::Int(0));
                match arr.borrow().get(&k) {
                    Some(v) => v,
                    None => {
                        let kn = match &k {
                            ArrKey::Int(i) => format!("{}", i),
                            ArrKey::Str(s) => format!("\"{}\"", s),
                            ArrKey::Tomb => "0".into(),
                        };
                        let _ = self.warn(&format!("Undefined array key {}", kn));
                        Value::Null
                    }
                }
            }
            "offsetexists" => {
                let k = args
                    .cells
                    .first()
                    .map(|c| to_key(&c.borrow()))
                    .unwrap_or(ArrKey::Int(0));
                Value::Bool(arr.borrow().get(&k).is_some())
            }
            "offsetset" => {
                let v = args
                    .cells
                    .get(1)
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                match args.cells.first().map(|c| c.borrow().clone()) {
                    Some(Value::Null) | None => arr.borrow_mut().push(v),
                    Some(kv) => arr.borrow_mut().set(to_key(&kv), v),
                }
                Value::Null
            }
            "offsetunset" => {
                let k = args
                    .cells
                    .first()
                    .map(|c| to_key(&c.borrow()))
                    .unwrap_or(ArrKey::Int(0));
                arr.borrow_mut().unset(&k);
                // pos pointing past the end stays clamped at reads.
                Value::Null
            }
            "getflags" => Value::Int(flags),
            "setflags" => {
                let f = args
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let f = match self.spl_int_arg(&f, family, canonical, 1, "flags") {
                    Ok(f) => f,
                    Err(e) => return self.fail(e),
                };
                if let Some(ObjectInternal::ArrayIter { flags: fp, .. }) =
                    &mut obj.borrow_mut().internal
                {
                    *fp = f;
                }
                Value::Null
            }
            "getiterator" => {
                // IteratorAggregate entry point — shares the storage Rc
                // so iterator writes land in the object's storage.
                let icname = {
                    let ob = obj.borrow();
                    match &ob.internal {
                        Some(ObjectInternal::ArrayIter { iterator_class, .. }) => {
                            iterator_class.clone()
                        }
                        _ => None,
                    }
                }
                .unwrap_or_else(|| "ArrayIterator".into());
                match self.classes.get(&icname.to_lowercase()).cloned() {
                    Some(icls) => {
                        let it = self.alloc_obj(PhpObject {
                            class: icls,
                            props: Default::default(),
                            prop_order: Vec::new(),
                            id: 0,
                            internal: Some(ObjectInternal::ArrayIter {
                                arr: arr.clone(),
                                pos: 0,
                                flags,
                                iterator_class: None,
                                src: None,
                            }),
                            unset_props: Default::default(),
                        });
                        Value::Object(it)
                    }
                    None => return Ok(None),
                }
            }
            "getiteratorclass" => {
                let ic = {
                    let ob = obj.borrow();
                    match &ob.internal {
                        Some(ObjectInternal::ArrayIter { iterator_class, .. }) => {
                            iterator_class.clone()
                        }
                        _ => None,
                    }
                };
                match ic {
                    Some(n) => match self.classes.get(&n.to_lowercase()) {
                        Some(c) => Value::str(c.name()),
                        None => Value::str(&n),
                    },
                    None => Value::str("ArrayIterator"),
                }
            }
            "setiteratorclass" => {
                let v = args.cells.first().unwrap().borrow().clone();
                match self.ao_iterator_class(&v, family, canonical, 1) {
                    Ok(n) => {
                        if let Some(ObjectInternal::ArrayIter { iterator_class, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *iterator_class = Some(n);
                        }
                        Value::Null
                    }
                    Err(e) => return self.fail(e),
                }
            }
            "asort" | "ksort" => {
                let flag = args.cells.first().map(|c| c.borrow().clone());
                let flag = match flag {
                    Some(v) => match self.spl_int_arg(&v, family, canonical, 1, "flags") {
                        Ok(f) => f,
                        Err(e) => return self.fail(e),
                    },
                    None => 0,
                };
                let mut a = arr.borrow_mut();
                if lname == "asort" {
                    match flag {
                        // SORT_NUMERIC / SORT_STRING / plain regular.
                        1 => a.entries.sort_by(|(_, x), (_, y)| {
                            x.borrow()
                                .to_float()
                                .partial_cmp(&y.borrow().to_float())
                                .unwrap_or(std::cmp::Ordering::Equal)
                        }),
                        2 | 5 | 3 => a.entries.sort_by(|(_, x), (_, y)| {
                            x.borrow().to_php_string().cmp(&y.borrow().to_php_string())
                        }),
                        _ => a
                            .entries
                            .sort_by(|(_, x), (_, y)| compare(&x.borrow(), &y.borrow())),
                    }
                } else {
                    match flag {
                        1 => a.entries.sort_by(|(x, _), (y, _)| {
                            key_value(x)
                                .to_float()
                                .partial_cmp(&key_value(y).to_float())
                                .unwrap_or(std::cmp::Ordering::Equal)
                        }),
                        2 | 5 | 3 => a.entries.sort_by(|(x, _), (y, _)| {
                            key_value(x)
                                .to_php_string()
                                .cmp(&key_value(y).to_php_string())
                        }),
                        _ => a
                            .entries
                            .sort_by(|(x, _), (y, _)| compare(&key_value(x), &key_value(y))),
                    }
                }
                Value::Bool(true)
            }
            "uasort" | "uksort" => {
                let cb = args
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                if !self.is_callable_value(&cb) {
                    let detail = self.zpp_callback_detail(&cb);
                    let e = self.spl_throw(
                        "TypeError",
                        format!(
                            "{}(): Argument #2 ($callback) must be a valid callback, {}",
                            lname, detail
                        ),
                    );
                    return self.fail(e);
                }
                // Bubble-sort through the callback — zend uses a stable
                // sort and the comparator sees (a, b) pairs of cells.
                let mut sorted: Vec<(ArrKey, Cell)> = arr.borrow().iter().cloned().collect();
                let mut swapped = true;
                while swapped {
                    swapped = false;
                    for i in 0..sorted.len().saturating_sub(1) {
                        let (ka, ca) = sorted[i].clone();
                        let (kb, cbb) = sorted[i + 1].clone();
                        let call_args = if lname == "uksort" {
                            vec![cell(key_value(&ka)), cell(key_value(&kb))]
                        } else {
                            vec![ca.clone(), cbb.clone()]
                        };
                        let r = self.call_value(&cb, CallArgs::positional(call_args))?;
                        if r.to_int() > 0 {
                            sorted.swap(i, i + 1);
                            swapped = true;
                        }
                    }
                }
                arr.borrow_mut().entries = sorted;
                Value::Bool(true)
            }
            "natsort" | "natcasesort" => {
                let ci = lname == "natcasesort";
                arr.borrow_mut().entries.sort_by(|(_, x), (_, y)| {
                    let mut a = x.borrow().to_php_string();
                    let mut b = y.borrow().to_php_string();
                    if ci {
                        a = a.to_lowercase();
                        b = b.to_lowercase();
                    }
                    compare(&Value::str(a), &Value::str(b))
                });
                Value::Bool(true)
            }
            "serialize" => {
                // Legacy spl payload: `x:i:<flags>;<ser-arr>;m:<ser-props>`
                // (the storage slot holds the backing OBJECT when the
                // source was one — zend writes it verbatim).
                let props = self.ao_props_arr(obj);
                let src = self.ao_src(obj);
                let s1 = crate::builtins::var::php_serialize(self, &src)?;
                let s2 = crate::builtins::var::php_serialize(
                    self,
                    &Value::Array(Rc::new(RefCell::new(props))),
                )?;
                Value::str(format!("x:i:{};{};m:{}", flags, s1, s2))
            }
            "unserialize" => {
                let data = args
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let data = match &data {
                    Value::Str(s) => crate::value::lossy(s).into_owned(),
                    other => {
                        let tn = self.zval_type_name(other);
                        let e = self.spl_throw(
                            "TypeError",
                            format!(
                                "{}::{}(): Argument #1 ($data) must be of type string, {} given",
                                family, canonical, tn
                            ),
                        );
                        return self.fail(e);
                    }
                };
                match self.ao_parse_payload(&data) {
                    Ok((pflags, sarr, parr)) => {
                        let mut ob = obj.borrow_mut();
                        ob.internal = Some(ObjectInternal::ArrayIter {
                            arr: sarr,
                            pos: 0,
                            flags: pflags,
                            iterator_class: None,
                            src: None,
                        });
                        drop(ob);
                        let mut ob = obj.borrow_mut();
                        for (k, c) in parr.borrow().iter() {
                            let kn = match k {
                                ArrKey::Str(s) => s.to_string(),
                                ArrKey::Int(i) => i.to_string(),
                                ArrKey::Tomb => continue,
                            };
                            if !ob.prop_order.contains(&kn) {
                                ob.prop_order.push(kn.clone());
                            }
                            ob.props.insert(kn, c.clone());
                        }
                        Value::Null
                    }
                    Err(pos) => {
                        let e = self.spl_throw(
                            "UnexpectedValueException",
                            format!("Error at offset {} of {} bytes", pos, data.len()),
                        );
                        return self.fail(e);
                    }
                }
            }
            "__serialize" => {
                let props = self.ao_props_arr(obj);
                let mut out = PhpArray::new();
                out.push(Value::Int(flags));
                out.push(self.ao_src(obj));
                out.push(Value::Array(Rc::new(RefCell::new(props))));
                out.push(Value::Null);
                Value::Array(Rc::new(RefCell::new(out)))
            }
            "__unserialize" => {
                let data = args.cells.first().unwrap().borrow().clone();
                let arr_v = match &data {
                    Value::Array(a) => a.clone(),
                    other => {
                        let tn = self.zval_type_name(other);
                        let e = self.spl_throw(
                            "TypeError",
                            format!(
                                "{}::{}(): Argument #1 ($data) must be of type array, {} given",
                                family, canonical, tn
                            ),
                        );
                        return self.fail(e);
                    }
                };
                let bad = |it: &mut Self| -> PhpError {
                    it.spl_throw(
                        "UnexpectedValueException",
                        "Incomplete or ill-typed serialization data".to_string(),
                    )
                };
                // Slots: [0] flags int, [1] storage array|object,
                // [2] props array, [3] NULL (optional).
                let d = arr_v.borrow();
                let f = d.get(&ArrKey::Int(0));
                let st = d.get(&ArrKey::Int(1));
                let pr = d.get(&ArrKey::Int(2));
                let aux = d.get(&ArrKey::Int(3));
                let (Some(f), Some(st), Some(pr)) = (f, st, pr) else {
                    let e = bad(self);
                    return self.fail(e);
                };
                let flags_i = match f {
                    Value::Int(i) => i,
                    _ => {
                        let e = bad(self);
                        return self.fail(e);
                    }
                };
                if !matches!(st, Value::Array(_) | Value::Object(_)) {
                    let e = self.spl_throw(
                        "UnexpectedValueException",
                        "Passed variable is not an array or object".to_string(),
                    );
                    return self.fail(e);
                }
                let Value::Array(props_arr) = &pr else {
                    let e = bad(self);
                    return self.fail(e);
                };
                if let Some(aux) = aux {
                    if !matches!(aux, Value::Null) {
                        let e = bad(self);
                        return self.fail(e);
                    }
                }
                let props_arr = props_arr.clone();
                drop(d);
                let (backing, _) = self.ao_backing(&st, family, canonical)?;
                let src_obj = match &st {
                    Value::Object(o) => Some(o.clone()),
                    _ => None,
                };
                let mut ob = obj.borrow_mut();
                ob.internal = Some(ObjectInternal::ArrayIter {
                    arr: backing,
                    pos: 0,
                    flags: flags_i,
                    iterator_class: None,
                    src: src_obj,
                });
                for (k, c) in props_arr.borrow().iter() {
                    let kn = match k {
                        ArrKey::Str(s) => s.to_string(),
                        ArrKey::Int(i) => i.to_string(),
                        ArrKey::Tomb => continue,
                    };
                    if !ob.prop_order.contains(&kn) {
                        ob.prop_order.push(kn.clone());
                    }
                    ob.props.insert(kn, c.clone());
                }
                Value::Null
            }
            "haschildren" => {
                // RecursiveArrayIterator: current element is child-able
                // when it's an array or object.
                Value::Bool(match arr.borrow().iter().nth(pos) {
                    Some((_, c)) => matches!(*c.borrow(), Value::Array(_) | Value::Object(_)),
                    None => false,
                })
            }
            "getchildren" => {
                // `new static(element)` — same class, element storage.
                let el = arr
                    .borrow()
                    .iter()
                    .nth(pos)
                    .map(|(_, c)| c.borrow().clone())
                    .unwrap_or(Value::Null);
                if !matches!(el, Value::Array(_) | Value::Object(_)) {
                    // zend type-checks against the parent ctor's arginfo:
                    // "ArrayIterator::__construct()" regardless of class.
                    let tn = self.zval_type_name(&el);
                    let e = self.spl_throw(
                        "TypeError",
                        format!(
                            "{}::__construct(): Argument #1 ($array) must be of type array, {} given",
                            family, tn
                        ),
                    );
                    return Err(e);
                }
                // zend reports the object-backing deprecation under
                // the ctor's name here ("ArrayIterator::__construct").
                let backing = match self.ao_backing(&el, family, "__construct") {
                    Ok((b, sf)) => (b, sf),
                    Err(e) => return Err(e),
                };
                let cls_name = obj.borrow().class.name().to_string();
                let icls = self.classes.get(&cls_name.to_lowercase()).cloned();
                match icls {
                    Some(icls) => {
                        let src_obj = match &el {
                            Value::Object(o) => Some(o.clone()),
                            _ => None,
                        };
                        let it = self.alloc_obj(PhpObject {
                            class: icls,
                            props: Default::default(),
                            prop_order: Vec::new(),
                            id: 0,
                            internal: Some(ObjectInternal::ArrayIter {
                                arr: backing.0,
                                pos: 0,
                                flags: backing.1.unwrap_or(flags),
                                iterator_class: None,
                                src: src_obj,
                            }),
                            unset_props: Default::default(),
                        });
                        Value::Object(it)
                    }
                    None => Value::Null,
                }
            }
            "__debuginfo" => {
                let mut out = PhpArray::new();
                out.set(ArrKey::Str("storage".into()), Value::Array(arr.clone()));
                Value::Array(Rc::new(RefCell::new(out)))
            }
            _ => return Ok(None),
        };
        Ok(Some(v))
    }

    /// Raise a catchable engine exception for the spl array-object
    /// methods (`self.exception` + `throw`).
    fn spl_throw(&mut self, class: &str, msg: impl Into<String>) -> PhpError {
        let e = self.exception(class, &msg.into());
        self.throw(e)
    }

    /// ARRAY_AS_PROPS (flag bit 2): undeclared prop access on this spl
    /// array-object routes to the storage hash.
    pub(in crate::interp) fn aap_active(&mut self, o: &Rc<RefCell<PhpObject>>) -> bool {
        match &o.borrow().internal {
            Some(ObjectInternal::ArrayIter { flags, .. }) => *flags & 2 != 0,
            _ => false,
        }
    }

    /// (storage, pos, flags) of an spl array-object, lazily creating the
    /// internal storage on first access — zend materializes it on demand
    /// for `newInstanceWithoutConstructor` objects too.
    pub(in crate::interp) fn ao_state(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
    ) -> (Rc<RefCell<PhpArray>>, usize, i64) {
        let mut ob = obj.borrow_mut();
        if !matches!(ob.internal, Some(ObjectInternal::ArrayIter { .. })) {
            ob.internal = Some(ObjectInternal::ArrayIter {
                arr: Rc::new(RefCell::new(PhpArray::new())),
                pos: 0,
                flags: 0,
                iterator_class: None,
                src: None,
            });
        }
        match &ob.internal {
            Some(ObjectInternal::ArrayIter {
                arr, pos, flags, ..
            }) => (arr.clone(), *pos, *flags),
            _ => unreachable!(),
        }
    }

    fn ao_set_pos(&mut self, obj: &Rc<RefCell<PhpObject>>, p: usize) {
        if let Some(ObjectInternal::ArrayIter { pos, .. }) = &mut obj.borrow_mut().internal {
            *pos = p;
        }
    }

    /// Weak-mode int coercion for a zpp `int` arg; TypeError otherwise.
    fn spl_int_arg(
        &mut self,
        v: &Value,
        family: &str,
        mname: &str,
        n: usize,
        pname: &str,
    ) -> Result<i64, PhpError> {
        let ok = match v {
            Value::Int(i) => Some(*i),
            Value::Bool(b) => Some(*b as i64),
            Value::Float(f) => Some(*f as i64),
            Value::Str(s) => {
                let s = crate::value::lossy(s);
                s.trim()
                    .parse::<i64>()
                    .ok()
                    .or_else(|| s.trim().parse::<f64>().ok().map(|f| f as i64))
            }
            _ => None,
        };
        match ok {
            Some(i) => Ok(i),
            None => {
                let tn = self.zval_type_name(v);
                Err(self.spl_throw(
                    "TypeError",
                    format!(
                        "{}::{}(): Argument #{} (${}) must be of type int, {} given",
                        family, mname, n, pname, tn
                    ),
                ))
            }
        }
    }

    /// `iteratorClass` arg: coerced to a class name (like a `string`
    /// zpp param — objects must __toString or Error), resolved with
    /// autoload, and must derive from ArrayIterator; anything else is
    /// a TypeError whose repr is the coerced name (NUL-truncated).
    fn ao_iterator_class(
        &mut self,
        v: &Value,
        family: &str,
        mname: &str,
        n: usize,
    ) -> Result<String, PhpError> {
        let name = match v {
            Value::Str(s) => crate::value::lossy(s).into_owned(),
            Value::Int(i) => format!("{}", i),
            Value::Float(f) => crate::value::format_float_repr(*f),
            Value::Bool(b) => if *b { "1" } else { "" }.to_string(),
            Value::Null => String::new(),
            Value::Array(_) => {
                self.warn_pub("Array to string conversion")?;
                "Array".to_string()
            }
            Value::Object(o) => {
                if self
                    .find_method_in(&o.borrow().class, "__tostring")
                    .is_some()
                {
                    match self.method_invoke(o.clone(), "__toString", CallArgs::empty()) {
                        Ok(Value::Str(s)) => crate::value::lossy(&s).into_owned(),
                        Ok(_) => String::new(),
                        Err(e) => return Err(e),
                    }
                } else {
                    return Err(self.spl_throw(
                        "Error",
                        format!(
                            "Object of class {} could not be converted to string",
                            o.borrow().class.name()
                        ),
                    ));
                }
            }
            other => self.zval_type_name(other),
        };
        // zend prints the name up to the first NUL byte.
        let name = name.split('\0').next().unwrap_or("").to_string();
        let resolved = self
            .resolve_class(&name)
            .filter(|c| self.is_a_str(c, "arrayiterator"));
        match resolved {
            Some(c) => Ok(c),
            None => Err(self.spl_throw(
                "TypeError",
                format!(
                    "{}::{}(): Argument #{} ($iteratorClass) must be a class name derived from ArrayIterator, {} given",
                    family, mname, n, name
                ),
            )),
        }
    }

    /// spl storage coercion for `array|object` inputs: arrays copy
    /// (shared php-reference cells stay bound); objects emit the object
    /// deprecation — spl-array sources hand over their storage cells
    /// (+flags), plain objects bind their prop cells live.
    fn ao_backing(
        &mut self,
        v: &Value,
        family: &str,
        mname: &str,
    ) -> Result<(Rc<RefCell<PhpArray>>, Option<i64>), PhpError> {
        match v {
            Value::Array(a) => Ok((Rc::new(RefCell::new(ao_copy(&a.borrow()))), None)),
            Value::Object(o) => {
                self.deprecated(&format!(
                    "{}::{}(): Using an object as a backing array for {} is deprecated, as it allows violating class constraints and invariants",
                    family, mname, family
                ))?;
                let src = {
                    let ob = o.borrow();
                    match &ob.internal {
                        Some(ObjectInternal::ArrayIter { arr, flags, .. }) => {
                            Some((arr.clone(), *flags))
                        }
                        _ => None,
                    }
                };
                if let Some((src, src_flags)) = src {
                    return Ok((
                        Rc::new(RefCell::new(ao_copy(&src.borrow()))),
                        Some(src_flags),
                    ));
                }
                // Objects iterate their prop cells BY REFERENCE —
                // writes through $v update the prop (typed gate
                // still applies); deprecated since 8.5
                // (typed_properties_113/114/115).
                let mut copy = PhpArray::new();
                let pairs: Vec<(String, Cell)> = o
                    .borrow()
                    .props
                    .iter()
                    .map(|(k, c)| (k.clone(), c.clone()))
                    .collect();
                for (k, c) in pairs {
                    let pn = k.rsplit('\0').next().unwrap_or(&k).to_string();
                    if let Some((pd, dcls)) = self.decl_prop(o, &pn) {
                        let p = Rc::as_ptr(&c) as usize;
                        if let Some(tys) = &pd.ty {
                            self.typed_slots.insert(
                                p,
                                (c.clone(), tys.clone(), dcls.name().to_string(), pn.clone()),
                            );
                            self.slot_anchor
                                .insert(p, SlotAnchor::Obj(Rc::downgrade(o), k.clone()));
                            self.slot_owners.entry(p).or_default().push((
                                tys.clone(),
                                dcls.name().to_string(),
                                pn.clone(),
                                SlotAnchor::Obj(Rc::downgrade(o), k.clone()),
                            ));
                        }
                        if pd.readonly {
                            self.readonly_cells
                                .insert(p, (dcls.name().to_string(), pn.clone()));
                        }
                    }
                    copy.bind_cell(ArrKey::Str(Rc::from(k.as_str())), c);
                }
                Ok((Rc::new(RefCell::new(copy)), None))
            }
            _ => unreachable!("callers validate array|object before ao_backing"),
        }
    }

    /// `__serialize` slot 1 / the legacy payload's storage slot: the
    /// backing OBJECT when storage came from an object input, else the
    /// storage array (zend serializes the object verbatim so
    /// unserialize can re-bind it).
    fn ao_src(&mut self, obj: &Rc<RefCell<PhpObject>>) -> Value {
        match &obj.borrow().internal {
            Some(ObjectInternal::ArrayIter { arr, src, .. }) => match src {
                Some(o) => Value::Object(o.clone()),
                None => Value::Array(arr.clone()),
            },
            _ => Value::Null,
        }
    }

    /// The object's live prop table as a PhpArray (serialize payloads).
    fn ao_props_arr(&mut self, obj: &Rc<RefCell<PhpObject>>) -> PhpArray {
        let ob = obj.borrow();
        let mut props = PhpArray::new();
        for name in &ob.prop_order {
            if let Some(c) = ob.props.get(name) {
                props.set(ArrKey::Str(Rc::from(name.as_str())), c.borrow().clone());
            }
        }
        props
    }

    /// Parse the legacy spl payload `x:i:<flags>;<a:…>;m:<props>` —
    /// returns (storage, props) or Err(consumed-offset).
    fn ao_parse_payload(&mut self, data: &str) -> Result<AoUnserData, usize> {
        let b = data.as_bytes();
        let mut pos = 0usize;
        // x:i:<flags>; — flags are consumed but not restored by
        // serialize()/unserialize() (zend stores them in the payload but
        // a plain unserialize doesn't write ar_flags? — zend DOES read
        // flags from x:; keep them).
        if !data.starts_with("x:i:") {
            return Err(0);
        }
        pos += 4;
        let fstart = pos;
        while pos < b.len() && b[pos] != b';' {
            pos += 1;
        }
        if pos >= b.len() {
            return Err(pos);
        }
        let fl: i64 = data[fstart..pos].parse().map_err(|_| pos)?;
        pos += 1; // ;
                  // storage: serialized array
        let st = {
            let mut ie = None;
            crate::builtins::var::php_unserialize(self, data, &mut pos, &mut ie).map_err(|_| pos)?
        };
        let Value::Array(st) = st else {
            return Err(pos);
        };
        // ;m:<props>
        if pos + 2 >= b.len() || &data[pos..pos + 3] != ";m:" {
            return Err(pos);
        }
        pos += 3;
        let pr = {
            let mut ie = None;
            crate::builtins::var::php_unserialize(self, data, &mut pos, &mut ie).map_err(|_| pos)?
        };
        let Value::Array(pr) = pr else {
            return Err(pos);
        };
        if pos != b.len() {
            return Err(pos);
        }
        Ok((fl, st, pr))
    }

    /// Call-arg list for `invokeArgs`/`newInstanceArgs`: array entries
    /// with string keys become named args (named_params/call_user_func).
    pub(in crate::interp) fn args_from_array(&mut self, v: &Value) -> CallArgs {
        let mut ca = CallArgs::empty();
        if let Value::Array(a) = v {
            for (k, c) in a.borrow().iter() {
                match k {
                    ArrKey::Str(s) => ca.named.push((s.to_string(), c.clone(), true, false)),
                    _ => ca.cells.push(c.clone()),
                }
            }
        }
        ca
    }

    pub(in crate::interp) fn method_call(
        &mut self,
        obj: &Expr,
        name: &PropName,
        args: &[Expr],
        nullsafe: bool,
    ) -> Result<Value, PhpError> {
        let mn = Self::nul_trunc(&self.prop_name(name)?);
        let ov = self.eval(obj)?;
        match ov {
            Value::Null if nullsafe => Ok(Value::Null),
            Value::Object(o) => {
                let params = self
                    .find_method_in(&o.borrow().class.clone(), &mn)
                    .map(|m| m.0.decl.params.clone())
                    .unwrap_or_default();
                let argvals = self.arg_cells(args, &params, &format!("{}()", mn), false)?;
                // method_invoke handles builtin (Throwable), __call, undefined.
                self.method_invoke_vis(o.clone(), &mn, argvals)
            }
            Value::Null => {
                if nullsafe {
                    return Ok(Value::Null);
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!("Call to a member function {}() on null", mn),
                    0,
                ))
            }
            // Closures expose Closure::__invoke/call/bindTo
            // (named_params/call_user_func's `$closure->__invoke(...)`).
            Value::Callable(c) if mn.eq_ignore_ascii_case("__invoke") => {
                let params = match &c.kind {
                    CallableKind::Closure(d) => d.params.clone(),
                    _ => vec![],
                };
                let argvals = self.arg_cells(args, &params, &format!("{}()", mn), false)?;
                // `$f->__invoke()` runs the internal Closure::__invoke —
                // diagnostics name `Closure::__invoke` and drop the
                // ", called in" suffix (closure_059).
                self.pending_call_alias = Some("Closure::__invoke".into());
                let r = self.call_value(&Value::Callable(c), argvals);
                self.pending_call_alias = None;
                r
            }
            Value::Callable(c) if mn.eq_ignore_ascii_case("call") => {
                // `$fn->call($newThis, ...$args)`: bind with an omitted
                // scope then invoke — previous scope preserved when the
                // new instance is compatible (closure_036/038).
                let argvals = self.arg_cells(args, &[], &format!("{}()", mn), false)?;
                let mut ca = argvals;
                let newthis = ca
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                if let Value::Object(t) = newthis {
                    // call() scopes to the new instance's class
                    // (unlike bindTo's default 'static' scope).
                    match self.rebind_closure(&c, Some(t.clone()), Some(Value::Object(t)))? {
                        Some(nc) => {
                            ca.cells.remove(0);
                            return self.call_value(&Value::Callable(Rc::new(nc)), ca);
                        }
                        // A failed bind (warned) skips the invocation
                        // (closure_from_callable_rebinding).
                        None => return Ok(Value::Null),
                    }
                }
                self.call_value(&Value::Callable(c), ca)
            }
            Value::Callable(c) if mn.eq_ignore_ascii_case("bindto") => {
                let argvals = self.arg_cells(args, &[], &format!("{}()", mn), false)?;
                let this = argvals.cells.first().map(|c| c.borrow().clone());
                let scope = argvals.cells.get(1).map(|c| c.borrow().clone());
                let new_this = match &this {
                    None | Some(Value::Null) => None,
                    Some(Value::Object(o)) => Some(o.clone()),
                    Some(v) => {
                        let e = self.exception(
                            "TypeError",
                            &format!(
                                "Closure::bindTo(): Argument #1 ($newThis) must be of type ?object, {} given",
                                v.gettype()
                            ),
                        );
                        return Err(self.throw(e));
                    }
                };
                let scope_arg = match &scope {
                    None => None,
                    Some(v @ (Value::Null | Value::Object(_) | Value::Str(_))) => Some(v.clone()),
                    Some(v) => {
                        let e = self.exception(
                            "TypeError",
                            &format!(
                                "Closure::bindTo(): Argument #2 ($newScope) must be of type object|string|null, {} given",
                                v.gettype()
                            ),
                        );
                        return Err(self.throw(e));
                    }
                };
                match self.rebind_closure(&c, new_this, scope_arg)? {
                    Some(nc) => Ok(Value::Callable(Rc::new(nc))),
                    None => Ok(Value::Null),
                }
            }
            other => self.fail(PhpError::uncaught(
                "Error",
                format!("Call to a member function {}() on {}", mn, other.gettype()),
                0,
            )),
        }
    }

    /// `$obj->method()` dispatch to a resolved decl.
    /// `dc` is the declaring class — used as the private-prop scope.
    fn invoke_method(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        m: &Rc<MethodDecl>,
        args: CallArgs,
        dc: Rc<PhpClass>,
    ) -> Result<Value, PhpError> {
        // Native SPL stubs (empty body, line 0) dispatch through
        // spl_method however the call resolved — direct, parent::,
        // or late-bound — so subclass PHP methods stay authoritative
        // while inherited engine behavior still fires.
        if m.decl.body.is_empty() && m.decl.line == 0 {
            let cn = obj.borrow().class.name().to_string();
            if self.is_a_str(&cn, "splfileinfo") {
                if let Some(v) = self.spl_method(&obj, &m.decl.name, &args)? {
                    return Ok(v);
                }
            }
            // Same for the ArrayIterator/ArrayObject storage family —
            // subclass methods that inherit the stubs still hit the
            // native offset*/iteration behavior (bug36214).
            if self.is_a_str(&cn, "arrayiterator") || self.is_a_str(&cn, "arrayobject") {
                if let Some(v) = self.array_iter_method(&obj, &m.decl.name, &args)? {
                    return Ok(v);
                }
            }
        }
        let called = obj.borrow().class.clone();
        self.pending_decl_class = Some(dc.clone());
        self.pending_called_class = Some(called);
        let r = if m.is_static {
            self.invoke_fn(&Rc::new(m.decl.clone()), args, None, Some(dc))
        } else {
            self.invoke_fn(&Rc::new(m.decl.clone()), args, Some(obj), Some(dc))
        };
        self.pending_decl_class = None;
        self.pending_called_class = None;
        r
    }

    /// Dispatch `$obj->name($args)` through `__call(name, args)`.
    /// Zend's $args array for __call/__callStatic: elements that were
    /// references in the caller's send array stay shared (bug50394);
    /// plain zvals are copied so var_dump shows no `&`
    /// (trampoline_closure_named_arguments).
    fn magic_args_array(&self, args: &CallArgs) -> PhpArray {
        let mut arr = PhpArray::new();
        let share = |a: &Cell| {
            if self.ref_cells.contains(&(Rc::as_ptr(a) as usize)) {
                a.clone()
            } else {
                cell(a.borrow().clone())
            }
        };
        for a in &args.cells {
            arr.push_cell(share(a));
        }
        for (n, a, ..) in &args.named {
            arr.set_cell(ArrKey::Str(Rc::from(n.as_str())), share(a));
        }
        arr
    }

    fn call_via_magic(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        m: &Rc<MethodDecl>,
        dc: Rc<PhpClass>,
        name: &str,
        args: CallArgs,
    ) -> Result<Value, PhpError> {
        let arr = self.magic_args_array(&args);
        self.invoke_method(
            obj,
            m,
            CallArgs::positional(vec![
                cell(Value::str(name)),
                cell(Value::Array(Rc::new(RefCell::new(arr)))),
            ]),
            dc,
        )
    }

    /// A private method owned by the calling scope binds statically:
    /// `$this->m()` inside `S::x` always resolves to `S::m`, bypassing
    /// the object's override (zend private methods are not virtual).
    fn scope_private_method(&mut self, name: &str) -> Option<(Rc<MethodDecl>, Rc<PhpClass>)> {
        let scope = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned())
            .or_else(|| self.const_self.clone())?;
        let m = scope.decl.find_method(&name.to_lowercase())?;
        (m.visibility == crate::ast::Visibility::Private).then_some((m, scope))
    }

    /// Userland `Cls::name()` dispatch: gate visibility at the call
    /// site, routing inaccessible methods through __callStatic.
    pub(in crate::interp) fn static_invoke_vis(
        &mut self,
        cls: Rc<PhpClass>,
        name: &str,
        args: CallArgs,
        called_class: Option<Rc<PhpClass>>,
        fwd: bool,
    ) -> Result<Value, PhpError> {
        if let Some((m, sc)) = self.scope_private_method(name) {
            let this_obj = if m.is_static {
                None
            } else {
                self.stack
                    .last()
                    .and_then(|f| f.this_obj.clone())
                    .filter(|o| {
                        let cname = o.borrow().class.name().to_string();
                        self.is_a_str(&cname, cls.name())
                    })
            };
            if !m.is_static && this_obj.is_none() {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Non-static method {}::{}() cannot be called statically",
                        sc.name(),
                        m.decl.name
                    ),
                    0,
                ));
            }
            self.pending_decl_class = Some(sc.clone());
            self.pending_called_class = Some(called_class.unwrap_or(cls.clone()));
            let r = self.invoke_fn(&Rc::new(m.decl.clone()), args, this_obj, Some(sc));
            self.pending_decl_class = None;
            self.pending_called_class = None;
            return r;
        }
        if let Some((m, dc)) = self.find_method_in(&cls, name) {
            if !self.method_access_ok(&m, &dc) {
                // Inaccessible found-method: same magic preference as
                // the missing path — __call first in object context,
                // else __callStatic (bug53826, bug48533).
                let this_obj = self
                    .stack
                    .last()
                    .and_then(|f| f.this_obj.clone())
                    .filter(|o| {
                        let cname = o.borrow().class.name().to_string();
                        self.is_a_str(&cname, cls.name())
                    });
                if let Some(o) = this_obj {
                    if let Some((cm, cdc)) = self.find_method_in(&cls, "__call") {
                        return self.call_via_magic(o, &cm, cdc, name, args);
                    }
                }
                if let Some((cm, cdc)) = self.find_method_in(&cls, "__callstatic") {
                    let arr = self.magic_args_array(&args);
                    self.pending_decl_class = Some(cdc.clone());
                    self.pending_called_class = Some(called_class.unwrap_or(cls.clone()));
                    let r = self.invoke_fn(
                        &Rc::new(cm.decl.clone()),
                        CallArgs::positional(vec![
                            cell(Value::str(name)),
                            cell(Value::Array(Rc::new(RefCell::new(arr)))),
                        ]),
                        None,
                        Some(cdc),
                    );
                    self.pending_decl_class = None;
                    self.pending_called_class = None;
                    return r;
                }
                let e = self.method_vis_error(&m, &dc);
                return self.fail(e);
            }
        }
        self.static_invoke(cls, name, args, called_class, fwd)
    }

    /// Userland `$obj->name()` dispatch: gate visibility at the call
    /// site. Internal invocations (FCC bound scope, engine magic calls)
    /// go through `method_invoke` unchecked.
    pub(in crate::interp) fn method_invoke_vis(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        name: &str,
        args: CallArgs,
    ) -> Result<Value, PhpError> {
        let cls = obj.borrow().class.clone();
        // Scope-private binding takes precedence over the object's own
        // method table (private methods are not virtual) — but only
        // when the callee is an instance of the scope class. For
        // unrelated objects the scope's same-named private method is
        // invisible and normal dispatch applies (`$io->output->m()`
        // must not find IO's private m()).
        if let Some((m, sc)) = self.scope_private_method(name) {
            let cname = cls.name().to_string();
            if self.is_a_str(&cname, sc.name()) {
                if m.is_abstract {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot call abstract method {}::{}()", sc.name(), name),
                        0,
                    ));
                }
                return self.invoke_method(obj, &m, args, sc);
            }
        }
        if let Some((m, dc)) = self.find_method_in(&cls, name) {
            if !self.method_access_ok(&m, &dc) {
                // Inaccessible method routes through __call when
                // defined (zend_std_get_method fallback).
                if let Some((cm, cdc)) = self.find_method_in(&cls, "__call") {
                    return self.call_via_magic(obj, &cm, cdc, name, args);
                }
                let e = self.method_vis_error(&m, &dc);
                return self.fail(e);
            }
        }
        self.method_invoke(obj, name, args)
    }

    /// Method-call visibility against the current calling scope.
    pub(in crate::interp) fn method_access_ok(
        &mut self,
        m: &MethodDecl,
        dc: &Rc<PhpClass>,
    ) -> bool {
        let scope = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned())
            .or_else(|| self.const_self.clone());
        match m.visibility {
            crate::ast::Visibility::Public => true,
            crate::ast::Visibility::Private => scope
                .as_ref()
                .map(|s| s.name() == dc.name())
                .unwrap_or(false),
            crate::ast::Visibility::Protected => scope
                .as_ref()
                .map(|s| {
                    let proto = self.method_prototype(dc, &m.decl.name.to_lowercase());
                    self.is_a_str(s.name(), dc.name())
                        || self.is_a_str(dc.name(), s.name())
                        // Sibling access: protected `B::m()` callable from
                        // scope A when A is a descendant of the method's
                        // PROTOTYPE owner (the ancestor that first
                        // declared it — gh14009: A and B both extend P,
                        // P first declared `common`).
                        || self.is_a_str(s.name(), &proto)
                })
                .unwrap_or(false),
        }
    }

    /// The ancestor class that first declared `lname` (non-private) —
    /// the prototype owner for protected-member access rules.
    pub(in crate::interp) fn method_prototype(
        &mut self,
        cls: &Rc<PhpClass>,
        lname: &str,
    ) -> String {
        let mut owner = cls.name().to_string();
        let mut cur = cls.decl.parent.clone();
        while let Some(p) = cur {
            let Some(pc) = self.classes.get(&p.to_lowercase()).cloned() else {
                break;
            };
            if pc.decl.methods.iter().any(|m| {
                m.decl.name.to_lowercase() == lname
                    && !matches!(m.visibility, crate::ast::Visibility::Private)
            }) {
                owner = pc.decl.name.clone();
            }
            cur = pc.decl.parent.clone();
        }
        owner
    }

    /// `Call to private/protected method X::m() from scope Y` — catchable.
    fn method_vis_error(&mut self, m: &MethodDecl, dc: &Rc<PhpClass>) -> PhpError {
        let vis = match m.visibility {
            crate::ast::Visibility::Public => "public",
            crate::ast::Visibility::Protected => "protected",
            crate::ast::Visibility::Private => "private",
        };
        let scope = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned())
            .or_else(|| self.const_self.clone());
        let from = match &scope {
            Some(s) => format!("scope {}", s.name()),
            None => "global scope".to_string(),
        };
        PhpError::uncaught(
            "Error",
            format!(
                "Call to {} method {}::{}() from {}",
                vis,
                dc.name(),
                m.decl.name,
                from
            ),
            0,
        )
    }

    /// Calls a method by name through an object cell (magic methods, __call).
    pub fn method_invoke(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        name: &str,
        args: CallArgs,
    ) -> Result<Value, PhpError> {
        // Builtin exception methods implemented natively.
        if let Value::Object(_) = Value::Object(obj.clone()) {}
        let is_throwable = {
            let ob = obj.borrow();
            matches!(ob.internal, Some(ObjectInternal::Exception { .. }))
                || self.is_throwable_name(&ob.class.decl.name)
        };
        let cls = obj.borrow().class.clone();
        if is_throwable {
            // Native method only when the resolved method is a builtin
            // registration (line 0 — userland always runs, even an empty
            // body: `__construct(public $x) {}` still promotes).
            // find_method_in walks the parent chain so inherited stubs
            // (Exception::getTrace on a userland subclass) resolve
            // (tests/lang/038, error_2_exception_001).
            let stub = self
                .find_method_in(&cls, name)
                .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
                .unwrap_or(false);
            if stub {
                if let Some(v) = self.throwable_method(&obj, name, &args) {
                    return Ok(v);
                }
            }
        }
        // Reflection stubs are native: constructor stores the target
        // name, methods act on it (gh15438_2).
        if cls.name().starts_with("Reflection") || cls.name().starts_with("reflection") {
            let stub = self
                .find_method_in(&cls, name)
                .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
                .unwrap_or(false);
            if stub {
                // Native Reflection calls leave a `Cls->m()` frame in
                // uncaught traces (named_params/attributes_named_flags).
                self.call_trace.push(TraceFrame {
                    file: self.diag_file(),
                    line: self.cur_line as u32,
                    function: name.to_string(),
                    class: Some(cls.name().to_string()),
                    ty: "->".into(),
                    args: Vec::new(),
                    named_args: Vec::new(),
                    internal: false,
                });
                let r = self.reflection_method(&obj, name, &args);
                self.call_trace.pop();
                if let Some(v) = r? {
                    return Ok(v);
                }
            }
        }
        // SplDoublyLinkedList / SplStack — list state on \0dll\0items.
        if matches!(
            cls.name().to_lowercase().as_str(),
            "spldoublylinkedlist" | "splstack" | "splqueue"
        ) {
            let dll_method = |o: &Rc<RefCell<PhpObject>>| -> Vec<Value> {
                match o.borrow().props.get("\0dll\0items") {
                    Some(c) => match &*c.borrow() {
                        Value::Array(a) => {
                            a.borrow().iter().map(|(_, c)| c.borrow().clone()).collect()
                        }
                        _ => Vec::new(),
                    },
                    None => Vec::new(),
                }
            };
            match name.to_lowercase().as_str() {
                "count" => return Ok(Value::Int(dll_method(&obj).len() as i64)),
                "isempty" => return Ok(Value::Bool(dll_method(&obj).is_empty())),
                "top" => return Ok(dll_method(&obj).first().cloned().unwrap_or(Value::Null)),
                "bottom" => return Ok(dll_method(&obj).last().cloned().unwrap_or(Value::Null)),
                "push" => {
                    let v = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mut items = dll_method(&obj);
                    items.push(v);
                    let mut a = PhpArray::new();
                    for (i, iv) in items.into_iter().enumerate() {
                        a.set(ArrKey::Int(i as i64), iv);
                    }
                    obj.borrow_mut().props.insert(
                        "\0dll\0items".into(),
                        cell(Value::Array(Rc::new(RefCell::new(a)))),
                    );
                    return Ok(Value::Null);
                }
                "pop" | "shift" => {
                    let mut items = dll_method(&obj);
                    let r = if name.eq_ignore_ascii_case("pop") {
                        items.pop()
                    } else {
                        if items.is_empty() {
                            None
                        } else {
                            Some(items.remove(0))
                        }
                    };
                    let mut a = PhpArray::new();
                    for (i, iv) in items.into_iter().enumerate() {
                        a.set(ArrKey::Int(i as i64), iv);
                    }
                    obj.borrow_mut().props.insert(
                        "\0dll\0items".into(),
                        cell(Value::Array(Rc::new(RefCell::new(a)))),
                    );
                    return Ok(r.unwrap_or(Value::Null));
                }
                _ => {}
            }
        }
        // DateTime: minimal native clock — the ctor stores the parsed
        // timestamp so getTimestamp/diff can read it back
        // (closure_call_internal).
        if matches!(
            cls.name().to_lowercase().as_str(),
            "datetime" | "datetimeimmutable"
        ) {
            match name.to_lowercase().as_str() {
                "__construct" => {
                    let ts = match args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null)
                    {
                        Value::Str(s) => {
                            let s = crate::value::lossy(&s);
                            match s.strip_prefix('@') {
                                Some(num) => num.trim().parse::<i64>().unwrap_or(0),
                                // Relative formats beyond '@N' are
                                // stubs — treat as epoch for now.
                                None => 0,
                            }
                        }
                        _ => 0,
                    };
                    obj.borrow_mut()
                        .props
                        .insert("\0dt\0ts".into(), cell(Value::Int(ts)));
                    return Ok(Value::Null);
                }
                "gettimestamp" => {
                    return Ok(obj
                        .borrow()
                        .props
                        .get("\0dt\0ts")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Int(0)));
                }
                _ => {}
            }
        }
        // ArrayIterator / ArrayObject: native storage state on the
        // object internal. Only native stubs dispatch here — a userland
        // override on a subclass (ArrayIteratorEx::rewind, myArray::
        // offsetGet — array_020/021/024, bug32134) still wins.
        if self.is_a_str(cls.name(), "arrayiterator") || self.is_a_str(cls.name(), "arrayobject") {
            let stub = self
                .find_method_in(&cls, name)
                .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
                .unwrap_or(true);
            if stub {
                if let Some(v) = self.array_iter_method(&obj, name, &args)? {
                    return Ok(v);
                }
            }
        }
        // Generator: same pattern — native iteration state.
        if cls.name().eq_ignore_ascii_case("generator") {
            if let Some(v) = self.generator_method(&obj, name, &args)? {
                return Ok(v);
            }
        }
        // SplFileInfo / DirectoryIterator family: SPL filesystem
        // objects — is_a covers FilesystemIterator,
        // RecursiveDirectoryIterator and userland subclasses. Only the
        // native stubs dispatch here — a userland override (e.g.
        // symfony/finder's current()) still wins.
        if self.is_a_str(cls.name(), "splfileinfo") {
            let stub = self
                .find_method_in(&cls, name)
                .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
                .unwrap_or(false);
            if stub {
                if let Some(v) = self.spl_method(&obj, name, &args)? {
                    return Ok(v);
                }
            }
        }
        // PDO / PDOStatement: sqlite-backed storage surface (#15 spike).
        if cls.name().eq_ignore_ascii_case("pdo") {
            if let Some(v) = crate::pdo::pdo_method(self, &obj, name, &args)? {
                return Ok(v);
            }
        }
        if cls.name().eq_ignore_ascii_case("pdostatement") {
            if let Some(v) = crate::pdo::pdostmt_method(self, &obj, name, &args)? {
                return Ok(v);
            }
        }
        match self.find_method_in(&cls, name) {
            Some((m, dc)) => {
                if m.is_abstract {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot call abstract method {}::{}()", dc.name(), name),
                        0,
                    ));
                }
                self.invoke_method(obj, &m, args, dc)
            }
            None => {
                if let Some((m, dc)) = self.find_method_in(&cls, "__call") {
                    return self.call_via_magic(obj, &m, dc, name, args);
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!("Call to undefined method {}::{}()", cls.name(), name),
                    0,
                ))
            }
        }
    }

    /// Native implementations of Throwable methods.
    pub(in crate::interp) fn throwable_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        _args: &[Cell],
    ) -> Option<Value> {
        let ob = obj.borrow();
        let lname = name.to_lowercase();
        match lname.as_str() {
            "getmessage" => Some(
                ob.props
                    .get("message")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null),
            ),
            "getcode" => Some(
                ob.props
                    .get("code")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Int(0)),
            ),
            "getfile" => match &ob.internal {
                Some(ObjectInternal::Exception { file, .. }) => Some(Value::str(file.clone())),
                _ => Some(Value::str(self.diag_file())),
            },
            "getline" => match &ob.internal {
                Some(ObjectInternal::Exception { line, .. }) => Some(Value::Int(*line as i64)),
                _ => Some(Value::Int(self.cur_line as i64)),
            },
            "gettrace" => {
                let mut arr = PhpArray::new();
                let frames: Vec<TraceFrame> = match &ob.internal {
                    Some(ObjectInternal::Exception { frames, .. }) => (**frames).clone(),
                    _ => Vec::new(),
                };
                // PHP orders innermost call first (tests/lang/038);
                // internal-function call sites carry no file/line.
                for fr in frames.iter().rev() {
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
                    for (n, av) in &fr.named_args {
                        a.set(ArrKey::Str(n.clone().into()), av.borrow().clone());
                    }
                    f.set(
                        ArrKey::Str("args".into()),
                        Value::Array(Rc::new(RefCell::new(a))),
                    );
                    arr.push(Value::Array(Rc::new(RefCell::new(f))));
                }
                Some(Value::Array(Rc::new(RefCell::new(arr))))
            }
            "gettraceasstring" => match &ob.internal {
                Some(ObjectInternal::Exception { trace, .. }) if !trace.is_empty() => {
                    Some(Value::str(trace.clone()))
                }
                Some(ObjectInternal::Exception { frames, .. }) if !frames.is_empty() => {
                    Some(Value::str(format_trace(frames)))
                }
                _ => Some(Value::str("#0 {main}")),
            },
            "getprevious" => Some(Value::Null),
            "__tostring" => {
                let msg = ob
                    .props
                    .get("message")
                    .map(|c| c.borrow().to_php_string())
                    .unwrap_or_default();
                let (file, line, trace, full) = match &ob.internal {
                    Some(ObjectInternal::Exception {
                        file,
                        line,
                        trace,
                        frames,
                        full_msg,
                        ..
                    }) => {
                        let t = if !trace.is_empty() {
                            trace.clone()
                        } else if !frames.is_empty() {
                            format_trace(frames)
                        } else {
                            "#0 {main}".into()
                        };
                        (file.clone(), *line, t, full_msg.clone())
                    }
                    _ => (
                        self.diag_file(),
                        self.cur_line as u32,
                        "#0 {main}".into(),
                        String::new(),
                    ),
                };
                let msg = if full.is_empty() { msg } else { full };
                Some(Value::str(format!(
                    "{}: {} in {}:{}\nStack trace:\n{}",
                    ob.class.name(),
                    msg,
                    file,
                    line,
                    trace
                )))
            }
            "__construct" => {
                // Builtin ctor: props from args message/code.
                drop(ob);
                let mut ob = obj.borrow_mut();
                let msg = _args
                    .first()
                    .map(|c| c.borrow().to_php_string())
                    .unwrap_or_default();
                let code = _args.get(1).map(|c| c.borrow().to_int()).unwrap_or(0);
                ob.props.insert("message".into(), cell(Value::str(msg)));
                ob.props.insert("code".into(), cell(Value::Int(code)));
                if !ob.prop_order.contains(&"message".into()) {
                    ob.prop_order.push("message".into());
                    ob.prop_order.push("code".into());
                }
                Some(Value::Null)
            }
            _ => None,
        }
    }

    /// `X::` member access where X may be a trait: traits resolve to a
    /// synthesized class holding their statics; direct trait member
    /// access is deprecated (direct_static_member_access). Returns the
    /// resolved class plus the trait's display name when it is one.
    pub(in crate::interp) fn member_class_of(
        &mut self,
        class: &Expr,
    ) -> Result<(Rc<PhpClass>, Option<String>), PhpError> {
        let name = self.class_name_of(class)?;
        if let Some(td) = self.traits.get(&name.to_lowercase()).cloned() {
            let key = td.name.to_lowercase();
            let cls = self
                .trait_statics
                .entry(key)
                .or_insert_with(|| {
                    Rc::new(PhpClass {
                        decl: td.clone(),
                        statics: RefCell::new(HashMap::new()),
                        statics_init: RefCell::new(false),
                    })
                })
                .clone();
            return Ok((cls, Some(td.name.clone())));
        }
        Ok((self.class_of(class)?, None))
    }

    /// is_callable(['Cls'|$obj, 'm']): 'parent'/'self'/'static' names
    /// resolve against the caller's scope; "parent"/"self" string
    /// callables are deprecated once they resolve (bug76773-deprecated).
    pub fn is_callable_arr(&mut self, first: &Value, mname: &str) -> bool {
        let cls = match first {
            Value::Callable(_) => return mname.eq_ignore_ascii_case("__invoke"),
            Value::Object(o) => Some(o.borrow().class.clone()),
            Value::Str(n) => {
                let n = crate::value::lossy(n);
                let ln = n.trim_start_matches('\\').to_lowercase();
                match ln.as_str() {
                    "parent" | "self" | "static" => {
                        let Some(scope) = self.caller_scope_name() else {
                            return false;
                        };
                        let Some(sc) = self.classes.get(&scope.to_lowercase()).cloned() else {
                            return false;
                        };
                        let target = match ln.as_str() {
                            "parent" => sc
                                .decl
                                .parent
                                .as_ref()
                                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned()),
                            "static" => self
                                .stack
                                .last()
                                .and_then(|f| f.called_class.clone())
                                .or(Some(sc)),
                            _ => Some(sc),
                        };
                        if let Some(c) = &target {
                            if self.find_method_in(c, mname).is_some() {
                                self.deprecated(&format!(
                                    "Use of \"{}\" in callables is deprecated",
                                    ln
                                ))
                                .ok();
                                return true;
                            }
                        }
                        return false;
                    }
                    _ => {
                        if !self.classes.contains_key(&ln) {
                            let _ = self.run_autoload(&n);
                            self.pending_exception = None;
                        }
                        self.classes.get(&ln).cloned()
                    }
                }
            }
            _ => None,
        };
        match cls {
            Some(c) => self.find_method_in(&c, mname).is_some(),
            None => false,
        }
    }
}

/// zend_hash copy for spl storage copies: plain cells copy by value,
/// shared php-reference cells stay bound (`new ArrayObject` /
/// `exchangeArray` / `getArrayCopy` all behave this way in zend).
fn ao_copy(a: &PhpArray) -> PhpArray {
    let mut copy = PhpArray::new();
    for (k, c) in &a.entries {
        if matches!(k, ArrKey::Tomb) {
            continue;
        }
        // zend array_dup keeps IS_REFERENCE elements bound — our is_ref
        // flag marks arrays that gained &-aliases, so a shared cell
        // alone (e.g. prop cells bound for object-backing) isn't a
        // reference and copies by value.
        if a.is_ref && Rc::strong_count(c) > 1 {
            copy.is_ref = true;
            copy.bind_cell(k.clone(), c.clone());
        } else {
            copy.set(k.clone(), c.borrow().clone());
        }
    }
    copy
}
