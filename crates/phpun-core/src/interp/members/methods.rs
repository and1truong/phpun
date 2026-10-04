//! Method dispatch + magic: method calls, `__call`/`__get` magic
//! routing, visibility errors and prototypes, throwable plumbing,
//! native `ArrayIterator`/callable dispatch helpers.

use super::*;

impl<'a> Interp<'a> {
    /// Native bodies for the ArrayIterator stub. Iteration state lives in
    /// the `ArrayIter` object internal; unknown methods return None so
    /// the generic dispatch can report `Call to undefined method`.
    pub(in crate::interp) fn array_iter_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        args: &CallArgs,
    ) -> Result<Option<Value>, PhpError> {
        let lname = name.to_lowercase();
        let mk_arr = |ob: &mut PhpObject, a: Rc<RefCell<PhpArray>>, flags: i64| {
            ob.internal = Some(ObjectInternal::ArrayIter {
                arr: a,
                pos: 0,
                flags,
            });
            Value::Null
        };
        match lname.as_str() {
            "__construct" => {
                let flags = args.cells.get(1).map(|c| c.borrow().to_int()).unwrap_or(0);
                let mut ob = obj.borrow_mut();
                let a = match args.cells.first().map(|c| c.borrow().clone()) {
                    Some(Value::Array(a)) => {
                        let mut copy = PhpArray::new();
                        for (k, c) in &a.borrow().entries {
                            copy.set(k.clone(), c.borrow().clone());
                        }
                        Rc::new(RefCell::new(copy))
                    }
                    // Objects iterate their prop cells BY REFERENCE —
                    // writes through $v update the prop (typed gate
                    // still applies); deprecated since 8.5
                    // (typed_properties_113/114/115).
                    Some(Value::Object(o)) => {
                        drop(ob);
                        self.deprecated(
                            "ArrayIterator::__construct(): Using an object as a backing array for ArrayIterator is deprecated, as it allows violating class constraints and invariants",
                        )?;
                        ob = obj.borrow_mut();
                        let mut copy = PhpArray::new();
                        copy.is_ref = true;
                        let pairs: Vec<(String, Cell)> = o
                            .borrow()
                            .props
                            .iter()
                            .map(|(k, c)| (k.clone(), c.clone()))
                            .collect();
                        for (k, c) in pairs {
                            let pn = k.rsplit('\0').next().unwrap_or(&k).to_string();
                            if let Some((pd, dcls)) = self.decl_prop(&o, &pn) {
                                let p = Rc::as_ptr(&c) as usize;
                                if let Some(tys) = &pd.ty {
                                    // Writes through the shared cell
                                    // still hit the typed gate.
                                    self.typed_slots.insert(
                                        p,
                                        (
                                            c.clone(),
                                            tys.clone(),
                                            dcls.name().to_string(),
                                            pn.clone(),
                                        ),
                                    );
                                    self.slot_anchor
                                        .insert(p, SlotAnchor::Obj(Rc::downgrade(&o), k.clone()));
                                    self.slot_owners.entry(p).or_default().push((
                                        tys.clone(),
                                        dcls.name().to_string(),
                                        pn.clone(),
                                        SlotAnchor::Obj(Rc::downgrade(&o), k.clone()),
                                    ));
                                }
                                // Remember readonly cells — by-ref
                                // acquisition must fail (115).
                                if pd.readonly {
                                    self.readonly_cells
                                        .insert(p, (dcls.name().to_string(), pn.clone()));
                                }
                            }
                            copy.bind_cell(ArrKey::Str(Rc::from(k.as_str())), c);
                        }
                        Rc::new(RefCell::new(copy))
                    }
                    _ => Rc::new(RefCell::new(PhpArray::new())),
                };
                Ok(Some(mk_arr(&mut ob, a, flags)))
            }
            _ => {
                // All remaining methods need initialized state.
                let (arr, pos) = {
                    let ob = obj.borrow();
                    match &ob.internal {
                        Some(ObjectInternal::ArrayIter { arr, pos, .. }) => (arr.clone(), *pos),
                        _ => return Ok(None),
                    }
                };
                let v = match lname.as_str() {
                    "rewind" => {
                        if let Some(ObjectInternal::ArrayIter { pos: p, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *p = 0;
                        }
                        Value::Null
                    }
                    "valid" => Value::Bool(pos < arr.borrow().entries.len()),
                    "current" => arr
                        .borrow()
                        .entries
                        .get(pos)
                        .map(|(_, c)| c.borrow().clone())
                        .unwrap_or(Value::Bool(false)),
                    "key" => arr
                        .borrow()
                        .entries
                        .get(pos)
                        .map(|(k, _)| key_value(k))
                        .unwrap_or(Value::Null),
                    "next" => {
                        if let Some(ObjectInternal::ArrayIter { pos: p, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *p += 1;
                        }
                        Value::Null
                    }
                    "seek" => {
                        let i = args.cells.first().map(|c| c.borrow().to_int()).unwrap_or(0);
                        let len = arr.borrow().entries.len() as i64;
                        if i < 0 || i >= len.max(1) && !(i == 0 && len == 0) {
                            return self.fail(PhpError::uncaught(
                                "OutOfBoundsException",
                                format!("Seek position {} is out of range", i),
                                0,
                            ));
                        }
                        if let Some(ObjectInternal::ArrayIter { pos: p, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *p = i as usize;
                        }
                        Value::Null
                    }
                    "count" => Value::Int(arr.borrow().entries.len() as i64),
                    "getarraycopy" => Value::Array(arr.clone()),
                    "offsetget" => {
                        let k = args
                            .cells
                            .first()
                            .map(|c| to_key(&c.borrow()))
                            .unwrap_or(ArrKey::Int(0));
                        match arr.borrow().get(&k) {
                            Some(v) => v,
                            None => {
                                let kn = key_value(&k).to_php_string();
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
                        Value::Null
                    }
                    "getflags" => Value::Int({
                        let ob = obj.borrow();
                        match &ob.internal {
                            Some(ObjectInternal::ArrayIter { flags, .. }) => *flags,
                            _ => 0,
                        }
                    }),
                    "setflags" => {
                        let f = args.cells.first().map(|c| c.borrow().to_int()).unwrap_or(0);
                        if let Some(ObjectInternal::ArrayIter { flags, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *flags = f;
                        }
                        Value::Null
                    }
                    "asort" | "ksort" => {
                        let mut a = arr.borrow_mut();
                        if lname == "asort" {
                            a.entries
                                .sort_by(|(_, x), (_, y)| compare(&x.borrow(), &y.borrow()));
                        } else {
                            a.entries.sort_by(|(x, _), (y, _)| match (x, y) {
                                (ArrKey::Int(a), ArrKey::Int(b)) => a.cmp(b),
                                _ => compare(&key_value(x), &key_value(y)),
                            });
                        }
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
                    _ => return Ok(None),
                };
                Ok(Some(v))
            }
        }
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
    pub(in crate::interp) fn invoke_method(
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
        if m.decl.body.is_empty()
            && m.decl.line == 0
            && self.is_a_str(obj.borrow().class.name(), "splfileinfo")
        {
            if let Some(v) = self.spl_method(&obj, &m.decl.name, &args)? {
                return Ok(v);
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
    pub(in crate::interp) fn magic_args_array(&self, args: &CallArgs) -> PhpArray {
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

    pub(in crate::interp) fn call_via_magic(
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
    pub(in crate::interp) fn scope_private_method(
        &mut self,
        name: &str,
    ) -> Option<(Rc<MethodDecl>, Rc<PhpClass>)> {
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
    pub(in crate::interp) fn method_vis_error(
        &mut self,
        m: &MethodDecl,
        dc: &Rc<PhpClass>,
    ) -> PhpError {
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
        // ArrayIterator: native iteration state on the object internal.
        if cls.name().eq_ignore_ascii_case("arrayiterator") {
            if let Some(v) = self.array_iter_method(&obj, name, &args)? {
                return Ok(v);
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
