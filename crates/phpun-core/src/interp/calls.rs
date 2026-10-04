//! Call dispatch: arg binding, named args, first-class callables,
//! closure rebind, `bind_and_run`/`invoke_fn` frames and the signature
//! checks (params/returns/`#[ReturnTypeWillChange]`) around them.

use super::util::*;
use super::*;

impl<'a> Interp<'a> {
    // ----- calls -----

    /// Param decls a callable Value will bind against — needed so
    /// arg_cells aliases by-ref params (first_class_callable_refs).
    fn callable_params(&mut self, v: &Value) -> Vec<Param> {
        match v {
            Value::Callable(c) => match &c.kind {
                CallableKind::Closure(d) => d.params.clone(),
                CallableKind::Named(n) => self
                    .functions
                    .get(&n.trim_start_matches('\\').to_lowercase())
                    .map(|d| d.params.clone())
                    .unwrap_or_default(),
                CallableKind::Method { obj, class, name } => {
                    let cls = match obj {
                        Some(o) => Some(o.borrow().class.clone()),
                        None => class.clone(),
                    };
                    cls.and_then(|c| self.find_method_in(&c, name))
                        .map(|(m, _)| m.decl.params.clone())
                        .unwrap_or_default()
                }
            },
            // `$obj()` invokes __invoke — the params are that
            // method's (by-ref flags included, closure_014).
            Value::Object(o) => self
                .find_method_in(&o.borrow().class.clone(), "__invoke")
                .map(|(m, _)| m.decl.params.clone())
                .unwrap_or_default(),
            _ => vec![],
        }
    }

    pub(in crate::interp) fn call(
        &mut self,
        name: &Expr,
        args: &[Expr],
    ) -> Result<Value, PhpError> {
        // Resolve callee name/value.
        let fname = match name {
            Expr::Str(s) => s.to_string(),
            Expr::Var(_) | Expr::VarVar(_) => {
                let v = self.eval(name)?;
                match v {
                    Value::Callable(_) | Value::Object(_) => {
                        // $closure() / $obj->__invoke()
                        let params = self.callable_params(&v);
                        let ctx = format!("{}()", self.callable_ctx_name(&v));
                        let vals = self.arg_cells(args, &params, &ctx, false)?;
                        return self.call_value(&v, vals);
                    }
                    Value::Array(_) => {
                        // `[obj,'m']` / `[$closure,'__invoke']` array
                        // callables (bug78689).
                        let c = self.fcc_val(&v)?;
                        let params = self.callable_params(&c);
                        let ctx = format!("{}()", self.callable_ctx_name(&c));
                        let vals = self.arg_cells(args, &params, &ctx, false)?;
                        return self.call_value(&c, vals);
                    }
                    _ => self.conv_str(&v).unwrap_or_default(),
                }
            }

            Expr::StaticProp { class, name } => {
                // `C::$var()` — dynamic static method call.
                let cls = self.class_of(class)?;
                let mn = Self::nul_trunc(&self.prop_name(name)?);
                let params = self
                    .find_method_in(&cls, &mn)
                    .map(|m| m.0.decl.params.clone())
                    .unwrap_or_default();
                let vals = self.arg_cells(args, &params, &format!("{}()", mn), false)?;
                let fwd = matches!(&**class, Expr::Const(_) | Expr::Str(_) | Expr::AnonClass(_));
                return self.static_invoke_vis(cls, &mn, vals, None, fwd);
            }
            Expr::Prop { .. } | Expr::MethodCall { .. } | Expr::Index { .. } => {
                let v = self.eval(name)?;
                let params = self.callable_params(&v);
                let ctx = format!("{}()", self.callable_ctx_name(&v));
                let vals = self.arg_cells(args, &params, &ctx, false)?;
                return self.call_value(&v, vals);
            }
            _ => {
                let v = self.eval(name)?;
                match v {
                    // `(expr)()` — IIFE on a closure/invokable value.
                    Value::Callable(_) | Value::Object(_) => {
                        let params = self.callable_params(&v);
                        let ctx = format!("{}()", self.callable_ctx_name(&v));
                        let vals = self.arg_cells(args, &params, &ctx, false)?;
                        return self.call_value(&v, vals);
                    }
                    Value::Array(_) => {
                        let c = self.fcc_val(&v)?;
                        let params = self.callable_params(&c);
                        let ctx = format!("{}()", self.callable_ctx_name(&c));
                        let vals = self.arg_cells(args, &params, &ctx, false)?;
                        return self.call_value(&c, vals);
                    }
                    _ => self.conv_str(&v).unwrap_or_default(),
                }
            }
        };
        self.call_named(&fname, args)
    }

    /// Evaluate args into cells (by-ref params alias caller storage).
    /// `named` params collected as (name, cell) too.
    pub(in crate::interp) fn arg_cells(
        &mut self,
        args: &[Expr],
        decl: &[Param],
        ctx: &str,
        internal: bool,
    ) -> Result<CallArgs, PhpError> {
        let mut out = CallArgs::empty();
        // Position of the *next positional* arg for by-ref lookup — named
        // args don't advance it (they bind by name at call time).
        let mut pos = 0usize;
        let mut seen_named = false;
        for a in args {
            let (name, expr): (Option<String>, &Expr) = match a {
                Expr::Binary {
                    op: "named", l, r, ..
                } => {
                    let n = match l.as_ref() {
                        Expr::Str(s) => s.clone(),
                        _ => match self.eval(l)? {
                            Value::Str(s) => crate::value::lossy(&s).into_owned(),
                            v => v.to_php_string(),
                        },
                    };
                    (Some(n), r.as_ref())
                }
                _ => (None, a),
            };
            if let Expr::Unpack(e) = expr {
                // `...$arr`: int-keyed entries become positionals (in
                // iteration order), string-keyed become named args
                // (named_params/unpack*). Entries from a Traversable are
                // fresh cells — a by-ref param gets the unpack warning
                // and a by-value bind (named_params/unpack's test2).
                if seen_named {
                    // The parser rejects `...` after named at compile
                    // time; unreachable for normal calls.
                    return self.fail(PhpError::fatal(
                        "Cannot use argument unpacking after named arguments",
                        0,
                    ));
                }
                let mut v = self.eval(e)?;
                if let (Expr::Var(_), Value::Array(a)) = (e.as_ref(), &v) {
                    // `...$ary` may hand out element cells for by-ref
                    // binding — Zend cow-separates $ary first so other
                    // variables sharing the array keep the old cells
                    // (named_params/unpack's $ary2 stays 0).
                    if Rc::strong_count(a) > 1 {
                        let mut na = a.borrow().clone();
                        for (_, c) in na.entries.iter_mut() {
                            let v = c.borrow().clone();
                            *c = cell(v);
                        }
                        let nv = Value::Array(Rc::new(RefCell::new(na)));
                        if let Ok(c) = self.eval_cell(e) {
                            *c.borrow_mut() = nv.clone();
                        }
                        v = nv;
                    }
                }
                let trav = matches!(&v, Value::Object(_));
                let mut unpack_named = false;
                for (k, c) in self.unpack_items(&v)? {
                    match k {
                        Some(n) => {
                            seen_named = true;
                            unpack_named = true;
                            out.named.push((n.to_string(), c, true, trav));
                        }
                        None if unpack_named => {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                "Cannot use positional argument after named argument during unpacking",
                                0,
                            ));
                        }
                        None => {
                            if trav {
                                out.trav_cells.push(out.cells.len());
                            }
                            out.cells.push(c);
                            pos += 1;
                        }
                    }
                }
                continue;
            }
            let by_ref = match &name {
                // Unknown named args land in the variadic — a by-ref
                // `&...$refs` variadic binds them as cells
                // (named_params/variadic's test2 increments $x/$y).
                Some(n) => decl
                    .iter()
                    .find(|p| !p.variadic && p.name == *n)
                    .map(|p| p.by_ref)
                    .unwrap_or_else(|| decl.iter().any(|p| p.variadic && p.by_ref)),
                None => decl
                    .get(pos)
                    .map(|p| p.by_ref)
                    .unwrap_or_else(|| decl.iter().any(|p| p.variadic && p.by_ref)),
            };
            if by_ref {
                match expr {
                    Expr::Var(_) | Expr::Index { .. } | Expr::Prop { .. } | Expr::VarVar(_) => {
                        match self.eval_cell(expr) {
                            Ok(c) => {
                                if let Some(n) = name {
                                    out.named.push((n, c, true, false));
                                    seen_named = true;
                                } else {
                                    out.cells.push(c);
                                    pos += 1;
                                }
                            }
                            Err(_) => {
                                return self.fail(PhpError::fatal(
                                    "Only variables should be passed by reference",
                                    0,
                                ))
                            }
                        }
                    }
                    Expr::Assign {
                        op: "=&", target, ..
                    } => {
                        // `f($x =& v)` binds the target by reference
                        // (passByReference_010); plain `=` throws Error below.
                        self.eval(expr)?;
                        let c = self.eval_cell(target)?;
                        if let Some(n) = name {
                            out.named.push((n, c, true, false));
                            seen_named = true;
                        } else {
                            out.cells.push(c);
                            pos += 1;
                        }
                    }
                    Expr::Call { .. } | Expr::MethodCall { .. } | Expr::StaticCall { .. } => {
                        // `f(g())`: binds only when g() returns by reference,
                        // otherwise a notice and pass by value (passByReference_004/007).
                        let (c, was_ref) = self.eval_call_cell(expr)?;
                        if !was_ref {
                            self.notice("Only variables should be passed by reference")?;
                        }
                        if let Some(n) = name {
                            out.named.push((n, c, was_ref, false));
                            seen_named = true;
                        } else {
                            out.cells.push(c);
                            pos += 1;
                        }
                    }
                    Expr::New { .. } => {
                        // `new` produces a reference-able object → PHP
                        // warns "Only variables should be passed by
                        // reference" and binds a temp (internal and
                        // user functions alike).
                        self.notice("Only variables should be passed by reference")?;
                        let c = cell(self.eval(expr)?);
                        if let Some(n) = name {
                            out.named.push((n, c, true, false));
                            seen_named = true;
                        } else {
                            out.cells.push(c);
                            pos += 1;
                        }
                    }
                    _ if internal && matches!(ctx, "current()" | "pos()") => {
                        // current()/pos() declare pass-by-value in zend
                        // arginfo — literals are legal (bug55754).
                        let c = cell(self.eval(expr)?);
                        if let Some(n) = name {
                            out.named.push((n, c, true, false));
                            seen_named = true;
                        } else {
                            out.cells.push(c);
                            pos += 1;
                        }
                    }
                    _ => {
                        let argno = match &name {
                            Some(n) => decl
                                .iter()
                                .position(|p| !p.variadic && p.name == *n)
                                .map(|i| i + 1)
                                .unwrap_or(pos + 1),
                            None => pos + 1,
                        };
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "{}: Argument #{} (${}) could not be passed by reference",
                                ctx,
                                argno,
                                name.as_deref()
                                    .or_else(|| decl.get(pos).map(|p| p.name.as_str()))
                                    .unwrap_or("")
                            ),
                            0,
                        ));
                    }
                }
            } else {
                let v = self.eval(expr)?;
                if let Some(n) = name {
                    out.named.push((n, cell(v), false, false));
                    seen_named = true;
                } else {
                    out.cells.push(cell(v));
                    pos += 1;
                }
            }
        }
        Ok(out)
    }

    /// Spreadable items of `...$v`: arrays yield entries, Traversables
    /// iterate via the rewind/valid/current/key/next protocol
    /// (IteratorAggregate chains resolve first). `None` key = positional.
    pub(in crate::interp) fn unpack_items(&mut self, v: &Value) -> Result<SpreadItems, PhpError> {
        match v {
            Value::Array(a) => {
                // Element cells are handed to the call as potential
                // references — Zend separates the array first so a
                // shared copy (e.g. `$ary2 = $ary`) keeps its own
                // values (named_params/unpack).
                for (_, c) in a.borrow_mut().entries.iter_mut() {
                    let fresh = cell(c.borrow().clone());
                    *c = fresh;
                }
                let mut out = Vec::new();
                for (k, c) in a.borrow().iter() {
                    let n = match k {
                        ArrKey::Str(s) => Some(s.clone()),
                        _ => None,
                    };
                    out.push((n, c.clone()));
                }
                Ok(out)
            }
            Value::Object(o) => {
                // IteratorAggregate → getIterator() chain to a real Iterator.
                let mut cur = o.clone();
                let it = loop {
                    if self.obj_is_a(&cur, "IteratorAggregate") {
                        match self.method_invoke(cur.clone(), "getIterator", CallArgs::empty())? {
                            Value::Object(io) => cur = io,
                            _ => {
                                return self.fail(PhpError::uncaught(
                                    "Exception",
                                    "Objects returned by getIterator() must be traversable or implement interface Iterator",
                                    0,
                                ))
                            }
                        }
                    } else if self.obj_is_a(&cur, "Iterator") {
                        break cur;
                    } else {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Only arrays and Traversables can be unpacked, {} given",
                                cur.borrow().class.name()
                            ),
                            0,
                        ));
                    }
                };
                let _ = self.method_invoke(it.clone(), "rewind", CallArgs::empty());
                let mut out = Vec::new();
                loop {
                    let ok = self
                        .method_invoke(it.clone(), "valid", CallArgs::empty())
                        .map(|v| v.is_truthy())
                        .unwrap_or(false);
                    if !ok {
                        break;
                    }
                    let val = self
                        .method_invoke(it.clone(), "current", CallArgs::empty())
                        .unwrap_or(Value::Null);
                    let key = self
                        .method_invoke(it.clone(), "key", CallArgs::empty())
                        .unwrap_or(Value::Null);
                    let n = match &key {
                        Value::Str(s) => Some(crate::value::lossy(&s).into_owned().into()),
                        _ => None,
                    };
                    out.push((n, cell(val)));
                    let _ = self.method_invoke(it.clone(), "next", CallArgs::empty());
                }
                Ok(out)
            }
            _ => self.fail(PhpError::uncaught(
                "Error",
                format!(
                    "Only arrays and Traversables can be unpacked, {} given",
                    self.zval_type_name(v)
                ),
                0,
            )),
        }
    }

    /// Call a named function (builtin or user-defined).
    pub(in crate::interp) fn call_named(
        &mut self,
        fname: &str,
        args: &[Expr],
    ) -> Result<Value, PhpError> {
        // `\u{1}f` marks a source-literal unqualified call — only it may
        // fall back `ns\f` -> `f`; dynamic names are fully qualified.
        let (unqualified, lname) = match fname.strip_prefix('\u{1}') {
            Some(n) => (true, n.to_lowercase()),
            None => (false, fname.trim_start_matches('\\').to_lowercase()),
        };
        // `__HALT_COMPILER()` stops execution of the file (ns_080).
        if lname == "__halt_compiler" {
            return Err(PhpError {
                trace: None,
                thrown_line: None,
                display_msg: None,
                kind: ErrorKind::Fatal,
                message: "\u{1}exit:0".to_string(),
                line: 0,
            });
        }
        let mut decl = self.functions.get(&lname).cloned();
        // A namespaced user function outranks the global/builtin one for
        // unqualified calls (namespaces/ns_013).
        let mut ns_resolved = false;
        // When the ns\name fallback misses too, the undefined-function
        // error names the ns-qualified candidate (bugs/77376).
        let mut miss_name = fname.trim_start_matches('\u{1}').to_string();
        if decl.is_none() && unqualified {
            let ns = self.caller_ns();
            if !ns.is_empty() {
                let cand = format!("{}\\{}", ns.to_lowercase(), lname);
                decl = self.functions.get(&cand).cloned();
                ns_resolved = decl.is_some();
                if !ns_resolved {
                    miss_name = format!("{}\\{}", ns, fname.trim_start_matches('\u{1}'));
                }
            }
        }
        // Synthetic params carrying builtin by-ref flags so call results in
        // by-ref slots emit "Only variables should be passed by reference"
        // (passByReference_012, array_shift(array_shift($a))).
        let builtin_params: Vec<Param> = if decl.is_none() {
            let sig = crate::builtins::builtin_sig(&lname).unwrap_or_default();
            builtin_byref(&lname)
                .map(|flags| {
                    flags
                        .iter()
                        .enumerate()
                        .map(|(i, by_ref)| Param {
                            name: sig.get(i).map(|(n, _)| n.clone()).unwrap_or_default(),
                            default: None,
                            by_ref: *by_ref,
                            variadic: false,
                            ty: None,
                            promoted: false,
                            vis: None,
                            readonly: false,
                            is_final: false,
                            set_vis: None,
                            hooks: None,
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let argvals = self.arg_cells(
            args,
            decl.as_deref()
                .map(|d| d.params.as_slice())
                .unwrap_or(&builtin_params),
            &format!("{}()", fname.trim_start_matches('\u{1}')),
            decl.is_none(),
        )?;
        if !ns_resolved {
            if let Some(v) = self.call_builtin(&lname, &argvals)? {
                return Ok(v);
            }
        }
        let decl = match decl {
            Some(d) => d,
            None => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Call to undefined function {}()", miss_name),
                    0,
                ))
            }
        };
        self.invoke_fn(&decl, argvals, None, None)
    }

    /// Call any callable-ish Value: Callable, string name, [obj,'m'], obj
    /// with __invoke.
    pub fn call_value(&mut self, v: &Value, args: CallArgs) -> Result<Value, PhpError> {
        match v {
            Value::Callable(c) => {
                match &c.kind {
                    CallableKind::Closure(decl) => {
                        // A `yield`-bearing closure body makes the call
                        // a Generator factory — `function() { yield }`
                        // returns a Generator like any other function
                        // (iterable_003).
                        if Self::decl_contains_yield(&decl.body) {
                            return Ok(Value::Object(self.make_generator(GenSetup::Invoke {
                                decl: decl.clone(),
                                args,
                                this_obj: c.this_obj.clone(),
                                scope_class: c.scope_class.clone(),
                                decl_class: None,
                                called_class: c.called_class.clone(),
                                captures: c.captures.clone(),
                            })));
                        }
                        let mut frame_args = Vec::new();
                        // fn_name is the closure's Zend name
                        // (`{closure:enclosing():L}`) — __FUNCTION__/
                        // __METHOD__ read it, and a nested closure's
                        // `enclosing` resolves through it (closure_065).
                        let mut frame = Frame::new(decl.name.clone());
                        frame.closure_rc = Some(c.clone());
                        frame.call_alias = self.pending_call_alias.take();
                        frame.fn_line = decl.line;
                        frame.file = decl.file.clone();
                        frame.ret_by_ref = decl.by_ref;
                        for (n, cap, by_ref) in &c.captures {
                            // By-value captures re-import the stored
                            // value on every call — the caller's writes
                            // inside the closure don't persist
                            // (closure_009/011).
                            let c2 = if *by_ref {
                                cap.clone()
                            } else {
                                cell(cap.borrow().clone())
                            };
                            frame.vars.insert(n.clone(), c2);
                        }
                        frame.this_obj = c.this_obj.clone();
                        frame.scope_class = c.scope_class.clone();
                        frame.called_class = c.called_class.clone();
                        // $this binds like a normal method frame —
                        // closures defined in an object context auto-capture it.
                        if let Some(o) = &c.this_obj {
                            frame
                                .vars
                                .insert("this".to_string(), cell(Value::Object(o.clone())));
                        }
                        frame.file = if decl.file.is_empty() {
                            self.cur_file.clone()
                        } else {
                            decl.file.clone()
                        };
                        let decl = decl.clone();
                        self.stack.push(frame);
                        // bind params manually (frame already pushed for captures)

                        self.bind_and_run(&decl, args, frame_args.split_off(0))
                    }
                    CallableKind::Named(n) => {
                        let n = n.trim_start_matches('\\');
                        if let Some(v) = self.call_builtin(&n.to_lowercase(), &args)? {
                            return Ok(v);
                        }
                        let decl = match self.functions.get(&n.to_lowercase()) {
                            Some(d) => d.clone(),
                            None => {
                                return self.fail(PhpError::uncaught(
                                    "Error",
                                    format!("Call to undefined function {}()", n),
                                    0,
                                ))
                            }
                        };
                        self.invoke_fn(&decl, args, None, None)
                    }
                    CallableKind::Method { obj, class, name } => match obj {
                        Some(o) => self.method_invoke(o.clone(), name, args),
                        None => match class {
                            Some(cls) => self.static_invoke(cls.clone(), name, args, None, true),
                            None => self.fail(PhpError::fatal("bad callable", 0)),
                        },
                    },
                }
            }
            Value::Str(s) => {
                // Fully-qualified dynamic names carry a leading `\`
                // (namespaces/ns_032).
                let name = crate::value::lossy(s).trim_start_matches('\\').to_string();
                // "Class::method" string callables
                if let Some((cls, m)) = name.split_once("::") {
                    if let Some(c) = self.resolve_class(cls) {
                        let cls = self.classes.get(&c.to_lowercase()).cloned();
                        if let Some(cls) = cls {
                            return self.static_invoke_vis(cls, m, args, None, true);
                        }
                    }
                }
                if let Some(v) = self.call_builtin(&name.to_lowercase(), &args)? {
                    return Ok(v);
                }
                let decl = match self.functions.get(&name.to_lowercase()) {
                    Some(d) => d.clone(),
                    None => {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!("Call to undefined function {}()", name),
                            0,
                        ))
                    }
                };
                self.invoke_fn(&decl, args, None, None)
            }
            Value::Array(a) => {
                // [$obj, 'method'] or ['Class', 'method']
                let a = a.borrow();
                let o0 = a.get(&ArrKey::Int(0));
                let m = a.get(&ArrKey::Int(1));
                match (o0, m) {
                    (Some(t), Some(mv)) => {
                        let mname = mv.to_php_string();
                        match t {
                            // `[$closure, '__invoke']` is callable
                            // (closure_invoke_ref_warning).
                            Value::Callable(c) if mname.eq_ignore_ascii_case("__invoke") => {
                                // `[$closure,'__invoke']` reports args
                                // under `Closure::__invoke` (zend).
                                self.pending_call_alias = Some("Closure::__invoke".into());
                                let r = self.call_value(&Value::Callable(c.clone()), args);
                                self.pending_call_alias = None;
                                r
                            }
                            Value::Object(o) => self.method_invoke_vis(o.clone(), &mname, args),
                            Value::Str(cn) => {
                                let cls = self
                                    .resolve_class(&crate::value::lossy(&cn))
                                    .and_then(|c| self.classes.get(&c.to_lowercase()).cloned());
                                match cls {
                                    Some(cls) => {
                                        self.static_invoke_vis(cls, &mname, args, None, true)
                                    }
                                    None => self.fail(PhpError::uncaught(
                                        "Error",
                                        format!("Class \"{}\" not found", crate::value::lossy(&cn)),
                                        0,
                                    )),
                                }
                            }
                            _ => self.fail(PhpError::fatal("invalid callable array", 0)),
                        }
                    }
                    _ => self.fail(PhpError::fatal("invalid callable array", 0)),
                }
            }
            Value::Object(o) => {
                let icls = o.borrow().class.clone();
                if self.find_method_in(&icls, "__invoke").is_some() {
                    // `$b()` calls __invoke with NO visibility check —
                    // only an explicit `->` invoke is gated
                    // (bug61025).
                    self.method_invoke(o.clone(), "__invoke", args)
                } else {
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Object of type {} is not callable", o.borrow().class.name()),
                        0,
                    ))
                }
            }
            _ => self.fail(PhpError::uncaught("Error", "Value is not callable", 0)),
        }
    }
    /// Display name used in call-time diagnostics (`f(): Argument #N`),
    /// matching zend's callable naming (closure_019).
    pub fn callable_ctx_name(&mut self, v: &Value) -> String {
        match v {
            Value::Callable(c) => match &c.kind {
                CallableKind::Closure(d) => d.name.clone(),
                CallableKind::Named(n) => n.trim_start_matches('\\').to_string(),
                CallableKind::Method { obj, class, name } => {
                    let cn = obj
                        .as_ref()
                        .map(|o| o.borrow().class.name().to_string())
                        .or_else(|| class.as_ref().map(|c| c.name().to_string()))
                        .unwrap_or_else(|| "Closure".into());
                    format!("{}::{}", cn, name)
                }
            },
            Value::Object(o) => format!("{}::__invoke", o.borrow().class.name()),
            Value::Str(s) => crate::value::lossy(s).trim_start_matches('\\').to_string(),
            _ => self.conv_str(v).unwrap_or_default(),
        }
    }

    /// `expr(...)` — first-class callable creation (PHP 8.1). Errors at
    /// creation are thrown `Error`s (catchable); abstract methods fail
    /// only when the closure is invoked (constexpr/error_abstract).
    pub(in crate::interp) fn fcc(&mut self, e: &Expr) -> Result<Value, PhpError> {
        match e {
            Expr::Call { name, .. } => match name.as_ref() {
                Expr::Str(s) => self.fcc_named(s),
                other => {
                    let v = self.eval(other)?;
                    self.fcc_val(&v)
                }
            },
            Expr::MethodCall { obj, name, .. } => {
                let ov = self.eval(obj)?;
                let mn = Self::nul_trunc(&self.prop_name(name)?);
                self.fcc_method(&ov, &mn)
            }
            Expr::StaticCall { class, name, .. } => {
                let cls = self.fcc_class_of(class)?;
                self.fcc_static(cls, name)
            }
            Expr::StaticCallDyn { class, name, .. } => {
                let cls = self.fcc_class_of(class)?;
                let nv = self.eval(name)?;
                let mn = match nv {
                    Value::Str(s) => Self::nul_trunc(&crate::value::lossy(&s)),
                    _ => {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Method name must be a string",
                            0,
                        ))
                    }
                };
                self.fcc_static(cls, &mn)
            }
            other => {
                let v = self.eval(other)?;
                self.fcc_val(&v)
            }
        }
    }

    /// `C::m(...)` class resolution — traits resolve too (calling a
    /// static trait method directly is deprecated, not undefined;
    /// constexpr/error_static_call_trait_method).
    fn fcc_class_of(&mut self, e: &Expr) -> Result<Rc<PhpClass>, PhpError> {
        if let Expr::Const(n) | Expr::Str(n) = e {
            let rn = self.resolve_class_name(n);
            if let Some(t) = self.traits.get(&rn.to_lowercase()).cloned() {
                // Traits aren't PhpClass-registered; wrap the decl so
                // find_method_in/late-static binding see the trait's
                // own methods (constexpr/error_static_call_trait_method).
                return Ok(Rc::new(PhpClass {
                    decl: t,
                    statics: std::cell::RefCell::new(std::collections::HashMap::new()),
                    statics_init: std::cell::RefCell::new(false),
                }));
            }
        }
        self.class_of(e)
    }

    /// `name(...)` — fn-name FCC with the same resolution as call_named
    /// (ns fallback for unqualified literals, \u{1} marker).
    fn fcc_named(&mut self, fname: &str) -> Result<Value, PhpError> {
        let (unqualified, lname) = match fname.strip_prefix('\u{1}') {
            Some(n) => (true, n.to_lowercase()),
            None => (false, fname.trim_start_matches('\\').to_lowercase()),
        };
        // Zend resolves unqualified FCC names ns\f -> f once per call
        // site and caches it — a later-conditionally-defined ns\f does
        // NOT rebind existing sites (constexpr/namespace_004).
        let caller = self
            .stack
            .last()
            .map(|f| f.fn_name.clone())
            .unwrap_or_else(|| "{main}".to_string());
        let cache_key = if unqualified {
            Some((caller, self.caller_ns().to_lowercase(), lname.clone()))
        } else {
            None
        };
        if let Some(k) = &cache_key {
            if let Some(hit) = self.fcc_fn_cache.get(k).cloned() {
                return self.fcc_named_emit(hit, fname);
            }
        }
        // Zend resolves unqualified FCC names ns\f -> f at creation;
        // a namespaced user function outranks the global builtin
        // (constexpr/namespace_003).
        let mut resolved: Option<String> = None;
        let mut miss = fname
            .trim_start_matches('\u{1}')
            .trim_start_matches('\\')
            .to_string();
        if unqualified {
            let ns = self.caller_ns();
            if !ns.is_empty() {
                let cand = format!("{}\\{}", ns.to_lowercase(), lname);
                if self.functions.contains_key(&cand) {
                    resolved = Some(cand);
                } else {
                    miss = format!("{}\\{}", ns, fname.trim_start_matches('\u{1}'));
                }
            }
        }
        if resolved.is_none()
            && (self.functions.contains_key(&lname) || builtins::is_builtin(&lname))
        {
            resolved = Some(lname.clone());
        }
        if let Some(k) = cache_key {
            self.fcc_fn_cache.insert(k, resolved.clone());
        }
        self.fcc_named_emit(resolved, &miss)
    }

    fn fcc_named_emit(&mut self, resolved: Option<String>, miss: &str) -> Result<Value, PhpError> {
        match resolved {
            // Function names resolve case-insensitively but display in
            // declared case (ReflectionFunction::getNamespaceName,
            // closure_068).
            Some(r) => Ok(Value::Callable(self.new_callable(PhpCallable {
                id: std::cell::Cell::new(0),
                kind: CallableKind::Named(
                    self.functions.get(&r).map(|d| d.name.clone()).unwrap_or(r),
                ),
                captures: Vec::new(),
                this_obj: None,
                scope_class: None,
                called_class: None,
                is_static: false,
            }))),
            None => self.fail(PhpError::uncaught(
                "Error",
                format!("Call to undefined function {}()", miss),
                0,
            )),
        }
    }

    /// Shared `Closure::bind`/`bindTo`/`call` rebinding model
    /// (closure_036-044/061/063, zend_closures, bug70685):
    /// - binding an instance to a static closure warns → NULL
    /// - unbinding $this warns — "of method" for method-created
    ///   closures, "of closure using $this" otherwise
    /// - explicit scope arg: null → unscoped ("dummy"), object → its
    ///   class, string → resolved class; omitted or 'static' keeps
    ///   the previous scope — an unscoped closure stays unscoped
    ///   ("dummy scope", closure_046)
    /// - internal-class scopes are rejected for everything but
    ///   method-created closures (their scope already is internal)
    /// - fake closures (Named/Method kinds) can't change scope, but
    ///   CAN rebind $this freely (closure_063: silent success)
    ///
    /// Returns Ok(None) after emitting a warning → caller returns NULL.
    pub(in crate::interp) fn rebind_closure(
        &mut self,
        c: &PhpCallable,
        new_this: Option<Rc<RefCell<PhpObject>>>,
        scope_arg: Option<Value>,
    ) -> Result<Option<PhpCallable>, PhpError> {
        if new_this.is_some() && c.is_static {
            self.warn(
                "Cannot bind an instance to a static closure, this will be an error in PHP 9",
            )?;
            return Ok(None);
        }
        if new_this.is_none() {
            match &c.kind {
                // Method-created closures carry their target in
                // `kind.obj` — dropping it is the "of method" unbind
                // (closure_061).
                CallableKind::Method { obj: Some(_), .. } => {
                    self.warn("Cannot unbind $this of method, this will be an error in PHP 9")?;
                    return Ok(None);
                }
                // "uses $this" is the compile-time body flag, not
                // merely a bound instance — a static-scope closure
                // that references $this but never captured one
                // unbinds quietly (closure_062).
                CallableKind::Closure(d)
                    if c.this_obj.is_some() && Self::body_uses_this(&d.body) && !c.is_static =>
                {
                    self.warn(
                        "Cannot unbind $this of closure using $this, this will be an error in PHP 9",
                    )?;
                    return Ok(None);
                }
                _ => {}
            }
        }
        let scope: Option<Rc<PhpClass>> = match &scope_arg {
            Some(Value::Null) => None,
            Some(Value::Object(o)) => Some(o.borrow().class.clone()),
            Some(Value::Str(s)) => {
                let sn = crate::value::lossy(s).to_string();
                if sn.eq_ignore_ascii_case("static") {
                    // 'static' keeps the previous scope verbatim —
                    // same as omitting the argument (zend's default
                    // IS "static", closure_046).
                    c.scope_class.clone()
                } else {
                    match self
                        .resolve_class(&sn)
                        .and_then(|c| self.classes.get(&c.to_lowercase()).cloned())
                    {
                        Some(c) => Some(c),
                        None => {
                            // Unresolvable scope string — warning +
                            // NULL, not a throw (bug78658).
                            self.warn(&format!("Class \"{}\" not found", sn))?;
                            return Ok(None);
                        }
                    }
                }
            }
            // Omitted — keep the previous scope; an unscoped closure
            // stays on the "dummy scope" (isset() on foreign privates
            // is still false, closure_046).
            None => c.scope_class.clone(),
            // Other arg types were rejected by the caller's TypeError.
            Some(_) => None,
        };
        // Internal classes (decl.file empty) can't be closure scopes —
        // `call()` always resolves scope to the new instance's class,
        // so $x->call($std) hits this (closure_call). Method-kind
        // callables are exempt: their declaring scope is the internal
        // class already (closure_call_internal). This check precedes
        // the per-kind scope warnings — a fake-function closure bound
        // to stdClass reports the internal class (closure_061).
        if !matches!(c.kind, CallableKind::Method { .. }) {
            if let Some(sc) = &scope {
                if sc.decl.file.is_empty() && !sc.name().eq_ignore_ascii_case("closure") {
                    self.warn(&format!(
                        "Cannot bind closure to scope of internal class {}, this will be an error in PHP 9",
                        sc.name()
                    ))?;
                    return Ok(None);
                }
            }
        }
        match &c.kind {
            // A closure created from a function has no scope — only an
            // actual scope change warns; binding $this is silent
            // (bug70630 vs closure_063).
            CallableKind::Named(_) if Self::scope_changed(&scope, &c.scope_class) => {
                self.warn(
                    "Cannot rebind scope of closure created from function, this will be an error in PHP 9",
                )?;
                return Ok(None);
            }
            CallableKind::Method { name, .. } => {
                // The new instance must be instanceof the method's
                // DECLARING class — `scope_class` already holds it
                // (SplStack::count → SplDoublyLinkedList, bug70685).
                // Checked before the scope warning: call(new B) rebinds
                // scope AND target yet reports the target
                // (closure_from_callable_rebinding).
                if let Some(t) = &new_this {
                    let tc = t.borrow().class.clone();
                    let dc = c.scope_class.clone().unwrap_or_else(|| tc.clone());
                    if !self.is_a(&tc, dc.name()) {
                        self.warn(&format!(
                            "Cannot bind method {}::{}() to object of class {}, this will be an error in PHP 9",
                            dc.name(),
                            name,
                            tc.name()
                        ))?;
                        return Ok(None);
                    }
                }
                // A method-created closure keeps the declaring scope —
                // resolving to a different class is a rebind
                // (bug70685).
                if Self::scope_changed(&scope, &c.scope_class) {
                    self.warn(
                        "Cannot rebind scope of closure created from method, this will be an error in PHP 9",
                    )?;
                    return Ok(None);
                }
            }
            _ => {}
        }
        let mut nc = (*c).clone();
        nc.this_obj = new_this;
        nc.scope_class = scope.clone();
        nc.called_class = scope.clone();
        // Method-kind callables rebind the invocation target too.
        if let CallableKind::Method { obj, name, .. } = &mut nc.kind {
            if nc.this_obj.is_some() {
                *obj = nc.this_obj.clone();
            } else {
                let _ = name;
            }
        }
        Ok(Some(nc))
    }

    /// Scope comparison for rebind warnings: None-vs-Some counts as
    /// a change (dummy scope is a different scope, closure_061).
    fn scope_changed(a: &Option<Rc<PhpClass>>, b: &Option<Rc<PhpClass>>) -> bool {
        match (a, b) {
            (Some(x), Some(y)) => !x.name().eq_ignore_ascii_case(y.name()),
            (Some(_), None) | (None, Some(_)) => true,
            (None, None) => false,
        }
    }

    /// `Closure::fromCallable($v)` — like fcc_val but: failures throw
    /// TypeError (caller wraps), and the scope keywords
    /// `self`/`parent`/`static` are deprecated yet still resolve
    /// non-static methods against the current `$this`
    /// (closure_from_callable_basic).
    pub(in crate::interp) fn callable_to_closure(&mut self, v: &Value) -> Result<Value, PhpError> {
        type Spec = Option<(String, String, Option<Rc<RefCell<PhpObject>>>)>;
        let spec: Spec = match v {
            Value::Str(s) => crate::value::lossy(s)
                .split_once("::")
                .map(|(cn, mn)| (cn.to_string(), mn.to_string(), None)),
            Value::Array(a) => {
                let a = a.borrow();
                let t = a.get(&ArrKey::Int(0));
                let m = a.get(&ArrKey::Int(1));
                match (t, m) {
                    (Some(Value::Str(cn)), Some(mv)) => Some((
                        crate::value::lossy(&cn).to_string(),
                        mv.to_php_string(),
                        None,
                    )),
                    (Some(Value::Object(o)), Some(mv)) => {
                        Some((String::new(), mv.to_php_string(), Some(o.clone())))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let Some((cn, mn, bound_obj)) = spec else {
            return self.fcc_val(v);
        };
        let kw = cn.to_ascii_lowercase();
        let is_scope_kw = matches!(kw.as_str(), "self" | "parent" | "static");
        if bound_obj.is_none() && is_scope_kw {
            self.deprecated(&format!("Use of \"{}\" in callables is deprecated", kw))?;
        }
        let cls: Option<Rc<PhpClass>> = if let Some(o) = &bound_obj {
            Some(o.borrow().class.clone())
        } else if is_scope_kw {
            let f = self.stack.last();
            let scope = f.and_then(|f| f.decl_class.clone().or(f.scope_class.clone()));
            match kw.as_str() {
                "self" => scope,
                "parent" => scope.and_then(|s| {
                    s.decl
                        .parent
                        .as_ref()
                        .and_then(|p| self.classes.get(&p.to_lowercase()).cloned())
                }),
                "static" => f.and_then(|f| f.called_class.clone()).or(scope),
                _ => None,
            }
        } else {
            self.resolve_class(&cn)
                .and_then(|c| self.classes.get(&c.to_lowercase()).cloned())
        };
        let Some(cls) = cls else {
            return self.fcc_val(v);
        };
        let Some((m, dc)) = self.find_method_in(&cls, &mn) else {
            if bound_obj.is_none() {
                return self.fcc_static(cls, &mn);
            }
            return self.fcc_method(&Value::Object(bound_obj.unwrap()), &mn);
        };
        if m.is_abstract {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot call abstract method {}::{}()", dc.name(), mn),
                0,
            ));
        }
        self.fcc_vis_check(&m, &dc)?;
        if m.is_static || bound_obj.is_some() {
            if bound_obj.is_none() {
                return self.fcc_static(cls, &mn);
            }
            return Ok(Value::Callable(self.new_callable(PhpCallable {
                id: std::cell::Cell::new(0),
                kind: CallableKind::Method {
                    obj: bound_obj.clone(),
                    class: None,
                    name: mn.to_string(),
                },
                captures: Vec::new(),
                this_obj: bound_obj.clone(),
                scope_class: Some(dc.clone()),
                called_class: Some(dc),
                is_static: false,
            })));
        }
        // Scope-keyword callable to a non-static method binds the
        // current `$this` when it's an instance of the class.
        let this = self
            .stack
            .last()
            .and_then(|f| f.this_obj.clone())
            .filter(|o| {
                let cname = o.borrow().class.name().to_string();
                self.is_a_str(&cname, cls.name())
            });
        let Some(this) = this else {
            return self.fcc_static(cls, &mn);
        };
        Ok(Value::Callable(self.new_callable(PhpCallable {
            id: std::cell::Cell::new(0),
            kind: CallableKind::Method {
                obj: Some(this.clone()),
                class: None,
                name: mn.to_string(),
            },
            captures: Vec::new(),
            this_obj: Some(this),
            scope_class: Some(dc.clone()),
            called_class: Some(dc),
            is_static: false,
        })))
    }

    /// Any value → callable coercion for FCC (`$fn(...)`, `($c)(...)`,
    /// `[$o,'m'](...)`). Non-callables throw `Error` (Zend "not callable").
    fn fcc_val(&mut self, v: &Value) -> Result<Value, PhpError> {
        match v {
            Value::Callable(_) => Ok(v.clone()),
            Value::Str(s) => {
                let name = crate::value::lossy(s);
                let name = name.trim_start_matches('\\');
                if let Some((cls, m)) = name.split_once("::") {
                    if let Some(rcn) = self.resolve_class(cls) {
                        if let Some(c) = self.classes.get(&rcn.to_lowercase()).cloned() {
                            return self.fcc_static(c, m);
                        }
                    }
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Class \"{}\" not found", cls),
                        0,
                    ));
                }
                if self.functions.contains_key(&name.to_lowercase())
                    || builtins::is_builtin(&name.to_lowercase())
                {
                    Ok(Value::Callable(self.new_callable(PhpCallable {
                        id: std::cell::Cell::new(0),
                        kind: CallableKind::Named(name.to_lowercase()),
                        captures: Vec::new(),
                        this_obj: None,
                        scope_class: None,
                        called_class: None,
                        is_static: false,
                    })))
                } else {
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Call to undefined function {}()", name),
                        0,
                    ))
                }
            }
            Value::Object(o) => {
                let icls = o.borrow().class.clone();
                if self.find_method_in(&icls, "__invoke").is_some() {
                    Ok(Value::Callable(self.new_callable(PhpCallable {
                        id: std::cell::Cell::new(0),
                        kind: CallableKind::Method {
                            obj: Some(o.clone()),
                            class: None,
                            name: "__invoke".to_string(),
                        },
                        captures: Vec::new(),
                        this_obj: Some(o.clone()),
                        scope_class: Some(o.borrow().class.clone()),
                        called_class: Some(o.borrow().class.clone()),
                        is_static: false,
                    })))
                } else {
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Object of type {} is not callable", o.borrow().class.name()),
                        0,
                    ))
                }
            }
            Value::Array(a) => {
                let a = a.borrow();
                let t = a.get(&ArrKey::Int(0));
                let m = a.get(&ArrKey::Int(1));
                match (t, m) {
                    (Some(Value::Object(o)), Some(mv)) => {
                        let mn = mv.to_php_string();
                        self.fcc_method(&Value::Object(o.clone()), &mn)
                    }
                    // `[$closure, '__invoke']` — a closure is callable
                    // (bug78689).
                    (Some(Value::Callable(c)), Some(mv)) => {
                        let mn = mv.to_php_string();
                        if mn.eq_ignore_ascii_case("__invoke") {
                            Ok(Value::Callable(c.clone()))
                        } else {
                            self.fail(PhpError::uncaught(
                                "Error",
                                "Value of type array is not callable",
                                0,
                            ))
                        }
                    }
                    (Some(Value::Str(cn)), Some(mv)) => {
                        let mn = mv.to_php_string();
                        match self
                            .resolve_class(&crate::value::lossy(&cn))
                            .and_then(|c| self.classes.get(&c.to_lowercase()).cloned())
                        {
                            Some(c) => self.fcc_static(c, &mn),
                            None => self.fail(PhpError::uncaught(
                                "Error",
                                format!("Class \"{}\" not found", crate::value::lossy(&cn)),
                                0,
                            )),
                        }
                    }
                    _ => self.fail(PhpError::uncaught(
                        "Error",
                        format!(
                            "Value of type {} is not callable",
                            v.type_name().to_lowercase()
                        ),
                        0,
                    )),
                }
            }
            _ => self.fail(PhpError::uncaught(
                "Error",
                format!(
                    "Value of type {} is not callable",
                    v.type_name().to_lowercase()
                ),
                0,
            )),
        }
    }

    /// `$obj->method(...)` — bound method closure. Visibility is checked
    /// at creation from the calling scope (zend_closures).
    fn fcc_method(&mut self, ov: &Value, mn: &str) -> Result<Value, PhpError> {
        let o = match ov {
            Value::Object(o) => o.clone(),
            Value::Callable(_) if mn.eq_ignore_ascii_case("__invoke") => {
                // `$closure->__invoke(...)` — the closure itself.
                return Ok(ov.clone());
            }
            other => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Call to a member function {}() on {}",
                        mn,
                        other.type_name().to_lowercase()
                    ),
                    0,
                ))
            }
        };
        let cls = o.borrow().class.clone();
        match self.find_method_in(&cls, mn) {
            Some((m, dc)) => {
                if m.is_abstract {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot call abstract method {}::{}()", dc.name(), mn),
                        0,
                    ));
                }
                self.fcc_vis_check(&m, &dc)?;
                Ok(Value::Callable(self.new_callable(PhpCallable {
                    id: std::cell::Cell::new(0),
                    kind: CallableKind::Method {
                        obj: Some(o.clone()),
                        class: None,
                        name: mn.to_string(),
                    },
                    captures: Vec::new(),
                    this_obj: Some(o),
                    scope_class: Some(dc.clone()),
                    called_class: Some(dc),
                    is_static: false,
                })))
            }
            None => {
                if self.find_method_in(&cls, "__call").is_some() {
                    // `Foo::doesNotExist` routes through __call at call
                    // time (first_class_callable_005).
                    Ok(Value::Callable(self.new_callable(PhpCallable {
                        id: std::cell::Cell::new(0),
                        kind: CallableKind::Method {
                            obj: Some(o.clone()),
                            class: None,
                            name: mn.to_string(),
                        },
                        captures: Vec::new(),
                        this_obj: Some(o),
                        scope_class: Some(cls.clone()),
                        called_class: Some(cls),
                        is_static: false,
                    })))
                } else {
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Call to undefined method {}::{}()", cls.name(), mn),
                        0,
                    ))
                }
            }
        }
    }

    /// `C::method(...)` — static method closure; non-static methods fail
    /// "cannot be called statically" at creation (Error, catchable).
    fn fcc_static(&mut self, cls: Rc<PhpClass>, mn: &str) -> Result<Value, PhpError> {
        match self.find_method_in(&cls, mn) {
            Some((m, dc)) => {
                if m.is_abstract {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot call abstract method {}::{}()", dc.name(), mn),
                        0,
                    ));
                }
                if !m.is_static {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!(
                            "Non-static method {}::{}() cannot be called statically",
                            cls.name(),
                            mn
                        ),
                        0,
                    ));
                }
                // `Foo::m(...)` where Foo is a trait: allowed but
                // deprecated outside a using class (8.4+;
                // constexpr/error_static_call_trait_method).
                if self.traits.contains_key(&dc.name().to_lowercase()) {
                    self.deprecated(&format!(
                        "Calling static trait method {}::{} is deprecated, it should only be called on a class using the trait",
                        dc.name(),
                        m.decl.name
                    ))?;
                }
                self.fcc_vis_check(&m, &dc)?;
                Ok(Value::Callable(self.new_callable(PhpCallable {
                    id: std::cell::Cell::new(0),
                    // `class` is the called class for late static
                    // binding (`Bar::method(...)` -> static::class
                    // is Bar; first_class_callable_010).
                    kind: CallableKind::Method {
                        obj: None,
                        class: Some(cls.clone()),
                        name: mn.to_string(),
                    },
                    captures: Vec::new(),
                    this_obj: None,
                    scope_class: Some(dc),
                    called_class: Some(cls),
                    is_static: true,
                })))
            }
            None => {
                if self.find_method_in(&cls, "__callstatic").is_some() {
                    if self.in_const_expr > 0 {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Creating a callable for the magic __callStatic() method is not supported in constant expressions",
                            0,
                        ));
                    }
                    Ok(Value::Callable(self.new_callable(PhpCallable {
                        id: std::cell::Cell::new(0),
                        kind: CallableKind::Method {
                            obj: None,
                            class: Some(cls.clone()),
                            name: mn.to_string(),
                        },
                        captures: Vec::new(),
                        this_obj: None,
                        scope_class: Some(cls.clone()),
                        called_class: Some(cls),
                        is_static: true,
                    })))
                } else {
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Call to undefined method {}::{}()", cls.name(), mn),
                        0,
                    ))
                }
            }
        }
    }

    /// FCC visibility gate (zend_closures): checked at creation from the
    /// calling scope; `const_self` covers class-const initializers.
    fn fcc_vis_check(&mut self, m: &MethodDecl, dc: &Rc<PhpClass>) -> Result<(), PhpError> {
        let scope = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned())
            .or_else(|| self.const_self.clone());
        let ok = match m.visibility {
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
                        || self.is_a_str(s.name(), &proto)
                })
                .unwrap_or(false),
        };
        if ok {
            return Ok(());
        }
        let vis = match m.visibility {
            crate::ast::Visibility::Public => "public",
            crate::ast::Visibility::Protected => "protected",
            crate::ast::Visibility::Private => "private",
        };
        let from = match &scope {
            Some(s) => format!("scope {}", s.name()),
            None => "global scope".to_string(),
        };
        self.fail(PhpError::uncaught(
            "Error",
            format!(
                "Call to {} method {}::{}() from {}",
                vis,
                dc.name(),
                m.decl.name,
                from
            ),
            0,
        ))
    }

    /// Compile-time constant-expression eval: FCC shape rules apply,
    /// `self`/`parent` bind to `const_self`, magic __callStatic is
    /// rejected (constexpr/*).
    pub(in crate::interp) fn eval_const(&mut self, e: &Expr) -> Result<Value, PhpError> {
        self.const_fcc_check(e)?;
        self.in_const_expr += 1;
        let r = self.eval(e);
        self.in_const_expr -= 1;
        r
    }

    /// Scalar-literal callees (`(0)(...)`, `(1.5)(...)`) fail "Illegal
    /// function name"; everything else non-literal is `msg`.
    fn const_scalar_callee(&self, e: &Expr, msg: &str) -> Result<(), PhpError> {
        match e {
            Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::Null => {
                Err(PhpError::fatal("Illegal function name", self.cur_line))
            }
            _ => Err(PhpError::fatal(msg, self.cur_line)),
        }
    }

    /// FCC-in-constant-expression shape rules (zend_compile): the callee
    /// must be a literal function name or `LiteralClass::literalMethod`.
    /// Recursed so FCCs nested in const exprs get the same check.
    fn const_fcc_check(&self, e: &Expr) -> Result<(), PhpError> {
        match e {
            Expr::Fcc(inner) => match inner.as_ref() {
                Expr::Call { name, .. } => match name.as_ref() {
                    Expr::Str(_) => Ok(()),
                    Expr::Paren(p) => self.const_scalar_callee(
                        p.as_ref(),
                        "Cannot use dynamic function name in constant expression",
                    ),
                    other => self.const_scalar_callee(
                        other,
                        "Cannot use dynamic function name in constant expression",
                    ),
                },
                Expr::StaticCall { class, .. } | Expr::StaticCallDyn { class, .. } => {
                    match class.as_ref() {
                        Expr::Const(c) | Expr::Str(c) => {
                            if c.eq_ignore_ascii_case("static") {
                                Err(PhpError::fatal(
                                    "\"static\" is not allowed in compile-time constants",
                                    self.cur_line,
                                ))
                            } else {
                                Ok(())
                            }
                        }
                        _ => Err(PhpError::fatal(
                            "Constant expression contains invalid operations",
                            self.cur_line,
                        )),
                    }
                }
                _ => Err(PhpError::fatal(
                    "Constant expression contains invalid operations",
                    self.cur_line,
                )),
            },
            Expr::Paren(inner) | Expr::Assign { value: inner, .. } => self.const_fcc_check(inner),
            Expr::Binary { l, r, .. } => {
                self.const_fcc_check(l)?;
                self.const_fcc_check(r)
            }
            Expr::Unary { e, .. } => self.const_fcc_check(e),
            Expr::Ternary { c, t, f, .. } => {
                self.const_fcc_check(c)?;
                if let Some(t) = t.as_ref() {
                    self.const_fcc_check(t)?;
                }
                self.const_fcc_check(f)
            }
            Expr::Index { e, i, .. } => {
                self.const_fcc_check(e)?;
                if let Some(i) = i.as_ref() {
                    self.const_fcc_check(i)?;
                }
                Ok(())
            }
            Expr::ArrayLit(items) => {
                for (k, v) in items {
                    if let Some(k) = k {
                        self.const_fcc_check(k)?;
                    }
                    self.const_fcc_check(v)?;
                }
                Ok(())
            }
            Expr::Call { name, args, .. } => {
                self.const_fcc_check(name)?;
                for a in args {
                    self.const_fcc_check(a)?;
                }
                Ok(())
            }
            Expr::MethodCall { obj, args, .. } => {
                self.const_fcc_check(obj)?;
                for a in args {
                    self.const_fcc_check(a)?;
                }
                Ok(())
            }
            Expr::StaticCall { class, args, .. } | Expr::StaticCallDyn { class, args, .. } => {
                self.const_fcc_check(class)?;
                for a in args {
                    self.const_fcc_check(a)?;
                }
                Ok(())
            }
            Expr::New { class, args, .. } => {
                self.const_fcc_check(class)?;
                for a in args {
                    self.const_fcc_check(a)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// spl_autoload: invoke each registered loader until `name`
    /// resolves (resolve_class/class_of retry on miss).
    /// An exception thrown by an autoloader propagates to the code that
    /// triggered the load (PHP stops the chain on throw).
    pub fn run_autoload(&mut self, name: &str) -> Result<(), PhpError> {
        let key = name.trim_start_matches('\\').to_lowercase();
        if !self.autoloading.insert(key.clone()) {
            return Ok(());
        }
        let fns = self.autoload_fns.clone();
        // A loader triggered while a constant expression is mid-eval
        // (static-prop/const initializers) is ordinary user code: its
        // `self`/`static`/`parent` must bind to ITS own frames, not to
        // the class being initialized — otherwise `self::$x` inside the
        // loader resolves against the initializing class (composer's
        // ClassLoader reads self::$includeFile).
        let saved_const = (
            self.in_const_expr,
            self.class_const_ctx,
            self.const_self.take(),
        );
        self.in_const_expr = 0;
        self.class_const_ctx = 0;
        let mut res = Ok(());
        for f in fns {
            if let Err(e) = self.call_value(&f, CallArgs::positional(vec![cell(Value::str(name))]))
            {
                res = Err(e);
                break;
            }
            if self.classes.contains_key(&name.to_lowercase()) {
                break;
            }
        }
        self.in_const_expr = saved_const.0;
        self.class_const_ctx = saved_const.1;
        self.const_self = saved_const.2;
        self.autoloading.remove(&key);
        // A throwable escaping an autoloader while variance obligations
        // are pending leaves the in-progress class half-linked — Zend
        // falls back to a fatal naming the class being inherited
        // (variance/loading_exception*).
        if let Err(e) = &res {
            if e.kind == crate::error::ErrorKind::Throw && !self.variance_obligations.is_empty() {
                if let Some(Value::Object(o)) = &self.pending_exception {
                    let (cls, msg, file, line, tr) = {
                        let ob = o.borrow();
                        let msg = ob
                            .props
                            .get("message")
                            .map(|v| v.borrow().to_php_string())
                            .unwrap_or_default();
                        let (file, line, tr) = match &ob.internal {
                            Some(ObjectInternal::Exception {
                                file,
                                line,
                                trace,
                                frames,
                                ..
                            }) => (
                                file.clone(),
                                *line as usize,
                                if !trace.is_empty() {
                                    trace.clone()
                                } else {
                                    crate::value::format_trace(frames)
                                },
                            ),
                            _ => (self.diag_file(), e.line, "#0 {main}".to_string()),
                        };
                        (ob.class.name().to_string(), msg, file, line, tr)
                    };
                    self.pending_exception = None;
                    let outer = self
                        .declaring
                        .last()
                        .map(|d| d.name.clone())
                        .unwrap_or_default();
                    return Err(PhpError::fatal(
                        format!(
                            "During inheritance of {outer} with variance dependencies: Uncaught {cls}: {msg} in {file}:{line}\nStack trace:\n{}",
                            tr.trim_end()
                        ),
                        self.cur_line,
                    ));
                }
            }
        }
        res
    }
    /// Params binding + body run for a pushed frame context (closures).
    fn bind_and_run(
        &mut self,
        decl: &FunctionDecl,
        args: CallArgs,
        unused: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        // Callee `Stmt::Line` markers must not leak into the caller:
        // diagnostics after the call report the call-site line.
        let saved_line = self.cur_line;
        // A callback invoked from inside a builtin's own machinery
        // (internal_cb: ob handlers, sort callbacks) has call site
        // `[internal function]`; engine callbacks like the error handler
        // invoked mid-eval instead report the builtin's own call site
        // (bug32828 vs bug28213).
        let from_builtin =
            self.internal_cb > 0 && self.call_trace.last().map(|f| f.internal).unwrap_or(false);
        let (site_file, site_line) = if from_builtin {
            ("[internal function]".to_string(), 0)
        } else {
            // Call-site file = the frame below the callee (the caller's
            // executing file); top-level calls report the file currently
            // being run.
            let sf = self
                .stack
                .iter()
                .rev()
                .nth(1)
                .map(|f| f.file.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| self.cur_file.clone());
            (sf, saved_line as u32)
        };
        // Trace args are the send list normalized through the last
        // bound slot (unbound params render null; named args appear in
        // declaration order — `test3(NULL, 'B')` in named_params/defaults).
        // Named args collected by a variadic stay keyed
        // (`test(1, 2, x: 3, y: 4)` in named_params/backtrace).
        let mut targs_named: Vec<(String, Cell)> = Vec::new();
        let targs: Vec<Cell> = if args.named.is_empty() {
            args.cells.clone()
        } else {
            let mut last: i64 = -1;
            for (i, p) in decl.params.iter().enumerate() {
                if p.variadic {
                    break;
                }
                if args.cells.get(i).is_some() || args.named.iter().any(|(n, ..)| *n == p.name) {
                    last = i as i64;
                }
            }
            let mut t: Vec<Cell> = Vec::new();
            for (i, p) in decl.params.iter().enumerate() {
                if p.variadic || i as i64 > last {
                    break;
                }
                let c = args
                    .cells
                    .get(i)
                    .cloned()
                    .or_else(|| {
                        args.named
                            .iter()
                            .find(|(n, ..)| *n == p.name)
                            .map(|(_, c, ..)| c.clone())
                    })
                    .unwrap_or_else(|| cell(Value::Null));
                t.push(c);
            }
            if decl.params.iter().any(|p| p.variadic) {
                // Variadic: every sent positional shows up; named args
                // that matched no declared param stay keyed.
                for (i, c) in args.cells.iter().enumerate() {
                    if i >= t.len() {
                        t.push(c.clone());
                    }
                }
                for (n, c, ..) in &args.named {
                    if !decl.params.iter().any(|p| !p.variadic && p.name == *n) {
                        targs_named.push((n.clone(), c.clone()));
                    }
                }
            }
            t
        };
        let fr = self
            .stack
            .last()
            .map(|f| TraceFrame {
                function: if f.fn_name.starts_with("{closure:") {
                    format!("{{closure:{}:{}}}", f.file, f.fn_line)
                } else {
                    f.fn_name.clone()
                },
                // A closure bound to $this without a real scope runs
                // on the "dummy scope" — traces show `Closure->`
                // (closure_038).
                class: f
                    .scope_class
                    .as_ref()
                    .map(|c| c.name().to_string())
                    .or_else(|| {
                        if f.fn_name.starts_with("{closure:") && f.this_obj.is_some() {
                            Some("Closure".to_string())
                        } else {
                            None
                        }
                    }),
                ty: if f.this_obj.is_some() {
                    "->"
                } else if f.scope_class.is_some() {
                    "::"
                } else {
                    ""
                }
                .to_string(),
                file: site_file.clone(),
                line: site_line,
                args: targs.clone(),
                named_args: targs_named.clone(),
                internal: false,
            })
            .unwrap_or_else(|| TraceFrame {
                function: decl.name.clone(),
                class: None,
                ty: String::new(),
                file: site_file,
                line: site_line,
                args: targs,
                named_args: targs_named,
                internal: false,
            });
        self.call_trace.push(fr);
        self.last_call_by_ref = decl.by_ref;
        let r = self.bind_and_run_inner(decl, args, unused);
        // Overwrite (don't restore): the flag must describe THIS callee even
        // though nested calls overwrote it during the body.
        self.last_call_by_ref = decl.by_ref;
        self.call_trace.pop();
        self.cur_line = saved_line;
        // Zend decrefs the frame's CVs at unwind — a local object
        // whose last strong refs are that frame's cells runs its
        // __destruct now (bug52361).
        if let Some(f) = self.last_popped_frame.take() {
            let _ = self.destruct_frame_objs(&f);
        }
        r
    }

    /// PHP's compile-time checks on typed params (tests/lang/type_hints_*):
    /// `= null` on a non-nullable type is the implicit-nullable deprecation;
    /// a scalar literal default on a class type is a fatal.
    /// `#[ReturnTypeWillChange]` is method-only — any other target is
    /// a compile fatal (variance/return_type_will_change_*).
    pub(in crate::interp) fn check_rtwc_attr(
        &self,
        attrs: &[crate::ast::AttrDecl],
        target: &str,
    ) -> Result<(), PhpError> {
        for a in attrs {
            let short = a.name.rsplit('\\').next().unwrap_or(&a.name);
            if short.eq_ignore_ascii_case("ReturnTypeWillChange") {
                return Err(PhpError::compile_fatal(
                    format!(
                        "Attribute \"ReturnTypeWillChange\" cannot target {} (allowed targets: method)",
                        target
                    ),
                    a.line,
                ));
            }
        }
        Ok(())
    }

    pub(in crate::interp) fn decl_type_checks(
        &mut self,
        fname: &str,
        decl: &FunctionDecl,
        cls_ctx: Option<(&str, Option<String>)>,
    ) -> Result<(), PhpError> {
        if cls_ctx.is_none() {
            self.check_rtwc_attr(&decl.attrs, "function")?;
        }
        let builtins = [
            "int", "float", "string", "bool", "array", "object", "callable", "iterable", "mixed",
            "void", "never", "false", "true", "self", "parent", "static", "null",
        ];
        let saved = self.cur_line;
        for p in &decl.params {
            let Some(ty) = &p.ty else { continue };
            self.cur_line = decl.line;
            // `mixed` already spans null (and `?mixed` is a parse error),
            // so `mixed $x = null` is never the implicit-nullable case.
            let nullable = ty
                .iter()
                .any(|m| m.eq_ignore_ascii_case("null") || m.eq_ignore_ascii_case("mixed"));
            let null_default = match &p.default {
                Some(Expr::Null) => true,
                Some(Expr::Const(c)) => c.eq_ignore_ascii_case("null"),
                _ => false,
            };
            match &p.default {
                _ if null_default => {
                    if !nullable
                        && self
                            .dep_seen
                            .insert(format!("{}\0{}\0{}", decl.file, decl.line, p.name))
                    {
                        self.deprecated(&format!(
                            "{}(): Implicitly marking parameter ${} as nullable is deprecated, the explicit nullable type must be used instead",
                            fname, p.name
                        ))?;
                    }
                }
                Some(Expr::Int(_))
                | Some(Expr::Float(_))
                | Some(Expr::Str(_))
                | Some(Expr::Bool(_))
                | Some(Expr::Interp(_)) => {
                    // An Interp made of only literal parts is still a
                    // string literal default (`"x"` lexes as Interp);
                    // one with real interpolations isn't a literal.
                    let literal_interp = !matches!(
                        &p.default,
                        Some(Expr::Interp(parts)) if !parts
                            .iter()
                            .all(|p| matches!(p, crate::lexer::StringPart::Lit(_)))
                    );
                    let kind = match p.default {
                        Some(Expr::Int(_)) => "int",
                        Some(Expr::Float(_)) => "float",
                        Some(Expr::Str(_)) | Some(Expr::Interp(_)) => "string",
                        _ => "bool",
                    };
                    if !literal_interp {
                        if let Some(ty) = &p.ty {
                            if let Err(e) = self.check_ty_redundant(ty, &cls_ctx) {
                                self.cur_line = saved;
                                return Err(e);
                            }
                        }
                        continue;
                    }
                    let mut disp = Self::zpp_ty_disp(ty);
                    disp.retain(|m| !m.eq_ignore_ascii_case("null"));
                    let tn = disp.join("|");
                    // A literal default must satisfy a member EXACTLY —
                    // the only widening is int -> float
                    // (scalar_float_with_invalid_default).
                    let lit_ok = disp
                        .iter()
                        .all(|m| builtins.contains(&m.to_lowercase().as_str()))
                        && (disp.iter().any(|m| m.eq_ignore_ascii_case("mixed"))
                            || match kind {
                                "int" => disp.iter().any(|m| {
                                    m.eq_ignore_ascii_case("int") || m.eq_ignore_ascii_case("float")
                                }),
                                "float" => disp.iter().any(|m| m.eq_ignore_ascii_case("float")),
                                "string" => disp.iter().any(|m| m.eq_ignore_ascii_case("string")),
                                _ => disp.iter().any(|m| m.eq_ignore_ascii_case("bool")),
                            });
                    if !lit_ok {
                        self.cur_line = saved;
                        return Err(PhpError::fatal(
                            format!(
                                "Cannot use {} as default value for parameter ${} of type {}",
                                kind, p.name, tn
                            ),
                            decl.line,
                        ));
                    }
                }
                _ => {}
            }
            if let Some(ty) = &p.ty {
                if let Err(e) = self.check_ty_redundant(ty, &cls_ctx) {
                    self.cur_line = saved;
                    return Err(e);
                }
            }
        }
        if let Some(ty) = &decl.ret {
            if let Err(e) = self.check_ty_redundant(ty, &cls_ctx) {
                self.cur_line = saved;
                return Err(e);
            }
            // `return;` (or any `return` under `never`) is a compile
            // error in typed functions — generators are exempt
            // (typed_return_without_value, never).
            if !Self::decl_contains_yield(&decl.body) {
                let never = ty.iter().any(|m| m.eq_ignore_ascii_case("never"));
                let void = ty.iter().all(|m| m.eq_ignore_ascii_case("void"));
                if never {
                    if let Some(l) = Self::first_return_line(&decl.body, decl.line, false) {
                        self.cur_line = saved;
                        return Err(PhpError::compile_fatal(
                            "A never-returning function must not return",
                            l,
                        ));
                    }
                } else if !void {
                    if let Some(l) = Self::first_return_line(&decl.body, decl.line, true) {
                        self.cur_line = saved;
                        let hint = if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) {
                            " (did you mean \"return null;\" instead of \"return;\"?)"
                        } else {
                            ""
                        };
                        return Err(PhpError::compile_fatal(
                            format!("A function with return type must return a value{hint}"),
                            l,
                        ));
                    }
                }
            }
        }
        self.cur_line = saved;
        Ok(())
    }

    /// First `return`'s line within a body — `bare_only` restricts to
    /// value-less `return;`. Nested function/closure/class bodies are
    /// their own scope and skipped (typed_return_without_value).
    fn first_return_line(stmts: &[Stmt], mut cur: usize, bare_only: bool) -> Option<usize> {
        for s in stmts {
            match s {
                Stmt::Line(l) => cur = *l,
                Stmt::Return(e) if e.is_none() || !bare_only => return Some(cur),
                Stmt::Return(_) => {}
                Stmt::Block(b) => {
                    if let Some(l) = Self::first_return_line(b, cur, bare_only) {
                        return Some(l);
                    }
                }
                Stmt::If { then, else_, .. } => {
                    if let Some(l) = Self::first_return_line(then, cur, bare_only)
                        .or_else(|| Self::first_return_line(else_, cur, bare_only))
                    {
                        return Some(l);
                    }
                }
                Stmt::While { body, .. }
                | Stmt::DoWhile { body, .. }
                | Stmt::For { body, .. }
                | Stmt::Foreach { body, .. } => {
                    if let Some(l) = Self::first_return_line(body, cur, bare_only) {
                        return Some(l);
                    }
                }
                Stmt::Switch { cases, .. } => {
                    for (_, b) in cases {
                        if let Some(l) = Self::first_return_line(b, cur, bare_only) {
                            return Some(l);
                        }
                    }
                }
                Stmt::Try {
                    body,
                    catches,
                    finally,
                } => {
                    if let Some(l) = Self::first_return_line(body, cur, bare_only)
                        .or_else(|| {
                            catches
                                .iter()
                                .find_map(|c| Self::first_return_line(&c.body, cur, bare_only))
                        })
                        .or_else(|| {
                            finally
                                .as_ref()
                                .and_then(|b| Self::first_return_line(b, cur, bare_only))
                        })
                    {
                        return Some(l);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Union-type redundancy rules at decl time (PHP 8.x compile
    /// checks): iterable expands to array|Traversable; self/parent/
    /// static resolve against the declaring class; reports the member
    /// as written (or the expanded member it collides with).
    pub(in crate::interp) fn check_ty_redundant(
        &mut self,
        ty: &[String],
        cls: &Option<(&str, Option<String>)>,
    ) -> Result<(), PhpError> {
        // Confusable-type warnings (`integer`/`double`/`boolean`/
        // `resource` as class names) are a compile-time diagnostic
        // emitted by the parser — it owns the written-vs-resolved
        // distinction and the use-import table (confusable_type_warning).
        // Intersection conjuncts may only be class-like names — any
        // builtin scalar/compound member is a compile error
        // (invalid_types/*).
        const NON_CLASS: &[&str] = &[
            "int", "float", "string", "bool", "array", "object", "callable", "iterable", "mixed",
            "void", "never", "false", "true", "null", "numeric", "resource",
        ];
        for m in ty {
            if !m.contains('&') {
                continue;
            }
            for part in m.split('&') {
                let p = part.trim_start_matches('\\');
                if NON_CLASS.contains(&p.to_lowercase().as_str()) {
                    return Err(PhpError::fatal(
                        format!("Type {p} cannot be part of an intersection type"),
                        self.cur_line,
                    ));
                }
            }
        }
        // Conjunct-level dedupe inside an intersection member:
        // `A&A` (or `A&B` where B aliases A via `use`) is redundant.
        for m in ty {
            if !m.contains('&') {
                continue;
            }
            let mut conj: Vec<String> = Vec::new();
            for c in m.split('&') {
                if conj.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                    return Err(PhpError::fatal(
                        format!("Duplicate type {} is redundant", c),
                        self.cur_line,
                    ));
                }
                conj.push(c.to_string());
            }
        }
        if ty.len() < 2 {
            return Ok(());
        }
        let builtins = [
            "int",
            "float",
            "string",
            "bool",
            "array",
            "object",
            "callable",
            "iterable",
            "mixed",
            "void",
            "never",
            "false",
            "true",
            "null",
            "traversable",
        ];
        // `T|object` — a class member (incl. an intersection of
        // classes) alongside `object` is redundant
        // (dnf_types/redundant_types/object_and_dnf_type).
        if ty.iter().any(|m| m.eq_ignore_ascii_case("object"))
            && ty
                .iter()
                .any(|m| !builtins.contains(&m.to_lowercase().as_str()))
        {
            return Err(PhpError::fatal(
                format!(
                    "Type {} contains both object and a class type, which is redundant",
                    ty_norm_disp(ty)
                ),
                self.cur_line,
            ));
        }
        // `A&B|A` / `(A&B&C)|(A&B)` — an intersection member is
        // redundant when another member already covers it by name
        // (less_restrive_type_constraint_already_present*).
        for (i, m) in ty.iter().enumerate() {
            if !m.contains('&') {
                continue;
            }
            let conj: Vec<&str> = m.split('&').collect();
            for (j, s) in ty.iter().enumerate() {
                if i == j {
                    continue;
                }
                // Identical members are the seen-loop's "redundant
                // with" case, not the restrictive one.
                if m.eq_ignore_ascii_case(s) {
                    continue;
                }
                let covers = if s.contains('&') {
                    s.split('&')
                        .all(|sc| conj.iter().any(|c| c.eq_ignore_ascii_case(sc)))
                } else {
                    conj.iter().any(|c| c.eq_ignore_ascii_case(s))
                };
                if covers {
                    return Err(PhpError::fatal(
                        format!(
                            "Type {} is redundant as it is more restrictive than type {}",
                            m, s
                        ),
                        self.cur_line,
                    ));
                }
            }
        }
        let mut seen: Vec<(String, String)> = Vec::new();
        let mut seen_true = false;
        let mut seen_false = false;
        // Zend dedupes builtin scalars before class names: for
        // `iterable|iterable` the reported dup is `array`, not
        // `Traversable` (iterable_alias_redundancy_iterable).
        const DEDUP_BUILTINS: &[&str] = &[
            "int", "float", "string", "bool", "array", "object", "callable", "iterable", "mixed",
            "void", "never", "false", "true", "null",
        ];
        let ordered: Vec<&String> = ty
            .iter()
            .filter(|m| DEDUP_BUILTINS.contains(&m.to_lowercase().as_str()))
            .chain(
                ty.iter()
                    .filter(|m| !DEDUP_BUILTINS.contains(&m.to_lowercase().as_str())),
            )
            .collect();
        for m in ordered {
            let l = m.to_lowercase();
            let exps: Vec<String> = if l == "iterable" {
                vec!["array".into(), "Traversable".into()]
            } else {
                vec![m.clone()]
            };
            for e in exps {
                // self/parent resolve for comparison only — `static`
                // stays itself (`static|self` is not redundant;
                // static_to_self_to_unions).
                let cmp = {
                    let el = e.to_lowercase();
                    match (el.as_str(), cls.as_ref()) {
                        ("self", Some((c, _))) => c.to_string(),
                        ("parent", Some((_, p))) => p.clone().unwrap_or_else(|| e.clone()),
                        _ => e.clone(),
                    }
                };
                let dup = seen.iter().any(|(s, _)| {
                    s.eq_ignore_ascii_case(&cmp)
                        || (cmp.eq_ignore_ascii_case("false") || cmp.eq_ignore_ascii_case("true"))
                            && s.eq_ignore_ascii_case("bool")
                        || cmp.eq_ignore_ascii_case("closure") && s.eq_ignore_ascii_case("callable")
                });
                if dup {
                    // Identical intersection members report differently
                    // from plain dups (duplicate_class_alias_type).
                    if cmp.contains('&') {
                        let other = seen
                            .iter()
                            .find(|(s, _)| s.eq_ignore_ascii_case(&cmp))
                            .map(|(_, w)| w.clone())
                            .unwrap_or_else(|| e.clone());
                        return Err(PhpError::fatal(
                            format!("Type {} is redundant with type {}", e, other),
                            self.cur_line,
                        ));
                    }
                    if cmp.eq_ignore_ascii_case("null") {
                        return Err(PhpError::fatal(
                            "null cannot be marked as nullable".to_string(),
                            self.cur_line,
                        ));
                    }
                    let el = e.to_lowercase();
                    let builtin = [
                        "int", "float", "string", "bool", "array", "object", "callable",
                        "iterable", "mixed", "void", "never", "false", "true", "null",
                    ]
                    .contains(&el.as_str());
                    let shown = if el == "static" {
                        e.clone()
                    } else if ["self", "parent"].contains(&el.as_str()) {
                        cmp.clone()
                    } else if builtin {
                        el.clone()
                    } else {
                        e.clone()
                    };
                    return Err(PhpError::fatal(
                        format!("Duplicate type {} is redundant", shown),
                        self.cur_line,
                    ));
                }
                if cmp.eq_ignore_ascii_case("true") {
                    seen_true = true;
                }
                if cmp.eq_ignore_ascii_case("false") {
                    seen_false = true;
                }
                seen.push((cmp, e));
            }
        }
        if seen_true && seen_false {
            return Err(PhpError::fatal(
                "Type contains both true and false, bool must be used instead".to_string(),
                self.cur_line,
            ));
        }
        Ok(())
    }

    /// `static` members resolve to the called class for checks and
    /// messages; unbound (unscoped closure) stays literal `static`
    /// (static_type_return).
    fn resolve_static(&self, ty: &[String]) -> Vec<String> {
        if !ty.iter().any(|m| m.eq_ignore_ascii_case("static")) {
            return ty.to_vec();
        }
        let cn = self
            .stack
            .last()
            .and_then(|f| {
                f.called_class
                    .as_ref()
                    .or(f.scope_class.as_ref())
                    .or(f.decl_class.as_ref())
            })
            .map(|c| c.name().to_string());
        ty.iter()
            .map(|m| {
                if m.eq_ignore_ascii_case("static") {
                    cn.clone().unwrap_or_else(|| m.clone())
                } else {
                    m.clone()
                }
            })
            .collect()
    }

    /// Scalar literal default check context ends; whether `v` satisfies a
    /// type member — scalar builtins pass (weak-mode coercion territory).
    pub(in crate::interp) fn param_type_match(&mut self, m: &str, v: &Value) -> bool {
        // Intersection member `A&B`: every part must match
        // (intersection_types/variance).
        if m.contains('&') && !m.starts_with('(') {
            let parts: Vec<String> = m.split('&').map(|p| p.to_string()).collect();
            return parts.iter().all(|p| self.param_type_match(p, v));
        }
        let l = m.to_lowercase();
        match l.as_str() {
            "null" => matches!(v, Value::Null),
            "mixed" | "void" | "never" | "self" | "parent" => true,
            // `static` = instance of the called class (late static);
            // unresolved (unbound closure) it can match nothing —
            // displayed literally (static_type_return).
            "static" => match v {
                Value::Object(o) => {
                    let cn = self
                        .stack
                        .last()
                        .and_then(|f| {
                            f.called_class
                                .as_ref()
                                .or(f.scope_class.as_ref())
                                .or(f.decl_class.as_ref())
                        })
                        .map(|c| c.name().to_string())
                        .unwrap_or_else(|| "\u{1}static".to_string());
                    self.obj_is_a(o, &cn)
                }
                _ => false,
            },
            "false" => matches!(v, Value::Bool(false)),
            "true" => matches!(v, Value::Bool(true)),
            // Weak-mode scalar params accept what coercion can convert:
            // non-numeric strings are a TypeError, not silent (trait_type_errors).
            "int" => match v {
                Value::Int(_) | Value::Bool(_) => true,
                // Out-of-range/NaN floats can't coerce -> TypeError
                // (scalar_return_basic_64bit).
                Value::Float(f) => {
                    f.is_finite() && *f < 9.223372036854776e18 && *f >= -9.223372036854776e18
                }
                // Only well-formed numeric strings pass — `"1a"` and
                // `"0x1A"` are a TypeError in PHP 8 weak mode.
                Value::Str(b) => matches!(numeric(b), Numeric::Int(_) | Numeric::Float(_)),
                _ => false,
            },
            "float" => match v {
                Value::Int(_) | Value::Float(_) | Value::Bool(_) => true,
                Value::Str(b) => matches!(numeric(b), Numeric::Int(_) | Numeric::Float(_)),
                _ => false,
            },
            "string" => match v {
                Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Str(_) => true,
                // Objects coerce via __toString only — a stdClass is a
                // TypeError, not "" (scalar_return_basic_64bit).
                Value::Object(o) => {
                    let tcls = o.borrow().class.clone();
                    self.find_method_in(&tcls, "__tostring").is_some()
                }
                _ => false,
            },
            "bool" => matches!(
                v,
                Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Str(_)
            ),
            "array" => matches!(v, Value::Array(_)),
            "iterable" => {
                matches!(v, Value::Array(_))
                    || matches!(v, Value::Object(o) if self.obj_is_a(o, "Traversable"))
            }
            "callable" => self.is_callable_value(v),
            "object" => matches!(v, Value::Object(_)),
            // Named class/interface — instanceof check. `Closure` is
            // our Callable value's class (constexpr/default_args).
            _ => match v {
                Value::Callable(_) => m.eq_ignore_ascii_case("closure"),
                Value::Object(o) => self.obj_is_a(o, m),
                _ => false,
            },
        }
    }

    /// Weak-mode scalar coercion for typed params/returns: returns the
    /// coerced value, or None when no scalar member applies (objects
    /// pass through unchanged).
    fn coerce_scalar(&mut self, ty: &[String], v: &Value) -> Option<Value> {
        // A null value is never coerced to a scalar — `?T` params keep
        // null (scalar_null). The caller's `ok` check gates the member.
        if matches!(v, Value::Null) {
            return if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) {
                Some(Value::Null)
            } else {
                None
            };
        }
        // Zend weak-union coercion preference (type_checking_weak): a
        // numeric string picks the member matching its own kind first
        // (`"42.0"` prefers `float`), then members are tried in scalar
        // order int -> float -> string -> bool family. Non-scalar
        // members never coerce.
        let has = |n: &str| ty.iter().any(|m| m.eq_ignore_ascii_case(n));
        let kind_flt = matches!(v, Value::Str(b) if matches!(numeric(b), Numeric::Float(_)));
        let mut order: Vec<String> = Vec::with_capacity(4);
        if kind_flt && has("float") {
            order.push("float".into());
        }
        for n in ["int", "float", "string"] {
            if has(n) && !order.iter().any(|o| o == n) {
                order.push(n.into());
            }
        }
        if let Some(b) = ty
            .iter()
            .find(|m| matches!(m.to_lowercase().as_str(), "bool" | "false" | "true"))
        {
            order.push(b.clone());
        }
        for m in &order {
            let l = m.to_lowercase();
            match l.as_str() {
                "int" => match v {
                    Value::Int(_) => return Some(v.clone()),
                    Value::Float(f)
                        if f.is_finite()
                            && *f < 9.223372036854776e18
                            && *f >= -9.223372036854776e18 =>
                    {
                        return Some(Value::Int(*f as i64));
                    }
                    Value::Bool(b) => return Some(Value::Int(*b as i64)),
                    Value::Str(b) => match numeric(b) {
                        Numeric::Int(i) => return Some(Value::Int(i)),
                        Numeric::Float(f) => return Some(Value::Int(f as i64)),
                        _ => {}
                    },
                    _ => {}
                },
                "float" => match v {
                    Value::Float(_) => return Some(v.clone()),
                    Value::Int(i) => return Some(Value::Float(*i as f64)),
                    Value::Bool(b) => return Some(Value::Float(*b as i64 as f64)),
                    Value::Str(b) => match numeric(b) {
                        Numeric::Int(i) => return Some(Value::Float(i as f64)),
                        Numeric::Float(f) => return Some(Value::Float(f)),
                        _ => {}
                    },
                    _ => {}
                },
                "string" => {
                    if let Value::Float(f) = v {
                        if f.is_nan() {
                            let _ = self.emit_diag(
                                "Warning",
                                2,
                                "unexpected NAN value was coerced to string",
                            );
                        }
                    }
                    if let Ok(b) = self.conv_bytes(v) {
                        return Some(Value::Str(b.into()));
                    }
                }
                "bool" | "false" | "true" => {
                    if let Value::Float(f) = v {
                        if f.is_nan() {
                            let _ = self.emit_diag(
                                "Warning",
                                2,
                                "unexpected NAN value was coerced to bool",
                            );
                        }
                    }
                    let t = v.is_truthy();
                    // Standalone `false`/`true` members only accept
                    // values that coerce to exactly that bool.
                    if l == "bool" || t == (l == "true") {
                        return Some(Value::Bool(t));
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Strict value-in-members test for the weak path: weak union
    /// coercion only applies when the value doesn't exactly match a
    /// member — `bool|array` + `[]` stays `[]`, `float|int` + 1 stays
    /// int(1) (union_types/type_checking_weak, legal_default_values).
    /// Unlike `ty_exact` (strict boundary), an int is NOT exact for
    /// `float` — it still widens through coercion.
    fn ty_weak_exact(&mut self, ty: &[String], v: &Value) -> bool {
        ty.iter().any(|m| {
            let l = m.to_lowercase();
            match l.as_str() {
                "int" => matches!(v, Value::Int(_)),
                "float" => matches!(v, Value::Float(_)),
                "string" => matches!(v, Value::Str(_)),
                "bool" => matches!(v, Value::Bool(_)),
                "false" => matches!(v, Value::Bool(false)),
                "true" => matches!(v, Value::Bool(true)),
                "null" => matches!(v, Value::Null),
                "array" => matches!(v, Value::Array(_)),
                "iterable" => {
                    matches!(v, Value::Array(_))
                        || matches!(v, Value::Object(o) if self.obj_is_a(o, "Traversable"))
                }
                "object" => matches!(v, Value::Object(_)),
                "callable" => self.is_callable_value(v),
                "mixed" | "void" | "never" | "self" | "static" | "parent" => true,
                _ if m.contains('&') => {
                    let ok = m
                        .trim_start_matches('(')
                        .trim_end_matches(')')
                        .split('&')
                        .all(|p| self.ty_weak_exact(&[p.to_string()], v));
                    ok
                }
                _ => match v {
                    Value::Callable(_) => m.eq_ignore_ascii_case("closure"),
                    Value::Object(o) => self.obj_is_a(o, m),
                    _ => false,
                },
            }
        })
    }

    /// Is the currently-executing code inside a `strict_types=1` file?
    /// (prop writes, const writes, incdec — Zend uses the writer's file.)
    pub(in crate::interp) fn exec_file_strict(&self) -> bool {
        self.stack
            .last()
            .map(|f| self.strict_files.contains(&f.file))
            .unwrap_or_else(|| self.strict_files.contains(&self.cur_file))
    }

    /// Strictness for argument checks is determined by the file holding
    /// the call site — the frame just below the callee's own.
    pub(in crate::interp) fn caller_file_strict(&self) -> bool {
        if self.stack.len() < 2 {
            // Top-level call site: `self.globals` lives off `self.stack`,
            // so the caller is the top-level file currently executing
            // (`cur_file` swaps for includes mid-eval).
            return self.strict_files.contains(&self.cur_file);
        }
        self.strict_files
            .contains(&self.stack[self.stack.len() - 2].file)
    }

    /// `callable` accepts an actual callable: a Closure/FCC value, a
    /// function-name string, a `"Class::method"` string, a `[cls|obj, m]`
    /// pair, or an object with `__invoke` (callable_001).
    /// `is_callable($v, $syntax_only, $name)` name written back
    /// (closure_016): syntax_only gives the `Class::m` / closure's
    /// Zend-name form; the default form is the engine's
    /// `Class::__invoke` / `Closure::__invoke`.
    pub fn callable_name_of(&mut self, v: &Value, _syntax_only: bool) -> Option<String> {
        match v {
            Value::Callable(c) => Some(match &c.kind {
                // A Closure's name is always its Zend name, syntax
                // flag or not (closure_016).
                CallableKind::Closure(d) => d.name.clone(),
                CallableKind::Named(n) => n.trim_start_matches('\\').to_string(),
                CallableKind::Method { obj, class, name } => {
                    let cn = obj
                        .as_ref()
                        .map(|o| o.borrow().class.name().to_string())
                        .or_else(|| class.as_ref().map(|c| c.name().to_string()))
                        .unwrap_or_else(|| "Closure".into());
                    format!("{}::{}", cn, name)
                }
            }),
            Value::Object(o) => {
                let c = o.borrow().class.clone();
                self.find_method_in(&c, "__invoke")?;
                Some(format!("{}::__invoke", c.name()))
            }
            Value::Str(s) => Some(crate::value::lossy(s).trim_start_matches('\\').to_string()),
            Value::Array(a) => {
                let arr = a.borrow();
                let (f, m) = (
                    arr.get(&crate::value::ArrKey::Int(0))?,
                    arr.get(&crate::value::ArrKey::Int(1))?,
                );
                let mn = m.to_php_string();
                match f {
                    // `[$closure, '__invoke']` canonicalizes to
                    // `Closure::__invoke` in $name (closure_016).
                    Value::Callable(_c) if mn.eq_ignore_ascii_case("__invoke") => {
                        Some("Closure::__invoke".into())
                    }
                    Value::Object(o) => Some(format!("{}::{}", o.borrow().class.name(), mn)),
                    Value::Str(cn) => Some(format!("{}::{}", crate::value::lossy(&cn), mn)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub fn is_callable_value(&mut self, v: &Value) -> bool {
        match v {
            Value::Callable(_) => true,
            Value::Str(s) => {
                let s = String::from_utf8_lossy(s).to_string();
                if self.functions.contains_key(&s.to_lowercase())
                    || builtins::is_builtin(&s.to_lowercase())
                    || builtins::builtin_params(&s.to_lowercase()).is_some()
                {
                    return true;
                }
                let Some((cn, mn)) = s.split_once("::") else {
                    return false;
                };
                let Some(c) = self.classes.get(&cn.to_lowercase()).cloned() else {
                    return false;
                };
                // "Class::method" strings only call statics
                // (callable_001) — or anything when __callStatic
                // trampolines them.
                self.find_method_in(&c, mn)
                    .map(|(mm, _)| mm.is_static)
                    .unwrap_or(false)
                    || self.find_method_in(&c, "__callstatic").is_some()
            }
            Value::Object(o) => {
                let cn = o.borrow().class.decl.name.clone();
                let Some(c) = self.classes.get(&cn.to_lowercase()).cloned() else {
                    return false;
                };
                self.find_method_in(&c, "__invoke").is_some()
            }
            Value::Array(a) => {
                let arr = a.borrow();
                let first = arr.get(&crate::value::ArrKey::Int(0));
                let second = arr.get(&crate::value::ArrKey::Int(1));
                let (Some(first), Some(second)) = (first, second) else {
                    return false;
                };
                let Value::Str(mn) = &second else {
                    return false;
                };
                let mn = String::from_utf8_lossy(mn).to_string();
                let (cn, need_static) = match &first {
                    Value::Str(cn) => (String::from_utf8_lossy(cn).to_string(), true),
                    Value::Callable(_) => {
                        return mn.eq_ignore_ascii_case("__invoke");
                    }
                    Value::Object(o) => (o.borrow().class.decl.name.clone(), false),
                    _ => return false,
                };
                let Some(c) = self.classes.get(&cn.to_lowercase()).cloned() else {
                    return false;
                };
                // [class-string, method] only calls statics; [obj, m]
                // calls any (callable_001). __call/__callStatic make
                // any name callable (zend_is_callable's trampoline).
                self.find_method_in(&c, &mn)
                    .map(|(mm, _)| mm.is_static || !need_static)
                    .unwrap_or(false)
                    || self
                        .find_method_in(&c, if need_static { "__callstatic" } else { "__call" })
                        .is_some()
            }
            _ => false,
        }
    }

    /// PHP's "given" type word in TypeError messages.
    pub(in crate::interp) fn zval_type_name(&self, v: &Value) -> String {
        match v {
            Value::Null => "null".into(),
            Value::Bool(b) => if *b { "true" } else { "false" }.into(),
            Value::Int(_) => "int".into(),
            Value::Float(_) => "float".into(),
            Value::Str(_) => "string".into(),
            Value::Array(_) => "array".into(),
            Value::Object(o) => o.borrow().class.name().to_string(),
            Value::Callable(_) => "Closure".into(),
            Value::Resource(_) => "resource".into(),
        }
    }

    /// Strict-mode ZPP arg check for internal functions: `?` = nullable,
    /// `|` = union; int widens to float, everything else must match
    /// exactly (no scalar coercion, no __toString).
    pub(in crate::interp) fn zpp_strict_ok(&mut self, pty: &str, v: &Value) -> bool {
        let (pty, nullable) = match pty.strip_prefix('?') {
            Some(t) => (t, true),
            None => (pty, false),
        };
        if nullable && matches!(v, Value::Null) {
            return true;
        }
        pty.split('|').any(|t| match t {
            "string" => matches!(v, Value::Str(_)),
            "int" => matches!(v, Value::Int(_)),
            "float" => matches!(v, Value::Float(_) | Value::Int(_)),
            "bool" => matches!(v, Value::Bool(_)),
            "array" => matches!(v, Value::Array(_)),
            "object" => matches!(v, Value::Object(_) | Value::Callable(_)),
            "callable" => self.is_callable_value(v),
            "iterable" => {
                matches!(v, Value::Array(_))
                    || matches!(v, Value::Object(o) if {
                        let n = o.borrow().class.name().to_string();
                        self.is_a_str(&n, "traversable")
                    })
            }
            "resource" => matches!(v, Value::Resource(_)),
            _ => true, // mixed and unknown tags accept everything
        })
    }

    /// Zend's callback-validation error detail for internal functions
    /// (the part after `must be a valid callback`/`or null,`).
    pub(in crate::interp) fn zpp_callback_detail(&mut self, v: &Value) -> String {
        match v {
            Value::Array(a) => {
                let a = a.borrow();
                let mut it = a.entries.iter();
                match it.next() {
                    None => "first array member is not a valid class name or object".into(),
                    Some((_, c0)) => {
                        if !matches!(&*c0.borrow(), Value::Str(_) | Value::Object(_)) {
                            return "first array member is not a valid class name or object".into();
                        }
                        let c0 = c0.borrow().clone();
                        let second = it.next().map(|(_, c)| c.borrow().clone());
                        let Some(Value::Str(m)) = second else {
                            return "second array member is not a valid method".into();
                        };
                        let m = String::from_utf8_lossy(&m).to_string();
                        match c0 {
                            Value::Str(cn) => format!(
                                "class {} does not have a method \"{}\"",
                                String::from_utf8_lossy(&cn),
                                m
                            ),
                            Value::Object(o) => format!(
                                "class {} does not have a method \"{}\"",
                                o.borrow().class.name(),
                                m
                            ),
                            _ => "first array member is not a valid class name or object".into(),
                        }
                    }
                }
            }
            Value::Str(s) => format!(
                "function \"{}\" not found or invalid function name",
                String::from_utf8_lossy(s)
            ),
            _ => "no array or string given".into(),
        }
    }

    /// ZPP-style type display: `iterable` expands to `Traversable|array`
    /// in param/return TypeErrors and default-value fatals (iterable_*).
    fn zpp_ty_disp(ty: &[String]) -> Vec<String> {
        ty.iter()
            .flat_map(|m| {
                if m.eq_ignore_ascii_case("iterable") {
                    vec!["Traversable".to_string(), "array".to_string()]
                } else if let Some(pos) = m.find("@anonymous$") {
                    vec![format!("{}@anonymous", &m[..pos])]
                } else if m.contains('&') && ty.len() > 1 {
                    // Intersection members parenthesize inside a union
                    // ((X&Y)|(W&Z) — dnf_2_intersection).
                    vec![format!("({m})")]
                } else {
                    vec![m.clone()]
                }
            })
            .collect()
    }

    /// Display name for a decl in diagnostics — closures are named
    /// `{closure:FILE:LINE}` like Zend (named_params/call_user_func).
    fn decl_fname(&self, decl: &FunctionDecl) -> String {
        let base = if decl.name.is_empty() {
            format!("{{closure:{}:{}}}", decl.file, decl.line)
        } else {
            decl.name.clone()
        };
        self.stack
            .last()
            .and_then(|f| f.decl_class.as_ref().map(|c| c.name().to_string()))
            .map(|c| format!("{}::{}", c, base))
            .unwrap_or(base)
    }

    fn bind_and_run_inner(
        &mut self,
        decl: &FunctionDecl,
        args: CallArgs,
        _unused: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        let required = decl
            .params
            .iter()
            .filter(|p| p.default.is_none() && !p.variadic)
            .count();
        // With named args, missing-required is reported per-param during
        // binding ("Argument #N ($x) not passed"); the count check below
        // is the positional-only form.
        if args.named.is_empty() && args.len() < required {
            self.stack.pop();
            return self.fail(PhpError::uncaught(
                "ArgumentCountError",
                format!(
                    "Too few arguments to function {}(), {} passed in {} on line {} and {} {} expected",
                    self.decl_fname(decl),
                    args.len(),
                    self.diag_file(),
                    self.cur_line,
                    if required == decl.params.len() { "exactly" } else { "at least" },
                    required
                ),
                0,
            ));
        }
        // Named arguments resolve against decl.params by name
        // (Zend/tests/named_params): unknown names land in a trailing
        // variadic's array as string keys, else "Unknown named
        // parameter"; a name colliding with a positional or a prior
        // named arg is the "overwrites previous argument" Error.
        let n_pos = args.cells.len();
        let mut by_name: Vec<Option<(Cell, bool, bool)>> = vec![None; decl.params.len()];
        let mut variadic_named: Vec<(String, Cell)> = Vec::new();
        let has_variadic = decl.params.iter().any(|p| p.variadic);
        for (n, c, refable, trav) in &args.named {
            match decl.params.iter().position(|p| !p.variadic && p.name == *n) {
                Some(j) if j < n_pos || by_name[j].is_some() => {
                    self.stack.pop();
                    // Caller-side arg-verify error: the callee frame
                    // never existed (gh19653_2).
                    if self
                        .call_trace
                        .last()
                        .map(|f| f.function == decl.name)
                        .unwrap_or(false)
                    {
                        self.call_trace.pop();
                    }
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Named parameter ${} overwrites previous argument", n),
                        0,
                    ));
                }
                Some(j) => by_name[j] = Some((c.clone(), *refable, *trav)),
                None if has_variadic => variadic_named.push((n.clone(), c.clone())),
                None => {
                    self.stack.pop();
                    if self
                        .call_trace
                        .last()
                        .map(|f| f.function == decl.name)
                        .unwrap_or(false)
                    {
                        self.call_trace.pop();
                    }
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Unknown named parameter ${}", n),
                        0,
                    ));
                }
            }
        }
        // Enforce declared param types (tests/lang/type_hints_*.phpt).
        for (i, p) in decl.params.iter().enumerate() {
            let (Some(ty), Some(a)) = (
                &p.ty,
                args.cells.get(i).or(by_name[i].as_ref().map(|t| &t.0)),
            ) else {
                continue;
            };
            if p.by_ref {
                // By-ref params bind cells, not values — the contained
                // value isn't checked at the boundary (typed_properties_010),
                // but weak scalar args still coerce into the caller's
                // cell (scalar_weak_reference).
                if !self.caller_file_strict() {
                    let bv = a.borrow().clone();
                    if !self.ty_weak_exact(ty, &bv) {
                        if let Some(cv) = self.coerce_scalar(ty, &bv) {
                            *a.borrow_mut() = cv;
                        }
                    }
                }
                continue;
            }
            let v = a.borrow().clone();
            let implicit_null = !ty.iter().any(|m| m.eq_ignore_ascii_case("null"))
                && match &p.default {
                    Some(Expr::Null) => true,
                    Some(Expr::Const(c)) => c.eq_ignore_ascii_case("null"),
                    _ => false,
                };
            let ok = (implicit_null && matches!(v, Value::Null))
                || if self.caller_file_strict() {
                    self.ty_exact(ty, &v)
                } else {
                    ty.iter().any(|m| self.param_type_match(m, &v))
                };
            let caller_strict = self.caller_file_strict();
            if ok && !caller_strict && !self.ty_weak_exact(ty, &v) {
                if let Some(cv) = self.coerce_scalar(ty, &v) {
                    // Arg-coercion deprecations attribute to the
                    // callee's declaration line (scalar_basic).
                    let pl = self.cur_line;
                    self.cur_line = decl.line;
                    self.deprecate_lossy_int(ty, &v, &cv);
                    self.cur_line = pl;
                    *a.borrow_mut() = cv;
                }
            }
            // strict mode still allows the int->float widening stored
            // back for visibility in the callee.
            if ok
                && caller_strict
                && ty.iter().any(|m| m.eq_ignore_ascii_case("float"))
                && !self.ty_weak_exact(ty, &v)
            {
                if let Value::Int(i) = v {
                    *a.borrow_mut() = Value::Float(i as f64);
                }
            }
            if !ok {
                let mut fname = self.decl_fname(decl);
                // Anonymous-class methods report args under just the
                // class name (union_types/anonymous_class).
                if let Some(pos) = fname.find("@anonymous::") {
                    fname = fname[..pos + "@anonymous".len()].into();
                }
                // Implicit-nullable needs the phantom `null` member so
                // an intersection renders `(X&Y)|null`
                // (implicit_nullable_intersection_type_error).
                let tyv: Vec<String> = if implicit_null {
                    let mut t = ty.to_vec();
                    t.push("null".into());
                    t
                } else {
                    ty.to_vec()
                };
                let mut disp: Vec<String> = Self::zpp_ty_disp(&self.resolve_static(&tyv));
                disp.retain(|m| !m.eq_ignore_ascii_case("null"));
                if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) || implicit_null {
                    if disp.len() == 1 && !disp[0].contains('&') {
                        disp[0] = format!("?{}", disp[0]);
                    } else {
                        disp.push("null".into());
                    }
                }
                let given = self.zval_type_name(&v);
                // getMessage() is the short form; the uncaught display
                // appends ` and defined in FILE:M` (catchable_error_002).
                // The ", called in FILE on line" suffix only applies to
                // function-call style invocations — invoking through the
                // internal `Closure::__invoke` ( `$f->__invoke()` or
                // `[$f,'__invoke']`) drops it (closure_059).
                let call_alias = self.stack.last().and_then(|f| f.call_alias.clone());
                let msg = if call_alias.is_some() {
                    format!(
                        "{}(): Argument #{} (${}) must be of type {}, {} given",
                        fname,
                        i + 1,
                        p.name,
                        disp.join("|"),
                        given,
                    )
                } else {
                    format!(
                        "{}(): Argument #{} (${}) must be of type {}, {} given, called in {} on line {}",
                        fname,
                        i + 1,
                        p.name,
                        disp.join("|"),
                        given,
                        self.diag_file(),
                        self.cur_line
                    )
                };
                let display = format!("{} and defined", msg);
                let argdesc = args
                    .iter()
                    .map(|a| trace_arg(&a.borrow()))
                    .collect::<Vec<_>>()
                    .join(", ");
                // Trace frames render `->` for instance calls while the
                // message keeps `::` (namespaces/ns_071).
                let arrow = if self
                    .stack
                    .last()
                    .and_then(|f| f.this_obj.as_ref())
                    .is_some()
                {
                    "->"
                } else {
                    "::"
                };
                let tname = fname.replacen("::", arrow, 1);
                let frame = format!(
                    "{}({}): {}({})",
                    self.diag_file(),
                    self.cur_line,
                    tname,
                    argdesc
                );
                let call_line = self.cur_line;
                // Frames below the call site (include/require and
                // outer calls) join the synthetic #0 — the callee's
                // own trace frame is the top of call_trace.
                let mut frs = vec![frame];
                for fr in self.call_trace.iter().rev().skip(1) {
                    if crate::value::trace_frame_hidden(fr) {
                        continue;
                    }
                    frs.push(crate::value::trace_frame_str(fr));
                }
                self.stack.pop();
                let mut e = PhpError::uncaught("TypeError", msg, call_line);
                e.trace = Some(frs);
                e.thrown_line = Some(decl.line);
                e.display_msg = Some(display);
                let r = self.fail(e);
                if let Some(Value::Object(o)) = &self.pending_exception {
                    if let Some(crate::value::ObjectInternal::Exception { file, .. }) =
                        &mut o.borrow_mut().internal
                    {
                        *file = decl.file.clone();
                    }
                }
                self.last_err_file = decl.file.clone();
                return r;
            }
        }
        {
            // Compute param bindings first (defaults may eval exprs that
            // need &mut self).
            let mut binds: Vec<(String, Cell)> = Vec::new();
            for (i, p) in decl.params.iter().enumerate() {
                if p.variadic {
                    let mut arr = PhpArray::new();
                    // `&...$refs` aliases the arg cells themselves
                    // (named_params/variadic's test2 increments $x/$y) —
                    // an array containing references is itself a
                    // reference set: by-ref foreach iterates it live
                    // without separating.
                    for v in &args.cells[i.min(args.cells.len())..] {
                        if p.by_ref {
                            arr.is_ref = true;
                            arr.push_cell(v.clone());
                        } else {
                            arr.push(v.borrow().clone());
                        }
                    }
                    for (n, c) in &variadic_named {
                        if p.by_ref {
                            arr.is_ref = true;
                            arr.set_cell(ArrKey::Str(n.clone().into()), c.clone());
                        } else {
                            arr.set(ArrKey::Str(n.clone().into()), c.borrow().clone());
                        }
                    }
                    binds.push((
                        p.name.clone(),
                        cell(Value::Array(Rc::new(RefCell::new(arr)))),
                    ));
                } else if let Some((v, refable, trav)) = args
                    .cells
                    .get(i)
                    .map(|c| {
                        (
                            c,
                            // A nonref (call_user_func) slot still
                            // forwards when the element itself is a
                            // reference — zend keeps ref-ness through
                            // cufa arrays (bug50394).
                            !args.nonref_cells.contains(&i)
                                || self.ref_cells.contains(&(Rc::as_ptr(c) as usize)),
                            args.trav_cells.contains(&i),
                        )
                    })
                    .or(by_name[i].as_ref().map(|t| (&t.0, t.1, t.2)))
                {
                    if p.by_ref {
                        if trav {
                            let fname = self.decl_fname(decl);
                            self.warn(&format!(
                                "Cannot pass by-reference argument {} of {}() by unpacking a Traversable, passing by-value instead",
                                i + 1,
                                fname
                            ))?;
                            binds.push((p.name.clone(), cell(v.borrow().clone())));
                            continue;
                        }
                        if !refable {
                            let fname = self
                                .stack
                                .last()
                                .and_then(|f| f.call_alias.clone())
                                .unwrap_or_else(|| self.decl_fname(decl));
                            self.warn(&format!(
                                "{}(): Argument #{} (${}) must be passed by reference, value given",
                                fname,
                                i + 1,
                                p.name
                            ))?;
                        }
                        // The callee's var becomes a Zend IS_REFERENCE
                        // over the caller's cell — write-through errors
                        // say "reference held by property"
                        // (typed_properties_055/108).
                        self.ref_cells.insert(Rc::as_ptr(v) as usize);
                        binds.push((p.name.clone(), v.clone()));
                    } else {
                        binds.push((p.name.clone(), cell(v.borrow().clone())));
                    }
                } else if let Some(d) = &p.default {
                    // Default exprs are evaluated at call time; an error
                    // (e.g. an undefined constant) propagates as the
                    // call's failure (namespaces/ns_077) and attributes
                    // to the declaration line (named_params/defaults).
                    let prev_line = self.cur_line;
                    self.cur_line = decl.line;
                    let prev = self
                        .stack
                        .last()
                        .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned());
                    let old = match prev {
                        Some(c) => self.const_self.replace(c),
                        None => self.const_self.take(),
                    };
                    let r = self.eval_decl_const(d, &decl.file);
                    self.const_self = old;
                    self.cur_line = prev_line;
                    let mut dv = match r {
                        Ok(v) => v,
                        Err(e) => {
                            self.stack.pop();
                            return self.fail(e);
                        }
                    };
                    // `float $f = 0` — the int default widens to float
                    // at bind time, even under strict_types
                    // (scalar_float_with_integer_default_strict). In a
                    // union this applies whenever no `int` member can
                    // take it exactly (`float|string` = 3 -> float(3);
                    // `int|float` = 1 stays int — legal_default_values).
                    if let Some(ty) = &p.ty {
                        let float_widens = ty.iter().any(|m| m.eq_ignore_ascii_case("float"))
                            && !ty.iter().any(|m| m.eq_ignore_ascii_case("int"));
                        if float_widens {
                            if let Value::Int(i) = &dv {
                                dv = Value::Float(*i as f64);
                            }
                        }
                        // A non-literal default (const, expr) is checked
                        // like a passed arg — `int $a = NULL_CONST`
                        // TypeErrors when the default binds
                        // (scalar_constant_defaults).
                        let implicit_null = !ty.iter().any(|m| m.eq_ignore_ascii_case("null"))
                            && match &p.default {
                                Some(Expr::Null) => true,
                                Some(Expr::Const(c)) => c.eq_ignore_ascii_case("null"),
                                _ => false,
                            };
                        let ok = (implicit_null && matches!(dv, Value::Null))
                            || ty.iter().any(|m| self.param_type_match(m, &dv));
                        if !ok {
                            self.stack.pop();
                            let tyv: Vec<String> = if implicit_null {
                                let mut t = ty.to_vec();
                                t.push("null".into());
                                t
                            } else {
                                ty.to_vec()
                            };
                            let mut disp: Vec<String> =
                                Self::zpp_ty_disp(&self.resolve_static(&tyv));
                            disp.retain(|m| !m.eq_ignore_ascii_case("null"));
                            if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) || implicit_null {
                                if disp.len() == 1 && !disp[0].contains('&') {
                                    disp[0] = format!("?{}", disp[0]);
                                } else {
                                    disp.push("null".into());
                                }
                            }
                            let fname = self.decl_fname(decl);
                            let msg = format!(
                                "{}(): Argument #{} (${}) must be of type {}, {} given, called in {} on line {}",
                                fname,
                                i + 1,
                                p.name,
                                disp.join("|"),
                                self.zval_type_name(&dv),
                                self.diag_file(),
                                self.cur_line
                            );
                            let display = format!("{} and defined", msg);
                            let mut e = PhpError::uncaught("TypeError", msg, self.cur_line);
                            e.display_msg = Some(display);
                            e.thrown_line = Some(decl.line);
                            self.stack.pop();
                            return self.fail(e);
                        }
                        if !self.caller_file_strict() && !self.ty_weak_exact(ty, &dv) {
                            if let Some(cv) = self.coerce_scalar(ty, &dv) {
                                dv = cv;
                            }
                        }
                    }
                    binds.push((p.name.clone(), cell(dv)));
                } else {
                    // Unbound required param — only reachable via named
                    // args (the positional count check runs earlier).
                    let fname = self.decl_fname(decl);
                    self.stack.pop();
                    return self.fail(PhpError::uncaught(
                        "ArgumentCountError",
                        format!("{}(): Argument #{} (${}) not passed", fname, i + 1, p.name),
                        0,
                    ));
                }
            }
            let frame = self.stack.last_mut().unwrap();
            // func_get_arg(i)/func_num_args(): the bound non-variadic
            // params (named or positional) plus positional extras —
            // variadic extras don't count (named_params/variadic).
            let n_fixed = decl.params.iter().take_while(|p| !p.variadic).count();
            // func_num_args()/func_get_args(): Zend binds named args into
            // the CV table positionally, so a named call fills the table
            // up to the highest bound param — `test(c:'C', a:'A')`
            // reports 3 args, not 2 (named_params/func_get_args).
            let max_bound = (0..n_fixed)
                .filter(|i| args.cells.get(*i).is_some() || by_name[*i].is_some())
                .max();
            let mut fa: Vec<Cell> = Vec::new();
            if let Some(max_i) = max_bound {
                for bind in binds.iter().take(max_i + 1) {
                    fa.push(bind.1.clone());
                }
            }
            for a in &args.cells[n_fixed.min(args.cells.len())..] {
                fa.push(a.clone());
            }
            // Promoted ctor params: declare+assign $this->{name}
            // (error_2_exception_001).
            let is_ctor = decl.name.eq_ignore_ascii_case("__construct");
            let this_obj = frame.this_obj.clone();
            let mut promoted_writes: Vec<(Rc<RefCell<PhpObject>>, String, Value)> = Vec::new();
            for (i, p) in decl.params.iter().enumerate() {
                if p.promoted && is_ctor {
                    if let Some(obj) = &this_obj {
                        let v = binds[i].1.borrow().clone();
                        promoted_writes.push((obj.clone(), p.name.clone(), v));
                    }
                }
            }
            for (n, c) in binds {
                frame.vars.insert(n, c);
            }
            frame.args = fa;
            for (obj, pname, v) in promoted_writes {
                // Promoted assignment goes through prop write semantics —
                // hooked promoted props run their set hook (gh15438_1).
                let _ = self.store_prop(Value::Object(obj), &pname, v)?;
            }
        }
        let flow = self.exec_block(&decl.body);
        let ret_fname = self.decl_fname(decl);
        // `static` resolves against THIS frame's called class — after
        // the pop, `stack.last()` is the caller (static_type_return).
        let resolved_ret = decl.ret.as_ref().map(|ty| self.resolve_static(ty));
        let popped = self.stack.pop();
        // Zend decrefs the frame's CVs at unwind — the popped frame
        // is handed to bind_and_run, which runs its __destruct pass
        // after the call-trace pop so the dtor's trace attributes to
        // the caller's site (bug52361).
        self.last_popped_frame = popped;
        match flow {
            Flow::Return(v) => {
                // In a generator body `return v` is the iterator's
                // getReturn() payload — the declared return type binds
                // the produced Generator object, not this value
                // (generator_return_return_type).
                if decl.ret.is_some() && Self::decl_contains_yield(&decl.body) {
                    return Ok(v);
                }
                if let Some(ty) = &resolved_ret {
                    let ret_strict = self.strict_files.contains(&decl.file);
                    let ok = ty
                        .iter()
                        .any(|m| self.param_type_match(m, &v) || m.eq_ignore_ascii_case("void"))
                        && (!ret_strict || self.ty_exact(ty, &v));
                    if ok {
                        if !ret_strict && !self.ty_weak_exact(ty, &v) {
                            match self.coerce_scalar(ty, &v) {
                                Some(cv) => {
                                    let pl = self.cur_line;
                                    self.cur_line = decl.line;
                                    self.deprecate_lossy_int(ty, &v, &cv);
                                    self.cur_line = pl;
                                    Ok(cv)
                                }
                                None => Ok(v),
                            }
                        } else {
                            Ok(v)
                        }
                    } else {
                        let mut disp: Vec<String> = Self::zpp_ty_disp(&self.resolve_static(ty));
                        disp.retain(|m| !m.eq_ignore_ascii_case("null"));
                        if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) {
                            if disp.len() == 1 && !disp[0].contains('&') {
                                disp[0] = format!("?{}", disp[0]);
                            } else {
                                disp.push("null".into());
                            }
                        }
                        let given = self.zval_type_name(&v);
                        let msg = format!(
                            "{}(): Return value must be of type {}, {} returned",
                            ret_fname,
                            disp.join("|"),
                            given
                        );
                        self.fail(PhpError::uncaught("TypeError", msg, self.cur_line))
                    }
                } else {
                    // __toString carries an implicit `string` contract
                    // — scalars coerce weakly; other types are
                    // TypeErrors (bug26166).
                    if decl.name.eq_ignore_ascii_case("__tostring") {
                        match &v {
                            Value::Str(_) => Ok(v),
                            Value::Int(_) | Value::Float(_) | Value::Bool(_) => {
                                Ok(Value::str(v.to_php_string()))
                            }
                            other => {
                                let given = self.zval_type_name(other);
                                self.fail(PhpError::uncaught(
                                    "TypeError",
                                    format!(
                                        "{}(): Return value must be of type string, {} returned",
                                        ret_fname, given
                                    ),
                                    self.cur_line,
                                ))
                            }
                        }
                    } else {
                        Ok(v)
                    }
                }
            }
            Flow::Throw(v) => {
                self.pending_exception = Some(v);
                Err(PhpError {
                    trace: None,
                    thrown_line: None,
                    display_msg: None,
                    kind: ErrorKind::Throw,
                    message: "throw".into(),
                    line: 0,
                })
            }
            Flow::Exit(c) => Err(PhpError {
                trace: None,
                thrown_line: None,
                display_msg: None,
                kind: ErrorKind::Fatal,
                message: format!("\u{1}exit:{}", c),
                line: 0,
            }),
            Flow::Break(_) | Flow::Continue(_) => self.fail(PhpError::fatal(
                "'break' or 'continue' outside of loop or switch context",
                0,
            )),
            Flow::Goto(l) => self.fail(PhpError::fatal(
                format!("'goto' to undefined label '{}'", l),
                0,
            )),
            Flow::Normal => {
                // Falling off the end of a typed function still checks
                // the return type: `none returned` TypeError for real
                // types, `must not implicitly return` for `never`
                // (typed_return*_without_value). Generators are exempt —
                // their declared type describes the produced object.
                if decl.ret.is_some() && Self::decl_contains_yield(&decl.body) {
                    return Ok(Value::Null);
                }
                if let Some(ty) = &resolved_ret {
                    let never = ty.iter().any(|m| m.eq_ignore_ascii_case("never"));
                    let void = ty.iter().all(|m| m.eq_ignore_ascii_case("void"));
                    if never {
                        let msg = format!(
                            "{}: never-returning function must not implicitly return",
                            ret_fname
                        );
                        let mut e = PhpError::uncaught("TypeError", msg, self.cur_line);
                        e.thrown_line = Some(decl.line);
                        return self.fail(e);
                    }
                    if !void {
                        let mut disp_v = Self::zpp_ty_disp(&self.resolve_static(ty));
                        disp_v.retain(|m| !m.eq_ignore_ascii_case("null"));
                        if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) {
                            if disp_v.len() == 1 && !disp_v[0].contains('&') {
                                disp_v[0] = format!("?{}", disp_v[0]);
                            } else {
                                disp_v.push("null".into());
                            }
                        }
                        let disp = disp_v.join("|");
                        let msg = format!(
                            "{}(): Return value must be of type {}, none returned",
                            ret_fname, disp
                        );
                        let mut e = PhpError::uncaught("TypeError", msg, self.cur_line);
                        e.thrown_line = Some(decl.end_line);
                        return self.fail(e);
                    }
                }
                // Falling off an untyped __toString is the same
                // `none returned` TypeError (bug26166).
                if decl.name.eq_ignore_ascii_case("__tostring") {
                    let mut e = PhpError::uncaught(
                        "TypeError",
                        format!(
                            "{}(): Return value must be of type string, none returned",
                            ret_fname
                        ),
                        self.cur_line,
                    );
                    e.thrown_line = Some(decl.end_line);
                    return self.fail(e);
                }
                Ok(Value::Null)
            }
        }
    }

    pub(in crate::interp) fn invoke_fn(
        &mut self,
        decl: &Rc<FunctionDecl>,
        args: CallArgs,
        this_obj: Option<Rc<RefCell<PhpObject>>>,
        scope_class: Option<Rc<PhpClass>>,
    ) -> Result<Value, PhpError> {
        // Native SPL stubs (empty body, line 0) hit spl_method even on
        // paths that bypass method dispatch — parent:: calls reach here
        // with the stub decl directly.
        if decl.body.is_empty() && decl.line == 0 {
            if let Some(o) = &this_obj {
                if self.is_a_str(o.borrow().class.name(), "splfileinfo") {
                    if let Some(v) = self.spl_method(o, &decl.name, &args)? {
                        return Ok(v);
                    }
                }
            }
        }
        let required = decl
            .params
            .iter()
            .filter(|p| p.default.is_none() && !p.variadic)
            .count();
        if args.named.is_empty() && args.len() < required {
            return self.fail(PhpError::uncaught(
                "ArgumentCountError",
                format!(
                    "Too few arguments to function {}(), {} passed in {} on line {} and {} {} expected",
                    self.decl_fname(decl),
                    args.len(),
                    self.diag_file(),
                    self.cur_line,
                    if required == decl.params.len() { "exactly" } else { "at least" },
                    required
                ),
                0,
            ));
        }
        // A `yield`-bearing body makes the call a Generator factory:
        // the caller gets a Generator object immediately and the body
        // only runs when iteration first demands it.
        if Self::decl_contains_yield(&decl.body) {
            let dc = self.pending_decl_class.take();
            let cc = self.pending_called_class.take();
            return Ok(Value::Object(self.make_generator(GenSetup::Invoke {
                decl: decl.clone(),
                args,
                this_obj,
                scope_class,
                decl_class: dc,
                called_class: cc,
                captures: Vec::new(),
            })));
        }
        let dc = self.pending_decl_class.take();
        let cc = self.pending_called_class.take();
        self.invoke_fn_run(decl, args, this_obj, scope_class, dc, cc)
    }

    /// Frame push + body run — the part of invoke_fn the Generator
    /// start path also uses (the yield check must not re-trip here).
    pub(in crate::interp) fn invoke_fn_run(
        &mut self,
        decl: &Rc<FunctionDecl>,
        args: CallArgs,
        this_obj: Option<Rc<RefCell<PhpObject>>>,
        scope_class: Option<Rc<PhpClass>>,
        decl_class: Option<Rc<PhpClass>>,
        called_class: Option<Rc<PhpClass>>,
    ) -> Result<Value, PhpError> {
        let mut frame = Frame::new(decl.name.clone());
        frame.fn_line = decl.line;
        frame.file = decl.file.clone();
        frame.ns = decl.ns.clone();
        frame.ret_by_ref = decl.by_ref;
        if let Some(obj) = &this_obj {
            frame
                .vars
                .insert("this".to_string(), cell(Value::Object(obj.clone())));
        }
        frame.decl_class = decl_class;
        frame.called_class = called_class;
        frame.hook_prop = self.pending_hook_prop.take();
        frame.this_obj = this_obj;
        frame.scope_class = scope_class;
        frame.trait_origin = decl.decl_in.clone();
        frame.file = if decl.file.is_empty() {
            self.cur_file.clone()
        } else {
            decl.file.clone()
        };
        let pending_caps = std::mem::take(&mut self.pending_gen_captures);
        self.stack.push(frame);
        if let Some(top) = self.stack.last_mut() {
            for (n, c, by_ref) in pending_caps {
                let c2 = if by_ref { c } else { cell(c.borrow().clone()) };
                top.vars.insert(n, c2);
            }
        }
        self.bind_and_run(decl, args, Vec::new())
    }
}

/// By-ref flags for builtin parameters (only slots that accept references are
/// `true`). Used to warn on non-variable args in by-ref positions and to alias
/// real cells for mutating builtins like array_pop/sort/preg_match.
fn builtin_byref(name: &str) -> Option<&'static [bool]> {
    Some(match name {
        "array_pop" | "array_shift" | "array_walk" | "sort" | "rsort" | "asort" | "arsort"
        | "ksort" | "krsort" | "usort" | "uasort" | "uksort" | "natsort" | "natcasesort"
        | "shuffle" | "reset" | "end" | "next" | "prev" | "current" | "pos" | "each"
        | "array_push" | "array_unshift" | "array_splice" | "array_multisort" => &[true],
        "preg_match" | "preg_match_all" => &[false, false, true],
        "preg_replace"
        | "preg_replace_callback"
        | "preg_filter"
        | "str_replace"
        | "str_ireplace" => &[false, false, false, false, true],
        "preg_replace_callback_array" => &[false, false, false, true],
        "parse_str" => &[false, true],
        "is_callable" => &[false, false, true],
        "sscanf" | "fscanf" => &[false, false],
        "exec" => &[false, true, true],
        "passthru" | "system" => &[false, true],
        "preg_grep" => &[false],
        _ => return None,
    })
}

/// Zend's normalized union display for redundancy errors: iterable
/// expands to its members, class names first (written order), then
/// `object`, then `array`, then remaining builtins, `null` last.
fn ty_norm_disp(ty: &[String]) -> String {
    let mut classes: Vec<String> = Vec::new();
    let mut scalars: Vec<String> = Vec::new();
    let mut obj = false;
    let mut arr = false;
    let mut nul = false;
    for m in ty {
        let mut members: Vec<String> = if m.eq_ignore_ascii_case("iterable") {
            vec!["Traversable".into(), "array".into()]
        } else {
            vec![m.clone()]
        };
        for e in members.drain(..) {
            let el = e.to_lowercase();
            match el.as_str() {
                "null" => nul = true,
                "object" => obj = true,
                "array" => arr = true,
                "self" | "static" | "parent" => {
                    if !classes.iter().any(|c| c.eq_ignore_ascii_case(&e)) {
                        classes.push(e);
                    }
                }
                "int" | "float" | "string" | "bool" | "callable" | "iterable" | "mixed"
                | "void" | "never" | "false" | "true" => {
                    if !scalars.iter().any(|c| c == &el) {
                        scalars.push(el);
                    }
                }
                _ => {
                    if !classes.iter().any(|c| c.eq_ignore_ascii_case(&e)) {
                        if e.contains('&') {
                            classes.push(format!("({})", e));
                        } else {
                            classes.push(e);
                        }
                    }
                }
            }
        }
    }
    let mut out = classes;
    if obj {
        out.push("object".into());
    }
    if arr {
        out.push("array".into());
    }
    out.extend(scalars);
    if nul {
        out.push("null".into());
    }
    out.join("|")
}
