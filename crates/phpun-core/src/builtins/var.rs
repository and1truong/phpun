//! Variable handling: var_dump/print_r/var_export, serialize, type predicates and casts.

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        // ----- output/debug -----
        "var_dump" => {
            for a in args {
                var_dump(it, &a.borrow(), 0, false, false);
            }
            Value::Null
        }
        "debug_zval_dump" => {
            for a in args {
                var_dump(it, &a.borrow(), 0, true, false);
            }
            Value::Null
        }
        "print_r" => {
            let v = arg(args, 0);
            let ret = arg(args, 1).is_truthy();
            let s = print_r(it, &v, 0);
            if ret {
                Value::str(s)
            } else {
                it.emit(&s);
                // print_r echoes a trailing newline only for arrays/objects.
                if matches!(v, Value::Array(_) | Value::Object(_) | Value::Callable(_)) {
                    it.emit("\n");
                }
                Value::Bool(true)
            }
        }
        "var_export" => {
            let v = arg(args, 0);
            let ret = arg(args, 1).is_truthy();
            let s = var_export(it, &v);
            if ret {
                Value::str(s)
            } else {
                it.emit(&s);
                Value::Null
            }
        }

        // ----- type introspection -----
        "gettype" => Value::str(arg(args, 0).gettype()),
        "get_debug_type" => Value::str(
            match arg(args, 0) {
                Value::Null => "null",
                Value::Bool(_) => "bool",
                Value::Int(_) => "int",
                Value::Float(_) => "float",
                Value::Str(_) => "string",
                Value::Array(_) => "array",
                Value::Object(o) => {
                    return Ok(Some(Value::str(o.borrow().class.name().to_string())))
                }
                Value::Callable(_) => "Closure",
                Value::Resource(r) => {
                    if matches!(&*r.borrow(), PhpResource::Closed { .. }) {
                        "resource (closed)"
                    } else {
                        "resource"
                    }
                }
            }
            .to_string(),
        ),
        "settype" => {
            let t = arg_str(it, args, 1);
            if let Some(c) = args.first() {
                let nv = cast_to(&c.borrow(), &t);
                *c.borrow_mut() = nv;
            }
            Value::Bool(true)
        }
        "intval" | "ip2long" => Value::Int(arg(args, 0).to_int()),
        "floatval" | "doubleval" => Value::Float(arg(args, 0).to_float()),
        "strval" => Value::str(it.to_string_of(&arg(args, 0))),
        "boolval" => Value::Bool(arg(args, 0).is_truthy()),
        "is_int" | "is_integer" | "is_long" => Value::Bool(matches!(arg(args, 0), Value::Int(_))),
        "is_float" | "is_double" | "is_real" => {
            Value::Bool(matches!(arg(args, 0), Value::Float(_)))
        }
        "is_string" => Value::Bool(matches!(arg(args, 0), Value::Str(_))),
        "is_bool" => Value::Bool(matches!(arg(args, 0), Value::Bool(_))),
        "is_null" => Value::Bool(matches!(arg(args, 0), Value::Null)),
        "is_array" => Value::Bool(matches!(arg(args, 0), Value::Array(_))),
        "is_object" => Value::Bool(matches!(
            arg(args, 0),
            Value::Object(_) | Value::Callable(_)
        )),
        "is_numeric" => match arg(args, 0) {
            Value::Int(_) | Value::Float(_) => Value::Bool(true),
            Value::Str(s) => Value::Bool(!matches!(numeric(&s), Numeric::NonNumeric)),
            _ => Value::Bool(false),
        },
        "is_scalar" => Value::Bool(matches!(
            arg(args, 0),
            Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(_)
        )),
        "is_callable" => {
            let v = arg(args, 0);
            let syntax_only = arg(args, 1).is_truthy();
            // A throwing autoloader's exception propagates through
            // is_callable — it is not swallowed into a false.
            let ok = it.try_is_callable_value(&v)?;
            // $callable_name writes back through the arg cell —
            // syntax_only gives the canonical `Class::m` /
            // `{closure:fn():L}` form (closure_016).
            if ok {
                if let Some(nm) = it.callable_name_of(&v, syntax_only) {
                    if let Some(c) = args.get(2) {
                        *c.borrow_mut() = Value::str(nm);
                    }
                }
            }
            Value::Bool(ok)
        }
        "is_iterable" => Value::Bool(matches!(arg(args, 0), Value::Array(_))),
        "is_countable" => Value::Bool(match arg(args, 0) {
            Value::Array(_) => true,
            Value::Object(o) => o
                .borrow()
                .class
                .decl
                .implements
                .iter()
                .any(|i| i.eq_ignore_ascii_case("countable")),
            _ => false,
        }),
        // is_resource() is false on closed handles (zend_list_close
        // leaves a `resource (closed)` zval, not a live resource).
        "is_resource" => Value::Bool(match arg(args, 0) {
            Value::Resource(r) => !matches!(&*r.borrow(), PhpResource::Closed { .. }),
            _ => false,
        }),
        "is_nan" => Value::Bool(matches!(arg(args, 0), Value::Float(f) if f.is_nan())),
        "is_finite" => Value::Bool(matches!(arg(args, 0), Value::Float(f) if f.is_finite())),
        "is_infinite" => Value::Bool(matches!(arg(args, 0), Value::Float(f) if f.is_infinite())),

        // ----- serialization -----
        "serialize" => Value::str(php_serialize(it, &arg(args, 0))?),
        "unserialize" => {
            let s = arg_str(it, args, 0);
            let mut pos = 0;
            let mut err = None;
            let mut vhash: Vec<Cell> = Vec::new();
            match php_unserialize(it, &s, &mut pos, &mut err, &mut vhash) {
                Ok(v) => v.borrow().clone(),
                Err(_) => {
                    if let Some(e) = err {
                        return Err(e);
                    }
                    let _ = it.warn_pub(&format!(
                        "unserialize(): Error at offset {} of {} bytes",
                        pos,
                        s.len()
                    ));
                    Value::Bool(false)
                }
            }
        }
        _ => return Ok(None),
    }))
}

// ----- helpers -----

/// var_dump one zval; `is_ref` prints PHP's `&` prefix for reference cells.
fn var_dump(it: &mut Interp, v: &Value, indent: usize, zval: bool, is_ref: bool) {
    let pad = "  ".repeat(indent);
    // debug_zval_dump appends `refcount(N)` to every line/header.
    let rc = |n: usize| -> String {
        if zval {
            format!(" refcount({})", n)
        } else {
            String::new()
        }
    };
    let r = if is_ref { "&" } else { "" };
    match v {
        Value::Null => it.emit(&format!("{}{}NULL{}\n", pad, r, rc(1))),
        Value::Bool(b) => it.emit(&format!("{}{}bool({}){}\n", pad, r, b, rc(1))),
        Value::Int(i) => it.emit(&format!("{}{}int({}){}\n", pad, r, i, rc(1))),
        Value::Float(f) => {
            let prec = it.ini_int("serialize_precision", -1);
            it.emit(&format!(
                "{}{}float({}){}\n",
                pad,
                r,
                crate::value::format_float_prec(*f, prec),
                rc(1)
            ))
        }
        Value::Str(s) => it.emit_bytes(
            &[
                format!("{}{}string({}) \"", pad, r, s.len()).into_bytes(),
                s.to_vec(),
                format!("\"{}\n", rc(1)).into_bytes(),
            ]
            .concat(),
        ),
        Value::Array(a) => {
            let aptr = Rc::as_ptr(a) as usize;
            if !it.dump_stack.insert(aptr) {
                it.emit(&format!("{}*RECURSION*\n", pad));
                return;
            }
            let rcn = Rc::strong_count(a);
            let a = a.borrow();
            // zval: `array(2) refcount(1){` — plain: `array(2) {`.
            let tail = if zval { rc(rcn) } else { " ".to_string() };
            it.emit(&format!("{}{}array({}){}{{\n", pad, r, a.len(), tail));
            for (k, c) in a.iter() {
                match k {
                    ArrKey::Int(i) => it.emit(&format!("{}  [{}]=>\n", pad, i)),
                    ArrKey::Str(s) => it.emit(&format!("{}  [\"{}\"]=>\n", pad, s)),
                    ArrKey::Tomb => continue,
                }
                var_dump(
                    it,
                    &c.borrow(),
                    indent + 1,
                    zval,
                    // `&` is zend's IS_REFERENCE mark, not sharing —
                    // prop-bound cells (AoStore mirrors) shared by
                    // structure print plain.
                    it.is_ref_cell(c) && Rc::strong_count(c) > 1,
                );
            }
            it.emit(&format!("{}}}\n", pad));
            it.dump_stack.remove(&aptr);
        }
        Value::Object(o) => {
            let optr = Rc::as_ptr(o) as usize;
            if !it.dump_stack.insert(optr) {
                it.emit(&format!("{}*RECURSION*\n", pad));
                return;
            }
            let ob = o.borrow();
            // Enum cases print `enum(E::Case1)` (single line).
            if ob.class.decl.kind == crate::ast::ClassKind::Enum {
                if let Some(nm) = ob.props.get("name") {
                    if let Value::Str(case) = &*nm.borrow() {
                        it.emit(&format!(
                            "{}{}enum({}::{})\n",
                            pad,
                            r,
                            ob.class.name(),
                            crate::value::lossy(case)
                        ));
                        it.dump_stack.remove(&optr);
                        return;
                    }
                }
            }
            // Count live props only — unset() tombstones prop_order slots.
            let mut live = ob
                .prop_order
                .iter()
                .filter(|n| ob.props.contains_key(*n))
                .count();
            // Internal engine state Zend exposes in var_dump:
            // Generator's creating function and ArrayIterator's
            // private storage (iterable_001).
            let internal_props: Vec<(String, Value)> = match &ob.internal {
                Some(crate::value::ObjectInternal::Generator(st)) => {
                    let st = st.borrow();
                    let fname = match &st.setup {
                        // Methods dump as `C::test` (generator_return_
                        // containing_extra_types).
                        crate::value::GenSetup::Invoke {
                            decl, decl_class, ..
                        } => match decl_class {
                            Some(c) => format!("{}::{}", c.decl.name, decl.name),
                            None => decl.name.clone(),
                        },
                    };
                    vec![("\"function\"".to_string(), Value::str(&fname))]
                }
                Some(crate::value::ObjectInternal::ArrayIter { store, flags, .. }) => {
                    // A self-backed object (ctor arg `$this`, engine
                    // flag 0x1000000) has NO storage section — its
                    // props print directly.
                    if flags & 0x1000000 != 0 {
                        Vec::new()
                    } else {
                        let st = store.borrow();
                        // Object-backed storage dumps the SOURCE
                        // object (zend); array-backed dumps the table.
                        let sv = match &st.src {
                            Some(so) => Value::Object(so.clone()),
                            None => Value::Array(st.arr.clone()),
                        };
                        // The private `storage` prop prints under its
                        // declaring class — ArrayObject or ArrayIterator
                        // (bug36214).
                        let dcl = if it.obj_is_a_str(ob.class.name(), "arrayobject") {
                            "ArrayObject"
                        } else {
                            "ArrayIterator"
                        };
                        vec![(format!("\"storage\":\"{}\":private", dcl), sv)]
                    }
                }
                _ => Vec::new(),
            };
            live += internal_props.len();
            let tail = if zval {
                rc(Rc::strong_count(o))
            } else {
                " ".to_string()
            };
            it.emit(&format!(
                "{}{}object({})#{} ({}){}{{\n",
                pad,
                r,
                ob.class.name(),
                ob.id,
                live,
                tail
            ));
            for n in &ob.prop_order {
                // Reserved-but-cellless slots are uninitialized typed
                // props — zend prints `uninitialized(T)` (recursion).
                if !ob.props.contains_key(n) {
                    if let Some(pd) = it.decl_for_slot(o, n) {
                        if let Some(tys) = &pd.ty {
                            let ty = if tys.len() == 2 && tys.iter().any(|t| t == "null") {
                                format!("?{}", tys.iter().find(|t| *t != "null").unwrap())
                            } else {
                                tys.join("|")
                            };
                            let (vis, dcls) = it.prop_visibility(&ob.class, n);
                            let disp = n
                                .strip_prefix('\0')
                                .and_then(|r| r.split('\0').nth(1))
                                .unwrap_or(n.as_str());
                            let key = match vis {
                                crate::ast::Visibility::Private => {
                                    format!("\"{}\":\"{}\":private", disp, dcls)
                                }
                                crate::ast::Visibility::Protected => {
                                    format!("\"{}\":protected", disp)
                                }
                                crate::ast::Visibility::Public => {
                                    format!("\"{}\"", disp)
                                }
                            };
                            it.emit(&format!("{}  [{}]=>\n", pad, key));
                            it.emit(&format!("{}  uninitialized({})\n", pad, ty));
                        }
                    }
                    continue;
                }
                if let Some(c) = ob.props.get(n) {
                    // Int-keyed buckets (SPL `[]=` appends) display
                    // their index unquoted, like array elements.
                    if let Some(i) = crate::value::int_prop_index(n) {
                        it.emit(&format!("{}  [{}]=>\n", pad, i));
                        var_dump(
                            it,
                            &c.borrow(),
                            indent + 1,
                            zval,
                            it.is_ref_cell(c) && Rc::strong_count(c) > 1,
                        );
                        continue;
                    }
                    let (vis, dcls) = it.prop_visibility(&ob.class, n);
                    // Mangled private keys "\0Cls\0name" display only `name`.
                    let disp = n
                        .strip_prefix('\0')
                        .and_then(|r| r.split('\0').nth(1))
                        .unwrap_or(n.as_str());
                    let key = match vis {
                        crate::ast::Visibility::Private => {
                            format!("\"{}\":\"{}\":private", disp, dcls)
                        }
                        crate::ast::Visibility::Protected => {
                            format!("\"{}\":protected", disp)
                        }
                        crate::ast::Visibility::Public => format!("\"{}\"", disp),
                    };
                    it.emit(&format!("{}  [{}]=>\n", pad, key));
                    // `&` is zend's IS_REFERENCE mark on the SLOT —
                    // a typed prop can't hold a ref (zend stores the
                    // value), so no `&` even though the shared cell
                    // stays marked for write-through gating.
                    let typed = it
                        .decl_for_slot(o, n)
                        .map(|pd| pd.ty.is_some())
                        .unwrap_or(false);
                    var_dump(
                        it,
                        &c.borrow(),
                        indent + 1,
                        zval,
                        !typed && it.is_ref_cell(c) && Rc::strong_count(c) > 1,
                    );
                }
            }
            for (k, v) in &internal_props {
                it.emit(&format!("{}  [{}]=>\n", pad, k));
                var_dump(it, v, indent + 1, zval, false);
            }
            it.emit(&format!("{}}}\n", pad));
            it.dump_stack.remove(&optr);
        }
        Value::Callable(c) => {
            let cptr = Rc::as_ptr(c) as usize;
            if !it.dump_stack.insert(cptr) {
                it.emit(&format!("{}*RECURSION*\n", pad));
                return;
            }
            let props = closure_debug_props(it, c);
            it.emit(&format!(
                "{}{}object(Closure)#{} ({}) {{\n",
                pad,
                r,
                c.id.get(),
                props.len()
            ));
            for (k, v) in &props {
                it.emit(&format!("{}  [\"{}\"]=>\n", pad, k));
                var_dump(it, v, indent + 1, zval, false);
            }
            it.emit(&format!("{}}}\n", pad));
            it.dump_stack.remove(&cptr);
        }
        Value::Resource(r) => {
            let rb = r.borrow();
            it.emit(&format!(
                "{}resource({}) of type ({})\n",
                pad,
                rb.id(),
                rb.type_name()
            ));
        }
    }
}

/// The props Zend reports for a Closure in var_dump/print_r
/// (zend_closures.c get_debug_info): `function` for callables made
/// from functions/methods, `name`/`file`/`line` for literals, then
/// `static` (use-captures ∪ function static vars), bound `this`, and
/// `parameter` — each only when present.
fn closure_debug_props(it: &mut Interp, c: &crate::value::PhpCallable) -> Vec<(String, Value)> {
    use crate::value::CallableKind;
    let mut props: Vec<(String, Value)> = Vec::new();
    let mut params: Vec<(String, bool)> = Vec::new();
    let mut body_statics: Vec<String> = Vec::new();
    let mut statics_key: Option<String> = None;
    match &c.kind {
        CallableKind::Named(n) => {
            props.push(("function".into(), Value::str(n.clone())));
            if let Some(d) = it.functions.get(&n.to_lowercase()) {
                params = d
                    .params
                    .iter()
                    .map(|p| (p.name.clone(), p.default.is_none() && !p.variadic))
                    .collect();
                static_var_names(&d.body, &mut body_statics);
                statics_key = Some(d.name.clone());
            } else if let Some(sig) = builtin_sig(&n.to_lowercase()) {
                params = sig;
            }
        }
        CallableKind::Method { obj, class, name } => {
            let cn = c
                .scope_class
                .as_ref()
                .map(|sc| sc.name().to_string())
                .or_else(|| {
                    obj.as_ref()
                        .map(|o| o.borrow().class.name().to_string())
                        .or_else(|| class.as_ref().map(|cl| cl.name().to_string()))
                })
                .unwrap_or_default();
            props.push(("function".into(), Value::str(format!("{}::{}", cn, name))));
            let cls = obj
                .as_ref()
                .map(|o| o.borrow().class.clone())
                .or_else(|| class.clone());
            if let Some((m, dc)) = cls.and_then(|cl| it.find_method_in(&cl, name)) {
                params = m
                    .decl
                    .params
                    .iter()
                    .map(|p| (p.name.clone(), p.default.is_none() && !p.variadic))
                    .collect();
                static_var_names(&m.decl.body, &mut body_statics);
                statics_key = Some(format!("{}\u{0}{}", dc.name(), name));
            }
        }
        CallableKind::Closure(d) => {
            // `name` is the Zend scope name `{closure:scope():L}`
            // computed at creation — falls back to file:line for
            // decls that never got one.
            props.push((
                "name".into(),
                Value::str(if d.name.is_empty() {
                    format!("{{closure:{}:{}}}", d.file, d.line)
                } else {
                    d.name.clone()
                }),
            ));
            props.push(("file".into(), Value::str(d.file.clone())));
            props.push(("line".into(), Value::Int(d.line as i64)));
            params = d
                .params
                .iter()
                .map(|p| (p.name.clone(), p.default.is_none() && !p.variadic))
                .collect();
            static_var_names(&d.body, &mut body_statics);
            // Closure statics live per-instance — the table is keyed
            // by decl name + the callable's object id.
            statics_key = Some(format!("{}\u{0}c{}", d.name, c.id.get()));
        }
    }
    // `static` member: bound use-vars first, then function statics —
    // declared-but-unrun statics report NULL (gh8083, bug79778).
    let mut sa = PhpArray::new();
    for (n, cap, _by_ref) in &c.captures {
        sa.set_cell(ArrKey::Str(n.clone().into()), cap.clone());
    }
    if let Some(key) = &statics_key {
        for n in body_statics {
            if sa.get(&ArrKey::Str(n.clone().into())).is_none() {
                let cv = it
                    .statics
                    .get(key)
                    .and_then(|t| t.get(&n).map(|c| cell(c.borrow().clone())))
                    .unwrap_or_else(|| cell(Value::Null));
                sa.set_cell(ArrKey::Str(n.into()), cv);
            }
        }
    }
    if !sa.is_empty() {
        props.push((
            "static".into(),
            Value::Array(std::rc::Rc::new(std::cell::RefCell::new(sa))),
        ));
    }
    let this_obj = match &c.kind {
        CallableKind::Method { obj, .. } => obj.clone().or_else(|| c.this_obj.clone()),
        _ => c.this_obj.clone(),
    };
    if let Some(o) = this_obj {
        props.push(("this".into(), Value::Object(o)));
    }
    if !params.is_empty() {
        let mut pa = PhpArray::new();
        for (pn, req) in &params {
            let word = if *req { "<required>" } else { "<optional>" };
            pa.set(ArrKey::Str(format!("${}", pn).into()), Value::str(word));
        }
        props.push((
            "parameter".into(),
            Value::Array(std::rc::Rc::new(std::cell::RefCell::new(pa))),
        ));
    }
    props
}

/// Names of `static $x` declarations anywhere in a body — nested
/// function/class bodies declare their own (bug79778).
fn static_var_names(stmts: &[crate::ast::Stmt], out: &mut Vec<String>) {
    use crate::ast::Stmt;
    for st in stmts {
        match st {
            Stmt::Static { vars, .. } => {
                for (n, ..) in vars {
                    if !out.iter().any(|x| x == n) {
                        out.push(n.clone());
                    }
                }
            }
            Stmt::Block(b) => static_var_names(b, out),
            Stmt::If { then, else_, .. } => {
                static_var_names(then, out);
                static_var_names(else_, out);
            }
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::For { body, .. }
            | Stmt::Foreach { body, .. } => static_var_names(body, out),
            Stmt::Switch { cases, .. } => {
                for (_, b) in cases {
                    static_var_names(b, out);
                }
            }
            Stmt::Try {
                body,
                catches,
                finally,
            } => {
                static_var_names(body, out);
                for c in catches {
                    static_var_names(&c.body, out);
                }
                if let Some(f) = finally {
                    static_var_names(f, out);
                }
            }
            _ => {}
        }
    }
}

fn print_r(_it: &mut Interp, v: &Value, indent: usize) -> String {
    match v {
        Value::Array(a) => {
            let a = a.borrow();
            let mut s = String::from("Array\n");
            s.push_str(&"    ".repeat(indent));
            s.push_str("(\n");
            for (k, c) in a.iter() {
                s.push_str(&"    ".repeat(indent + 1));
                s.push_str(&format!("[{}] => ", key_str(k)));
                let inner = print_r(_it, &c.borrow(), indent + 2);
                s.push_str(&inner);
                s.push('\n');
                if matches!(
                    *c.borrow(),
                    Value::Array(_) | Value::Object(_) | Value::Callable(_)
                ) {
                    s.push('\n');
                }
            }
            s.push_str(&"    ".repeat(indent));
            s.push(')');
            s
        }
        Value::Callable(c) => {
            let mut s = String::from("Closure Object\n");
            s.push_str(&"    ".repeat(indent));
            s.push_str("(\n");
            for (k, v) in closure_debug_props(_it, c) {
                s.push_str(&"    ".repeat(indent + 1));
                s.push_str(&format!("[{}] => ", k));
                s.push_str(&print_r(_it, &v, indent + 2));
                s.push('\n');
                if matches!(v, Value::Array(_) | Value::Object(_) | Value::Callable(_)) {
                    s.push('\n');
                }
            }
            s.push_str(&"    ".repeat(indent));
            s.push(')');
            s
        }
        Value::Object(o) => {
            let ob = o.borrow();
            let mut s = format!("{} Object\n", ob.class.name());
            s.push_str(&"    ".repeat(indent));
            s.push_str("(\n");
            for n in &ob.prop_order {
                if let Some(c) = ob.props.get(n) {
                    // Int-keyed buckets print their bare index.
                    let disp = match crate::value::int_prop_index(n) {
                        Some(i) => i.to_string(),
                        None => n.clone(),
                    };
                    s.push_str(&"    ".repeat(indent + 1));
                    s.push_str(&format!("[{}] => ", disp));
                    s.push_str(&print_r(_it, &c.borrow(), indent + 2));
                    s.push('\n');
                    if matches!(
                        *c.borrow(),
                        Value::Array(_) | Value::Object(_) | Value::Callable(_)
                    ) {
                        s.push('\n');
                    }
                }
            }
            // spl array-objects print their internal storage as a
            // private `storage` prop (same as var_dump). Object-backed
            // prints the source object; self-backed skips the section.
            if let Some(crate::value::ObjectInternal::ArrayIter { store, flags, .. }) = &ob.internal
            {
                if flags & 0x1000000 == 0 {
                    let sv = {
                        let st = store.borrow();
                        match &st.src {
                            Some(so) => Value::Object(so.clone()),
                            None => Value::Array(st.arr.clone()),
                        }
                    };
                    let dcl = if _it.obj_is_a_str(ob.class.name(), "arrayobject") {
                        "ArrayObject"
                    } else {
                        "ArrayIterator"
                    };
                    s.push_str(&"    ".repeat(indent + 1));
                    s.push_str(&format!("[storage:{}:private] => ", dcl));
                    s.push_str(&print_r(_it, &sv, indent + 2));
                    s.push('\n');
                    s.push('\n');
                }
            }
            s.push_str(&"    ".repeat(indent));
            s.push(')');
            s
        }
        Value::Float(f) => {
            let prec = _it.ini_int("precision", 14);
            crate::value::format_float_prec(*f, prec)
        }
        other => other.to_php_string(),
    }
}

fn var_export(it: &mut Interp, v: &Value) -> String {
    var_export_depth(it, v, 0)
}

fn var_export_depth(it: &mut Interp, v: &Value, depth: usize) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            let prec = it.ini_int("serialize_precision", -1);
            let s = crate::value::format_float_prec(*f, prec);
            // var_export always renders a decimal point: 0.0, 100.0.
            if s.bytes().all(|b| b.is_ascii_digit() || b == b'-') {
                format!("{}.0", s)
            } else {
                s
            }
        }
        Value::Str(s) => format!(
            "'{}'",
            crate::value::lossy(&s)
                .replace('\\', "\\\\")
                .replace('\'', "\\'")
        ),
        Value::Array(a) => {
            let a = a.borrow();
            let pad = "  ".repeat(depth + 1);
            let mut s = String::from("array (\n");
            for (k, c) in a.iter() {
                s.push_str(&pad);
                s.push_str(&match k {
                    ArrKey::Int(i) => i.to_string(),
                    ArrKey::Str(st) => {
                        format!("'{}'", st.replace('\\', "\\\\").replace('\'', "\\'"))
                    }
                    ArrKey::Tomb => continue,
                });
                s.push_str(" => ");
                // A nested array value renders on its own line at key depth
                // ('key' => \n  array (...)) — matches zend var_export.
                let inner = c.borrow();
                if matches!(&*inner, Value::Array(_)) {
                    s.push('\n');
                    s.push_str(&pad);
                }
                s.push_str(&var_export_depth(it, &inner, depth + 1));
                s.push_str(",\n");
            }
            s.push_str(&"  ".repeat(depth));
            s.push(')');
            s
        }
        Value::Object(o) => {
            // All decl entries (both private `changed`s), hooked props
            // via `get`, plain emitted names (property_hooks/dump).
            let mut s = format!("\\{}::__set_state(array(\n", o.borrow().class.name());
            // spl array-objects export their internal storage as the
            // object's "properties" (zend uses spl's property hash).
            let ao_arr = if matches!(
                o.borrow().internal,
                Some(crate::value::ObjectInternal::ArrayIter { .. })
            ) {
                Some(it.ao_arr(o))
            } else {
                None
            };
            let has_ao = ao_arr.is_some();
            if let Some(arr) = ao_arr {
                for (k, c) in arr.borrow().iter() {
                    // Int keys print bare, strings quoted — zend's
                    // var_export key rule.
                    let ks = match k {
                        ArrKey::Int(i) => format!("   {} => ", i),
                        ArrKey::Str(st) => format!("   '{}' => ", st),
                        ArrKey::Tomb => continue,
                    };
                    s.push_str(&ks);
                    s.push_str(&var_export_depth(it, &c.borrow(), depth + 1));
                    s.push_str(",\n");
                }
            }
            for (out, slot, decl) in it.object_serial_entries(o) {
                // Int-keyed buckets (SPL `[]=` appends) print their
                // index unquoted; on spl storage the storage table
                // above already emitted them.
                if let Some(i) = crate::value::int_prop_index(&out) {
                    if has_ao {
                        continue;
                    }
                    if let Some(v) = o.borrow().props.get(&slot).map(|c| c.borrow().clone()) {
                        s.push_str(&format!("   {} => ", i));
                        s.push_str(&var_export_depth(it, &v, depth + 1));
                        s.push_str(",\n");
                    }
                    continue;
                }
                let v = match &decl {
                    Some((p, dcls)) => it.serial_entry_value(o, p, dcls, &slot),
                    None => o.borrow().props.get(&slot).map(|c| c.borrow().clone()),
                };
                if let Some(v) = v {
                    s.push_str(&format!("   '{}' => ", out));
                    s.push_str(&var_export_depth(it, &v, depth + 1));
                    s.push_str(",\n");
                }
            }
            s.push_str("))");
            s
        }
        _ => "NULL".into(),
    }
}

/// zend serialize()'s var_hash: every serialized ELEMENT zval takes
/// one slot index (array keys and prop names take none). Objects
/// seen again serialize as `r:<slot>`; shared IS_REFERENCE cells as
/// `R:<slot>`; plain arrays never dedup.
struct SerCtx {
    /// Next element slot index (1-based for the root).
    n: usize,
    /// object alloc ptr -> first-seen slot.
    objs: std::collections::HashMap<usize, usize>,
    /// ref-cell ptr -> first-seen slot.
    refs: std::collections::HashMap<usize, usize>,
}

fn ser_key(k: &ArrKey) -> String {
    match k {
        ArrKey::Int(i) => format!("i:{};", i),
        ArrKey::Str(s) => format!("s:{}:\"{}\";", s.len(), crate::value::lossy(s.as_ref())),
        ArrKey::Tomb => "N;".into(),
    }
}

fn ser_cell(it: &mut Interp, c: &Cell, ctx: &mut SerCtx) -> Result<String, PhpError> {
    ctx.n += 1;
    // An IS_REFERENCE cell still aliased elsewhere registers its slot
    // — a repeat anywhere serializes as R:<slot> instead of contents.
    if it.is_ref_cell(c) && Rc::strong_count(c) > 1 {
        let p = Rc::as_ptr(c) as usize;
        if let Some(id) = ctx.refs.get(&p) {
            return Ok(format!("R:{};", id));
        }
        // A ref pointing at an already-serialized object repeats that
        // element's slot — R: since the repeating zval is a reference
        // (a plain zval repeating the object emits r: instead).
        if let Value::Object(o) = &*c.borrow() {
            if let Some(id) = ctx.objs.get(&(Rc::as_ptr(o) as usize)) {
                return Ok(format!("R:{};", id));
            }
        }
        ctx.refs.insert(p, ctx.n);
    }
    ser_value(it, &c.borrow(), ctx)
}

fn ser_value(it: &mut Interp, v: &Value, ctx: &mut SerCtx) -> Result<String, PhpError> {
    Ok(match v {
        Value::Null => "N;".into(),
        Value::Bool(b) => format!("b:{};", *b as i32),
        Value::Int(i) => format!("i:{};", i),
        Value::Float(f) => format!("d:{};", crate::value::format_float_repr(*f)),
        Value::Str(s) => format!("s:{}:\"{}\";", s.len(), crate::value::lossy(&s)),
        Value::Array(a) => {
            let a = a.borrow();
            let mut s = format!("a:{}:{{", a.len());
            for (k, c) in a.iter() {
                s.push_str(&ser_key(k));
                s.push_str(&ser_cell(it, c, ctx)?);
            }
            s.push('}');
            s
        }
        Value::Object(o) => {
            // Object dedup: registered at its element slot — a repeat
            // anywhere emits r:<slot> (this is what terminates
            // self-referential structures).
            let p = Rc::as_ptr(o) as usize;
            if let Some(id) = ctx.objs.get(&p) {
                return Ok(format!("r:{};", id));
            }
            ctx.objs.insert(p, ctx.n);
            // Anonymous classes can't be serialized anywhere in the
            // payload — zend throws an Exception naming the class
            // (up to the "@anonymous" marker).
            {
                let cn = o.borrow().class.name().to_string();
                if let Some(pos) = cn.find("@anonymous") {
                    return Err(PhpError::uncaught(
                        "Exception",
                        format!(
                            "Serialization of '{}' is not allowed",
                            &cn[..pos + "@anonymous".len()]
                        ),
                        0,
                    ));
                }
            }
            // Zend prefers __serialize whenever the class defines it:
            // the returned array's ENTRIES go inside
            // O:<len>:"<cls>":<n>:{...} (spl array-objects emit their
            // 4-slot payload this way).
            let has_ser = it
                .find_method_in(&o.borrow().class, "__serialize")
                .is_some();
            if has_ser {
                if let Ok(Value::Array(sl)) =
                    it.method_invoke(o.clone(), "__serialize", crate::interp::CallArgs::empty())
                {
                    let sl = sl.borrow();
                    let mut body = String::new();
                    let mut n = 0;
                    for (k, c) in sl.iter() {
                        body.push_str(&ser_key(k));
                        body.push_str(&ser_cell(it, c, ctx)?);
                        n += 1;
                    }
                    return Ok(format!(
                        "O:{}:\"{}\":{}:{{{}}}",
                        o.borrow().class.name().len(),
                        o.borrow().class.name(),
                        n,
                        body
                    ));
                }
            }
            // Serializable implementors serialize as C:...{payload}
            // where the payload is whatever ->serialize() returns.
            if it.obj_implements(o, "serializable") {
                if let Ok(payload) =
                    it.method_invoke(o.clone(), "serialize", crate::interp::CallArgs::empty())
                {
                    let Value::Str(pb) = &payload else {
                        return Ok("N;".into());
                    };
                    let p = crate::value::lossy(pb);
                    return Ok(format!(
                        "C:{}:\"{}\":{}:{{{}}}",
                        o.borrow().class.name().len(),
                        o.borrow().class.name(),
                        p.len(),
                        p
                    ));
                }
            }
            let ob = o.borrow();
            let mut body = String::new();
            let mut n = 0;
            let pairs: Vec<(String, Cell)> = ob
                .prop_order
                .iter()
                .filter_map(|name| ob.props.get(name).map(|c| (name.clone(), c.clone())))
                .collect();
            drop(ob);
            for (name, c) in pairs {
                // Int-keyed buckets (SPL `[]=` appends) serialize
                // their key as `i:N;` like an array's int member.
                match crate::value::int_prop_index(&name) {
                    Some(i) => body.push_str(&format!("i:{};", i)),
                    None => body.push_str(&format!("s:{}:\"{}\";", name.len(), name)),
                }
                body.push_str(&ser_cell(it, &c, ctx)?);
                n += 1;
            }
            format!(
                "O:{}:\"{}\":{}:{{{}}}",
                o.borrow().class.name().len(),
                o.borrow().class.name(),
                n,
                body
            )
        }
        _ => "N;".into(),
    })
}

pub(crate) fn php_serialize(it: &mut Interp, v: &Value) -> Result<String, PhpError> {
    // The root zval takes slot 1 like an element; zend dereferences
    // the argument, so a ref-typed arg registers by VALUE.
    let mut ctx = SerCtx {
        n: 1,
        objs: Default::default(),
        refs: Default::default(),
    };
    ser_value(it, v, &mut ctx)
}

/// Err(None) = malformed payload (warn + false like zend);
/// Err(Some(e)) = a userland __unserialize threw — propagate it.
/// Parse an array/prop KEY inside a serialized stream — zend reads
/// keys inline (int|string literals) without a var_hash slot.
fn php_unserialize_key(s: &str, pos: &mut usize) -> Result<Value, ()> {
    let b = s.as_bytes();
    match b.get(*pos) {
        Some(b'i') => {
            *pos += 2;
            let start = *pos;
            while *pos < b.len() && b[*pos] != b';' {
                *pos += 1;
            }
            if *pos >= b.len() {
                return Err(());
            }
            let n: i64 = s[start..*pos].parse().map_err(|_| ())?;
            *pos += 1;
            Ok(Value::Int(n))
        }
        Some(b's') => {
            *pos += 2;
            let start = *pos;
            while *pos < b.len() && b[*pos] != b':' {
                *pos += 1;
            }
            if *pos >= b.len() {
                return Err(());
            }
            let len: usize = s[start..*pos].parse().map_err(|_| ())?;
            *pos += 1; // :
            *pos += 1; // opening quote
            if *pos + len > b.len() {
                return Err(());
            }
            let st = String::from_utf8_lossy(&b[*pos..*pos + len]).into_owned();
            *pos += len + 2; // closing quote + ;
            Ok(Value::str(st))
        }
        _ => Err(()),
    }
}

/// Deserialize one zval into a fresh-or-shared Cell. `vhash` is
/// zend's var_hash: every parsed element/prop VALUE occupies one slot
/// (keys take none, `r:`/`R:` elements resolve earlier slots but still
/// occupy their own). Containers register their cell BEFORE parsing
/// children so self-references inside the subtree resolve to the live
/// object. `R:` returns the SAME cell — element refs survive a
/// serialize/unserialize round-trip; `r:` copies the value into a new
/// cell (only the underlying object is shared).
pub(crate) fn php_unserialize(
    it: &mut Interp,
    s: &str,
    pos: &mut usize,
    err: &mut Option<PhpError>,
    vhash: &mut Vec<Cell>,
) -> Result<Cell, ()> {
    let b = s.as_bytes();
    let take_until = |pos: &mut usize, ch: u8| -> Result<String, ()> {
        let start = *pos;
        while *pos < b.len() && b[*pos] != ch {
            *pos += 1;
        }
        if *pos >= b.len() {
            return Err(());
        }
        let s = String::from_utf8_lossy(&b[start..*pos]).into_owned();
        *pos += 1;
        Ok(s)
    };
    match b.get(*pos) {
        Some(b'N') => {
            *pos += 2;
            let c = cell(Value::Null);
            vhash.push(c.clone());
            Ok(c)
        }
        Some(b'b') => {
            *pos += 2;
            let n = take_until(pos, b';')?;
            let c = cell(Value::Bool(n == "1"));
            vhash.push(c.clone());
            Ok(c)
        }
        Some(b'i') => {
            *pos += 2;
            let n = take_until(pos, b';')?;
            let c = cell(Value::Int(n.parse().map_err(|_| ())?));
            vhash.push(c.clone());
            Ok(c)
        }
        Some(b'd') => {
            *pos += 2;
            let n = take_until(pos, b';')?;
            let v = match n.as_str() {
                "NAN" => Value::Float(f64::NAN),
                "INF" => Value::Float(f64::INFINITY),
                "-INF" => Value::Float(f64::NEG_INFINITY),
                _ => Value::Float(n.parse().map_err(|_| ())?),
            };
            let c = cell(v);
            vhash.push(c.clone());
            Ok(c)
        }
        Some(b's') => {
            *pos += 2;
            let len: usize = take_until(pos, b':')?.parse().map_err(|_| ())?;
            *pos += 1; // opening quote
            let st = String::from_utf8_lossy(&b[*pos..*pos + len]).into_owned();
            *pos += len + 2; // closing quote + ;
            let c = cell(Value::str(st));
            vhash.push(c.clone());
            Ok(c)
        }
        // r:<n> object reference, R:<n> reference — resolve to the
        // zval already parsed at var_hash slot n (1-based), then take
        // a slot of their own like any element. R: binds the slot's
        // very cell (a true zend reference); r: copies the zval into a
        // new cell, keeping only the shared object underneath.
        Some(b'r') | Some(b'R') => {
            let lower = b[*pos] == b'r';
            *pos += 2;
            let n: usize = take_until(pos, b';')?.parse().map_err(|_| ())?;
            let target = n
                .checked_sub(1)
                .and_then(|i| vhash.get(i))
                .cloned()
                .ok_or(())?;
            if lower {
                if !matches!(&*target.borrow(), Value::Object(_)) {
                    return Err(());
                }
                let c = cell(target.borrow().clone());
                vhash.push(c.clone());
                Ok(c)
            } else {
                // zend turns the target slot into an IS_REFERENCE
                // bucket — the shared cell must mark so var_dump
                // prints `&` and a later serialize re-emits R:.
                it.mark_ref(&target);
                vhash.push(target.clone());
                Ok(target)
            }
        }
        Some(b'a') => {
            *pos += 2;
            let n: usize = take_until(pos, b':')?.parse().map_err(|_| ())?;
            *pos += 1; // {
            let arr = Rc::new(RefCell::new(PhpArray::new()));
            // Register BEFORE elements: a self-reference inside the
            // array's own subtree resolves to this same cell.
            let this = cell(Value::Array(arr.clone()));
            vhash.push(this.clone());
            for _ in 0..n {
                let k = php_unserialize_key(s, pos)?;
                let v = php_unserialize(it, s, pos, err, vhash)?;
                arr.borrow_mut().bind_cell(to_key(&k), v);
            }
            *pos += 1; // }
            Ok(this)
        }
        Some(b'C') => {
            // C:<clen>:"<class>":<plen>:{<payload>} — a Serializable
            // payload; instantiate without ctor and call ->unserialize().
            *pos += 2;
            let clen: usize = take_until(pos, b':')?.parse().map_err(|_| ())?;
            *pos += 1; // opening quote
            if *pos + clen > b.len() {
                return Err(());
            }
            let cname = String::from_utf8_lossy(&b[*pos..*pos + clen]).into_owned();
            *pos += clen;
            *pos += 1; // closing quote
            *pos += 1; // :
            let plen: usize = take_until(pos, b':')?.parse().map_err(|_| ())?;
            *pos += 1; // {
                       // zend bounds-checks the payload AND its closing `}`
                       // together — past the end it warns "Insufficient data"
                       // and fails at the payload start.
            if *pos + plen >= b.len() {
                let _ = it.warn_pub(&format!(
                    "Insufficient data for unserializing - {} required, {} present",
                    plen,
                    b.len() - *pos
                ));
                return Err(());
            }
            // A payload whose plen doesn't land exactly on `}` fails AT
            // the offending byte.
            if b[*pos + plen] != b'}' {
                *pos += plen;
                return Err(());
            }
            let payload = String::from_utf8_lossy(&b[*pos..*pos + plen]).into_owned();
            *pos += plen + 1; // payload + }
            let obj = match it.instantiate(&cname.to_lowercase(), &[]) {
                Ok(Value::Object(o)) => o,
                _ => return Err(()),
            };
            let this = cell(Value::Object(obj.clone()));
            vhash.push(this.clone());
            if it.obj_implements(&obj, "serializable") {
                if let Err(e) = it.method_invoke(
                    obj.clone(),
                    "unserialize",
                    crate::interp::CallArgs::positional(vec![cell(Value::str(payload))]),
                ) {
                    // zend propagates whatever ->unserialize() throws — a
                    // native spl storage-parse failure surfaces as its
                    // UnexpectedValueException, a userland throw as itself.
                    *err = Some(e);
                    return Err(());
                }
            } else {
                // No Serializable: zend warns and still returns the
                // (uninitialized) object.
                let _ = it.warn_pub(&format!("Class {} has no unserializer", cname));
            }
            Ok(this)
        }
        Some(b'O') => {
            // O:<clen>:"<class>":<n>:{<pairs>}
            *pos += 2;
            let clen: usize = take_until(pos, b':')?.parse().map_err(|_| ())?;
            *pos += 1; // opening quote
            if *pos + clen > b.len() {
                return Err(());
            }
            let cname = String::from_utf8_lossy(&b[*pos..*pos + clen]).into_owned();
            *pos += clen;
            *pos += 1; // closing quote
            *pos += 1; // :
            let n: usize = take_until(pos, b':')?.parse().map_err(|_| ())?;
            *pos += 1; // {
            let obj = match it.instantiate(&cname.to_lowercase(), &[]) {
                Ok(Value::Object(o)) => o,
                _ => return Err(()),
            };
            // The object takes its var_hash slot before member values
            // parse — r:/R: refs inside point back to it.
            let this = cell(Value::Object(obj.clone()));
            vhash.push(this.clone());
            // Classes defining __unserialize receive the parsed pairs
            // as an array (zend routes O: payloads through it instead
            // of writing props).
            if it
                .find_method_in(&obj.borrow().class, "__unserialize")
                .is_some()
            {
                let mut slots = PhpArray::new();
                for _ in 0..n {
                    let k = php_unserialize_key(s, pos)?;
                    let v = php_unserialize(it, s, pos, err, vhash)?;
                    match k {
                        Value::Int(i) => {
                            slots.bind_cell(ArrKey::Int(i), v);
                        }
                        Value::Str(ks) => {
                            slots.bind_cell(
                                ArrKey::Str(Rc::from(crate::value::lossy(&ks).into_owned())),
                                v,
                            );
                        }
                        _ => return Err(()),
                    }
                }
                *pos += 1; // }
                if let Err(e) = it.method_invoke(
                    obj.clone(),
                    "__unserialize",
                    crate::interp::CallArgs::positional(vec![cell(Value::Array(Rc::new(
                        RefCell::new(slots),
                    )))]),
                ) {
                    // Exceptions from ANY __unserialize — userland or
                    // spl-internal — propagate through unserialize().
                    *err = Some(e);
                    return Err(());
                }
                return Ok(this);
            }
            for _ in 0..n {
                let k = php_unserialize_key(s, pos)?;
                // `i:` prop keys land as plain string-name props —
                // unserialize writes the decimal name into the
                // (string-keyed) prop table, so `i:0` round-trips as
                // the quoted `["0"]` prop, not an int bucket.
                let int_key = match &k {
                    Value::Int(i) => Some(*i),
                    _ => None,
                };
                let Value::Str(ks) = k else {
                    if let Some(i) = int_key {
                        let v = php_unserialize(it, s, pos, err, vhash)?;
                        let mut ob = obj.borrow_mut();
                        let key = i.to_string();
                        if !ob.prop_order.contains(&key) {
                            ob.prop_order.push(key.clone());
                        }
                        ob.props.insert(key, v);
                        continue;
                    }
                    return Err(());
                };
                let plain = ks
                    .strip_prefix(&[0u8][..])
                    .and_then(|r| r.split(|b| *b == 0).nth(1))
                    .unwrap_or(ks.as_ref());
                // Virtual hooked props have no backing to fill — zend
                // aborts the whole unserialize, reporting the offset
                // right after the property name (unserialize.phpt).
                if it.unserial_prop_virtual(&obj, &crate::value::lossy(&plain)) {
                    let _ = it.warn_pub(&format!(
                        "unserialize(): Cannot unserialize value for virtual property {}::${}",
                        cname,
                        crate::value::lossy(&plain)
                    ));
                    let _ = it.warn_pub(&format!(
                        "unserialize(): Error at offset {} of {} bytes",
                        pos,
                        s.len()
                    ));
                    return Err(());
                }
                let v = php_unserialize(it, s, pos, err, vhash)?;
                let mut ob = obj.borrow_mut();
                let key = crate::value::lossy(&ks).into_owned();
                if !ob.prop_order.contains(&key) {
                    ob.prop_order.push(key.clone());
                }
                ob.props.insert(key, v);
            }
            *pos += 1; // }
            Ok(this)
        }
        _ => Err(()),
    }
}

fn cast_to(v: &Value, t: &str) -> Value {
    match t {
        "int" | "integer" => Value::Int(v.to_int()),
        "float" | "double" | "real" => Value::Float(v.to_float()),
        "string" => Value::str(v.to_php_string()),
        "bool" | "boolean" => Value::Bool(v.is_truthy()),
        "null" | "unset" => Value::Null,
        "array" => match v {
            Value::Array(_) => v.clone(),
            Value::Null => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
            _ => {
                let mut a = PhpArray::new();
                a.push(v.clone());
                Value::Array(Rc::new(RefCell::new(a)))
            }
        },
        "object" => v.clone(),
        _ => v.clone(),
    }
}
