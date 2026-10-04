//! Member access: property hooks, prop read/write cells, method
//! dispatch and magic methods, static props/calls, class consts,
//! visibility rules, `is_a` and backtrace/reflection helpers.

use super::util::*;
use super::*;

mod statics;

mod methods;

mod props;

impl<'a> Interp<'a> {
    /// Name of the class whose scope the current frame runs in —
    /// private props are only visible to their own declaring class.
    /// Namespace of the currently executing code — the running
    /// function's declaring namespace, or the file-level `namespace`
    /// for top-level statements (Zend/tests/namespaces).
    pub fn caller_ns(&self) -> String {
        self.stack
            .last()
            .map(|f| f.ns.clone())
            .unwrap_or_else(|| self.globals.ns.clone())
    }

    pub fn caller_scope_name(&self) -> Option<String> {
        self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|c| c.name().to_string())
        })
    }

    /// Foreach iteration spec for a plain object: `(emitted key, slot
    /// key, decl name)` entries for declared props in first-declaration
    /// order — parent props first, a child redecl keeps the first decl's
    /// slot position, private props keep per-class mangled keys, virtual
    /// hooked props appear (no slot) at their decl position. Dynamic
    /// props are NOT included — the loop scans prop_order live so props
    /// added mid-iteration still appear (foreach_002). Returns the spec
    /// plus the declared-name set used to hide shadowed dynamics.
    pub(in crate::interp) fn object_foreach_spec(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
    ) -> (
        Vec<(String, String, String)>,
        std::collections::HashSet<String>,
    ) {
        let mut spec: Vec<(String, String, String)> = Vec::new();
        let mut decl_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut cur = Some(o.borrow().class.clone());
        let mut chain = Vec::new();
        while let Some(c) = cur {
            let par = c.decl.parent.clone();
            chain.push(c.clone());
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        chain.reverse();
        let scope = self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|c| c.name().to_string())
        });
        for c in &chain {
            for p in &c.decl.props {
                if p.is_static {
                    continue;
                }
                decl_names.insert(p.name.clone());
            }
        }
        // One entry per prop NAME: each name resolves to a single decl
        // through scope-private-first — a private decl wins only for its
        // own declaring scope, else the first non-private decl. The
        // entry emits at the RESOLVED decl's position (a child's private
        // redecl iterates at the end, after inherited protecteds), and
        // only when that resolved decl is visible — an invisible
        // resolution suppresses the name entirely (C::e skipped under
        // an E scope, but emitted under C).
        let mut emitted: Vec<(usize, usize, String, String)> = Vec::new();
        let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
        for c in &chain {
            for p in &c.decl.props {
                if p.is_static || done.contains(&p.name) {
                    continue;
                }
                done.insert(p.name.clone());
                // Resolve: first private decl matching the caller scope,
                // else the first non-private decl.
                let mut pick: Option<(usize, usize)> = None;
                for (ci, cc) in chain.iter().enumerate() {
                    for (pi, p2) in cc.decl.props.iter().enumerate() {
                        if p2.name != p.name || p2.is_static {
                            continue;
                        }
                        if p2.visibility == crate::ast::Visibility::Private {
                            if scope.as_ref().is_some_and(|sc| sc == &cc.decl.name) {
                                pick = Some((ci, pi));
                                break;
                            }
                        } else if pick.is_none() {
                            pick = Some((ci, pi));
                        }
                    }
                    if pick.as_ref().is_some_and(|&(ci, pi)| {
                        chain[ci].decl.props[pi].visibility == crate::ast::Visibility::Private
                    }) {
                        break;
                    }
                }
                let Some((ci, pi)) = pick else { continue };
                let decl = &chain[ci].decl.props[pi];
                let dcls = &chain[ci];
                let visible = match decl.visibility {
                    crate::ast::Visibility::Public => true,
                    crate::ast::Visibility::Private => {
                        scope.as_ref().is_some_and(|sc| sc == &dcls.decl.name)
                    }
                    crate::ast::Visibility::Protected => scope.as_ref().is_some_and(|sc| {
                        self.is_a_str(sc, &dcls.decl.name) || self.is_a_str(&dcls.decl.name, sc)
                    }),
                };
                if !visible {
                    continue;
                }
                // Position: a private decl emits at its own decl position;
                // a non-private prop shares the first declaration's slot.
                let (pci, ppi) = if decl.visibility == crate::ast::Visibility::Private {
                    (ci, pi)
                } else {
                    chain
                        .iter()
                        .enumerate()
                        .find_map(|(i, cc)| {
                            cc.decl
                                .props
                                .iter()
                                .enumerate()
                                .find(|(_, p3)| p3.name == decl.name && !p3.is_static)
                                .map(|(j, _)| (i, j))
                        })
                        .unwrap_or((ci, pi))
                };
                let slot_key = if decl.visibility == crate::ast::Visibility::Private {
                    format!("\0{}\0{}", dcls.decl.name, decl.name)
                } else {
                    decl.name.clone()
                };
                emitted.push((pci, ppi, decl.name.clone(), slot_key));
            }
        }
        emitted.sort_by_key(|(a, b, _, _)| (*a, *b));
        for (_, _, n, k) in emitted {
            spec.push((n.clone(), k, n));
        }
        (spec, decl_names)
    }

    /// Serialization view for get_object_vars/json_encode/var_export:
    /// per-DECL entries in parent-first order — non-private decls
    /// dedupe by name at their first position while private decls emit
    /// one entry per declaring class (both `changed`s in dump.phpt).
    /// Returns `(emitted key, slot key, decl+decl class)` entries;
    /// dynamic props appended live in insertion order carry `None` —
    /// the caller filters by visibility and resolves values.
    pub fn object_serial_entries(&self, o: &Rc<RefCell<PhpObject>>) -> Vec<SerialEntry> {
        let mut entries = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut cur = Some(o.borrow().class.clone());
        let mut chain = Vec::new();
        while let Some(c) = cur {
            let par = c.decl.parent.clone();
            chain.push(c.clone());
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        chain.reverse();
        for c in &chain {
            for p in &c.decl.props {
                if p.is_static {
                    continue;
                }
                if p.visibility == crate::ast::Visibility::Private {
                    entries.push((
                        p.name.clone(),
                        format!("\0{}\0{}", c.decl.name, p.name),
                        Some((p.clone(), c.clone())),
                    ));
                } else if seen.insert(p.name.clone()) {
                    entries.push((p.name.clone(), p.name.clone(), Some((p.clone(), c.clone()))));
                }
            }
        }
        // Dynamic props follow the declared entries in insertion order
        // (gh20479's g/h, oss-fuzz-382922236's b); mangled keys are
        // declared-private slots emitted by their own entries already.
        let emitted: std::collections::HashSet<String> =
            entries.iter().map(|(_, s, _)| s.clone()).collect();
        for k in &o.borrow().prop_order {
            if emitted.contains(k) || k.starts_with('\0') {
                continue;
            }
            entries.push((k.clone(), k.clone(), None));
        }
        entries
    }

    /// Resolve one serial entry's value: a hooked prop runs its `get`
    /// (write-only props are skipped); a plain prop yields the live
    /// slot (uninitialized slots are skipped).
    pub fn serial_entry_value(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        p: &PropDecl,
        dcls: &Rc<PhpClass>,
        slot: &str,
    ) -> Option<Value> {
        let hs: MergedHooks = if p.visibility == crate::ast::Visibility::Private {
            p.hooks
                .as_ref()
                .map(|hs| hs.iter().cloned().map(|h| (h, dcls.clone())).collect())
                .unwrap_or_default()
        } else {
            match self.hooked_prop(o, &p.name) {
                Some((_, hs)) => hs,
                None => Vec::new(),
            }
        };
        if !hs.is_empty() {
            if let Some((h, c)) = hs.iter().find(|(h, _)| h.is_get && h.body.is_some()) {
                // Serialization bypasses the caller's visibility — a
                // private hook runs in its own declaring scope (dump).
                let v = self.run_hook(o, c, &p.name, h, None).ok()?;
                return self.hook_get_typecheck(p, c, v).ok();
            }
            // A set-only hooked prop still backs a slot — serialization
            // reads it raw, like a plain prop (gh17988).
        }
        o.borrow().props.get(slot).map(|c| c.borrow().clone())
    }

    /// get_class_vars(): declared prop defaults in parent-first decl
    /// order, filtered by caller visibility — hooked props keep their
    /// raw default (no `get` run), virtual props and private props
    /// outside scope are omitted (gh15456).
    pub fn class_default_props(&mut self, cls: &Rc<PhpClass>) -> Vec<(String, Value)> {
        let mut chain = Vec::new();
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            let par = c.decl.parent.clone();
            chain.push(c.clone());
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        chain.reverse();
        let scope = self.caller_scope_name();
        let mut out: Vec<(String, Value)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (ci, c) in chain.iter().enumerate() {
            for p in &c.decl.props {
                let visible = match p.visibility {
                    crate::ast::Visibility::Public => true,
                    crate::ast::Visibility::Private => {
                        scope.as_ref().is_some_and(|sc| sc == &c.decl.name)
                    }
                    crate::ast::Visibility::Protected => scope.as_ref().is_some_and(|sc| {
                        self.is_a_str(sc, &c.decl.name) || self.is_a_str(&c.decl.name, sc)
                    }),
                };
                if !visible {
                    continue;
                }
                if p.visibility != crate::ast::Visibility::Private && !seen.insert(p.name.clone()) {
                    continue;
                }
                // Virtual hooked props have no storage — not in vars.
                if p.hooks.is_some()
                    && !Self::prop_is_backed(p)
                    && !chain[..=ci].iter().any(|c2| {
                        c2.decl.props.iter().any(|p2| {
                            p2.name == p.name
                                && p2.hooks.is_none()
                                && p2.visibility != crate::ast::Visibility::Private
                        })
                    })
                {
                    continue;
                }
                let v = match &p.default {
                    Some(d) => {
                        let old = self.const_self.replace(c.clone());
                        self.class_const_ctx += 1;
                        let v = self.eval_const(d).unwrap_or(Value::Null);
                        self.class_const_ctx -= 1;
                        self.const_self = old;
                        v
                    }
                    None => Value::Null,
                };
                out.push((p.name.clone(), v));
            }
        }
        out
    }

    /// Native bodies for the Reflection* stubs. The reflected
    /// class/function/prop names live under `\0rc\0` prop keys.
    pub(in crate::interp) fn reflection_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        args: &CallArgs,
    ) -> Result<Option<Value>, PhpError> {
        let lname = name.to_lowercase();
        match lname.as_str() {
            "__construct" => {
                let mut ob = obj.borrow_mut();
                let cls = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                // Zend reflectors keep the class NAME, not the object —
                // `new ReflectionClass(new T)` drops the arg temp so
                // its __destruct runs at statement end (bug29368_2).
                let cls = match &cls {
                    Value::Object(o) => Value::str(o.borrow().class.name()),
                    _ => cls,
                };
                let prop = args
                    .get(1)
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                ob.props.insert("\0rc\0class".into(), cell(cls.clone()));
                ob.props.insert("\0rc\0prop".into(), cell(prop.clone()));
                // Public metadata props the real reflectors expose:
                // ReflectionProperty::{class,name}, ReflectionMethod::
                // {class,name}, ReflectionClass/Function::name. `class`
                // keeps the canonical (declared-case) class name.
                let cname = match &cls {
                    Value::Object(o) => o.borrow().class.name().to_string(),
                    Value::Str(s) => {
                        let raw = String::from_utf8_lossy(s).to_string();
                        let resolved = self.resolve_class(&raw).unwrap_or_else(|| raw.clone());
                        self.classes
                            .get(&resolved.to_lowercase())
                            .map(|c| c.decl.name.clone())
                            .unwrap_or(resolved)
                    }
                    // A closure first arg is a Closure object to Zend
                    // (bug69802_2).
                    Value::Callable(_) => "Closure".into(),
                    _ => String::new(),
                };
                match ob.class.name().to_lowercase().as_str() {
                    "reflectionproperty" | "reflectionmethod" | "reflectionclassconstant" => {
                        ob.props.insert("name".into(), cell(prop));
                        ob.props.insert("class".into(), cell(Value::str(&cname)));
                        // Public metadata props render in var_dump in
                        // declaration order: name, then class.
                        for k in ["name", "class"] {
                            if !ob.prop_order.contains(&k.into()) {
                                ob.prop_order.push(k.into());
                            }
                        }
                    }
                    "reflectionclass" | "reflectionfunction" => {
                        // A closure reflector's `name` is its Zend name
                        // `{closure:enclosing():L}` (closure_065).
                        let nm = match &cls {
                            Value::Callable(c) => match &c.kind {
                                CallableKind::Closure(d) => Value::str(&d.name),
                                CallableKind::Named(n) => Value::str(n),
                                CallableKind::Method { name, .. } => Value::str(name),
                            },
                            _ => cls,
                        };
                        ob.props.insert("name".into(), cell(nm));
                        if !ob.prop_order.contains(&"name".into()) {
                            ob.prop_order.push("name".into());
                        }
                    }
                    _ => {}
                }
                Ok(Some(Value::Null))
            }
            // ReflectionFunction::invoke(...$args) and
            // ReflectionMethod::invoke($object, ...$args) forward named
            // args to the target (named_params/call_user_func).
            "invoke" => {
                let is_method = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                if is_method {
                    let target = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = self.conv_str(&mn)?.to_string();
                    let ca = CallArgs {
                        cells: args.cells[1.min(args.cells.len())..].to_vec(),
                        named: args.named.clone(),
                        trav_cells: Vec::new(),
                        nonref_cells: Vec::new(),
                    };
                    match target {
                        Value::Object(o) => Ok(Some(self.method_invoke(o, &mn, ca)?)),
                        Value::Null => {
                            // Static context: Class::method or null $this.
                            let cn = obj
                                .borrow()
                                .props
                                .get("\0rc\0class")
                                .map(|c| c.borrow().clone())
                                .unwrap_or(Value::Null);
                            let cn = self.conv_str(&cn)?.to_string();
                            Ok(Some(self.call_named(&format!("{}::{}", cn, mn), &[])?))
                        }
                        _ => Ok(Some(Value::Null)),
                    }
                } else {
                    let cb = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let ca = CallArgs {
                        cells: args.cells.clone(),
                        named: args.named.clone(),
                        trav_cells: args.trav_cells.clone(),
                        nonref_cells: args.nonref_cells.clone(),
                    };
                    Ok(Some(self.call_value(&cb, ca)?))
                }
            }
            "invokeargs" => {
                let is_method = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                if is_method {
                    let target = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let arr = args
                        .get(1)
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = self.conv_str(&mn)?.to_string();
                    let ca = self.args_from_array(&arr);
                    match target {
                        Value::Object(o) => Ok(Some(self.method_invoke(o, &mn, ca)?)),
                        _ => Ok(Some(Value::Null)),
                    }
                } else {
                    let arr = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let cb = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let ca = self.args_from_array(&arr);
                    Ok(Some(self.call_value(&cb, ca)?))
                }
            }
            // Name introspection shared by function/class reflectors
            // (closure_067/068): closures report their zend name.
            "getshortname" | "getnamespacename" | "innamespace" | "isanonymous" => {
                let stored = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let (fname, anon) = match &stored {
                    Value::Callable(c) => (
                        self.callable_ctx_name(&stored),
                        matches!(c.kind, CallableKind::Closure(_)),
                    ),
                    _ => {
                        let n = self.conv_str(&stored)?.to_string();
                        // A class reflector is anonymous when the CLASS
                        // is — anon classes carry `class@anonymous` in
                        // their generated name (the closure check below
                        // only covers function reflectors).
                        let anon_cls = self
                            .classes
                            .get(&n.to_lowercase())
                            .map(|c| c.decl.name.contains("class@anonymous"))
                            .unwrap_or(false);
                        (n, anon_cls)
                    }
                };
                // A closure's "short name" is its whole zend name —
                // the `\` inside `{closure:Foo\Bar::baz():N}` is part
                // of the literal (closure_067).
                let (short, ns) = if anon {
                    (fname.clone(), String::new())
                } else {
                    (
                        fname.rsplit('\\').next().unwrap_or(&fname).to_string(),
                        match fname.rfind('\\') {
                            Some(i) => fname[..i].to_string(),
                            None => String::new(),
                        },
                    )
                };
                Ok(Some(match lname.as_str() {
                    "getshortname" => Value::str(short),
                    "getnamespacename" => Value::str(ns),
                    "innamespace" => Value::Bool(!anon && fname.contains('\\')),
                    _ => Value::Bool(anon),
                }))
            }
            // ReflectionFunctionAbstract closure accessors
            // (closure_031/042). A function reflector keeps the
            // callable under \0rc\0class; a method reflector keeps
            // {class-name, method-name} — resolve it for scope.
            "isclosure"
            | "getclosure"
            | "getclosurescopeclass"
            | "getclosurecalledclass"
            | "getclosurethis" => {
                let stored = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let is_method = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                let cb = match &stored {
                    Value::Callable(c) => Some(c.clone()),
                    _ => None,
                };
                if lname == "isclosure" {
                    return Ok(Some(Value::Bool(matches!(
                        cb.as_ref().map(|c| &c.kind),
                        Some(CallableKind::Closure(_))
                    ))));
                }
                if lname == "getclosure" {
                    if cb.is_some() {
                        return Ok(Some(stored));
                    }
                    if is_method {
                        let cn = self.conv_str(&stored)?.to_string();
                        let mn = obj
                            .borrow()
                            .props
                            .get("\0rc\0prop")
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Null);
                        let mn = self.conv_str(&mn)?.to_string();
                        let target = args
                            .first()
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Null);
                        let (mc, tc) = match target {
                            Value::Object(o) => (o.borrow().class.clone(), Some(o.clone())),
                            _ => (
                                self.classes
                                    .get(&cn.to_lowercase())
                                    .cloned()
                                    .ok_or_else(|| {
                                        PhpError::uncaught(
                                            "ReflectionException",
                                            format!("Class {} does not exist", cn),
                                            0,
                                        )
                                    })?,
                                None,
                            ),
                        };
                        let (mdecl, decl_cls) = self
                            .find_method_in(&mc, &mn)
                            .map(|(m, dc)| (Some(m), dc))
                            .unwrap_or_else(|| (None, mc.clone()));
                        return Ok(Some(Value::Callable(self.new_callable(PhpCallable {
                            id: std::cell::Cell::new(0),
                            kind: CallableKind::Method {
                                obj: tc,
                                class: Some(mc.clone()),
                                name: mn,
                            },
                            captures: Vec::new(),
                            this_obj: None,
                            scope_class: Some(decl_cls.clone()),
                            called_class: Some(mc),
                            // Static methods produce static closures —
                            // rebinding an instance warns (closure_061).
                            is_static: mdecl.map(|m| m.is_static).unwrap_or(false),
                        }))));
                    }
                    // A function reflector's getClosure is a named
                    // callable — builtins included (bug70630).
                    let fname = self.conv_str(&stored)?.to_string();
                    if !fname.is_empty() {
                        return Ok(Some(Value::Callable(self.new_callable(PhpCallable {
                            id: std::cell::Cell::new(0),
                            kind: CallableKind::Named(fname),
                            captures: Vec::new(),
                            this_obj: None,
                            scope_class: None,
                            called_class: None,
                            is_static: false,
                        }))));
                    }
                    return Ok(Some(Value::Null));
                }
                // Scope/this accessors.
                let (scope, called, this) = match &cb {
                    Some(c) => (
                        c.scope_class.clone(),
                        c.called_class.clone(),
                        c.this_obj.clone(),
                    ),
                    None if is_method => {
                        let cn = self.conv_str(&stored)?.to_string();
                        let mc = self.classes.get(&cn.to_lowercase()).cloned();
                        (mc.clone(), mc, None)
                    }
                    None => (None, None, None),
                };
                match lname.as_str() {
                    "getclosurethis" => Ok(Some(match this {
                        Some(o) => Value::Object(o),
                        None => Value::Null,
                    })),
                    _ => {
                        let rc = if lname == "getclosurescopeclass" {
                            scope
                        } else {
                            called
                        };
                        // Dummy scope: a closure bound to $this with
                        // no real scope reflects class Closure
                        // (closure_042).
                        let nm = rc.map(|c| c.name().to_string()).or_else(|| {
                            if lname == "getclosurescopeclass" && this.is_some() {
                                Some("Closure".to_string())
                            } else {
                                None
                            }
                        });
                        match nm {
                            Some(n) => {
                                let r = self.instantiate("reflectionclass", &[])?;
                                if let Value::Object(o) = &r {
                                    let mut ob = o.borrow_mut();
                                    ob.props.insert("name".into(), cell(Value::str(&n)));
                                    ob.props.insert("\0rc\0class".into(), cell(Value::str(&n)));
                                }
                                Ok(Some(r))
                            }
                            None => Ok(Some(Value::Null)),
                        }
                    }
                }
            }
            // ReflectionClass::getProperty($name) -> ReflectionProperty
            // carrying {\0rc\0class, \0rc\0prop} (typed_properties_018).
            "getproperty" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let pn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                let cls = self.classes.get(&cn.to_lowercase()).cloned();
                let found = cls.as_ref().and_then(|c| self.find_prop_decl(c, &pn));
                match found {
                    Some((_, dcls)) => {
                        let rp = self.instantiate("reflectionproperty", &[])?;
                        if let Value::Object(o) = &rp {
                            let mut ob = o.borrow_mut();
                            ob.props
                                .insert("\0rc\0class".into(), cell(Value::str(dcls.name())));
                            ob.props.insert("\0rc\0prop".into(), cell(Value::str(&pn)));
                            ob.props.insert("name".into(), cell(Value::str(&pn)));
                            ob.props
                                .insert("class".into(), cell(Value::str(dcls.name())));
                        }
                        Ok(Some(rp))
                    }
                    None => self.fail(PhpError::uncaught(
                        "ReflectionException",
                        format!("Property {}::${} does not exist", cn, pn),
                        0,
                    )),
                }
            }
            "hasproperty" => {
                let pn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                let target = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                // ReflectionObject wraps the object itself; check its
                // live + declared props (bug50146). Closures never have
                // props.
                match &target {
                    Value::Object(t) => {
                        let has =
                            t.borrow().props.contains_key(&pn) || self.decl_prop(t, &pn).is_some();
                        return Ok(Some(Value::Bool(has)));
                    }
                    Value::Callable(_) | Value::Null => {
                        return Ok(Some(Value::Bool(false)));
                    }
                    _ => {}
                }
                let cn = self.conv_str(&target)?.to_string();
                let has = self
                    .classes
                    .get(&cn.to_lowercase())
                    .cloned()
                    .and_then(|c| self.find_prop_decl(&c, &pn))
                    .is_some();
                Ok(Some(Value::Bool(has)))
            }
            // ReflectionProperty::getType() -> ReflectionNamedType with
            // the declared members under \0rp\0ty (same convention as
            // ReflectionParameter).
            "gettype" => {
                let is_prop = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionproperty");
                if is_prop {
                    let cn = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let cn = self.conv_str(&cn)?.to_string();
                    let pn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let pn = self.conv_str(&pn)?.to_string();
                    let ty = self
                        .classes
                        .get(&cn.to_lowercase())
                        .cloned()
                        .and_then(|c| self.find_prop_decl(&c, &pn))
                        .and_then(|(pd, _)| pd.ty);
                    match ty {
                        Some(tys) => {
                            let nt = self.instantiate("reflectionnamedtype", &[])?;
                            if let Value::Object(o) = &nt {
                                let mut ta = PhpArray::default();
                                for m in &tys {
                                    ta.push(Value::str(m));
                                }
                                let mut ob = o.borrow_mut();
                                ob.props.insert(
                                    "\0rp\0ty".into(),
                                    cell(Value::Array(Rc::new(RefCell::new(ta)))),
                                );
                                ob.props
                                    .insert("name".into(), cell(Value::str(tys.first().unwrap())));
                            }
                            Ok(Some(nt))
                        }
                        None => Ok(Some(Value::Null)),
                    }
                } else {
                    // ReflectionParameter::getType() — members stored
                    // under \0rp\0ty by getParameters()
                    // (trampoline_closure_named_arguments).
                    let is_param = obj
                        .borrow()
                        .class
                        .name()
                        .eq_ignore_ascii_case("reflectionparameter");
                    let tys = if is_param {
                        obj.borrow()
                            .props
                            .get("\0rp\0ty")
                            .map(|c| c.borrow().clone())
                            .and_then(|v| match v {
                                Value::Array(a) => Some(a),
                                _ => None,
                            })
                    } else {
                        None
                    };
                    match tys {
                        Some(ta) => {
                            let members: Vec<String> = ta
                                .borrow()
                                .entries
                                .iter()
                                .map(|(_, c)| c.borrow().to_php_string())
                                .collect();
                            let nt = self.instantiate("reflectionnamedtype", &[])?;
                            if let Value::Object(o) = &nt {
                                let mut ob = o.borrow_mut();
                                ob.props
                                    .insert("\0rp\0ty".into(), cell(Value::Array(ta.clone())));
                                if let Some(first) = members.first() {
                                    ob.props
                                        .insert("name".into(), cell(Value::str(first.clone())));
                                }
                            }
                            Ok(Some(nt))
                        }
                        None => Ok(Some(Value::Null)),
                    }
                }
            }
            // ReflectionClass::getDefaultProperties(): prop defaults
            // keyed by prop name; typed props without defaults are
            // absent (bug #77673 — typed_properties_105).
            "getdefaultproperties" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mut arr = PhpArray::default();
                if let Some(cls) = self.classes.get(&cn.to_lowercase()).cloned() {
                    let mut chain: Vec<Rc<PhpClass>> = Vec::new();
                    let mut cur = Some(cls);
                    while let Some(c) = cur {
                        chain.push(c.clone());
                        cur = c
                            .decl
                            .parent
                            .as_ref()
                            .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
                    }
                    for c in chain.iter().rev() {
                        for p in &c.decl.props {
                            if p.ty.is_some() && p.default.is_none() {
                                continue;
                            }
                            let dv = match &p.default {
                                Some(d) => {
                                    let old = self.const_self.replace(c.clone());
                                    self.class_const_ctx += 1;
                                    let r = self.eval_decl_const(d, &c.decl.file);
                                    self.class_const_ctx -= 1;
                                    self.const_self = old;
                                    match r {
                                        Ok(v) => v,
                                        Err(_) => continue,
                                    }
                                }
                                None => Value::Null,
                            };
                            arr.set(ArrKey::Str(p.name.clone().into()), dv);
                        }
                    }
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "getparameters" => {
                // Each param becomes a ReflectionParameter carrying its
                // declared type members under \0rp\0ty (callable_002).
                let is_method = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                let decl: Option<Rc<crate::ast::FunctionDecl>> = if is_method {
                    let cn = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = self.conv_str(&mn)?.to_string();
                    if matches!(&cn, Value::Callable(_)) {
                        // new ReflectionMethod($closure, '__invoke') —
                        // Closure::__invoke carries the wrapped
                        // function's signature (bug69802_2).
                        if mn.eq_ignore_ascii_case("__invoke") {
                            self.callable_decl(&cn)
                        } else {
                            None
                        }
                    } else {
                        let cn = self.conv_str(&cn)?.to_string();
                        let c = self.classes.get(&cn.to_lowercase()).cloned();
                        match c {
                            Some(c) => self
                                .find_method_in(&c, &mn)
                                .map(|(m, _)| Rc::new(m.decl.clone())),
                            None => None,
                        }
                    }
                } else {
                    let cb = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    self.callable_decl(&cb)
                };
                let mut arr = PhpArray::default();
                if let Some(d) = decl {
                    for p in &d.params {
                        let rp = self.instantiate("reflectionparameter", &[])?;
                        if let Value::Object(o) = &rp {
                            o.borrow_mut()
                                .props
                                .insert("\0rp\0name".into(), cell(Value::str(&p.name)));
                            // Zend's ReflectionParameter exposes the name
                            // as a public prop rendered by var_dump.
                            let mut ob = o.borrow_mut();
                            ob.props.insert("name".into(), cell(Value::str(&p.name)));
                            if !ob.prop_order.contains(&"name".into()) {
                                ob.prop_order.push("name".into());
                            }
                            drop(ob);
                            o.borrow_mut()
                                .props
                                .insert("\0rp\0variadic".into(), cell(Value::Bool(p.variadic)));
                            let mut ta = PhpArray::default();
                            if let Some(ty) = &p.ty {
                                for m in ty {
                                    ta.push(Value::str(m));
                                }
                            }
                            o.borrow_mut().props.insert(
                                "\0rp\0ty".into(),
                                cell(Value::Array(Rc::new(RefCell::new(ta)))),
                            );
                        }
                        arr.push(rp);
                    }
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "isvariadic" => Ok(Some(Value::Bool(
                obj.borrow()
                    .props
                    .get("\0rp\0variadic")
                    .is_some_and(|c| c.borrow().is_truthy()),
            ))),
            "hastype" => {
                // ReflectionParameter::hasType() — \0rp\0ty members
                // populated by getParameters().
                let has = obj
                    .borrow()
                    .props
                    .get("\0rp\0ty")
                    .map(|c| c.borrow().clone())
                    .and_then(|v| match v {
                        Value::Array(a) => Some(!a.borrow().entries.is_empty()),
                        _ => None,
                    })
                    .unwrap_or(false);
                Ok(Some(Value::Bool(has)))
            }
            "getclass" => {
                // Deprecated since 8.0 — returns a ReflectionClass for
                // the first class/interface member of the declared
                // type (a union picks the class part — bug69802_2).
                self.deprecated(
                    "Method ReflectionParameter::getClass() is deprecated since 8.0, use ReflectionParameter::getType() instead",
                )?;
                let ty = obj
                    .borrow()
                    .props
                    .get("\0rp\0ty")
                    .map(|c| c.borrow().clone())
                    .and_then(|v| match v {
                        Value::Array(a) => Some(a),
                        _ => None,
                    });
                let class_ty = ty.and_then(|ta| {
                    ta.borrow().entries.iter().find_map(|(_, c)| {
                        let n = c.borrow().to_php_string();
                        self.classes
                            .get(&n.to_lowercase())
                            .map(|cl| cl.decl.name.clone())
                            .or_else(|| {
                                self.interfaces
                                    .get(&n.to_lowercase())
                                    .map(|d| d.name.clone())
                            })
                    })
                });
                match class_ty {
                    Some(n) => {
                        let rc = self.instantiate("reflectionclass", &[])?;
                        if let Value::Object(o) = &rc {
                            let mut ob = o.borrow_mut();
                            ob.props.insert("\0rc\0class".into(), cell(Value::str(&n)));
                            ob.props.insert("name".into(), cell(Value::str(&n)));
                            if !ob.prop_order.contains(&"name".into()) {
                                ob.prop_order.push("name".into());
                            }
                        }
                        Ok(Some(rc))
                    }
                    None => Ok(Some(Value::Null)),
                }
            }
            "iscallable" => {
                self.deprecated(
                    "Method ReflectionParameter::isCallable() is deprecated since 8.0, use ReflectionParameter::getType() instead",
                )?;
                let has = obj
                    .borrow()
                    .props
                    .get("\0rp\0ty")
                    .map(|c| c.borrow().clone())
                    .and_then(|v| match v {
                        Value::Array(a) => Some(a),
                        _ => None,
                    })
                    .map(|a| {
                        a.borrow().entries.iter().any(|(_, c)| {
                            matches!(&*c.borrow(), Value::Str(s) if s.eq_ignore_ascii_case(b"callable"))
                        })
                    })
                    .unwrap_or(false);
                Ok(Some(Value::Bool(has)))
            }
            "getattributes" => {
                let is_fn = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionfunction");
                let tn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let tn = self.conv_str(&tn)?.to_string();
                let is_cc = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionclassconstant");
                if is_cc {
                    let cn = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let cn = self.conv_str(&cn)?.to_string();
                    let pn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let pn = self.conv_str(&pn)?.to_string();
                    let decls = self
                        .find_const_decl(&cn, &pn)
                        .map(|(cd, _)| cd.attrs.clone())
                        .unwrap_or_default();
                    let fname = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .map(|v| self.conv_str(&v).map(|s| s.to_string()))
                        .transpose()?
                        .unwrap_or_default();
                    let mut arr = PhpArray::default();
                    for a in decls {
                        if !fname.is_empty() && !a.name.eq_ignore_ascii_case(&fname) {
                            continue;
                        }
                        let v = self.instantiate("reflectionattribute", &[])?;
                        if let Value::Object(o) = &v {
                            o.borrow_mut().internal = Some(ObjectInternal::ReflectionAttribute {
                                name: a.name.clone(),
                                args: Rc::new(a.args.clone()),
                                target: 16,
                            });
                        }
                        arr.push(v);
                    }
                    return Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))));
                }
                let is_m = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                if is_m {
                    let cn = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let cn = self.conv_str(&cn)?.to_string();
                    let mn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = self.conv_str(&mn)?.to_string();
                    let decls = self
                        .classes
                        .get(&cn.to_lowercase())
                        .cloned()
                        .and_then(|c| self.find_method_in(&c, &mn))
                        .map(|(m, _)| m.decl.attrs.clone())
                        .unwrap_or_default();
                    let fname = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .map(|v| self.conv_str(&v).map(|s| s.to_string()))
                        .transpose()?
                        .unwrap_or_default();
                    let mut arr = PhpArray::default();
                    for a in decls {
                        if !fname.is_empty() && !a.name.eq_ignore_ascii_case(&fname) {
                            continue;
                        }
                        let v = self.instantiate("reflectionattribute", &[])?;
                        if let Value::Object(o) = &v {
                            o.borrow_mut().internal = Some(ObjectInternal::ReflectionAttribute {
                                name: a.name.clone(),
                                args: Rc::new(a.args.clone()),
                                target: 4,
                            });
                        }
                        arr.push(v);
                    }
                    return Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))));
                }
                let (decls, target): (Vec<crate::ast::AttrDecl>, i64) = if is_fn {
                    (
                        self.functions
                            .get(&tn.to_lowercase())
                            .map(|d| d.attrs.clone())
                            .unwrap_or_default(),
                        2,
                    )
                } else {
                    (
                        self.classes
                            .get(&tn.to_lowercase())
                            .map(|c| c.decl.attrs.clone())
                            .unwrap_or_default(),
                        1,
                    )
                };
                // Optional class-name filter arg.
                let fname = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .map(|v| self.conv_str(&v).map(|s| s.to_string()))
                    .transpose()?
                    .unwrap_or_default();
                let mut arr = PhpArray::default();
                for a in decls {
                    if !fname.is_empty() && !a.name.eq_ignore_ascii_case(&fname) {
                        continue;
                    }
                    let v = self.instantiate("reflectionattribute", &[])?;
                    if let Value::Object(o) = &v {
                        o.borrow_mut().internal = Some(ObjectInternal::ReflectionAttribute {
                            name: a.name.clone(),
                            args: Rc::new(a.args.clone()),
                            target,
                        });
                    }
                    arr.push(v);
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "getarguments" => {
                let exprs = match &obj.borrow().internal {
                    Some(ObjectInternal::ReflectionAttribute { args, .. }) => args.clone(),
                    _ => return Ok(Some(Value::Null)),
                };
                let mut arr = PhpArray::default();
                for e in exprs.iter() {
                    if let Expr::Binary { op: "named", l, r } = e {
                        if let Expr::Str(n) = l.as_ref() {
                            let v = self.eval_const(r)?;
                            arr.set(ArrKey::Str(n.as_str().into()), v);
                            continue;
                        }
                    }
                    arr.push(self.eval_const(e)?);
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "newinstance" | "newinstanceargs" => {
                if let Some(ObjectInternal::ReflectionAttribute {
                    name,
                    args: aexprs,
                    target,
                }) = &obj.borrow().internal
                {
                    let (name, aexprs, target) = (name.clone(), aexprs.clone(), *target);
                    let lname = name.trim_start_matches('\\').to_lowercase();
                    let cls = match self.classes.get(&lname).cloned() {
                        Some(c) => c,
                        None => {
                            // `new ReflectionClass` may not have loaded
                            // the attribute class yet — trigger autoload.
                            let resolved = self
                                .resolve_class(name.trim_start_matches('\\'))
                                .unwrap_or_else(|| name.clone());
                            match self.classes.get(&resolved.to_lowercase()).cloned() {
                                Some(c) => c,
                                None => {
                                    return self
                                        .fail::<Option<Value>>(PhpError::uncaught(
                                            "Error",
                                            format!("Class \"{}\" not found", name),
                                            0,
                                        ))
                                        .map(|_| None);
                                }
                            }
                        }
                    };
                    let short = |n: &str| n.rsplit('\\').next().unwrap_or(n).to_string();
                    let marker = cls
                        .decl
                        .attrs
                        .iter()
                        .find(|a| short(&a.name).eq_ignore_ascii_case("attribute"));
                    let Some(marker) = marker else {
                        return self
                            .fail::<Option<Value>>(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Attempting to use non-attribute class \"{}\" as attribute",
                                    name
                                ),
                                0,
                            ))
                            .map(|_| None);
                    };
                    // `#[Attribute(flags: N)]` (or first positional) gates
                    // which declarations the attribute may target.
                    let mut mask = 63i64;
                    let flags_e = marker
                        .args
                        .iter()
                        .find_map(|a| {
                            if let Expr::Binary { op: "named", l, r } = a {
                                if matches!(l.as_ref(), Expr::Str(n) if n == "flags") {
                                    return Some(r.as_ref());
                                }
                                None
                            } else {
                                None
                            }
                        })
                        .or_else(|| marker.args.first());
                    if let Some(e) = flags_e {
                        mask = self.eval_const(e)?.to_int();
                    }
                    if mask & target == 0 {
                        let tn = match target {
                            1 => "class",
                            2 => "function",
                            4 => "method",
                            8 => "property",
                            16 => "class constant",
                            32 => "parameter",
                            _ => "unknown",
                        };
                        let allowed: Vec<&str> = [
                            (1i64, "class"),
                            (2, "function"),
                            (4, "method"),
                            (8, "property"),
                            (16, "class constant"),
                            (32, "parameter"),
                        ]
                        .iter()
                        .filter(|(b, _)| mask & b != 0)
                        .map(|(_, n)| *n)
                        .collect();
                        return self
                            .fail::<Option<Value>>(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Attribute \"{}\" cannot target {} (allowed targets: {})",
                                    name,
                                    tn,
                                    allowed.join(", ")
                                ),
                                0,
                            ))
                            .map(|_| None);
                    }
                    let mut cells = Vec::new();
                    let mut named = Vec::new();
                    for e in aexprs.iter() {
                        if let Expr::Binary { op: "named", l, r } = e {
                            if let Expr::Str(n) = l.as_ref() {
                                named.push((n.clone(), cell(self.eval_const(r)?), true, false));
                                continue;
                            }
                        }
                        cells.push(cell(self.eval_const(e)?));
                    }
                    let ca = CallArgs {
                        cells,
                        named,
                        trav_cells: Vec::new(),
                        nonref_cells: Vec::new(),
                    };
                    return self.new_instance(&name, ca).map(Some);
                }
                let ca = if lname == "newinstanceargs" {
                    let arr = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    self.args_from_array(&arr)
                } else {
                    CallArgs {
                        cells: args.cells.clone(),
                        named: args.named.clone(),
                        trav_cells: args.trav_cells.clone(),
                        nonref_cells: args.nonref_cells.clone(),
                    }
                };
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                Ok(Some(self.new_instance(&cn, ca)?))
            }
            "getname" => {
                if let Some(ObjectInternal::ReflectionAttribute { name, .. }) =
                    &obj.borrow().internal
                {
                    return Ok(Some(Value::str(name.clone())));
                }
                let clsname = obj.borrow().class.name().to_lowercase();
                // ReflectionNamedType::getName() -> the stored type name.
                if clsname == "reflectionnamedtype" {
                    return Ok(Some(
                        obj.borrow()
                            .props
                            .get("name")
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Null),
                    ));
                }
                // ReflectionClassConstant::getName() is the const name;
                // ReflectionProperty::getName() the prop name; every
                // other reflector reports its class/subject.
                let key = match clsname.as_str() {
                    "reflectionclassconstant"
                    | "reflectionclass"
                    | "reflectionfunction"
                    | "reflectionmethod" => "name",
                    "reflectionproperty" => "\0rc\0prop",
                    "reflectionparameter" => "\0rp\0name",
                    _ => "\0rc\0class",
                };
                Ok(Some(
                    obj.borrow()
                        .props
                        .get(key)
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null),
                ))
            }
            // ReflectionClass::getParentClass() -> a ReflectionClass of
            // the parent, or false when there is none (php-enum walks
            // ancestors this way).
            "getparentclass" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let parent = self
                    .classes
                    .get(&cn.to_lowercase())
                    .and_then(|c| c.decl.parent.clone());
                match parent {
                    Some(p) => {
                        let pcn = self
                            .classes
                            .get(&p.to_lowercase())
                            .map(|c| c.decl.name.clone())
                            .unwrap_or(p);
                        let rc = self.instantiate("reflectionclass", &[])?;
                        if let Value::Object(o) = &rc {
                            let mut ob = o.borrow_mut();
                            ob.props
                                .insert("\0rc\0class".into(), cell(Value::str(&pcn)));
                            ob.props.insert("name".into(), cell(Value::str(&pcn)));
                        }
                        Ok(Some(rc))
                    }
                    None => Ok(Some(Value::Bool(false))),
                }
            }
            // getshortname/getnamespacename are handled by the earlier
            // combined name-introspection arm (a second arm here would be
            // unreachable — clippy failure surfaced by the #50 merge).
            "newinstancewithoutconstructor" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?;
                Ok(Some(self.instantiate(&cn.to_lowercase(), &[])?))
            }
            "isfinal" | "isabstract" | "isstatic" | "ispublic" | "isprotected" | "isprivate"
            | "isenumcase" | "isdeprecated" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                let refl_kind = obj.borrow().class.name().to_lowercase();
                let b = match refl_kind.as_str() {
                    // ReflectionClassConstant: visibility/final/enum_case
                    // come off the ConstDecl (marc-mabe Enum::getConstants).
                    "reflectionclassconstant" => {
                        let cd = self.find_const_decl(&cn, &mn);
                        match cd {
                            Some(cd) => match lname.as_str() {
                                "isfinal" => cd.0.is_final,
                                "ispublic" => cd.0.visibility == crate::ast::Visibility::Public,
                                "isprotected" => {
                                    cd.0.visibility == crate::ast::Visibility::Protected
                                }
                                "isprivate" => cd.0.visibility == crate::ast::Visibility::Private,
                                "isenumcase" => cd.0.enum_case,
                                _ => false,
                            },
                            None => false,
                        }
                    }
                    // ReflectionProperty: visibility/static/readonly off
                    // the PropDecl.
                    "reflectionclass" | "reflectionobject" | "reflectionenum" => {
                        match self.classes.get(&cn.to_lowercase()) {
                            Some(c) => match lname.as_str() {
                                "isfinal" => c.decl.is_final,
                                "isabstract" => c.decl.is_abstract,
                                "isinterface" => c.decl.kind == crate::ast::ClassKind::Interface,
                                _ => false,
                            },
                            None => false,
                        }
                    }
                    "reflectionproperty" => {
                        let pd = self
                            .classes
                            .get(&cn.to_lowercase())
                            .cloned()
                            .and_then(|c| self.find_prop_decl(&c, &mn))
                            .map(|(pd, _)| pd);
                        match pd {
                            Some(pd) => match lname.as_str() {
                                "isstatic" => pd.is_static,
                                "ispublic" => pd.visibility == crate::ast::Visibility::Public,
                                "isprotected" => pd.visibility == crate::ast::Visibility::Protected,
                                "isprivate" => pd.visibility == crate::ast::Visibility::Private,
                                "isfinal" => pd.is_final,
                                _ => false,
                            },
                            None => false,
                        }
                    }
                    _ => {
                        let m = self
                            .classes
                            .get(&cn.to_lowercase())
                            .cloned()
                            .and_then(|c| self.find_method_in(&c, &mn).map(|(m, _)| m));
                        match m {
                            Some(m) => match lname.as_str() {
                                "isfinal" => m.is_final,
                                "isabstract" => m.is_abstract,
                                "isstatic" => m.is_static,
                                "ispublic" => m.visibility == crate::ast::Visibility::Public,
                                "isprotected" => m.visibility == crate::ast::Visibility::Protected,
                                _ => m.visibility == crate::ast::Visibility::Private,
                            },
                            None => false,
                        }
                    }
                };
                Ok(Some(Value::Bool(b)))
            }
            // ReflectionClass file location — user classes carry
            // decl.file; internal classes report false like Zend.
            "getfilename" | "isinternal" | "isuserdefined" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                let file = if !mn.is_empty() {
                    self.classes
                        .get(&cn.to_lowercase())
                        .cloned()
                        .and_then(|c| self.find_method_in(&c, &mn).map(|(m, _)| m))
                        .map(|m| m.decl.file.clone())
                        .unwrap_or_default()
                } else {
                    self.classes
                        .get(&cn.to_lowercase())
                        .map(|c| c.decl.file.clone())
                        .unwrap_or_default()
                };
                let internal = file.is_empty() || file == "builtin";
                Ok(Some(match lname.as_str() {
                    "getfilename" => {
                        if internal {
                            Value::Bool(false)
                        } else {
                            Value::str(file)
                        }
                    }
                    "isinternal" => Value::Bool(internal),
                    _ => Value::Bool(!internal),
                }))
            }
            // Class-level kind predicates (the member-flag arm above
            // only resolves Reflection{Method,Property,ClassConstant};
            // isAnonymous lives in the name-introspection arm which
            // covers both closure and class subjects).
            "isinterface" | "istrait" | "isenum" | "isinstantiable" | "iscloneable"
            | "isreadonly" | "isiterable" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let decl = self.classes.get(&cn.to_lowercase()).map(|c| c.decl.clone());
                let b = decl
                    .map(|d| match lname.as_str() {
                        "isinterface" => d.kind == crate::ast::ClassKind::Interface,
                        "istrait" => d.kind == crate::ast::ClassKind::Trait,
                        "isenum" => d.kind == crate::ast::ClassKind::Enum,
                        "isreadonly" => d.readonly,
                        "isiterable" => d.implements.iter().any(|i| {
                            i.eq_ignore_ascii_case("traversable")
                                || i.eq_ignore_ascii_case("iterator")
                                || i.eq_ignore_ascii_case("iteratoraggregate")
                        }),
                        _ => d.kind == crate::ast::ClassKind::Class && !d.is_abstract,
                    })
                    .unwrap_or(false);
                Ok(Some(Value::Bool(b)))
            }
            // isSubclassOf/implementsInterface — walk the parent chain /
            // transitive interface set. Arg: class-string or reflector.
            "issubclassof" | "implementsinterface" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let target = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let tname = match &target {
                    Value::Object(o) => o
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null),
                    v => v.clone(),
                };
                let tname = self.conv_str(&tname)?.to_string().to_lowercase();
                let mut hit = false;
                let mut cur = self.classes.get(&cn.to_lowercase()).cloned();
                let mut seen = std::collections::HashSet::new();
                while let Some(c) = cur {
                    if lname == "issubclassof" {
                        cur = c
                            .decl
                            .parent
                            .as_ref()
                            .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
                        if let Some(n) = cur.as_ref().map(|c| c.decl.name.to_lowercase()) {
                            if n == tname {
                                hit = true;
                            }
                        }
                    } else {
                        // transitive interface set incl. interface-extends
                        let mut q: Vec<String> = c.decl.implements.clone();
                        if c.decl.kind == crate::ast::ClassKind::Interface {
                            q.extend(c.decl.parent.iter().cloned());
                        }
                        for i in q {
                            if i.to_lowercase() == tname {
                                hit = true;
                            }
                            if let Some(ic) = self.classes.get(&i.to_lowercase()).cloned() {
                                for pp in ic.decl.parent.iter().chain(ic.decl.implements.iter()) {
                                    if pp.to_lowercase() == tname {
                                        hit = true;
                                    }
                                }
                            }
                        }
                        cur = c
                            .decl
                            .parent
                            .as_ref()
                            .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
                    }
                    let k = cur.as_ref().map(|c| c.decl.name.to_lowercase());
                    if k.is_none() || !seen.insert(k.unwrap()) {
                        break;
                    }
                }
                Ok(Some(Value::Bool(hit)))
            }
            // getMethods(filter) — own + inherited methods as
            // ReflectionMethod objects; declaring class per PHP.
            "getmethods" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let filter = args.first().map(|c| c.borrow().to_int()).unwrap_or(0);
                let mut arr = PhpArray::default();
                let mut seen = std::collections::HashSet::new();
                let mut cur = self.classes.get(&cn.to_lowercase()).cloned();
                while let Some(c) = cur {
                    for m in &c.decl.methods {
                        let key = m.decl.name.to_lowercase();
                        if !seen.insert(key) {
                            continue;
                        }
                        let ok = filter == 0
                            || (filter & 1 != 0 && m.visibility == crate::ast::Visibility::Public)
                            || (filter & 2 != 0
                                && m.visibility == crate::ast::Visibility::Protected)
                            || (filter & 4 != 0 && m.visibility == crate::ast::Visibility::Private)
                            || (filter & 16 != 0 && m.is_static)
                            || (filter & 32 != 0 && m.is_final)
                            || (filter & 64 != 0 && m.is_abstract);
                        if !ok {
                            continue;
                        }
                        let rm = self.instantiate("reflectionmethod", &[])?;
                        if let Value::Object(o) = &rm {
                            let mut ob = o.borrow_mut();
                            ob.props.insert(
                                "\0rc\0class".into(),
                                cell(Value::str(c.decl.name.clone())),
                            );
                            ob.props
                                .insert("\0rc\0prop".into(), cell(Value::str(m.decl.name.clone())));
                            ob.props
                                .insert("name".into(), cell(Value::str(m.decl.name.clone())));
                            ob.props
                                .insert("class".into(), cell(Value::str(c.decl.name.clone())));
                        }
                        arr.push(rm);
                    }
                    cur = c
                        .decl
                        .parent
                        .as_ref()
                        .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "getmethod" | "hasmethod" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                let cls = self.classes.get(&cn.to_lowercase()).cloned();
                let found = cls.as_ref().and_then(|c| self.find_method_in(c, &mn));
                match (lname.as_str(), found) {
                    ("hasmethod", f) => Ok(Some(Value::Bool(f.is_some()))),
                    (_, Some((m, dcls))) => {
                        let rm = self.instantiate("reflectionmethod", &[])?;
                        if let Value::Object(o) = &rm {
                            let mut ob = o.borrow_mut();
                            ob.props.insert(
                                "\0rc\0class".into(),
                                cell(Value::str(dcls.decl.name.clone())),
                            );
                            ob.props
                                .insert("\0rc\0prop".into(), cell(Value::str(m.decl.name.clone())));
                            ob.props
                                .insert("name".into(), cell(Value::str(m.decl.name.clone())));
                            ob.props
                                .insert("class".into(), cell(Value::str(dcls.decl.name.clone())));
                        }
                        Ok(Some(rm))
                    }
                    _ => self.fail(PhpError::uncaught(
                        "ReflectionException",
                        format!("Method {}::{}() does not exist", cn, mn),
                        0,
                    )),
                }
            }
            // ReflectionMethod identity/lines/return-type plumbing.
            "isconstructor" | "isdestructor" => {
                let mn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                Ok(Some(Value::Bool(mn.eq_ignore_ascii_case(
                    if lname == "isconstructor" {
                        "__construct"
                    } else {
                        "__destruct"
                    },
                ))))
            }
            "getstartline" | "getendline" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                let ln = self
                    .classes
                    .get(&cn.to_lowercase())
                    .cloned()
                    .and_then(|c| self.find_method_in(&c, &mn).map(|(m, _)| m))
                    .map(|m| {
                        if lname == "getstartline" {
                            m.decl.line
                        } else {
                            m.decl.end_line
                        }
                    })
                    .unwrap_or(0);
                Ok(Some(Value::Int(ln as i64)))
            }
            "getdeclaringnamespace" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let ns = cn
                    .rsplit_once('\\')
                    .map(|(n, _)| n.to_string())
                    .unwrap_or_default();
                Ok(Some(Value::str(ns)))
            }
            "hasreturntype" | "getreturntype" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                let tys = self
                    .classes
                    .get(&cn.to_lowercase())
                    .cloned()
                    .and_then(|c| self.find_method_in(&c, &mn).map(|(m, _)| m))
                    .and_then(|m| m.decl.ret.clone());
                match (lname.as_str(), tys) {
                    ("hasreturntype", t) => Ok(Some(Value::Bool(t.is_some()))),
                    (_, Some(tys)) => {
                        let nt = self.instantiate("reflectionnamedtype", &[])?;
                        if let Value::Object(o) = &nt {
                            let mut ta = PhpArray::default();
                            for m in &tys {
                                ta.push(Value::str(m));
                            }
                            let mut ob = o.borrow_mut();
                            ob.props.insert(
                                "\0rp\0ty".into(),
                                cell(Value::Array(Rc::new(RefCell::new(ta)))),
                            );
                            if let Some(first) = tys.first() {
                                ob.props.insert("name".into(), cell(Value::str(first)));
                            }
                        }
                        Ok(Some(nt))
                    }
                    _ => Ok(Some(Value::Null)),
                }
            }
            "getdoccomment" => Ok(Some(Value::Bool(false))),
            "gettraitaliases" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mut arr = PhpArray::default();
                if let Some(c) = self.classes.get(&cn.to_lowercase()).cloned() {
                    for m in &c.decl.methods {
                        if let Some(orig) = &m.trait_alias_of {
                            let v =
                                format!("{}::{}", m.decl.decl_in.clone().unwrap_or_default(), orig);
                            arr.set(ArrKey::Str(m.decl.name.as_str().into()), Value::str(v));
                        }
                    }
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            // ReflectionClass::getInterfaceNames() — declared-case
            // interface names; getInterfaces() returns the reflectors.
            "getinterfacenames" | "getinterfaces" => {
                let cv = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = match &cv {
                    Value::Object(o) => o.borrow().class.name().to_string(),
                    _ => self.conv_str(&cv)?.to_string(),
                };
                let l = self
                    .resolve_class(&cn)
                    .unwrap_or_else(|| cn.clone())
                    .to_lowercase();
                let decl = self
                    .classes
                    .get(&l)
                    .map(|c| c.decl.clone())
                    .or_else(|| self.interfaces.get(&l).cloned())
                    .or_else(|| self.traits.get(&l).cloned());
                let mut arr = PhpArray::default();
                if let Some(d) = decl {
                    for i in &d.implements {
                        if lname == "getinterfaces" {
                            let r = self.instantiate("reflectionclass", &[Value::str(i)])?;
                            arr.set(ArrKey::Str(i.as_str().into()), r);
                        } else {
                            arr.push(Value::str(i.clone()));
                        }
                    }
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "getconstant" | "getconstants" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                if lname == "getconstants" {
                    let mut arr = PhpArray::default();
                    for (n, _cd) in self.all_const_decls(&cn) {
                        // Go through class_const_named: it binds the
                        // declaring class so `self::X` inside const decls
                        // (e.g. Enum::MAPPING) resolves correctly.
                        if let Ok(v) = self.class_const_named(&cn, &n) {
                            arr.set(ArrKey::Str(n.into()), v);
                        }
                    }
                    return Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))));
                }
                let pn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                match self.class_const_named(&cn, &pn) {
                    Ok(v) => Ok(Some(v)),
                    Err(_) => Ok(Some(Value::Bool(false))),
                }
            }
            "getreflectionconstant" | "getreflectionconstants" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mk = |it: &mut Self, cname: &str, n: &str| -> Result<Value, PhpError> {
                    let v = it.instantiate("reflectionclassconstant", &[])?;
                    if let Value::Object(o) = &v {
                        o.borrow_mut()
                            .props
                            .insert("\0rc\0class".into(), cell(Value::str(cname)));
                        o.borrow_mut()
                            .props
                            .insert("\0rc\0prop".into(), cell(Value::str(n)));
                        o.borrow_mut()
                            .props
                            .insert("name".into(), cell(Value::str(n)));
                        o.borrow_mut()
                            .props
                            .insert("class".into(), cell(Value::str(cname)));
                    }
                    Ok(v)
                };
                if lname == "getreflectionconstants" {
                    let mut arr = PhpArray::default();
                    for (n, _) in self.all_const_decls(&cn) {
                        arr.push(mk(self, &cn, &n)?);
                    }
                    return Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))));
                }
                let pn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                if self.find_const_decl(&cn, &pn).is_none() {
                    return Ok(Some(Value::Bool(false)));
                }
                Ok(Some(mk(self, &cn, &pn)?))
            }
            "getvalue" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let pn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                match self.find_const_decl(&cn, &pn) {
                    Some((cd, f)) => {
                        // Const decls can reference self::X — bind the
                        // declaring scope during eval (Enum::MAPPING).
                        let old = self
                            .classes
                            .get(&cn.to_lowercase())
                            .map(|c| self.const_self.replace(c.clone()));
                        self.class_const_ctx += 1;
                        let r = self.eval_decl_const(&cd.value, &f);
                        self.class_const_ctx -= 1;
                        if let Some(o) = old {
                            self.const_self = o;
                        }
                        Ok(Some(r?))
                    }
                    None => Ok(Some(Value::Null)),
                }
            }
            "setvalue" => {
                // ReflectionProperty::setValue($object, $value) —
                // bypasses prop visibility without __set (bug72177).
                let pn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                let target = args.first().map(|c| c.borrow().clone());
                let val = args
                    .get(1)
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                if let Some(Value::Object(t)) = target {
                    let mut tb = t.borrow_mut();
                    if !tb.props.contains_key(&pn) && !tb.prop_order.iter().any(|k| k == &pn) {
                        tb.prop_order.push(pn.clone());
                    }
                    tb.props.insert(pn.clone(), cell(val));
                    tb.unset_props.remove(&pn);
                }
                Ok(Some(Value::Null))
            }
            "getdeclaringclass" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let v = self.instantiate("reflectionclass", &[])?;
                if let Value::Object(o) = &v {
                    o.borrow_mut()
                        .props
                        .insert("\0rc\0class".into(), cell(Value::str(cn.clone())));
                    o.borrow_mut()
                        .props
                        .insert("name".into(), cell(Value::str(cn)));
                }
                Ok(Some(v))
            }
            "isinitialized" => {
                let pn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?;
                let target = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                if let Value::Object(o) = target {
                    Ok(Some(Value::Bool(self.obj_prop_key(&o, &pn).is_some())))
                } else {
                    Ok(Some(Value::Bool(false)))
                }
            }
            _ => Ok(None),
        }
    }

    /// Declared class/interface/trait names for get_declared_*().
    pub fn declared_names(&self, kind: crate::ast::ClassKind) -> Vec<String> {
        let mut out = Vec::new();
        for n in &self.decl_order {
            let name = match kind {
                crate::ast::ClassKind::Trait => self.traits.get(n).map(|d| d.name.clone()),
                crate::ast::ClassKind::Interface => self.interfaces.get(n).map(|d| d.name.clone()),
                _ => self
                    .classes
                    .get(n)
                    .filter(|c| c.decl.kind == kind)
                    .map(|c| c.name().to_string()),
            };
            if let Some(nm) = name {
                out.push(nm);
            }
        }
        for (k, a) in &self.decl_aliases {
            if *k == kind {
                out.push(a.clone());
            }
        }
        out
    }

    /// class_alias($name, $alias): alias entries resolve like the
    /// original (classes, interfaces and traits alike).
    pub fn class_alias(&mut self, name: &str, alias: &str) -> Result<bool, PhpError> {
        // `class_alias($cls, 'int')` — the alias may not be a reserved
        // scalar type name (scalar_reserved*_class_alias).
        let short = alias
            .trim_start_matches('\\')
            .rsplit('\\')
            .next()
            .unwrap_or(alias)
            .to_lowercase();
        const RESERVED_ALS: &[&str] = &[
            "int", "float", "string", "bool", "void", "iterable", "object", "mixed", "never",
            "null", "false", "true",
        ];
        if RESERVED_ALS.contains(&short.as_str()) {
            return Err(PhpError::fatal(
                format!(
                    "Cannot use \"{}\" as a class alias as it is reserved",
                    short
                ),
                self.cur_line,
            ));
        }
        let alias_l = alias.trim_start_matches('\\').to_lowercase();
        let key = name.trim_start_matches('\\').to_lowercase();
        if let Some(c) = self.classes.get(&key).cloned() {
            self.classes.insert(alias_l.clone(), c);
            self.decl_aliases
                .push((crate::ast::ClassKind::Class, alias_l));
            return Ok(true);
        }
        if let Some(i) = self.interfaces.get(&key).cloned() {
            self.interfaces.insert(alias_l.clone(), i);
            self.decl_aliases
                .push((crate::ast::ClassKind::Interface, alias_l));
            return Ok(true);
        }
        if let Some(t) = self.traits.get(&key).cloned() {
            self.traits.insert(alias_l.clone(), t);
            self.decl_aliases
                .push((crate::ast::ClassKind::Trait, alias_l));
            return Ok(true);
        }
        self.warn(&format!("Class \"{}\" not found", name))?;
        Ok(false)
    }

    /// Does this object's class implement `iname` (transitively)?
    /// Used by serialize() for the Serializable C:-format branch.
    pub fn obj_implements(&mut self, o: &Rc<RefCell<PhpObject>>, iname: &str) -> bool {
        self.is_a(&o.borrow().class, iname)
    }

    /// Does `d` (a class decl) implement interface `iname`, directly or
    /// through the implements chain of interfaces it names?
    pub(in crate::interp) fn implements_iface(&self, d: &ClassDecl, iname: &str) -> bool {
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<String> = d.implements.clone();
        while let Some(i) = stack.pop() {
            let l = i.trim_start_matches('\\').to_lowercase();
            if l == iname {
                return true;
            }
            if !seen.insert(l.clone()) {
                continue;
            }
            if let Some(id) = self.interfaces.get(&l) {
                stack.extend(id.implements.iter().cloned());
            }
        }
        false
    }

    /// Zend backtrace text for debug_print_backtrace(): innermost-first
    /// frames, no `{main}` line (bug28213).
    pub fn format_backtrace(&self) -> String {
        let frames: Vec<TraceFrame> = self
            .call_trace
            .iter()
            .rev()
            .skip_while(|f| f.internal)
            .cloned()
            .collect();
        format_backtrace_frames(&frames)
    }

    /// debug_backtrace() array — same frames as format_backtrace().
    pub fn backtrace(&self) -> Vec<TraceFrame> {
        self.call_trace
            .iter()
            .rev()
            .skip_while(|f| f.internal)
            .cloned()
            .collect()
    }

    /// is-a check between two class-name strings.
    pub(in crate::interp) fn is_a_str(&mut self, a: &str, b: &str) -> bool {
        // Type-member checks during signature verification autoload the
        // compared classes — Zend verifies covariance with the real
        // hierarchy, so `C::m(): D` inside an autoloaded class sees `D`
        // even when it is declared later (abstract_method_9). A name
        // mid-link counts as resolvable without loading
        // (infinite_recursion — `class C extends Z implements C`).
        if !self.classes.contains_key(&a.to_lowercase())
            && !self.linking.iter().any(|c| c.name.eq_ignore_ascii_case(a))
        {
            // The check creates a delayed variance dependency either
            // way; a name mid-registration is still unlinked — the
            // decl answers it by name, but the obligation is recorded
            // so the check re-verifies after it links
            // (variance/loading_exception*).
            self.note_variance_obligation();
            if !self
                .declaring
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(a))
            {
                if let Err(e) = self.run_autoload(a) {
                    // An autoload failure is a compile-time fatal everywhere
                    // else; inside a signature check a missing class just
                    // means "not a subtype" — but the fatal itself must
                    // still reach the checking context (error3 cascade).
                    self.sig_fatal.get_or_insert(e);
                    self.pending_exception = None;
                }
            }
        }
        // Aliases canonicalize through the class table: `Bar` (an
        // alias of Foo) compares as `Foo` (typed_properties_084).
        let canon_b = self
            .classes
            .get(&b.trim_start_matches('\\').to_lowercase())
            .map(|c| c.name().to_string());
        let b = canon_b.as_deref().unwrap_or(b);
        match self.classes.get(&a.to_lowercase()).cloned() {
            Some(c) => self.is_a(&c, b),
            None => self.is_a_unresolved(a, b, 0),
        }
    }

    /// Ancestry check by NAME for a class not (yet) in `self.classes`:
    /// walks parent/implements names through `classes` and `linking`.
    pub(in crate::interp) fn is_a_unresolved(&mut self, a: &str, b: &str, depth: u8) -> bool {
        if a.trim_start_matches('\\').eq_ignore_ascii_case(b) {
            return true;
        }
        if depth > 16 {
            return false;
        }
        let d = self
            .classes
            .get(&a.to_lowercase())
            .map(|c| c.decl.clone())
            .or_else(|| {
                self.linking
                    .iter()
                    .rev()
                    .find(|d| d.name.eq_ignore_ascii_case(a))
                    .cloned()
            })
            .or_else(|| {
                self.declaring
                    .iter()
                    .rev()
                    .find(|d| d.name.eq_ignore_ascii_case(a))
                    .cloned()
            })
            .or_else(|| {
                self.interfaces
                    .get(&a.to_lowercase())
                    .cloned()
                    .or_else(|| self.traits.get(&a.to_lowercase()).cloned())
            });
        let Some(d) = d else {
            return false;
        };
        if let Some(p) = &d.parent {
            if self.is_a_unresolved(p, b, depth + 1) {
                return true;
            }
        }
        for i in &d.implements {
            if self.is_a_unresolved(i, b, depth + 1) {
                return true;
            }
        }
        false
    }

    pub fn find_method_in(
        &mut self,
        cls: &Rc<PhpClass>,
        name: &str,
    ) -> Option<(Rc<MethodDecl>, Rc<PhpClass>)> {
        let lname = name.to_lowercase();
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            if let Some(m) = c.decl.find_method(&lname) {
                return Some((m, c));
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        None
    }
}
