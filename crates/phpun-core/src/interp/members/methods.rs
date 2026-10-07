//! Method dispatch + magic: method calls, `__call`/`__get` magic
//! routing, visibility errors and prototypes, throwable plumbing,
//! native `ArrayIterator`/callable dispatch helpers.

use super::*;

/// Parsed legacy spl `serialize()` payload: (flags, storage, props).
type AoUnserData = (i64, Value, Rc<RefCell<PhpArray>>);

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
                    // User input is masked to the 16 user bits — bits
                    // >= 0x10000 are engine-internal (self-backed
                    // storage) and not forgeable through zpp.
                    Ok(i) => flags = i & 0xFFFF,
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
            // `parent::__construct($this)` — self-backed storage: the
            // object's own prop table IS the storage; zend marks it
            // with the engine flag bit 0x1000000 (serialize slot 1 → N).
            let self_backed = matches!(&first, Some(Value::Object(o)) if Rc::ptr_eq(o, obj));
            // Self-backing skips ao_backing (its own prop mirror), so
            // the object-arg deprecation is emitted here; other object
            // args get it inside ao_backing.
            if self_backed {
                self.ao_obj_deprecation(family, "__construct")?;
            }
            let backing = match &first {
                Some(Value::Object(o)) if self_backed => Some((self.ao_obj_backing(o)?, None)),
                Some(v) => Some(self.ao_backing(v, family, "__construct")?),
                None => None,
            };
            // Any object arg becomes `src` — for an spl source zend
            // stays object-backed too and resolves the source's live
            // storage on every access (ao_arr follows the chain).
            let src_obj = match &first {
                Some(Value::Object(o)) => Some(o.clone()),
                _ => None,
            };
            // No explicit flags arg → an spl-array source's user flags
            // carry over (zend spl_array_object_new_ex); array/plain
            // inputs default to 0. Engine bits never transfer — the
            // new object resolves through the src chain instead.
            if args.cells.len() < 2 {
                if let Some((_, Some(sf))) = &backing {
                    flags = *sf & 0xFFFF;
                }
            }
            if self_backed {
                flags |= 0x1000000;
            }
            let mut ob = obj.borrow_mut();
            ob.internal = Some(ObjectInternal::ArrayIter {
                store: Rc::new(RefCell::new(crate::value::AoStore {
                    arr: backing.map(|(a, _)| a).unwrap_or_default(),
                    src: src_obj,
                })),
                pos: 0,
                flags,
                iterator_class,
                sorting: false,
            });
            return Ok(Some(Value::Null));
        }
        // Everything else needs storage — zend lazily creates it on
        // first access (newInstanceWithoutConstructor).
        let (arr, pos, flags) = self.ao_state(obj);
        self.ao_sync_props(obj, &arr);
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
                if let Some(e) = self.ao_sorting_err(obj) {
                    return self.fail(e);
                }
                let v = args
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                arr.borrow_mut().push(v);
                Value::Null
            }
            "exchangearray" => {
                if let Some(e) = self.ao_sorting_err(obj) {
                    return self.fail(e);
                }
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
                // Passing the object itself: zend doesn't share its own
                // storage — the arg takes the plain-object path (prop-hash
                // backing, self-backed flag), which for an ArrayObject's
                // empty prop table means an empty storage.
                let self_arg = matches!(&v, Value::Object(o) if Rc::ptr_eq(o, obj));
                let (new, src_flags) = if self_arg {
                    self.deprecated(&format!(
                        "{}::{}(): Using an object as a backing array for {} is deprecated, as it allows violating class constraints and invariants",
                        family, canonical, family
                    ))?;
                    (self.ao_obj_backing(obj)?, None)
                } else {
                    self.ao_backing(&v, family, canonical)?
                };
                // The new backing object replaces `src` too — plain-object
                // input binds prop cells live; array/spl input clears it.
                let new_src = match &v {
                    Value::Object(o)
                        if self_arg
                            || !matches!(
                                o.borrow().internal,
                                Some(ObjectInternal::ArrayIter { .. })
                            ) =>
                    {
                        Some(o.clone())
                    }
                    _ => None,
                };
                // zend model, verified against the oracle:
                // - array arg: contents are copied INTO our shared
                //   storage hash (siblings of the target see the swap;
                //   the arg array itself stays intact).
                // - spl-object arg: the arg's storage table is adopted
                //   BY POINTER — writes through either object reach
                //   both (live share).
                let arg_is_spl = matches!(
                    &v,
                    Value::Object(o)
                        if !self_arg
                            && matches!(o.borrow().internal, Some(ObjectInternal::ArrayIter { .. }))
                );
                let st = self.ao_store(obj);
                let old_inner = {
                    let mut sb = st.borrow_mut();
                    {
                        let mut ob = obj.borrow_mut();
                        match &mut ob.internal {
                            Some(ObjectInternal::ArrayIter { flags: f, .. }) => {
                                // An spl-array source merges its user
                                // flags in (zend USE_OTHER |= ); plain
                                // inputs keep ours. Engine bits of the
                                // source stay behind — dst isn't
                                // self/other-backed itself.
                                if let Some(sf) = src_flags {
                                    *f |= sf & 0xFFFF;
                                }
                                if self_arg {
                                    *f |= 0x1000000;
                                }
                            }
                            _ => unreachable!(),
                        }
                    }
                    sb.src = new_src;
                    if Rc::ptr_eq(&sb.arr, &new) {
                        None
                    } else if arg_is_spl {
                        // zend caches the arg's table pointer — our own
                        // sb.arr becomes the arg's live table.
                        let prev = std::mem::replace(&mut sb.arr, new.clone());
                        let mut prev = prev.borrow_mut();
                        Some(std::mem::take(&mut *prev))
                    } else {
                        let copied = self.dup_array(&new.borrow());
                        Some(std::mem::replace(&mut *sb.arr.borrow_mut(), copied))
                    }
                };
                match old_inner {
                    Some(old_inner) => {
                        // Deep-copy so cells still bound to object props
                        // don't print `&`/alias in the returned array
                        // (bug41691: `NULL` not `&NULL`).
                        Value::Array(Rc::new(RefCell::new(self.dup_array(&old_inner))))
                    }
                    None => Value::Array(arr.clone()),
                }
            }
            "getarraycopy" => Value::Array(Rc::new(RefCell::new(self.dup_array(&arr.borrow())))),
            "offsetget" => {
                let raw_k = args
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let k = self.ao_dim_key(obj, &raw_k);
                // zend read_dimension(BP_VAR_W|RW) trips nApplyCount —
                // `$o[k]=`, `$o[k][j]=`, `$o[k]++`, `=& $o[k]` inside a
                // sort callback all error; plain reads don't.
                if self.dim_by_ref {
                    if let Some(e) = self.ao_sorting_err(obj) {
                        return self.fail(e);
                    }
                }
                let got = arr.borrow().get_cell(&k);
                match got {
                    Some(c) => {
                        // zend read_dimension returns the bucket zval —
                        // ++/-- and `=&` binds reach it through
                        // last_ret_cell and write through the cell.
                        self.last_ret_cell = Some(c.clone());
                        c.borrow().clone()
                    }
                    None => {
                        // By-ref reads (`$x =& $ao['k']`) silently
                        // create the bucket like zend's read_dimension
                        // (BP_VAR_RW) — the new cell is handed back.
                        if self.dim_by_ref {
                            // Object-backed: the new bucket is also
                            // a real prop so both views agree.
                            if let Some(src) = self.ao_src_obj(obj) {
                                self.ao_obj_dim_write(obj, &src, &arr, k.clone(), Value::Null);
                            } else {
                                arr.borrow_mut().bind_cell(k.clone(), cell(Value::Null));
                            }
                            let c = arr.borrow().get_cell(&k).unwrap();
                            self.last_ret_cell = Some(c.clone());
                            return Ok(Some(Value::Null));
                        }
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
                let raw_k = args
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let k = self.ao_dim_key(obj, &raw_k);
                Value::Bool(arr.borrow().get(&k).is_some())
            }
            "offsetset" => {
                if let Some(e) = self.ao_sorting_err(obj) {
                    return self.fail(e);
                }
                let v = args
                    .cells
                    .get(1)
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                match args.cells.first().map(|c| c.borrow().clone()) {
                    Some(Value::Null) | None => {
                        // `$ao[]=` on object-backed storage appends an
                        // int-keyed bucket to the prop table — counted
                        // and iterated but NOT reachable via dim reads
                        // (zend stores it under an int key while reads
                        // look the name up as a string).
                        if let Some(src) = self.ao_src_obj(obj) {
                            if !Rc::ptr_eq(&src, obj)
                                && matches!(
                                    src.borrow().internal,
                                    Some(ObjectInternal::ArrayIter { .. })
                                )
                            {
                                // SPL backing: `[]=` appends into its
                                // live storage (arr resolves there).
                                arr.borrow_mut().push(v);
                                return Ok(Some(Value::Null));
                            }
                            // zend's next-index over the prop hash:
                            // one past the highest int-keyed bucket.
                            let next = {
                                let arr_max = arr
                                    .borrow()
                                    .entries
                                    .iter()
                                    .filter_map(|(k, _)| match k {
                                        ArrKey::Int(i) => Some(*i),
                                        _ => None,
                                    })
                                    .max()
                                    .map(|m| m + 1);
                                let prop_max = src
                                    .borrow()
                                    .props
                                    .keys()
                                    // Only int-keyed buckets count —
                                    // a "5" string prop doesn't move
                                    // zend's next-index cursor.
                                    .filter_map(|k| crate::value::int_prop_index(k))
                                    .max()
                                    .map(|m| m + 1);
                                arr_max.max(prop_max).unwrap_or(0)
                            };
                            // Int-keyed bucket only — dim reads resolve
                            // prop NAMES, so it stays unreachable via
                            // $ao[0] / isset (oracle: counted + iterated
                            // but never readable). Zend lands the bucket
                            // in the backing object's prop hash too — as
                            // an INT-keyed slot — so dumps/count/casts
                            // and foreach see it.
                            let pc = cell(v);
                            arr.borrow_mut().bind_cell(ArrKey::Int(next), pc.clone());
                            {
                                let mut so = src.borrow_mut();
                                let pk = crate::value::int_prop_key(next);
                                if !so.props.contains_key(&pk) {
                                    so.prop_order.push(pk.clone());
                                }
                                so.props.insert(pk, pc);
                            }
                            return Ok(Some(Value::Null));
                        }
                        arr.borrow_mut().push(v)
                    }
                    Some(kv) => {
                        let k = self.ao_dim_key(obj, &kv);
                        // Object-backed storage IS the prop table: a
                        // dim write drops a fresh zval into the prop
                        // bucket — even severing a referenced prop.
                        if let Some(src) = self.ao_src_obj(obj) {
                            self.ao_obj_dim_write(obj, &src, &arr, k, v);
                            return Ok(Some(Value::Null));
                        }
                        // zend writes a fresh zval into the bucket: an
                        // &-reference element is severed (the alias
                        // keeps its old value), while a prop-bound
                        // cell (object backing / ARRAY_AS_PROPS)
                        // writes through so both views stay in sync.
                        let sever = arr
                            .borrow()
                            .get_cell(&k)
                            .map(|c| self.is_ref_cell(&c) && Rc::strong_count(&c) > 1)
                            .unwrap_or(false);
                        if sever {
                            arr.borrow_mut().bind_cell(k, cell(v));
                        } else {
                            arr.borrow_mut().set(k, v);
                        }
                    }
                }
                Value::Null
            }
            "offsetunset" => {
                if let Some(e) = self.ao_sorting_err(obj) {
                    return self.fail(e);
                }
                let raw_k = args
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let k = self.ao_dim_key(obj, &raw_k);
                // The evicted payload's last ref dies with the
                // cell — held objects/gens destruct now. Drop the
                // borrow before dtors run (bug65051).
                let evicted = arr.borrow_mut().unset(&k);
                if let Some(v) = evicted {
                    self.destruct_dying_value(&v)?;
                }
                // Object-backed storage mirrors props — the unset
                // removes the backing prop as well (spl backing has
                // no props; the arr.unset above already hit storage).
                if let Some(src) = self.ao_src_obj(obj) {
                    let pname = match &k {
                        ArrKey::Str(s) => Some(s.to_string()),
                        ArrKey::Int(i) => Some(i.to_string()),
                        ArrKey::Tomb => None,
                    };
                    if let Some(pname) = pname {
                        let spl_src = !Rc::ptr_eq(&src, obj)
                            && matches!(
                                src.borrow().internal,
                                Some(ObjectInternal::ArrayIter { .. })
                            );
                        if !spl_src {
                            src.borrow_mut().props.remove(&pname);
                        }
                    }
                }
                // pos pointing past the end stays clamped at reads.
                Value::Null
            }
            // Bits >= 0x10000 are engine-internal (self-backed storage)
            // — getFlags only reports the user array flags.
            "getflags" => Value::Int(flags & 0xFFFF),
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
                    // Replace the 16 user bits; engine bits
                    // (self-backed storage) are preserved.
                    *fp = (*fp & !0xFFFFi64) | (f & 0xFFFF);
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
                        let store = self.ao_store(obj);
                        let it = self.alloc_obj(PhpObject {
                            class: icls,
                            props: Default::default(),
                            prop_order: Vec::new(),
                            id: 0,
                            internal: Some(ObjectInternal::ArrayIter {
                                // The shared slot — the iterator sees
                                // later exchangeArray()/unserialize
                                // storage swaps on its parent.
                                store,
                                pos: 0,
                                flags,
                                iterator_class: None,
                                sorting: false,
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
            "asort" | "ksort" | "natsort" | "natcasesort" => {
                // zend spl_array_object_sort: flag param only on the
                // OPTIONAL_FLAG sorts; the nat sorts call the global
                // function with no flag arg (natcasesort ⇒
                // SORT_NATURAL|SORT_FLAG_CASE).
                let flag = match lname.as_str() {
                    "natsort" => 6,
                    "natcasesort" => 6 | 8,
                    _ => {
                        let flag = args.cells.first().map(|c| c.borrow().clone());
                        match flag {
                            Some(v) => match self.spl_int_arg(&v, family, canonical, 1, "flags") {
                                Ok(f) => f,
                                Err(e) => return self.fail(e),
                            },
                            None => 0,
                        }
                    }
                };
                Self::ao_set_sorting(obj, true);
                let src = arr.borrow().entries.clone();
                // zend's spl_array_object_sort invokes the GLOBAL builtin
                // (asort/ksort/natsort/natcasesort) — it leaves a builtin
                // frame on the stack that shows in thrown traces.
                let frame_args = if matches!(lname.as_str(), "natsort" | "natcasesort") {
                    vec![cell(Value::Array(arr.clone()))]
                } else {
                    vec![cell(Value::Array(arr.clone())), cell(Value::Int(flag))]
                };
                self.call_trace.push(crate::value::TraceFrame {
                    function: lname.clone(),
                    class: None,
                    ty: String::new(),
                    file: "[internal function]".into(),
                    line: 0,
                    args: frame_args,
                    named_args: Vec::new(),
                    internal: true,
                });
                self.internal_cb += 1;
                let (sorted, deep, conv_err) = crate::builtins::array::zend_sort_flags(
                    self,
                    &src,
                    flag,
                    false,
                    lname == "ksort",
                );
                self.internal_cb -= 1;
                Self::ao_set_sorting(obj, false);
                arr.borrow_mut().entries = sorted;
                // Notices queued inside the sort's compares (object→
                // number casts) must drain at THIS call site — the
                // builtin-boundary flush in call_builtin never runs on
                // the method path, so they'd strand onto the next
                // unrelated compare or vanish.
                let nr = self.emit_cmp_notices();
                if deep {
                    let e =
                        self.spl_throw("Error", "Nesting level too deep - recursive dependency?");
                    self.call_trace.pop();
                    nr?;
                    return self.fail(e);
                }
                if let Some(e) = conv_err {
                    self.call_trace.pop();
                    nr?;
                    return self.fail(e);
                }
                self.call_trace.pop();
                nr?;
                Value::Bool(true)
            }
            "uasort" | "uksort" => {
                let cb = args
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                if !self.is_callable_value(&cb) {
                    if let Some(pe) = self.take_callable_probe_err() {
                        return self.fail(pe);
                    }
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
                Self::ao_set_sorting(obj, true);
                let src = arr.borrow().entries.clone();
                self.call_trace.push(crate::value::TraceFrame {
                    function: lname.clone(),
                    class: None,
                    ty: String::new(),
                    file: "[internal function]".into(),
                    line: 0,
                    args: vec![cell(Value::Array(arr.clone())), cell(cb.clone())],
                    named_args: Vec::new(),
                    internal: true,
                });
                self.internal_cb += 1;
                let (sorted, cb_err) = crate::builtins::array::zend_sort_user(
                    self,
                    &src,
                    &cb,
                    lname == "uksort",
                    &lname,
                );
                self.internal_cb -= 1;
                Self::ao_set_sorting(obj, false);
                arr.borrow_mut().entries = sorted;
                let nr = self.emit_cmp_notices();
                if let Some(e) = cb_err {
                    self.call_trace.pop();
                    nr?;
                    return self.fail(e);
                }
                self.call_trace.pop();
                nr?;
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
                if data.is_empty() {
                    // zend returns early on an empty payload — storage
                    // is left untouched, no error.
                    return Ok(Some(Value::Null));
                }
                // zend order: ZPP → empty-payload early return →
                // nApplyCount guard → payload parse.
                if let Some(e) = self.ao_sorting_err(obj) {
                    return self.fail(e);
                }
                match self.ao_parse_payload(&data) {
                    Ok((pflags, sv, parr)) => {
                        // Storage goes through ao_backing like
                        // __unserialize — object payloads bind props and
                        // emit the backing deprecation.
                        let self_backed = pflags & 0x1000000 != 0;
                        let (backing, src_obj) = if self_backed {
                            (self.ao_obj_backing(obj)?, Some(obj.clone()))
                        } else {
                            let (backing, _) = match self.ao_backing(&sv, family, canonical) {
                                Ok(b) => b,
                                Err(e) => return self.fail(e),
                            };
                            let src_obj = match &sv {
                                Value::Object(o) => Some(o.clone()),
                                _ => None,
                            };
                            (backing, src_obj)
                        };
                        let mut ob = obj.borrow_mut();
                        ob.internal = Some(ObjectInternal::ArrayIter {
                            store: Rc::new(RefCell::new(crate::value::AoStore {
                                arr: backing,
                                src: src_obj,
                            })),
                            pos: 0,
                            flags: pflags,
                            iterator_class: None,
                            sorting: false,
                        });
                        drop(ob);
                        for (k, c) in parr.borrow().iter() {
                            let kn = match k {
                                ArrKey::Str(s) => s.to_string(),
                                ArrKey::Int(i) => i.to_string(),
                                ArrKey::Tomb => continue,
                            };
                            self.ao_restore_prop(obj, kn, c.clone())?;
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
                if let Some(e) = self.ao_sorting_err(obj) {
                    return self.fail(e);
                }
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
                // Self-backed storage serializes slot 1 as N — the
                // object's own prop table is the storage.
                let self_backed = flags_i & 0x1000000 != 0;
                if !(self_backed && matches!(st, Value::Null))
                    && !matches!(st, Value::Array(_) | Value::Object(_))
                {
                    let e = self.spl_throw(
                        "InvalidArgumentException",
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
                let (backing, src_obj) = if self_backed {
                    (self.ao_obj_backing(obj)?, Some(obj.clone()))
                } else {
                    let b = self.ao_backing(&st, family, canonical)?;
                    let s = match &st {
                        Value::Object(o) => Some(o.clone()),
                        _ => None,
                    };
                    (b.0, s)
                };
                let mut ob = obj.borrow_mut();
                ob.internal = Some(ObjectInternal::ArrayIter {
                    store: Rc::new(RefCell::new(crate::value::AoStore {
                        arr: backing,
                        src: src_obj,
                    })),
                    pos: 0,
                    flags: flags_i,
                    iterator_class: None,
                    sorting: false,
                });
                drop(ob);
                for (k, c) in props_arr.borrow().iter() {
                    let kn = match k {
                        ArrKey::Str(s) => s.to_string(),
                        ArrKey::Int(i) => i.to_string(),
                        ArrKey::Tomb => continue,
                    };
                    self.ao_restore_prop(obj, kn, c.clone())?;
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
                                store: Rc::new(RefCell::new(crate::value::AoStore {
                                    arr: backing.0,
                                    src: src_obj,
                                })),
                                pos: 0,
                                // The iterator is a different object —
                                // only the source's user flags carry.
                                flags: backing.1.unwrap_or(flags) & 0xFFFF,
                                iterator_class: None,
                                sorting: false,
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

    /// zend's nApplyCount++/−− around spl_array_object_sort: set while
    /// a sort method runs so writes below trip the guard.
    fn ao_set_sorting(obj: &Rc<RefCell<PhpObject>>, on: bool) {
        if let Some(ObjectInternal::ArrayIter { sorting, .. }) = &mut obj.borrow_mut().internal {
            *sorting = on;
        }
    }

    /// zend's spl_array_apply_count_guard: every storage write while a
    /// sort is in flight raises this Error (the message uses the
    /// runtime class name).
    pub(in crate::interp) fn ao_sorting_err(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
    ) -> Option<PhpError> {
        let sorted = matches!(
            obj.borrow().internal,
            Some(ObjectInternal::ArrayIter { sorting: true, .. })
        );
        if sorted {
            // zend hardcodes "ArrayObject" even when the object is an
            // ArrayIterator (spl_array_apply_count_guard).
            Some(self.spl_throw(
                "Error",
                "Modification of ArrayObject during sorting is prohibited",
            ))
        } else {
            None
        }
    }

    /// ARRAY_AS_PROPS (flag bit 2): undeclared prop access on this spl
    /// array-object routes to the storage hash.
    pub(in crate::interp) fn aap_active(&mut self, o: &Rc<RefCell<PhpObject>>) -> bool {
        match &o.borrow().internal {
            Some(ObjectInternal::ArrayIter { flags, .. }) => *flags & 2 != 0,
            _ => false,
        }
    }

    /// The shared storage slot — lazily created on first access, like
    /// zend materializing `intern->array` for `newInstanceWithoutConstructor`.
    pub(in crate::interp) fn ao_store(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
    ) -> Rc<RefCell<crate::value::AoStore>> {
        let mut ob = obj.borrow_mut();
        if !matches!(ob.internal, Some(ObjectInternal::ArrayIter { .. })) {
            ob.internal = Some(ObjectInternal::ArrayIter {
                store: Rc::new(RefCell::new(crate::value::AoStore {
                    arr: Rc::new(RefCell::new(PhpArray::new())),
                    src: None,
                })),
                pos: 0,
                flags: 0,
                iterator_class: None,
                sorting: false,
            });
        }
        match &ob.internal {
            Some(ObjectInternal::ArrayIter { store, .. }) => store.clone(),
            _ => unreachable!(),
        }
    }

    /// The CURRENT storage table of an spl object. When the backing is
    /// another spl object, zend's spl_array_get_hash_table hands out
    /// the source's live table pointer — an `exchangeArray`/`offsetSet`
    /// on the source stays visible through this wrapper, so resolve
    /// through the src chain instead of the mirror snapshot.
    pub(crate) fn ao_arr(&mut self, obj: &Rc<RefCell<PhpObject>>) -> Rc<RefCell<PhpArray>> {
        let mut cur = obj.clone();
        let mut seen = std::collections::HashSet::new();
        loop {
            seen.insert(Rc::as_ptr(&cur) as usize);
            let st = self.ao_store(&cur);
            let next = {
                let sb = st.borrow();
                match &sb.src {
                    Some(src_o)
                        if matches!(
                            src_o.borrow().internal,
                            Some(ObjectInternal::ArrayIter { .. })
                        ) && !seen.contains(&(Rc::as_ptr(src_o) as usize)) =>
                    {
                        Some(src_o.clone())
                    }
                    _ => None,
                }
            };
            match next {
                Some(src_o) => cur = src_o,
                None => return st.borrow().arr.clone(),
            }
        }
    }

    /// (storage, pos, flags) of an spl array-object, lazily creating the
    /// internal storage on first access — zend materializes it on demand
    /// for `newInstanceWithoutConstructor` objects too.
    pub(in crate::interp) fn ao_state(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
    ) -> (Rc<RefCell<PhpArray>>, usize, i64) {
        let arr = self.ao_arr(obj);
        let pos = match &obj.borrow().internal {
            Some(ObjectInternal::ArrayIter { pos, .. }) => *pos,
            _ => unreachable!(),
        };
        let flags = match &obj.borrow().internal {
            Some(ObjectInternal::ArrayIter { flags, .. }) => *flags,
            _ => unreachable!(),
        };
        (arr, pos, flags)
    }

    /// The backing object when storage came from an object input.
    pub(in crate::interp) fn ao_src_obj(
        &self,
        obj: &Rc<RefCell<PhpObject>>,
    ) -> Option<Rc<RefCell<PhpObject>>> {
        match &obj.borrow().internal {
            Some(ObjectInternal::ArrayIter { store, .. }) => store.borrow().src.clone(),
            _ => None,
        }
    }

    /// Restore a prop from a serialized payload — zend routes the
    /// write through write_property, so creating an undeclared prop
    /// deprecates like any dynamic-prop write (bug74669).
    fn ao_restore_prop(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        kn: String,
        c: crate::value::Cell,
    ) -> Result<(), PhpError> {
        let (is_new, exempt, cls_name) = {
            let ob = obj.borrow();
            (
                !ob.props.contains_key(&kn),
                ob.class.name().eq_ignore_ascii_case("stdclass")
                    || ob.class.decl.attrs.iter().any(|a| {
                        a.name
                            .rsplit('\\')
                            .next()
                            .unwrap_or(&a.name)
                            .eq_ignore_ascii_case("AllowDynamicProperties")
                    }),
                ob.class.name().to_string(),
            )
        };
        if is_new && !exempt && self.decl_prop(obj, &kn).is_none() {
            self.deprecated(&format!(
                "Creation of dynamic property {}::${} is deprecated",
                cls_name, kn
            ))?;
        }
        let mut ob = obj.borrow_mut();
        if !ob.prop_order.contains(&kn) {
            ob.prop_order.push(kn.clone());
        }
        ob.props.insert(kn, c);
        Ok(())
    }

    /// zend deprecation for object-backed storage — any object arg
    /// (plain, spl, or `$this`), keyed to the spl FAMILY name.
    fn ao_obj_deprecation(&mut self, family: &str, method: &str) -> Result<(), PhpError> {
        self.deprecated(&format!(
            "{}::{}(): Using an object as a backing array for {} is deprecated, \
             as it allows violating class constraints and invariants",
            family, method, family
        ))
    }

    /// Dim key for spl storage: array-backed keys canonicalize like a
    /// zend array ("0" → int 0); object-backed storage IS a property
    /// hash, where every offset resolves to its prop NAME (int 0 reads
    /// prop "0"), so int buckets written by `[]=` stay unreachable.
    /// An SPL backing object resolves through its own storage table —
    /// canonical array keys again, not prop names.
    fn ao_dim_key(&mut self, obj: &Rc<RefCell<PhpObject>>, kv: &Value) -> ArrKey {
        let spl_src = self.ao_src_obj(obj).is_some_and(|src| {
            !Rc::ptr_eq(&src, obj)
                && matches!(
                    src.borrow().internal,
                    Some(ObjectInternal::ArrayIter { .. })
                )
        });
        if self.ao_src_obj(obj).is_some() && !spl_src {
            let name = match kv {
                Value::Str(s) => crate::value::lossy(s).into_owned(),
                other => other.to_php_string(),
            };
            ArrKey::Str(Rc::from(name.as_str()))
        } else {
            to_key(kv)
        }
    }

    /// Object-backed dim/prop write: a fresh zval lands on the backing
    /// object's prop slot (a referenced prop is severed like zend's
    /// write_dimension) and the new cell binds into storage so both
    /// views stay in sync.
    pub(in crate::interp) fn ao_obj_dim_write(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        src: &Rc<RefCell<PhpObject>>,
        arr: &Rc<RefCell<PhpArray>>,
        k: ArrKey,
        v: Value,
    ) {
        // SPL backing object (a DIFFERENT object): the write lands
        // in its live storage table — `arr` is already that resolved
        // table. Self-backed is excluded: its prop table IS storage,
        // so it keeps the prop-write path below.
        if !Rc::ptr_eq(src, obj)
            && matches!(
                src.borrow().internal,
                Some(ObjectInternal::ArrayIter { .. })
            )
        {
            arr.borrow_mut().set(k, v);
            return;
        }
        let pname = match &k {
            ArrKey::Str(s) => s.to_string(),
            ArrKey::Int(i) => i.to_string(),
            ArrKey::Tomb => "0".to_string(),
        };
        let pc = cell(v);
        {
            let mut so = src.borrow_mut();
            if !so.props.contains_key(&pname) {
                so.prop_order.push(pname.clone());
            }
            so.props.insert(pname.clone(), pc.clone());
        }
        // The prop HT is the storage hash — `$ao[5]` writes a STRING
        // "5" slot (zend doesn't symtable-convert object prop names),
        // so storage iterates it under the string key and `[]=`'s
        // int-cursor doesn't see it.
        arr.borrow_mut().bind_cell(ArrKey::Str(pname.into()), pc);
    }

    /// Object-backed storage mirrors the live prop table (zend keeps
    /// the object's properties HT as storage): props added on the
    /// object appear, props unset on the object disappear, and a
    /// recreated prop rebinds its fresh cell.
    fn ao_sync_props(&mut self, obj: &Rc<RefCell<PhpObject>>, arr: &Rc<RefCell<PhpArray>>) {
        let Some(src) = self.ao_src_obj(obj) else {
            return;
        };
        // A DIFFERENT spl backing object: dims already resolve through
        // ao_arr — prop mirroring would only shadow the real storage.
        // (Self-backed keeps syncing: its prop table IS the storage.)
        if !Rc::ptr_eq(&src, obj)
            && matches!(
                src.borrow().internal,
                Some(ObjectInternal::ArrayIter { .. })
            )
        {
            return;
        }
        let so = src.borrow();
        for pname in &so.prop_order {
            // `\0Class\0priv` mangled names are visibility metadata,
            // not storage keys — zend keeps private props out of the
            // spl storage hash entirely.
            if pname.starts_with('\0') {
                continue;
            }
            if let Some(pc) = so.props.get(pname) {
                let k = ArrKey::Str(pname.clone().into());
                let mut a = arr.borrow_mut();
                match a.get_cell(&k) {
                    Some(c) if Rc::ptr_eq(&c, pc) => {}
                    _ => {
                        a.bind_cell(k, pc.clone());
                    }
                }
            }
        }
        let stale: Vec<ArrKey> = arr
            .borrow()
            .entries
            .iter()
            .filter_map(|(k, _)| match k {
                ArrKey::Str(s) if !so.props.contains_key(s.as_ref()) => Some(k.clone()),
                _ => None,
            })
            .collect();
        drop(so);
        for k in stale {
            // Drop the borrow before the evicted payload's dtors run.
            let evicted = arr.borrow_mut().unset(&k);
            if let Some(v) = evicted {
                let _ = self.destruct_dying_value(&v);
            }
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
            // A plain array input is CoW-separated: writes through the
            // wrapper must not reach the caller's variable.
            Value::Array(a) => Ok((Rc::new(RefCell::new(self.dup_array(&a.borrow()))), None)),
            Value::Object(o) => {
                self.deprecated(&format!(
                    "{}::{}(): Using an object as a backing array for {} is deprecated, as it allows violating class constraints and invariants",
                    family, mname, family
                ))?;
                let src_flags = {
                    let ob = o.borrow();
                    match &ob.internal {
                        Some(ObjectInternal::ArrayIter { flags, .. }) => Some(*flags),
                        _ => None,
                    }
                };
                if let Some(src_flags) = src_flags {
                    // An spl-array source shares the storage hash — writes
                    // through the new wrapper reach the source's CURRENT
                    // storage (zend spl_array_get_hash_table hands the
                    // live table out, so later exchangeArray on the
                    // source stays visible).
                    return Ok((self.ao_arr(o), Some(src_flags)));
                }
                self.ao_obj_backing(o).map(|b| (b, None))
            }
            _ => unreachable!("callers validate array|object before ao_backing"),
        }
    }

    /// Object storage backing (the deprecation already emitted): a
    /// prop-mirror table — objects iterate their prop cells BY
    /// REFERENCE so writes through `$v` update the prop (the typed
    /// gate still applies); deprecated since 8.5
    /// (typed_properties_113/114/115).
    fn ao_obj_backing(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
    ) -> Result<Rc<RefCell<PhpArray>>, PhpError> {
        let mut copy = PhpArray::new();
        // prop_order mirrors zend's properties HT order —
        // dumping/iterating the storage lists p before q.
        let pairs: Vec<(String, Cell)> = {
            let ob = o.borrow();
            let mut seen: Vec<(String, Cell)> = ob
                .prop_order
                .iter()
                // `\0Class\0priv` mangled names are visibility
                // metadata — zend keeps private props out of the
                // spl storage hash entirely; int-keyed buckets are
                // real storage slots.
                .filter(|n| !n.starts_with('\0') || crate::value::int_prop_index(n).is_some())
                .filter_map(|n| ob.props.get(n).map(|c| (n.clone(), c.clone())))
                .collect();
            for (k, c) in &ob.props {
                if (!k.starts_with('\0') || crate::value::int_prop_index(k).is_some())
                    && !seen.iter().any(|(n, _)| n == k)
                {
                    seen.push((k.clone(), c.clone()));
                }
            }
            seen
        };
        for (k, c) in pairs {
            // Int-keyed prop slots mirror back as int array keys.
            if let Some(i) = crate::value::int_prop_index(&k) {
                copy.bind_cell(ArrKey::Int(i), c.clone());
                continue;
            }
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
        Ok(Rc::new(RefCell::new(copy)))
    }

    /// `__serialize` slot 1 / the legacy payload's storage slot: the
    /// backing OBJECT when storage came from an object input, else the
    /// storage array (zend serializes the object verbatim so
    /// unserialize can re-bind it). A self-backed object serializes
    /// the slot as N (flag bit 0x1000000).
    pub(crate) fn ao_src(&mut self, obj: &Rc<RefCell<PhpObject>>) -> Value {
        match &obj.borrow().internal {
            Some(ObjectInternal::ArrayIter { store, flags, .. }) => {
                if flags & 0x1000000 != 0 {
                    Value::Null
                } else {
                    let st = store.borrow();
                    match &st.src {
                        Some(o) => Value::Object(o.clone()),
                        None => Value::Array(st.arr.clone()),
                    }
                }
            }
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

    /// Parse the legacy spl payload `x:i:<flags>;<storage>;m:<props>` —
    /// returns (flags, storage, props) or Err(zend's reported offset):
    /// the literal-match position for `x:`/`;`/m`:` separators, or the
    /// sub-parse's own start offset for flags/storage/props failures.
    fn ao_parse_payload(&mut self, data: &str) -> Result<AoUnserData, usize> {
        let b = data.as_bytes();
        let mut pos = 0usize;
        if b.get(pos) != Some(&b'x') {
            return Err(pos);
        }
        pos += 1;
        if b.get(pos) != Some(&b':') {
            return Err(pos);
        }
        pos += 1;
        // flags `i:<int>;` — any failure inside reports offset 2.
        let fstart = pos;
        if b.get(pos) != Some(&b'i') {
            return Err(fstart);
        }
        pos += 1;
        if b.get(pos) != Some(&b':') {
            return Err(fstart);
        }
        pos += 1;
        let dstart = pos;
        while pos < b.len() && b[pos] != b';' {
            pos += 1;
        }
        if pos >= b.len() {
            return Err(fstart);
        }
        let fl: i64 = data[dstart..pos].parse().map_err(|_| fstart)?;
        pos += 1; // ;
                  // storage: serialized array|object — zend reports the
                  // sub-parse's start offset, not where inside it died.
        let st_start = pos;
        let mut vhash: Vec<crate::value::Cell> = Vec::new();
        let sv = {
            let mut ie = None;
            crate::builtins::var::php_unserialize(self, data, &mut pos, &mut ie, &mut vhash)
                .map_err(|_| st_start)?
                .borrow()
                .clone()
        };
        if !matches!(sv, Value::Array(_) | Value::Object(_)) {
            return Err(st_start);
        }
        // `;m:` — each separator char reports its own position.
        if b.get(pos) != Some(&b';') {
            return Err(pos);
        }
        pos += 1;
        if b.get(pos) != Some(&b'm') {
            return Err(pos);
        }
        pos += 1;
        if b.get(pos) != Some(&b':') {
            return Err(pos);
        }
        pos += 1;
        let pr_start = pos;
        let pr = {
            let mut ie = None;
            crate::builtins::var::php_unserialize(self, data, &mut pos, &mut ie, &mut vhash)
                .map_err(|_| pr_start)?
                .borrow()
                .clone()
        };
        let Value::Array(pr) = pr else {
            return Err(pr_start);
        };
        // Trailing bytes after the props payload are ignored by zend.
        Ok((fl, sv, pr))
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
                    match self.rebind_closure(&c, Some(t.clone()), Some(Value::Object(t)), true)? {
                        Some(nc) => {
                            ca.cells.remove(0);
                            return self.call_value(&Value::Callable(nc), ca);
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
                match self.rebind_closure(&c, new_this, scope_arg, false)? {
                    Some(nc) => Ok(Value::Callable(nc)),
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
        // zend 8.5 deprecates SplObjectStorage's pre-offset* aliases —
        // the engine body still runs, so a subclass's own override
        // does NOT warn (dc differs from SplObjectStorage then).
        if dc.name().eq_ignore_ascii_case("splobjectstorage") {
            let alias = match m.decl.name.to_lowercase().as_str() {
                "attach" => Some("offsetSet"),
                "detach" => Some("offsetUnset"),
                "contains" => Some("offsetExists"),
                _ => None,
            };
            if let Some(new) = alias {
                self.deprecated(&format!(
                    "Method SplObjectStorage::{}() is deprecated since 8.5, use method SplObjectStorage::{}() instead",
                    m.decl.name, new
                ))?;
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
            if self.is_ref_cell(a) && Rc::strong_count(a) > 1 {
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
            let this_obj = if m.is_static || !fwd {
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
                let this_obj = if !fwd {
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
        // WeakReference::get() — upgrades the weak handle (null when
        // the target was collected).
        if name.eq_ignore_ascii_case("get") {
            let w = {
                let ob = obj.borrow();
                match &ob.internal {
                    Some(ObjectInternal::WeakRef(w)) => Some(w.upgrade()),
                    _ => None,
                }
            };
            if let Some(u) = w {
                return Ok(match u {
                    Some(o) => Value::Object(o),
                    None => Value::Null,
                });
            }
        }
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
                if let Some(v) = self.throwable_method(&obj, name, &args)? {
                    return Ok(v);
                }
            }
        }
        // php_user_filter stubs — zend's internal defaults: filter()
        // returns PSFS_ERR_FATAL, onCreate() true, onClose() void, the
        // on* hooks true. Only fires when the resolved method is the
        // registered stub — a userland override runs its own body.
        if self.obj_is_a_str(cls.name(), "php_user_filter") {
            let stub = self
                .find_method_in(&cls, name)
                .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
                .unwrap_or(false);
            if stub {
                return Ok(match name.to_lowercase().as_str() {
                    "filter" => Value::Int(0),
                    "oncreate" => Value::Bool(true),
                    "onclose" => Value::Null,
                    _ => Value::Bool(true),
                });
            }
        }
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
    ) -> Result<Option<Value>, PhpError> {
        let ob = obj.borrow();
        let lname = name.to_lowercase();
        Ok(match lname.as_str() {
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
            "getprevious" => Some(
                ob.props
                    .get("previous")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null),
            ),
            "getseverity" => Some(
                ob.props
                    .get("severity")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Int(1)),
            ),
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
                // Builtin throwable ctor: props from args. ErrorException's
                // own signature is (message, code, severity, filename,
                // line, previous) — zend declares it on ErrorException so
                // the whole subtree inherits the 6-arg shape; every other
                // throwable keeps (message, code, previous). Arg checks
                // are zend's weak-mode ZPP coercions (non-coercible arg →
                // TypeError naming it).
                drop(ob);
                let mut ob = obj.borrow_mut();
                let ee = self.is_a(&ob.class.clone(), "errorexception");
                // zend names the ctor's DECLARING scope — the ROOT
                // builtin throwable ancestor whose internal __construct
                // stub the method descends from (Exception for the
                // Exception tree, Error for the Error tree,
                // ErrorException for its subtree). A userland override
                // in the middle of the chain does not relabel it.
                let mut cls_name = ob.class.name().to_string();
                {
                    let mut cur = Some(ob.class.clone());
                    while let Some(c) = cur {
                        let internal = c.decl.methods.iter().any(|m| {
                            m.decl.name.eq_ignore_ascii_case("__construct")
                                && m.decl.body.is_empty()
                                && m.decl.line == 0
                        });
                        if internal {
                            cls_name = c.name().to_string();
                        }
                        cur = c
                            .decl
                            .parent
                            .as_ref()
                            .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
                    }
                    if ee {
                        cls_name = "ErrorException".into();
                    }
                }
                macro_rules! arg_err {
                    ($n:expr, $pname:expr, $ty:expr, $v:expr) => {{
                        let tn = self.zval_type_name($v);
                        return Err(self.spl_throw(
                            "TypeError",
                            format!(
                                "{}::__construct(): Argument #{} (${}) must be of type {}, {} given",
                                cls_name, $n, $pname, $ty, tn
                            ),
                        ));
                    }};
                }
                let getv = |i: usize| _args.get(i).map(|c| c.borrow().clone());
                let msg = match getv(0) {
                    Some(v @ Value::Array(_)) => arg_err!(1, "message", "string", &v),
                    Some(v) => v.to_php_string(),
                    None => String::new(),
                };
                let code = match getv(1) {
                    Some(Value::Int(i)) => i,
                    Some(v) => match weak_ty_coerce(&["int".into()], &v) {
                        Some(Value::Int(i)) => i,
                        _ => arg_err!(2, "code", "int", &v),
                    },
                    None => 0,
                };
                let (severity, filename, line, prev_arg) = if ee {
                    let severity = match getv(2) {
                        Some(Value::Int(i)) => i,
                        Some(v) => match weak_ty_coerce(&["int".into()], &v) {
                            Some(Value::Int(i)) => i,
                            _ => arg_err!(3, "severity", "int", &v),
                        },
                        None => 1, // E_ERROR
                    };
                    let filename = match getv(3) {
                        Some(Value::Null) | None => None,
                        Some(v @ Value::Array(_)) => arg_err!(4, "filename", "?string", &v),
                        Some(v) => Some(v.to_php_string()),
                    };
                    let line = match getv(4) {
                        Some(Value::Null) | None => None,
                        Some(Value::Int(i)) => Some(i),
                        Some(v) => match weak_ty_coerce(&["int".into()], &v) {
                            Some(Value::Int(i)) => Some(i),
                            _ => arg_err!(5, "line", "?int", &v),
                        },
                    };
                    (severity, filename, line, _args.get(5).cloned())
                } else {
                    (1, None, None, _args.get(2).cloned())
                };
                ob.props.insert("message".into(), cell(Value::str(msg)));
                ob.props.insert("code".into(), cell(Value::Int(code)));
                if !ob.prop_order.contains(&"message".into()) {
                    ob.prop_order.push("message".into());
                    ob.prop_order.push("code".into());
                }
                if ee {
                    ob.props
                        .insert("severity".into(), cell(Value::Int(severity)));
                    if !ob.prop_order.contains(&"severity".into()) {
                        ob.prop_order.push("severity".into());
                    }
                    // zend lets the ctor override the throw site's
                    // file/line — both the props and getFile()/getLine()
                    // report them.
                    if let Some(f) = &filename {
                        ob.props.insert("file".into(), cell(Value::str(f)));
                        if let Some(ObjectInternal::Exception { file, .. }) = &mut ob.internal {
                            *file = f.clone();
                        }
                    }
                    if let Some(l) = line {
                        ob.props.insert("line".into(), cell(Value::Int(l)));
                        if let Some(ObjectInternal::Exception { line: il, .. }) = &mut ob.internal {
                            *il = l as u32;
                        }
                    }
                }
                // zend's ?Throwable check — arg #6 on ErrorException,
                // #3 elsewhere. A Throwable lands in the `previous`
                // prop for getPrevious().
                if let Some(c) = prev_arg {
                    let pv = c.borrow().clone();
                    let ok = match &pv {
                        Value::Null => true,
                        Value::Object(o) => {
                            let cn = o.borrow().class.decl.name.clone();
                            self.is_throwable_name(&cn)
                        }
                        _ => false,
                    };
                    if !ok {
                        arg_err!(if ee { 6 } else { 3 }, "previous", "?Throwable", &pv);
                    }
                    if !matches!(pv, Value::Null) {
                        ob.props.insert("previous".into(), cell(pv));
                        if !ob.prop_order.contains(&"previous".into()) {
                            ob.prop_order.push("previous".into());
                        }
                    }
                }
                Some(Value::Null)
            }
            _ => None,
        })
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
