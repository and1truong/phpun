//! Class/object introspection: *_exists, get_*, class_parents and friends.

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        "class_alias" => {
            let name = arg_str(it, args, 0);
            let alias = arg_str(it, args, 1);
            Value::Bool(it.class_alias(&name, &alias)?)
        }
        "class_exists" | "interface_exists" | "trait_exists" | "enum_exists" => {
            let n = arg_str(it, args, 0);
            // $autoload defaults to true; an explicit false skips it.
            // A throwing autoloader's exception propagates (zend does
            // not swallow it into a `false` result).
            if args.len() <= 1 || arg(args, 1).is_truthy() {
                it.run_autoload(&n)?;
            }
            let key = n.trim_start_matches('\\').to_lowercase();
            match name {
                "interface_exists" => Value::Bool(it.interfaces.contains_key(&key)),
                "trait_exists" => Value::Bool(it.traits.contains_key(&key)),
                "enum_exists" => Value::Bool(matches!(
                    it.lookup_class(&n),
                    Some(c) if c.decl.kind == crate::ast::ClassKind::Enum
                )),
                _ => Value::Bool(it.lookup_class(&n).is_some()),
            }
        }
        "method_exists" => match arg(args, 0) {
            // Closures are objects of class Closure — __invoke exists
            // (bug52060, bug77627).
            Value::Callable(_) => {
                let m = arg_str(it, args, 1).to_lowercase();
                Value::Bool(m == "__invoke")
            }
            Value::Object(o) => {
                let m = arg_str(it, args, 1).to_lowercase();
                let cls = o.borrow().class.clone();
                Value::Bool(it.find_method_in(&cls, &m).is_some())
            }
            Value::Str(cn) => {
                if crate::value::lossy(&cn).eq_ignore_ascii_case("closure") {
                    return Ok(Some(Value::Bool(
                        arg_str(it, args, 1).eq_ignore_ascii_case("__invoke"),
                    )));
                }
                match it.lookup_class(&crate::value::lossy(&cn)) {
                    Some(c) => {
                        let m = arg_str(it, args, 1).to_lowercase();
                        Value::Bool(it.find_method_in(&c, &m).is_some())
                    }
                    None => Value::Bool(false),
                }
            }
            _ => Value::Bool(false),
        },
        "property_exists" => match arg(args, 0) {
            Value::Object(o) => {
                let n = arg_str(it, args, 1);
                Value::Bool(
                    o.borrow().props.contains_key(&n) || it.class_has_prop(&o.borrow().class, &n),
                )
            }
            Value::Str(cn) => {
                let n = arg_str(it, args, 1);
                match it.lookup_class(&crate::value::lossy(&cn)) {
                    Some(c) => Value::Bool(it.class_has_prop(&c, &n)),
                    None => Value::Bool(false),
                }
            }
            _ => Value::Bool(false),
        },
        "get_class" => match arg(args, 0) {
            // zend returns the INTERNAL class name — anon classes keep
            // their `\0file:line$seq` mangled suffix.
            Value::Object(o) => Value::str(o.borrow().class.decl.name.clone()),
            _ => Value::Bool(false),
        },
        "get_parent_class" => match arg(args, 0) {
            Value::Object(o) => match &o.borrow().class.decl.parent {
                Some(p) => Value::str(p.clone()),
                None => Value::Bool(false),
            },
            Value::Str(cn) => match it.lookup_class(&crate::value::lossy(&cn)) {
                Some(c) => match &c.decl.parent {
                    Some(p) => Value::str(p.clone()),
                    None => Value::Bool(false),
                },
                None => Value::Bool(false),
            },
            _ => Value::Bool(false),
        },
        "get_object_vars" | "get_mangled_object_vars" => match arg(args, 0) {
            Value::Object(o) => {
                let mut a = PhpArray::new();
                if name == "get_mangled_object_vars" {
                    // Raw slots with mangled keys — no hooks
                    // (property_hooks/dump); int-keyed buckets decode
                    // to int keys.
                    let ob = o.borrow();
                    for n in &ob.prop_order {
                        if let Some(c) = ob.props.get(n) {
                            let k = match crate::value::int_prop_index(n) {
                                Some(i) => ArrKey::Int(i),
                                None => ArrKey::Str(n.clone().into()),
                            };
                            a.set(k, c.borrow().clone());
                        }
                    }
                } else {
                    // Scope-visible decl entries; hooked props run `get`,
                    // write-only and uninitialized props are skipped.
                    let scope = it.caller_scope_name();
                    let entries = it.object_serial_entries(&o);
                    for (out, slot, decl) in entries {
                        let ok = match &decl {
                            None => true, // dynamic props are public
                            Some((p, dcls)) => match p.visibility {
                                crate::ast::Visibility::Public => true,
                                crate::ast::Visibility::Protected => {
                                    let oc = o.borrow().class.name().to_string();
                                    scope.as_ref().is_some_and(|sc| {
                                        it.obj_is_a_str(sc, &oc) || it.obj_is_a_str(&oc, sc)
                                    })
                                }
                                crate::ast::Visibility::Private => {
                                    scope.as_ref() == Some(&dcls.name().to_string())
                                }
                            },
                        };
                        if !ok {
                            continue;
                        }
                        let v = match &decl {
                            Some((p, dcls)) => it.serial_entry_value(&o, p, dcls, &slot),
                            None => o.borrow().props.get(&slot).map(|c| c.borrow().clone()),
                        };
                        if let Some(v) = v {
                            let k = match crate::value::int_prop_index(&out) {
                                Some(i) => ArrKey::Int(i),
                                None => ArrKey::Str(out.into()),
                            };
                            a.set(k, v);
                        }
                    }
                }
                Value::Array(Rc::new(RefCell::new(a)))
            }
            _ => Value::Null,
        },
        "get_class_methods" => match arg(args, 0) {
            Value::Object(o) => {
                let cls = o.borrow().class.clone();
                let mut a = PhpArray::new();
                for m in it.class_method_names(&cls) {
                    a.push(Value::str(m));
                }
                Value::Array(Rc::new(RefCell::new(a)))
            }
            Value::Str(cn) => match it.lookup_class(&crate::value::lossy(&cn)) {
                Some(c) => {
                    let mut a = PhpArray::new();
                    for m in it.class_method_names(&c) {
                        a.push(Value::str(m));
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
                None => Value::Bool(false),
            },
            _ => Value::Null,
        },
        "get_class_vars" => {
            let cls = match arg(args, 0) {
                Value::Object(o) => Some(o.borrow().class.clone()),
                Value::Str(cn) => it.lookup_class(&crate::value::lossy(&cn)),
                _ => None,
            };
            match cls {
                Some(c) => {
                    let mut a = PhpArray::new();
                    for (n, v) in it.class_default_props(&c) {
                        a.set(ArrKey::Str(n.into()), v);
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
                None => Value::Bool(false),
            }
        }
        "get_declared_classes" => {
            let mut a = PhpArray::new();
            for n in it.declared_names(crate::ast::ClassKind::Class) {
                a.push(Value::str(n));
            }
            for n in it.declared_names(crate::ast::ClassKind::Enum) {
                a.push(Value::str(n));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "get_declared_interfaces" => {
            let mut a = PhpArray::new();
            for n in it.declared_names(crate::ast::ClassKind::Interface) {
                a.push(Value::str(n));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "get_declared_traits" => {
            let mut a = PhpArray::new();
            for n in it.declared_names(crate::ast::ClassKind::Trait) {
                a.push(Value::str(n));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "debug_print_backtrace" => {
            it.emit(&it.format_backtrace());
            Value::Null
        }
        "debug_backtrace" => {
            let mut arr = PhpArray::new();
            let frames = it.backtrace();
            // The executing include frame carries no args in Zend's array
            // (`function: 'require'` only) — but only when it's the
            // innermost frame; deeper includes emit their path args
            // BEFORE the function key (probe9 vs oracle).
            let bare = frames
                .iter()
                .position(|f| !crate::value::trace_frame_hidden(f))
                .filter(|&pos| crate::value::include_frame(&frames[pos]));
            for (pos, fr) in frames.iter().enumerate() {
                let mut f = PhpArray::new();
                if fr.file != "[internal function]" {
                    f.set(ArrKey::Str("file".into()), Value::str(fr.file.clone()));
                    f.set(ArrKey::Str("line".into()), Value::Int(fr.line as i64));
                }
                let incl = crate::value::include_frame(fr);
                if incl && Some(pos) != bare {
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
                }
                f.set(
                    ArrKey::Str("function".into()),
                    Value::str(fr.function.clone()),
                );
                if let Some(c) = &fr.class {
                    f.set(ArrKey::Str("class".into()), Value::str(c.clone()));
                    f.set(ArrKey::Str("type".into()), Value::str(fr.ty.clone()));
                }
                if !incl {
                    let mut a = PhpArray::new();
                    for av in &fr.args {
                        a.push(av.borrow().clone());
                    }
                    // Variadic-collected named args keep their string keys
                    // (named_params/backtrace: `x`/`y` after the positionals).
                    for (n, av) in &fr.named_args {
                        a.set(ArrKey::Str(n.clone().into()), av.borrow().clone());
                    }
                    f.set(
                        ArrKey::Str("args".into()),
                        Value::Array(Rc::new(RefCell::new(a))),
                    );
                }
                arr.push(Value::Array(Rc::new(RefCell::new(f))));
            }
            Value::Array(Rc::new(RefCell::new(arr)))
        }
        "is_a" => match arg(args, 0) {
            Value::Object(o) => {
                let n = arg_str(it, args, 1);
                Value::Bool(it.obj_is_a(&o, &n))
            }
            Value::Callable(_) => {
                let n = arg_str(it, args, 1);
                Value::Bool(n.eq_ignore_ascii_case("closure"))
            }
            _ => Value::Bool(false),
        },
        "is_subclass_of" => {
            let n = arg_str(it, args, 1);
            match arg(args, 0) {
                Value::Object(o) => {
                    let cls = o.borrow().class.clone();
                    // Strict subclass: parents+interfaces transitively,
                    // self excluded.
                    Value::Bool(it.is_subclass_name(cls.name(), &n)?)
                }
                Value::Str(cn) => {
                    // allow_string arg (default true): a string
                    // subject resolves by name.
                    let allow = args.get(2).map(|c| c.borrow().is_truthy()).unwrap_or(true);
                    if !allow {
                        Value::Bool(false)
                    } else {
                        Value::Bool(it.is_subclass_name(&crate::value::lossy(&cn), &n)?)
                    }
                }
                _ => Value::Bool(false),
            }
        }
        "class_implements" | "class_uses" | "class_parents" => {
            let c0 = arg(args, 0);
            let cn = match &c0 {
                Value::Object(o) => o.borrow().class.name().to_string(),
                Value::Str(s) => crate::value::lossy(s).into_owned(),
                _ => {
                    return Err(PhpError::uncaught(
                        "TypeError",
                        format!(
                            "{}(): Argument #1 ($object_or_class) must be of type object|string, {} given",
                            name,
                            c0.debug_type()
                        ),
                        it.cur_line,
                    ))
                }
            };
            if args.len() <= 1 || arg(args, 1).is_truthy() {
                let _ = it.run_autoload(&cn);
            }
            // `fn(class-or-iface-key) -> Option<(decl, is_iface)>`
            let decl_of = |key: &str| -> Option<(Rc<crate::ast::ClassDecl>, bool)> {
                if let Some(c) = it.lookup_class(key) {
                    Some((c.decl.clone(), false))
                } else {
                    it.interfaces
                        .get(&key.to_lowercase())
                        .map(|d| (d.clone(), true))
                }
            };
            let mut out = PhpArray::new();
            match name {
                "class_parents" => {
                    match decl_of(&cn) {
                        // Interfaces report no parents (oracle: empty
                        // even when the iface extends another).
                        Some((_, true)) => {}
                        Some((mut d, _)) => {
                            while let Some(p) = d.parent.clone() {
                                let (pn, next) = match decl_of(&p) {
                                    Some((pd, _)) => (pd.name.clone(), pd),
                                    None => (p.clone(), {
                                        let mut z = (*d).clone();
                                        z.parent = None;
                                        Rc::new(z)
                                    }),
                                };
                                out.set(ArrKey::Str(pn.clone().into()), Value::str(pn));
                                d = next;
                            }
                        }
                        None => {
                            it.warn_pub(&format!("class_parents(): Class \"{}\" not found", cn))?;
                            return Ok(Some(Value::Bool(false)));
                        }
                    }
                }
                "class_implements" => match decl_of(&cn) {
                    Some((d, _)) => {
                        let mut seen = std::collections::HashSet::new();
                        let mut stack: Vec<String> = Vec::new();
                        let mut cur = Some(d.clone());
                        while let Some(cd) = cur {
                            for i in &cd.implements {
                                stack.push(i.clone());
                            }
                            cur = cd
                                .parent
                                .as_ref()
                                .and_then(|p| decl_of(p).map(|(pd, _)| pd));
                        }
                        while let Some(i) = stack.pop() {
                            let il = i.to_lowercase();
                            if seen.insert(il) {
                                let disp = it
                                    .interfaces
                                    .get(&i.to_lowercase())
                                    .map(|d| d.name.clone())
                                    .unwrap_or_else(|| i.clone());
                                out.set(ArrKey::Str(disp.clone().into()), Value::str(disp));
                                if let Some(pd) = it.interfaces.get(&i.to_lowercase()) {
                                    for p in &pd.implements {
                                        stack.push(p.clone());
                                    }
                                }
                            }
                        }
                    }
                    None => {
                        it.warn_pub(&format!("class_implements(): Class \"{}\" not found", cn))?;
                        return Ok(Some(Value::Bool(false)));
                    }
                },
                _ => {
                    // class_uses — traits of the class + its parents +
                    // traits-of-traits transitively.
                    match decl_of(&cn) {
                        Some((d, is_iface)) => {
                            let mut seen = std::collections::HashSet::new();
                            let mut stack: Vec<String> = Vec::new();
                            if !is_iface {
                                let mut cur = Some(d);
                                while let Some(cd) = cur {
                                    for t in &cd.traits {
                                        stack.push(t.clone());
                                    }
                                    cur = cd
                                        .parent
                                        .as_ref()
                                        .and_then(|p| decl_of(p).map(|(pd, _)| pd));
                                }
                            }
                            // Oracle: traits-of-traits are NOT included —
                            // only the traits each class in the chain
                            // used directly.
                            while let Some(t) = stack.pop() {
                                let tl = t.to_lowercase();
                                if seen.insert(tl) {
                                    let disp = it
                                        .traits
                                        .get(&t.to_lowercase())
                                        .map(|d| d.name.clone())
                                        .unwrap_or_else(|| t.clone());
                                    out.set(ArrKey::Str(disp.clone().into()), Value::str(disp));
                                }
                            }
                        }
                        None => {
                            it.warn_pub(&format!("class_uses(): Class \"{}\" not found", cn))?;
                            return Ok(Some(Value::Bool(false)));
                        }
                    }
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "closure_from_callable" | "closure::fromcallable" => arg(args, 0),
        "get_called_class" => it.called_class_name(),
        _ => return Ok(None),
    }))
}
