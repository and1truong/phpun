//! Static members + class consts: static prop reads/cells and lazy
//! init, `X::m()` calls and `static::` invocation, class constants
//! (`X::C`, `constant()`, `defined('X::C')`).

use super::*;

impl<'a> Interp<'a> {
    /// The nearest PropDecl for `pn` along the chain, honoring the same
    /// private-scope rules as `hooked_prop` (hooked or plain).
    /// Declared *static* prop lookup across the class chain
    /// (`Foo::$i = v` write checks).
    pub(in crate::interp) fn find_static_prop_decl(
        &self,
        cls: &Rc<PhpClass>,
        pn: &str,
    ) -> Option<(PropDecl, Rc<PhpClass>)> {
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            let priv_ok = std::rc::Rc::ptr_eq(&c, cls);
            if let Some(pd) = c.decl.props.iter().find(|p| {
                p.name == pn
                    && p.is_static
                    && (priv_ok || p.visibility != crate::ast::Visibility::Private)
            }) {
                return Some((pd.clone(), c.clone()));
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        None
    }

    pub(in crate::interp) fn static_prop_read(
        &mut self,
        class: &Expr,
        name: &PropName,
    ) -> Result<Value, PhpError> {
        let name = self.prop_name(name)?;
        let (cls, tname) = self.member_class_of(class)?;
        if let Some(t) = tname {
            self.deprecated(&format!(
                "Accessing static trait property {}::${} is deprecated, it should only be accessed on a class using the trait",
                t, name
            ))?;
        }
        self.statics_init(&cls);
        let v = cls.statics.borrow().get(&name).map(|c| c.borrow().clone());
        match v {
            Some(v) => Ok(v),
            None => {
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &name) {
                    if pd.ty.is_some() {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Typed static property {}::${} must not be accessed before initialization",
                                dcls.name(),
                                name
                            ),
                            0,
                        ));
                    }
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Access to undeclared static property {}::${}",
                        cls.name(),
                        name
                    ),
                    0,
                ))
            }
        }
    }

    pub(in crate::interp) fn static_prop_cell(
        &mut self,
        class: &Expr,
        name: &PropName,
    ) -> Result<Cell, PhpError> {
        let name = self.prop_name(name)?;
        self.static_prop_named(class, &name)
    }

    pub(in crate::interp) fn static_prop_named(
        &mut self,
        class: &Expr,
        name: &str,
    ) -> Result<Cell, PhpError> {
        let (cls, tname) = self.member_class_of(class)?;
        if let Some(t) = tname {
            self.deprecated(&format!(
                "Accessing static trait property {}::${} is deprecated, it should only be accessed on a class using the trait",
                t, name
            ))?;
        }
        self.statics_init(&cls);
        let found = cls.statics.borrow().get(name).cloned();
        match found {
            Some(c) => {
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, name) {
                    if let Some(tys) = &pd.ty {
                        let p = Rc::as_ptr(&c) as usize;
                        self.typed_slots.insert(
                            p,
                            (
                                c.clone(),
                                tys.clone(),
                                dcls.name().to_string(),
                                name.to_string(),
                            ),
                        );
                        self.slot_anchor.insert(
                            p,
                            SlotAnchor::Statics(dcls.name().to_string(), name.to_string()),
                        );
                    }
                }
                Ok(c)
            }
            None => {
                // Write/cell path materializes a declared static; an
                // undeclared one is an Error.
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, name) {
                    let c = cell(Value::Null);
                    cls.statics.borrow_mut().insert(name.to_string(), c.clone());
                    if let Some(tys) = &pd.ty {
                        self.last_fresh_cell = Some(Rc::as_ptr(&c) as usize);
                        let p = Rc::as_ptr(&c) as usize;
                        self.typed_slots.insert(
                            p,
                            (
                                c.clone(),
                                tys.clone(),
                                dcls.name().to_string(),
                                name.to_string(),
                            ),
                        );
                        self.slot_anchor.insert(
                            p,
                            SlotAnchor::Statics(dcls.name().to_string(), name.to_string()),
                        );
                    }
                    return Ok(c);
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Access to undeclared static property {}::${}",
                        cls.name(),
                        name
                    ),
                    0,
                ))
            }
        }
    }

    /// Lazily initialize static prop defaults.
    pub(in crate::interp) fn statics_init(&mut self, cls: &Rc<PhpClass>) {
        if *cls.statics_init.borrow() {
            return;
        }
        *cls.statics_init.borrow_mut() = true;
        // Inherited statics: PHP snapshots the parent's static-prop
        // values into the child's table at link time, so `static::$p`
        // on the child resolves parent defaults.
        if let Some(pname) = &cls.decl.parent {
            if let Some(p) = self.classes.get(&pname.to_lowercase()).cloned() {
                self.statics_init(&p);
                for (k, v) in p.statics.borrow().iter() {
                    cls.statics
                        .borrow_mut()
                        .entry(k.clone())
                        .or_insert_with(|| cell(v.borrow().clone()));
                }
            }
        }
        for p in &cls.decl.props {
            if !p.is_static {
                continue;
            }
            let mut default = match &p.default {
                Some(d) => {
                    let old = self.const_self.replace(cls.clone());
                    self.class_const_ctx += 1;
                    let v = self
                        .eval_decl_const(d, &cls.decl.file)
                        .unwrap_or(Value::Null);
                    self.class_const_ctx -= 1;
                    self.const_self = old;
                    v
                }
                None => Value::Null,
            };
            if p.ty.is_some() {
                if let Ok(d) = self.prop_typed_write_check(p, cls, default.clone()) {
                    default = d;
                }
            }
            // A typed static without a default stays *uninitialized*:
            // reads raise the uninit Error until first assignment.
            if p.ty.is_some() && p.default.is_none() {
                continue;
            }
            cls.statics
                .borrow_mut()
                .insert(p.name.clone(), cell(default));
        }
    }

    pub(in crate::interp) fn static_call(
        &mut self,
        class: &Expr,
        name: &str,
        args: &[Expr],
    ) -> Result<Value, PhpError> {
        let (cls, tname) = self.member_class_of(class)?;
        if let Some(t) = tname {
            self.deprecated(&format!(
                "Calling static trait method {}::{} is deprecated, it should only be called on a class using the trait",
                t, name
            ))?;
        }
        let params = self
            .find_method_in(&cls, name)
            .map(|m| m.0.decl.params.clone())
            .unwrap_or_default();
        let argvals = self.arg_cells(args, &params, &format!("{}()", name), false)?;
        // Only a syntactic class ref (self/parent/static/Foo) is a
        // forwarding call; `$x::m()` is not (bug48533).
        let fwd = matches!(class, Expr::Const(_) | Expr::Str(_) | Expr::AnonClass(_));
        // Forwarding calls (self::/parent::/static::) preserve the
        // current late-static-binding class instead of resetting it to
        // the resolved target: `parent::__construct()` on a subclass
        // still sees the subclass via `static::` inside the parent ctor.
        let called = match class {
            Expr::Const(n) | Expr::Str(n)
                if matches!(
                    n.trim_start_matches('\\').to_lowercase().as_str(),
                    "self" | "parent" | "static"
                ) =>
            {
                self.stack.last().and_then(|f| {
                    f.called_class
                        .clone()
                        .or_else(|| f.this_obj.as_ref().map(|o| o.borrow().class.clone()))
                })
            }
            _ => None,
        };
        self.static_invoke_vis(cls, name, argvals, called, fwd)
    }

    pub(in crate::interp) fn static_invoke(
        &mut self,
        cls: Rc<PhpClass>,
        name: &str,
        args: CallArgs,
        called_class: Option<Rc<PhpClass>>,
        fwd: bool,
    ) -> Result<Value, PhpError> {
        // Closure::{bind,fromCallable}: native callable rebinding.
        if cls.name().eq_ignore_ascii_case("closure") {
            let lname = name.to_lowercase();
            match lname.as_str() {
                "getcurrent" => {
                    // Current frame must itself be executing a closure
                    // body (closure_get_current).
                    return match self.stack.last().and_then(|f| f.closure_rc.clone()) {
                        Some(rc) => Ok(Value::Callable(rc)),
                        None => self.fail(PhpError::uncaught(
                            "Error",
                            "Current function is not a closure",
                            0,
                        )),
                    };
                }
                "bind" | "bindto" => {
                    // `Closure::bind($closure, $newThis, $newScope = ?)`.
                    let c = args.cells.first().map(|c| c.borrow().clone());
                    let this = args.cells.get(1).map(|c| c.borrow().clone());
                    let scope = args.cells.get(2).map(|c| c.borrow().clone());
                    let Value::Callable(cb) = c.unwrap_or(Value::Null) else {
                        return Ok(Value::Null);
                    };
                    let new_this = match &this {
                        None | Some(Value::Null) => None,
                        Some(Value::Object(o)) => Some(o.clone()),
                        Some(v) => {
                            let e = self.exception(
                                "TypeError",
                                &format!(
                                    "Closure::bind(): Argument #2 ($newThis) must be of type ?object, {} given",
                                    v.gettype()
                                ),
                            );
                            return Err(self.throw(e));
                        }
                    };
                    let scope_arg = match &scope {
                        None => None,
                        Some(v @ (Value::Null | Value::Object(_) | Value::Str(_))) => {
                            Some(v.clone())
                        }
                        Some(v) => {
                            let e = self.exception(
                                "TypeError",
                                &format!(
                                    "Closure::bind(): Argument #3 ($newScope) must be of type object|string|null, {} given",
                                    v.gettype()
                                ),
                            );
                            return Err(self.throw(e));
                        }
                    };
                    match self.rebind_closure(&cb, new_this, scope_arg)? {
                        Some(nc) => return Ok(Value::Callable(Rc::new(nc))),
                        None => return Ok(Value::Null),
                    }
                }
                "fromcallable" => {
                    let v = args
                        .cells
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    match self.callable_to_closure(&v) {
                        Ok(c) => return Ok(c),
                        Err(fail) => {
                            // Zend appends the reason:
                            // "Failed to create closure from callable:
                            // non-static method A::m() cannot be called
                            // statically" (from_callable_non_static).
                            let reason = if !fail.message.is_empty() {
                                fail.message
                                    .replacen("Non-static method", "non-static method", 1)
                            } else {
                                self.pending_exception
                                    .as_ref()
                                    .and_then(|e| match e {
                                        Value::Object(o) => o
                                            .borrow()
                                            .props
                                            .get("message")
                                            .map(|c| c.borrow().to_php_string()),
                                        _ => None,
                                    })
                                    .unwrap_or_default()
                            };
                            let e = self.exception(
                                "TypeError",
                                &format!("Failed to create closure from callable: {}", reason),
                            );
                            self.pending_exception = Some(e);
                            return Err(PhpError {
                                trace: None,
                                thrown_line: None,
                                display_msg: None,
                                kind: ErrorKind::Throw,
                                message: "fromCallable".into(),
                                line: 0,
                            });
                        }
                    }
                }
                _ => {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Call to undefined method Closure::{}()", name),
                        0,
                    ));
                }
            }
        }
        // Throwable methods are instance-only; look up incl. parents.
        match self.find_method_in(&cls, name) {
            Some((m, dc)) => {
                if m.is_abstract {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot call abstract method {}::{}()", dc.name(), name),
                        0,
                    ));
                }
                // Forwarding call: a non-static method invoked
                // statically still receives $this when the caller's
                // $this is an instance of the callee's class
                // (bug21961) — but only via a syntactic class ref;
                // `$obj::m()` is not a forwarding call (bug48533).
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
                // `parent::__construct()` on a throwable resolves to a
                // builtin stub — run the native impl (message/code
                // props, exception internals) exactly like the object
                // dispatch in method_invoke (PHPUnit's Exception chain
                // ctor-chains here).
                if m.decl.body.is_empty() && m.decl.line == 0 {
                    if let Some(o) = &this_obj {
                        let is_throwable = {
                            let ob = o.borrow();
                            matches!(ob.internal, Some(ObjectInternal::Exception { .. }))
                                || self.is_throwable_name(&ob.class.decl.name)
                        };
                        if is_throwable {
                            if let Some(v) = self.throwable_method(o, name, &args.cells) {
                                return Ok(v);
                            }
                        }
                    }
                }
                if !m.is_static && this_obj.is_none() {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!(
                            "Non-static method {}::{}() cannot be called statically",
                            dc.name(),
                            m.decl.name
                        ),
                        0,
                    ));
                }
                self.pending_decl_class = Some(dc.clone());
                self.pending_called_class = Some(called_class.clone().unwrap_or(cls.clone()));
                let r = self.invoke_fn(&Rc::new(m.decl.clone()), args, this_obj, Some(dc));
                self.pending_decl_class = None;
                self.pending_called_class = None;
                r
            }
            None => {
                // A missing __construct never reaches magic —
                // `Foo::__construct()` is "Cannot call constructor"
                // (call_static_006). __destruct etc. dispatch normally.
                if name.eq_ignore_ascii_case("__construct") {
                    return self.fail(PhpError::uncaught("Error", "Cannot call constructor", 0));
                }
                let mut arr = PhpArray::new();
                for a in &args.cells {
                    arr.push(a.borrow().clone());
                }
                for (n, a, ..) in &args.named {
                    arr.set(ArrKey::Str(Rc::from(n.as_str())), a.borrow().clone());
                }
                let magic_args = CallArgs::positional(vec![
                    cell(Value::str(name)),
                    cell(Value::Array(Rc::new(RefCell::new(arr)))),
                ]);
                // Object context prefers __call over __callStatic when
                // the caller's $this is an instance of the callee —
                // `self::x()` inside a method acts as an instance call
                // (call_static_003/007, bug45186).
                let this_obj = self
                    .stack
                    .last()
                    .and_then(|f| f.this_obj.clone())
                    .filter(|o| {
                        let cname = o.borrow().class.name().to_string();
                        self.is_a_str(&cname, cls.name())
                    });
                if let Some(o) = this_obj {
                    if self.find_method_in(&cls, "__call").is_some() {
                        return self.method_invoke(o, "__call", magic_args);
                    }
                }
                if let Some((m, dc)) = self.find_method_in(&cls, "__callstatic") {
                    self.pending_decl_class = Some(dc.clone());
                    self.pending_called_class = Some(called_class.clone().unwrap_or(cls.clone()));
                    let r = self.invoke_fn(&Rc::new(m.decl.clone()), magic_args, None, Some(dc));
                    self.pending_decl_class = None;
                    self.pending_called_class = None;
                    return r;
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!("Call to undefined method {}::{}()", cls.name(), name),
                    0,
                ))
            }
        }
    }

    /// Locate a class/interface/trait const decl by name — walks the
    /// class chain, used traits' merged consts and implemented
    /// interfaces (reflection APIs see trait consts on both sides).
    pub(in crate::interp) fn find_const_decl(
        &mut self,
        cname: &str,
        name: &str,
    ) -> Option<(crate::ast::ConstDecl, String)> {
        let key = cname.trim_start_matches('\\').to_lowercase();
        if let Some(td) = self.traits.get(&key) {
            for cd in &td.consts {
                if cd.name == name {
                    return Some((cd.clone(), td.file.clone()));
                }
            }
            return None;
        }
        if let Some(id) = self.interfaces.get(&key) {
            for cd in &id.consts {
                if cd.name == name {
                    return Some((cd.clone(), id.file.clone()));
                }
            }
            return None;
        }
        let mut cur = self.classes.get(&key).cloned();
        let mut ifaces: Vec<String> = Vec::new();
        while let Some(c) = cur {
            for cd in &c.decl.consts {
                if cd.name == name {
                    return Some((cd.clone(), c.decl.file.clone()));
                }
            }
            ifaces.extend(c.decl.implements.iter().cloned());
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        for iname in ifaces {
            if let Some(id) = self.interfaces.get(&iname.to_lowercase()) {
                for cd in &id.consts {
                    if cd.name == name {
                        return Some((cd.clone(), id.file.clone()));
                    }
                }
            }
        }
        None
    }

    /// All (name, (decl, file)) consts visible on `cname` — class chain
    /// + interfaces; trait members appear via the class's merged decl.
    pub(in crate::interp) fn all_const_decls(
        &mut self,
        cname: &str,
    ) -> Vec<(String, (crate::ast::ConstDecl, String))> {
        let mut out: Vec<(String, (crate::ast::ConstDecl, String))> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let key = cname.trim_start_matches('\\').to_lowercase();
        if let Some(td) = self.traits.get(&key).cloned() {
            for cd in &td.consts {
                if seen.insert(cd.name.clone()) {
                    out.push((cd.name.clone(), (cd.clone(), td.file.clone())));
                }
            }
            return out;
        }
        if let Some(id) = self.interfaces.get(&key).cloned() {
            for cd in &id.consts {
                if seen.insert(cd.name.clone()) {
                    out.push((cd.name.clone(), (cd.clone(), id.file.clone())));
                }
            }
            return out;
        }
        let mut cur = self.classes.get(&key).cloned();
        let mut ifaces: Vec<String> = Vec::new();
        while let Some(c) = cur {
            for cd in &c.decl.consts {
                if seen.insert(cd.name.clone()) {
                    out.push((cd.name.clone(), (cd.clone(), c.decl.file.clone())));
                }
            }
            ifaces.extend(c.decl.implements.iter().cloned());
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        for iname in ifaces {
            if let Some(id) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                for cd in &id.consts {
                    if seen.insert(cd.name.clone()) {
                        out.push((cd.name.clone(), (cd.clone(), id.file.clone())));
                    }
                }
            }
        }
        out
    }

    pub(in crate::interp) fn class_of(&mut self, e: &Expr) -> Result<Rc<PhpClass>, PhpError> {
        let name = self.class_name_of(e)?;
        if !self.classes.contains_key(&name.to_lowercase()) {
            self.run_autoload(&name)?;
        }
        match self.classes.get(&name.to_lowercase()) {
            Some(c) => Ok(c.clone()),
            None => self.fail(PhpError::uncaught(
                "Error",
                format!("Class \"{}\" not found", name),
                0,
            )),
        }
    }

    pub(in crate::interp) fn class_const(
        &mut self,
        class: &Expr,
        name: &str,
    ) -> Result<Value, PhpError> {
        let cname = self.class_name_of(class)?;
        self.class_const_named(&cname, name)
    }

    /// `Cls::CONST` lookup by plain class-name string (shared with the
    /// constant() builtin so `constant('T::X')` honours the trait-const
    /// rule — constant_018).
    pub fn class_const_named(&mut self, cname: &str, name: &str) -> Result<Value, PhpError> {
        let cname = cname.to_string();
        if name == "class" {
            return Ok(Value::str(cname));
        }
        // `X::CONST` on an unloaded class runs the autoloaders (real
        // psr-4 code hits this constantly — e.g. `Language::ENGLISH`).
        let resolved = self.resolve_class(&cname).unwrap_or_else(|| cname.clone());
        let ckey = resolved.to_lowercase();
        if !self.classes.contains_key(&ckey)
            && !self.traits.contains_key(&ckey)
            && !self.interfaces.contains_key(&ckey)
        {
            // An autoloader's throwable propagates through the `::`
            // lookup (PHP fatals the same way); on a clean miss it
            // leaves no exception behind.
            self.run_autoload(&resolved)?;
        }
        if let Some(td) = self.traits.get(&ckey).cloned() {
            return self.fail(PhpError::uncaught(
                "Error",
                format!(
                    "Cannot access trait constant {}::{} directly",
                    td.name, name
                ),
                0,
            ));
        }
        if let Some(iface) = self.interfaces.get(&ckey).cloned() {
            // Const on an interface (e.g. `FastRoute\Dispatcher::FOUND`):
            // walk it and its extended interfaces.
            let mut seen = std::collections::HashSet::new();
            let mut stack = vec![iface];
            while let Some(c) = stack.pop() {
                if !seen.insert(c.name.to_lowercase()) {
                    continue;
                }
                for cd in &c.consts {
                    if cd.name == name {
                        self.class_const_ctx += 1;
                        let r = self.eval_decl_const(&cd.value, &c.file);
                        self.class_const_ctx -= 1;
                        return match r {
                            Ok(v) => self.const_apply_ty(cd, &c.name, v),
                            Err(e) => Err(e),
                        };
                    }
                }
                for i in &c.implements {
                    if let Some(f) = self.interfaces.get(&i.to_lowercase()).cloned() {
                        stack.push(f);
                    }
                }
                if let Some(p) = &c.parent {
                    if let Some(f) = self.interfaces.get(&p.to_lowercase()).cloned() {
                        stack.push(f);
                    }
                }
            }
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Undefined constant {}", name),
                0,
            ));
        }
        let cls = match self.classes.get(&ckey) {
            Some(c) => c.clone(),
            None => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Class \"{}\" not found", cname),
                    0,
                ))
            }
        };
        // Walk chain for the const (class first, then implemented
        // interfaces transitively — interface consts are inherited).
        let mut cur = Some(cls.clone());
        let mut ifaces: Vec<String> = Vec::new();
        while let Some(c) = cur {
            for cd in &c.decl.consts {
                if cd.name == name {
                    if cd.enum_case {
                        return self.enum_case_value(&c.decl.name, &cd.name, cd);
                    }
                    let old = self.const_self.replace(c.clone());
                    self.class_const_ctx += 1;
                    let r = self.eval_decl_const(&cd.value, &c.decl.file);
                    self.class_const_ctx -= 1;
                    self.const_self = old;
                    return match r {
                        Ok(v) => self.const_apply_ty(cd, &c.decl.name, v),
                        Err(e) => Err(e),
                    };
                }
            }
            ifaces.extend(c.decl.implements.iter().cloned());
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        let mut seen = std::collections::HashSet::new();
        let mut queue: Vec<String> = ifaces;
        while let Some(iname) = queue.pop() {
            if !seen.insert(iname.to_lowercase()) {
                continue;
            }
            if let Some(c) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                for cd in &c.consts {
                    if cd.name == name {
                        let old = self.const_self.replace(cls.clone());
                        self.class_const_ctx += 1;
                        let r = self
                            .eval_decl_const(&cd.value, &c.file)
                            .and_then(|v| self.const_apply_ty(cd, &c.name, v));
                        self.class_const_ctx -= 1;
                        self.const_self = old;
                        return r;
                    }
                }
                queue.extend(c.implements.iter().cloned());
                if let Some(p) = &c.parent {
                    queue.push(p.clone());
                }
            }
        }
        self.fail(PhpError::uncaught(
            "Error",
            format!("Undefined constant {}", name),
            0,
        ))
    }

    /// `defined('Cls::CONST')` — declaration check only (no value
    /// eval), mirroring class_const_named's walk. The class is
    /// resolved (and autoloaded) first.
    pub fn class_const_defined(&mut self, cname: &str, name: &str) -> bool {
        if name == "class" {
            return self.resolve_class(cname).is_some()
                || self
                    .classes
                    .contains_key(&cname.trim_start_matches('\\').to_lowercase());
        }
        let ckey = self
            .resolve_class(cname)
            .unwrap_or_else(|| cname.to_string())
            .to_lowercase();
        if let Some(iface) = self.interfaces.get(&ckey).cloned() {
            let mut seen = std::collections::HashSet::new();
            let mut stack = vec![iface];
            while let Some(c) = stack.pop() {
                if !seen.insert(c.name.to_lowercase()) {
                    continue;
                }
                if c.consts.iter().any(|cd| cd.name == name) {
                    return true;
                }
                for i in &c.implements {
                    if let Some(f) = self.interfaces.get(&i.to_lowercase()).cloned() {
                        stack.push(f);
                    }
                }
                if let Some(p) = &c.parent {
                    if let Some(f) = self.interfaces.get(&p.to_lowercase()).cloned() {
                        stack.push(f);
                    }
                }
            }
            return false;
        }
        let Some(cls) = self.classes.get(&ckey).cloned() else {
            return false;
        };
        let mut cur = Some(cls);
        let mut ifaces: Vec<String> = Vec::new();
        while let Some(c) = cur {
            if c.decl.consts.iter().any(|cd| cd.name == name) {
                return true;
            }
            ifaces.extend(c.decl.implements.iter().cloned());
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        let mut seen = std::collections::HashSet::new();
        while let Some(iname) = ifaces.pop() {
            if !seen.insert(iname.to_lowercase()) {
                continue;
            }
            if let Some(c) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                if c.consts.iter().any(|cd| cd.name == name) {
                    return true;
                }
                ifaces.extend(c.implements.iter().cloned());
                if let Some(p) = &c.parent {
                    ifaces.push(p.clone());
                }
            }
        }
        false
    }
}
