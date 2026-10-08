//! Call dispatch: arg binding, named args, first-class callables,
//! closure rebind, `bind_and_run`/`invoke_fn` frames and the signature
//! checks (params/returns/`#[ReturnTypeWillChange]`) around them.

use super::util::*;
use super::*;

/// zend_is_callable_check_class's resolved "calling scope" record —
/// the class slot becomes a (ce, fcc->object, strict_class) triple.
pub(in crate::interp) struct CallableSite {
    /// Resolved class.
    pub ce: Rc<PhpClass>,
    /// The bound `$this` — the object slot itself, or the calling
    /// frame's $this when Zend's class-slot rule binds it.
    pub bound: Option<Rc<RefCell<PhpObject>>>,
    /// strict_class — everything but 'self' slots.
    #[allow(dead_code)]
    pub strict: bool,
}

/// A callback-resolution failure: `Msg` is a zpp diagnostic detail
/// (`must be a valid callback, <detail>`), `Thrown` is the
/// autoloader's own exception, which propagates instead of becoming a
/// validation error.
pub(in crate::interp) enum SiteErr {
    Msg(String),
    Thrown(PhpError),
}

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
        site: Option<usize>,
        callee: Option<usize>,
    ) -> Result<Value, PhpError> {
        // The call's own site covers frames pushed during callee
        // resolution (autoload); arg_cells re-sets it after arg eval so
        // nested calls inside the args can't clobber it.
        if let Some(s) = site {
            self.send_line = Some(s);
        }
        // Resolution-phase errors (undefined function, not-callable,
        // class-not-found) fire at zend's INIT_DYNAMIC_CALL — sited at
        // the callee's first-token line, before any arg op runs.
        let res = callee.or(site);
        // Resolve callee name/value.
        let fname = match name {
            Expr::Str(s) => {
                // `('Cls::m')()` — a source-literal static call: the
                // class name is verbatim (keywords stay unbound) and
                // $this forwards when the caller is-a Cls.
                let lit = s.trim_start_matches('\u{1}').trim_start_matches('\\');
                if let Some((cn, mn)) = lit.rsplit_once("::") {
                    let Some(cls) = self.str_callable_class(cn)? else {
                        if let Some(l) = res {
                            self.send_line = Some(l);
                        }
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!("Class \"{}\" not found", cn),
                            0,
                        ));
                    };
                    let params = self
                        .find_method_in(&cls, mn)
                        .map(|(m, _)| m.decl.params.clone())
                        .unwrap_or_default();
                    let vals = self.arg_cells(
                        args,
                        &params,
                        &format!("{}::{}()", cls.name(), mn),
                        false,
                        site,
                    )?;
                    return self.static_invoke_vis(cls, mn, vals, None, true);
                }
                s.to_string()
            }
            Expr::StaticProp { class, name } => {
                // `C::$var()` — dynamic static method call.
                let cls = self.class_of(class)?;
                let mn = Self::nul_trunc(&self.prop_name(name)?);
                let params = self
                    .find_method_in(&cls, &mn)
                    .map(|m| m.0.decl.params.clone())
                    .unwrap_or_default();
                let vals = self.arg_cells(args, &params, &format!("{}()", mn), false, site)?;
                let fwd = matches!(&**class, Expr::Const(_) | Expr::Str(_) | Expr::AnonClass(_));
                return self.static_invoke_vis(cls, &mn, vals, None, fwd);
            }
            _ => {
                // `$f()`, `($f)()`, `($o->p)()`, `g()()`, `$arr[0]()`,
                // `['Cb','m']()` — the callee is any value expression;
                // its resolution is the INIT op, before any arg op.
                let v = self.eval(name)?;
                match &v {
                    Value::Callable(_) => {
                        let params = self.callable_params(&v);
                        let ctx = format!("{}()", self.callable_ctx_name(&v));
                        let vals = self.arg_cells(args, &params, &ctx, false, site)?;
                        return self.call_value(&v, vals);
                    }
                    Value::Object(o) => {
                        // The __invoke check resolves at INIT — a miss
                        // errors before args evaluate.
                        let icls = o.borrow().class.clone();
                        if self.find_method_in(&icls, "__invoke").is_none() {
                            if let Some(l) = res {
                                self.send_line = Some(l);
                            }
                            return self.fail(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Object of type {} is not callable",
                                    o.borrow().class.name()
                                ),
                                0,
                            ));
                        }
                        let params = self.callable_params(&v);
                        let ctx = format!("{}()", self.callable_ctx_name(&v));
                        let vals = self.arg_cells(args, &params, &ctx, false, site)?;
                        return self.call_value(&v, vals);
                    }
                    Value::Array(_) => {
                        // `[obj,'m']` / `[$closure,'__invoke']` array
                        // callables resolve at INIT (bug78689).
                        if let Some(l) = res {
                            self.send_line = Some(l);
                        }
                        let c = self.fcc_val(&v)?;
                        if let Some(s) = site {
                            self.send_line = Some(s);
                        }
                        let params = self.callable_params(&c);
                        let ctx = format!("{}()", self.callable_ctx_name(&c));
                        let vals = self.arg_cells(args, &params, &ctx, false, site)?;
                        return self.call_value(&c, vals);
                    }
                    Value::Str(_) => self.conv_str(&v).unwrap_or_default(),
                    _ => {
                        if let Some(l) = res {
                            self.send_line = Some(l);
                        }
                        let tn = match &v {
                            Value::Null => "null",
                            Value::Bool(_) => "bool",
                            Value::Int(_) => "int",
                            Value::Float(_) => "float",
                            Value::Resource(_) => "resource",
                            _ => "value",
                        };
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!("Value of type {} is not callable", tn),
                            0,
                        ));
                    }
                }
            }
        };
        self.call_named(&fname, args, site, callee)
    }

    /// The `(file, line)` a pushed call frame or call diagnostic
    /// attributes its call site to: `"[internal function]", 0` when
    /// dispatched from inside builtin machinery (internal_cb with an
    /// internal prev frame — call_user_func* trampolines and frameless
    /// compile-specialized calls are transparent to the walk), else the
    /// caller's `file` + the pending `send_line` (falling back to
    /// `fallback_line` when no send is in flight). `no_frame_internal`
    /// decides what an empty visible trace means: true = the engine
    /// itself is the caller (shutdown fns, the dtor sweep); false = a
    /// plain top-level call.
    pub(in crate::interp) fn call_site(
        &self,
        no_frame_internal: bool,
        file: String,
        fallback_line: usize,
    ) -> (String, u32) {
        // An SPL-prelude frame stands in for zend's C-level SPL
        // delegation: calls it makes (gen resumes, inner-iterator
        // hops, user callbacks) all site `[internal function]` —
        // oracle II-of-II: `[internal function]: IteratorIterator->
        // rewind()`, never an eval()'d-code site. Transitively covers
        // SPL subclasses via is_a.
        let caller_is_spl_stub = self.stack.iter().rev().nth(1).is_some_and(|f| {
            // Executing prelude code too — a userland override in
            // an SPL subclass keeps real call sites.
            f.file.contains("eval()'d code")
                && f.decl_class
                    .as_ref()
                    .or(f.scope_class.as_ref())
                    .is_some_and(|c| self.class_is_spl_prelude(c))
        });
        let from_builtin = caller_is_spl_stub
            || (self.internal_cb > 0
                && self
                    .call_trace
                    .iter()
                    .rev()
                    .find(|f| !crate::value::trace_frame_hidden(f))
                    .map(|f| f.internal)
                    .unwrap_or(no_frame_internal));
        if from_builtin {
            ("[internal function]".to_string(), 0)
        } else {
            (
                file,
                self.send_line
                    .map(|l| l as u32)
                    .unwrap_or(fallback_line as u32),
            )
        }
    }

    /// Sees through the parser's `argline` per-arg line marker (every
    /// call arg) to the argument expression itself.
    pub(in crate::interp) fn unmark_arg(e: &Expr) -> &Expr {
        match e {
            Expr::Binary {
                op: "argline", r, ..
            } => r,
            _ => e,
        }
    }

    /// Evaluate args into cells (by-ref params alias caller storage).
    /// `named` params collected as (name, cell) too. `site` is the
    /// call's own source line — recorded as the frame's call site once
    /// arg evaluation (which may push nested frames) has finished.
    pub(in crate::interp) fn arg_cells(
        &mut self,
        args: &[Expr],
        decl: &[Param],
        ctx: &str,
        internal: bool,
        site: Option<usize>,
    ) -> Result<CallArgs, PhpError> {
        let mut out = CallArgs::empty();
        // zend's INIT_FCALL pushes the frame's arena span before args
        // evaluate — the push (or copy into a fresh segment) happens
        // here, once.
        self.vm_call_push(&mut out);
        // Position of the *next positional* arg for by-ref lookup — named
        // args don't advance it (they bind by name at call time).
        let mut pos = 0usize;
        let mut seen_named = false;
        let saved_line = self.cur_line;
        for a in args {
            // `argline` (each arg's own first-token line): Zend
            // attributes a diagnostic raised while evaluating an
            // argument to that arg's line, not the call's.
            if let Expr::Binary {
                op: "argline", l, ..
            } = a
            {
                if let Expr::Int(n) = l.as_ref() {
                    self.cur_line = *n as usize;
                    self.send_line = Some(*n as usize);
                }
            }
            let a = Self::unmark_arg(a);
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
                    // (named_params/unpack's $ary2 stays 0). By-value
                    // callees never alias the cells: zend addrefs the
                    // zvals straight onto vm_stack, so the separation
                    // (and its doubled arData) is pure churn — skip it.
                    if Rc::strong_count(a) > 1
                        && (decl.is_empty() || {
                            let b = a.borrow();
                            // zend binds each unpacked element like a
                            // sent arg: int keys take the next positional
                            // slots, string keys the same-named param —
                            // overflow and unknown names land in the
                            // variadic (the fallback the named-arg path
                            // below uses).
                            let vref = || decl.iter().any(|p| p.variadic && p.by_ref);
                            let mut slot = pos;
                            b.entries.iter().any(|(k, _)| match k {
                                ArrKey::Int(_) => {
                                    let hit =
                                        decl.get(slot).map(|p| p.by_ref).unwrap_or_else(&vref);
                                    slot += 1;
                                    hit
                                }
                                ArrKey::Str(s) => decl
                                    .iter()
                                    .find(|p| !p.variadic && &**s == p.name.as_str())
                                    .map(|p| p.by_ref)
                                    .unwrap_or_else(&vref),
                                ArrKey::Tomb => false,
                            })
                        })
                    {
                        // zend separates the shared hash table before
                        // binding — plain elements get fresh storage,
                        // live IS_REFERENCE buckets stay shared (the
                        // same split `=`-copies use).
                        let nv = Value::Array(Rc::new(RefCell::new(self.dup_array(&a.borrow()))));
                        if let Ok(c) = self.eval_cell(e) {
                            *c.borrow_mut() = nv.clone();
                        }
                        v = nv;
                    }
                }
                let trav = matches!(&v, Value::Object(_));
                let mut unpack_named = false;
                for (k, c, _) in self.unpack_items(&v, true)? {
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
                // SEND_UNPACK's extend_call_frame grows the arena by
                // the call's whole arg span — the source zval is still
                // live, so the emalloc guard sees its table charged.
                self.vm_call_push(&mut out);
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
                // `(expr)` parens stack `argline` markers — peel fully
                // for the shape check; `expr` itself (still marked) is
                // evaluated so inner diagnostics keep their own lines.
                let expr_u = Self::unmark_rhs(expr);
                match expr_u {
                    Expr::Var(_) | Expr::Index { .. } | Expr::Prop { .. } | Expr::VarVar(..)
                        | Expr::StaticProp { .. }
                        // zend's SEND_REF check rejects the $GLOBALS
                        // table itself (its elements are fine).
                        if !matches!(expr_u, Expr::Var(n) if n == "GLOBALS") =>
                    {
                        // zend evaluates a by-ref arg dim as BP_VAR_RW
                        // — string offsets fail with the catchable
                        // 'Cannot create references to/from string
                        // offsets' (and the str-key TypeError), not the
                        // generic scalar-as-array fatal.
                        let was = std::mem::replace(&mut self.dim_by_ref, true);
                        // `$this` can't sit behind IS_REFERENCE —
                        // a by-ref param binds a plain value cell
                        // (object still aliases via the handle).
                        let rc = if matches!(expr_u, Expr::Var(n) if n == "this") {
                            self.eval(expr).map(cell)
                        } else {
                            self.eval_cell(expr)
                        };
                        self.dim_by_ref = was;
                        // Cell-access errors (readonly/private prop,
                        // string offsets, undeclared static) are real
                        // catchable throwables — propagate them, not
                        // the bogus by-ref compile fatal.
                        let c = rc?;
                        if let Some(n) = name {
                            out.named.push((n, c, true, false));
                            seen_named = true;
                        } else {
                            out.cells.push(c);
                            pos += 1;
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
                    Expr::Call { .. }
                    | Expr::MethodCall { .. }
                    | Expr::StaticCall { .. }
                    | Expr::StaticCallDyn { .. } => {
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
                        // zend names the landing param only for a
                        // real (non-variadic) slot — `Argument #N
                        // ($name)`; an arg landing on the variadic
                        // prints bare `Argument #N`.
                        let pname = match &name {
                            Some(n) => decl
                                .iter()
                                .find(|p| !p.variadic && p.name == *n)
                                .map(|p| p.name.as_str()),
                            None => decl
                                .get(pos)
                                .filter(|p| !p.variadic)
                                .map(|p| p.name.as_str()),
                        };
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "{}: Argument #{}{} could not be passed by reference",
                                ctx,
                                argno,
                                pname.map(|n| format!(" (${})", n)).unwrap_or_default()
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
        // All args evaluated — the enclosing call's own line is the
        // frame's site (nested calls inside the args set their own),
        // and post-eval call diagnostics (arity, dispatch failures)
        // site at the call itself, zend's DO_FCALL line.
        // Plain/named args pushed by their own sends — catch up the
        // call's arena span once, after the last arg op.
        self.vm_call_push(&mut out);
        out.end_line = self.cur_line;
        self.cur_line = site.unwrap_or(saved_line);
        if let Some(s) = site {
            self.send_line = Some(s);
        }
        Ok(out)
    }

    /// Spreadable items of `...$v`: arrays yield entries, Traversables
    /// iterate via the rewind/valid/current/key/next protocol
    /// (IteratorAggregate chains resolve first). `None` key = positional.
    ///
    /// The non-iterable diagnostic's class splits by context:
    /// call-arg spread `f(...$v)` throws TypeError for every
    /// non-iterable (zend's arg-type check), while array-literal
    /// `[...$v]` throws Error for scalars/null and TypeError only for
    /// objects — the object arm below already throws TypeError.
    pub(in crate::interp) fn unpack_items(
        &mut self,
        v: &Value,
        in_call_args: bool,
    ) -> Result<SpreadItems, PhpError> {
        match v {
            Value::Array(a) => {
                // Elements hand out their real cells — by-ref params
                // bind them (a shared source is cow-separated at the
                // call site first), by-value params read the value.
                let mut out = Vec::new();
                for (k, c) in a.borrow().iter() {
                    let n = match k {
                        ArrKey::Str(s) => Some(s.clone()),
                        _ => None,
                    };
                    // The clone below would count itself — judge
                    // the ref's liveness while only the source's
                    // own handles exist.
                    let shared = self.is_ref_cell(c) && Rc::strong_count(c) > 1;
                    out.push((n, c.clone(), shared));
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
                        // TypeError in Zend — an object argument is a
                        // type violation, not an engine error.
                        return self.fail(PhpError::uncaught(
                            "TypeError",
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
                    out.push((n, cell(val), false));
                    let _ = self.method_invoke(it.clone(), "next", CallArgs::empty());
                }
                Ok(out)
            }
            _ => self.fail(PhpError::uncaught(
                if in_call_args { "TypeError" } else { "Error" },
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
        site: Option<usize>,
        callee: Option<usize>,
    ) -> Result<Value, PhpError> {
        if let Some(s) = site {
            self.send_line = Some(s);
        }
        // Resolution-phase errors site at the callee's first-token
        // line (zend's INIT op lineno), fired before args evaluate.
        let res = callee.or(site);
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
        // `$f='Cls::m'; $f()` — the dynamic string-callable path: the
        // class part is a LITERAL name (no scope keywords — 'self::x'
        // reports `Class "self" not found`), and $this is never
        // forwarded even when the caller is-a Cls.
        let raw_name = fname.trim_start_matches('\u{1}').trim_start_matches('\\');
        if let Some((cn, mn)) = raw_name.rsplit_once("::") {
            let Some(cls) = self.str_callable_class(cn)? else {
                if let Some(l) = res {
                    self.send_line = Some(l);
                }
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Class \"{}\" not found", cn),
                    0,
                ));
            };
            let params = self
                .find_method_in(&cls, mn)
                .map(|(m, _)| m.decl.params.clone())
                .unwrap_or_default();
            let vals = self.arg_cells(
                args,
                &params,
                &format!("{}::{}()", cls.name(), mn),
                false,
                site,
            )?;
            return self.static_invoke_vis(cls, mn, vals, None, false);
        }
        // zend_forbid_dynamic_call: compact() rejects any call that did
        // not come from a compile-time literal (the `\u{1}` marker or a
        // `\`-qualified name) — `$f()`, `($this->cb)()`, reflection and
        // call_user_func all hit 'Cannot call compact() dynamically'.
        if lname == "compact" && !unqualified && !fname.starts_with('\\') {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot call compact() dynamically",
                self.cur_line,
            ));
        }
        let mut decl = self.functions.get(&lname).cloned();
        // A namespaced user function outranks the global/builtin one for
        // unqualified calls (namespaces/ns_013).
        let mut ns_resolved = false;
        // When the ns\name fallback misses too, the undefined-function
        // error names the ns-qualified candidate (bugs/77376).
        let mut miss_name = fname
            .trim_start_matches('\u{1}')
            .trim_start_matches('\\')
            .to_string();
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
        // Zend resolves the callee at INIT — before any arg op — so an
        // unresolvable name aborts the call before args ever evaluate.
        if decl.is_none()
            && !crate::builtins::is_builtin(&lname)
            && crate::builtins::builtin_params(&lname).is_none()
            && builtin_byref(&lname).is_none()
        {
            if let Some(l) = res {
                self.send_line = Some(l);
            }
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Call to undefined function {}()", miss_name),
                0,
            ));
        }
        // Synthetic params carrying builtin by-ref flags so call results in
        // by-ref slots emit "Only variables should be passed by reference"
        // (passByReference_012, array_shift(array_shift($a))).
        let builtin_params: Vec<Param> = if decl.is_none() {
            let sig = crate::builtins::builtin_sig(&lname).unwrap_or_default();
            let bparams = crate::builtins::builtin_params(&lname);
            builtin_byref(&lname)
                .map(|flags| {
                    flags
                        .iter()
                        .enumerate()
                        .map(|(i, by_ref)| Param {
                            name: sig
                                .get(i)
                                .map(|(n, _)| n.clone())
                                .or_else(|| {
                                    bparams.and_then(|p| p.get(i).map(|(n, _)| n.to_string()))
                                })
                                .unwrap_or_default(),
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
            &format!(
                "{}()",
                fname.trim_start_matches('\u{1}').trim_start_matches('\\')
            ),
            decl.is_none(),
            site,
        )?;
        if !ns_resolved {
            // zend's ZEND_FRAMELESS_FUNCTION for a compile-time-bound
            // direct 2-arg min/max call does zend_compare(lhs, rhs) —
            // arg 1 is the compare's protected LEFT operand. The
            // generic builtin used by every indirect call instead
            // compares each new arg against the running best.
            // Compile-time-bound = an unqualified literal at global
            // scope or a `\min` qualified literal (INIT_FCALL);
            // unqualified calls inside a namespace stay dynamic.
            // `...` unpack compiles to SEND_UNPACK — a generic builtin
            // call, never the frameless path.
            if decl.is_none()
                && matches!(lname.as_str(), "min" | "max")
                && argvals.cells.len() == 2
                && argvals.named.is_empty()
                && !args
                    .iter()
                    .any(|a| matches!(Self::unmark_arg(a), Expr::Unpack(_)))
                && (fname.starts_with('\\') || (unqualified && self.caller_ns().is_empty()))
            {
                let lhs = argvals.cells[0].borrow().clone();
                let rhs = argvals.cells[1].borrow().clone();
                crate::value::clear_cmp_depth_err();
                let ord = crate::value::compare(&lhs, &rhs);
                self.emit_cmp_notices()?;
                if crate::value::cmp_depth_err() {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        "Nesting level too deep - recursive dependency?",
                        self.cur_line,
                    ));
                }
                let pick_lhs = if lname == "min" {
                    ord == std::cmp::Ordering::Less
                } else {
                    ord != std::cmp::Ordering::Less
                };
                return Ok(if pick_lhs { lhs } else { rhs });
            }
            // Zend keeps the callee frame in traces for every real
            // internal call — literal or dynamic (`substr`/`fprintf`
            // show #0 in conversion errors). The exception: literal
            // calls Zend compile-specializes into dedicated opcodes
            // emit no call at all, e.g. `sprintf(<const "%s"/"%d"/"%%"
            // fmt>, <exact arg count>)` → rope-concat
            // (sprintf_rope_optimization_002). Dynamic dispatches —
            // `$fn()`, `f(...$a)`, callables — are always real calls.
            // Compile-bound literal = a fully-qualified `\f`, or an
            // unqualified literal whose binding can't vary at runtime:
            // global scope (no ns\name fallback) or a `use function`
            // alias (ns_resolve already rewrote it to `\target`).
            // Unqualified calls inside a namespace bind at runtime, so
            // Zend can't specialize them — the frame is real.
            let literal = fname.starts_with('\\')
                || (fname.starts_with('\u{1}') && self.caller_ns().is_empty());
            let visible = args
                .iter()
                .any(|a| matches!(Self::unmark_arg(a), Expr::Unpack(_)))
                || !(literal && zend_literal_no_frame(&lname, args));
            if let Some(v) = self.call_builtin(&lname, &argvals, visible)? {
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
                                // Per-instance statics key off the
                                // callable id — the generator frame
                                // needs it like any closure frame.
                                closure_rc: Some(c.clone()),
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
                        frame.ns = decl.ns.clone();
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
                        frame.trait_origin = decl.decl_in.clone();
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
                        // zend_forbid_dynamic_call — every callable
                        // dispatch is a dynamic call.
                        if n.eq_ignore_ascii_case("compact") {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                "Cannot call compact() dynamically",
                                self.cur_line,
                            ));
                        }
                        if let Some(v) = self.call_builtin(&n.to_lowercase(), &args, true)? {
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
                // "Class::method" string callables — self/static/parent
                // bind to the calling scope; the split is at the LAST
                // '::' and an `X::m` method qualifier resolves against
                // it (bug45186 + the callable qualifiers).
                if let Some((cls, m)) = name.rsplit_once("::") {
                    match self.callable_site(cls, None, false, true) {
                        Ok(site) => {
                            let ce_org = site.ce.clone();
                            match self.callable_leg(&site, Some(&ce_org), m, false, true) {
                                Ok((ce, mn)) => {
                                    return self.callable_invoke(ce, &mn, args, site.bound.clone());
                                }
                                Err(SiteErr::Thrown(e)) => return Err(e),
                                Err(SiteErr::Msg(_)) => {
                                    // Only unvalidated callers land
                                    // here — zend's direct-call error
                                    // on the written form.
                                    return self.static_invoke_vis(site.ce, m, args, None, true);
                                }
                            }
                        }
                        Err(SiteErr::Thrown(e)) => return Err(e),
                        Err(SiteErr::Msg(detail)) => {
                            if let Some(c) = self.resolve_class(cls) {
                                if let Some(cls) = self.classes.get(&c.to_lowercase()).cloned() {
                                    return self.static_invoke_vis(cls, m, args, None, true);
                                }
                            }
                            let e = self.exception("TypeError", &detail);
                            let te = self.throw(e);
                            return self.fail(te);
                        }
                    }
                }
                if let Some(v) = self.call_builtin(&name.to_lowercase(), &args, true)? {
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
                // [$obj, 'method'] or ['Class', 'method'] — drop the
                // borrow before dispatch: resolving `['Cls','m']`
                // autoloads, and user loader code may write this array.
                let (o0, m) = {
                    let a = a.borrow();
                    (a.get(&ArrKey::Int(0)), a.get(&ArrKey::Int(1)))
                };
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
                            Value::Object(o) => {
                                // `[$o,'X::m']` — the qualifier pins
                                // the declaring class on the bound
                                // object (bug32290); plain `[$o,'m']`
                                // is a normal object dispatch.
                                let ce = o.borrow().class.clone();
                                let site = CallableSite {
                                    ce: ce.clone(),
                                    bound: Some(o.clone()),
                                    strict: false,
                                };
                                match self.callable_leg(&site, Some(&ce), &mname, false, true) {
                                    Ok((ce2, mn)) => {
                                        self.callable_invoke(ce2, &mn, args, Some(o.clone()))
                                    }
                                    Err(SiteErr::Thrown(e)) => Err(e),
                                    Err(SiteErr::Msg(_)) => {
                                        self.method_invoke_vis(o.clone(), &mname, args)
                                    }
                                }
                            }
                            Value::Str(cn) => {
                                // ['Cls','m'] — self/static/parent bind
                                // to the calling scope, and a method
                                // slot like 'parent::who' resolves its
                                // qualifier against the slot (bug45186,
                                // bug66719).
                                match self.callable_site(
                                    &crate::value::lossy(&cn),
                                    None,
                                    false,
                                    true,
                                ) {
                                    Ok(site) => {
                                        let ce_org = site.ce.clone();
                                        match self.callable_leg(
                                            &site,
                                            Some(&ce_org),
                                            &mname,
                                            false,
                                            true,
                                        ) {
                                            Ok((ce, mn)) => self.callable_invoke(
                                                ce,
                                                &mn,
                                                args,
                                                site.bound.clone(),
                                            ),
                                            Err(SiteErr::Thrown(e)) => Err(e),
                                            Err(SiteErr::Msg(_)) => self.static_invoke_vis(
                                                ce_org, &mname, args, None, true,
                                            ),
                                        }
                                    }
                                    Err(SiteErr::Thrown(e)) => Err(e),
                                    Err(SiteErr::Msg(detail)) => {
                                        let cls =
                                            self.resolve_class(&crate::value::lossy(&cn)).and_then(
                                                |c| self.classes.get(&c.to_lowercase()).cloned(),
                                            );
                                        if let Some(cls) = cls {
                                            return self
                                                .static_invoke_vis(cls, &mname, args, None, true);
                                        }
                                        let e = self.exception("TypeError", &detail);
                                        let te = self.throw(e);
                                        self.fail(te)
                                    }
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
        share_statics: bool,
    ) -> Result<Option<Rc<PhpCallable>>, PhpError> {
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
        // A rebound closure is a new object (fresh handle id) that
        // SNAPSHOTS the source's static vars — the two tables evolve
        // independently afterwards (probe_bind: bindTo copies values).
        // Closure::call's temporary rebind instead SHARES the table,
        // like Zend's fake closure (call=2, then f()=3,4).
        let nc_rc = Rc::new(nc);
        if share_statics {
            nc_rc.id.set(c.id.get());
        } else {
            let id = self.next_callable_id(&nc_rc);
            nc_rc.id.set(id);
            if let CallableKind::Closure(d) = &nc_rc.kind {
                let src_key = format!("{}\u{0}c{}", d.name, c.id.get());
                if let Some(src) = self.statics.get(&src_key).cloned() {
                    let mut snap = std::collections::HashMap::new();
                    for (n, sc) in &src {
                        let cc = cell(sc.borrow().clone());
                        if self.is_ref_cell(sc) {
                            self.mark_ref(&cc);
                        }
                        snap.insert(n.clone(), cc);
                    }
                    self.statics.insert(format!("{}\u{0}c{}", d.name, id), snap);
                }
            }
        }
        Ok(Some(nc_rc))
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
                .rsplit_once("::")
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
        // The class slot resolves BEFORE any deprecation fires — a
        // failed 'self'/'parent' reports the scope error, not the
        // deprecation (oracle: `cannot access "self" when no class
        // scope is active` with no Deprecated line).
        let site = match &bound_obj {
            Some(o) => CallableSite {
                ce: o.borrow().class.clone(),
                bound: Some(o.clone()),
                strict: false,
            },
            None => match self.callable_site(&cn, None, true, true) {
                Ok(s) => s,
                Err(SiteErr::Thrown(e)) => return Err(e),
                Err(SiteErr::Msg(detail)) => {
                    return self.fail(PhpError::uncaught("Error", detail, 0));
                }
            },
        };
        let mut cls = site.ce.clone();
        // `['A2','parent::who']`-style qualifiers resolve against the
        // slot class (string form already split at '::').
        let mut mn = mn;
        if mn.contains("::") {
            match self.callable_leg(&site, Some(&cls), &mn, true, true) {
                Ok((ce2, m2)) => {
                    cls = ce2;
                    mn = m2;
                }
                Err(SiteErr::Thrown(e)) => return Err(e),
                Err(SiteErr::Msg(detail)) => {
                    return self.fail(PhpError::uncaught("Error", detail, 0));
                }
            }
        }
        // zend carries fcc->object from the slot check into the call —
        // `Closure::fromCallable('self::m')` inside a method binds $this.
        let bound_obj = bound_obj.or_else(|| site.bound.clone());
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
                    // The lookup autoloads — a throwing loader's
                    // exception propagates (zend), it is not a
                    // "not found" Error.
                    if let Some(c) = self.str_callable_class(cls)? {
                        return self.fcc_static(c, m);
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
                // Drop the borrow before any dispatch below — a
                // throwing autoloader under `['Cls','m']` runs user
                // code that may write this same array.
                let (t, m) = {
                    let a = a.borrow();
                    (a.get(&ArrKey::Int(0)), a.get(&ArrKey::Int(1)))
                };
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
                        // `['Cls','m']` invoke autoloads the class — a
                        // throwing loader's exception propagates.
                        match self.str_callable_class(&crate::value::lossy(&cn))? {
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
            Expr::Binary {
                op: "argline", r, ..
            } => self.const_scalar_callee(r, msg),
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
        mut args: CallArgs,
        unused: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        // zend frees a call's vm_stack arg space when the FRAME dies,
        // not when args finish binding — keep the unpack page tokens
        // on the pushed frame so their charge outlives bind_and_run_inner's
        // CallArgs drop.
        if let Some(f) = self.stack.last_mut() {
            f.vm_sites.append(&mut args.vm_sites);
        }
        // Callee `Stmt::Line` markers must not leak into the caller:
        // diagnostics after the call report the call-site line.
        let saved_line = self.cur_line;
        // prop_cell's `=&`-source stash pins the receiver object — a
        // callee's leftover must not keep that object alive after the
        // call returns (typed_properties_094).
        let saved_prop_ov = self.last_prop_ov.take();
        // The by-ref dim-read flag describes the CALLER's current op
        // (`$a['k'] =&`/BP_VAR_RW) — a callee's own op context starts
        // clean: a native offsetGet nested inside an override would
        // otherwise take the silent by-ref bucket path and swallow
        // the missing-key warning zend emits on a normal read.
        let saved_dim_by_ref = std::mem::replace(&mut self.dim_by_ref, false);
        let fr = self.call_site_frame(decl, &args);
        self.call_trace.push(fr);
        self.last_call_by_ref = decl.by_ref;
        let r = self.bind_and_run_inner(decl, args, unused);
        // Overwrite (don't restore): the flag must describe THIS callee even
        // though nested calls overwrote it during the body.
        self.last_call_by_ref = decl.by_ref;
        self.call_trace.pop();
        self.cur_line = saved_line;
        self.send_line = Some(saved_line);
        // Zend decrefs the frame's CVs at unwind — a local object
        // whose last strong refs are that frame's cells runs its
        // __destruct now (bug52361). A dtor error on a clean return
        // replaces the result and aborts; during unwind it chains —
        // destruct_frame_objs guards that itself.
        let out = if let Some(f) = self.last_popped_frame.take() {
            // A generator body's frame pops as a suspension, not a
            // return — Zend keeps its CVs live in execute_data until
            // the gen closes or is destroyed, then decrefs them after
            // the finally journal. Stash the cells on the journal
            // rather than destructing at this (eager) run's end.
            let suspended = self
                .gen_run_state
                .as_ref()
                .is_some_and(|st| st.borrow().owns_frame(decl));
            if suspended {
                let st = self.gen_run_state.clone().unwrap();
                // CV order ~ the variable's first source position —
                // the frame teardown decrefs in that order, so a
                // held object's __destruct / gen release lands at
                // its own slot (HashMap order would scramble it).
                let body_src = format!("{:?}", decl.body);
                let mut pairs: Vec<(String, Cell)> =
                    f.vars.iter().map(|(n, c)| (n.clone(), c.clone())).collect();
                pairs.sort_by_cached_key(|(n, _)| {
                    body_src
                        .find(&format!("Var(\"{}\")", n))
                        .unwrap_or(usize::MAX)
                });
                for (i, c) in f.args.iter().enumerate() {
                    pairs.push((format!("\u{0}arg{i}"), c.clone()));
                }
                if let Some(o) = f.this_obj.clone() {
                    pairs.push(("\u{0}this".to_string(), cell(Value::Object(o))));
                }
                let fin_rc = st.borrow().fin_q.clone();
                let mut fin = fin_rc.borrow_mut();
                let prev = std::mem::take(&mut fin.suspended);
                if !prev.is_empty() {
                    // Re-run: the resumed frame's CVs are the current
                    // state — per name its cell wins; names the re-run
                    // never re-materialized keep the suspended cell
                    // (Zend's parked frame still holds them).
                    for (n, c) in &prev {
                        if !pairs.iter().any(|(m, _)| m == n) {
                            pairs.push((n.clone(), c.clone()));
                        }
                    }
                    // Cells the new frame displaced die with `prev`
                    // — a gen among them whose every ref lives in the
                    // displaced set is a re-run artifact: suppress its
                    // destruction journal (the logical successor's own
                    // teardown owns the close, not this bookkeeping
                    // death). Cells kept by name above don't drop.
                    let mut tally: HashMap<usize, usize> = HashMap::new();
                    for (_, c) in prev
                        .iter()
                        .filter(|(n, _)| pairs.iter().any(|(m, _)| m == n))
                    {
                        if let Value::Object(o) = &*c.borrow() {
                            *tally.entry(Rc::as_ptr(o) as usize).or_insert(0) += 1;
                        }
                    }
                    for (_, c) in prev
                        .iter()
                        .filter(|(n, _)| pairs.iter().any(|(m, _)| m == n))
                    {
                        if let Value::Object(o) = &*c.borrow() {
                            let n = tally.get(&(Rc::as_ptr(o) as usize)).copied().unwrap_or(0);
                            if n == 0 || Rc::strong_count(o) != n {
                                continue;
                            }
                            if let Some(crate::value::ObjectInternal::Generator(gs)) =
                                &o.borrow().internal
                            {
                                let fq = gs.borrow().fin_q.clone();
                                let mut f = fq.borrow_mut();
                                f.suppressed = true;
                                // The displaced incarnation's journaled
                                // tail never ran — its orphaned ob
                                // windows drop un-confirmed captures.
                                f.kill_tree();
                            }
                        }
                    }
                }
                fin.suspended = pairs;
                r
            } else {
                let dtor_err = self.destruct_frame_objs(&f).err();
                match (r, dtor_err) {
                    (Ok(_), Some(e)) => Err(e),
                    (r, _) => r,
                }
            }
        } else {
            r
        };
        self.last_prop_ov = saved_prop_ov;
        self.dim_by_ref = saved_dim_by_ref;
        out
    }

    /// The callee's call-trace frame for a call about to be dispatched —
    /// call-site file/line resolved like zend (internal callback drivers
    /// render `[internal function]`, hidden trampolines like
    /// call_user_func lend their own site) plus trace-format args.
    /// Arity/binding failures reuse this so a callee that never ran a
    /// body still appears in the exception's trace (probe11).
    fn call_site_frame(&mut self, decl: &FunctionDecl, args: &CallArgs) -> TraceFrame {
        // The callee's argline markers overwrite `send_line`; after
        // the call returns the enclosing op's own line is the pending
        // site again — engine checks running post-call (getIterator
        // validation, conversion warnings) site at the caller's line.
        // A callback invoked from inside a builtin's own machinery
        // (internal_cb: ob handlers, sort callbacks) has call site
        // `[internal function]`; engine callbacks like the error handler
        // invoked mid-eval instead report the builtin's own call site
        // (bug32828 vs bug28213). The zend-equivalent "prev frame"
        // skips call_user_func* trampolines and frameless compile-
        // specialized calls (rope sprintf) — they leave no execute_data.
        // At shutdown the trace is empty — the engine itself is the
        // caller, which is also `[internal function]` (registered
        // shutdown fns, the dtor sweep).
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
        let (site_file, site_line) = self.call_site(true, sf, self.cur_line);
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
            // Zend binds skipped interior slots to the param default —
            // `f(a:5, c:7)` traces `f(5, 1, 7)` — except params the
            // optional-before-required rule makes required, which stay
            // NULL.
            let req_arity = decl
                .params
                .iter()
                .rposition(|p| p.default.is_none() && !p.variadic)
                .map(|i| i + 1)
                .unwrap_or(0);
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
                    .unwrap_or_else(|| {
                        if i < req_arity {
                            cell(Value::Null)
                        } else {
                            p.default
                                .as_ref()
                                .and_then(|d| self.eval_decl_const(d, &decl.file, decl.line).ok())
                                .map(cell)
                                .unwrap_or_else(|| cell(Value::Null))
                        }
                    });
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
        let mut fr = self
            .stack
            .last()
            .map(|f| TraceFrame {
                // fn_name is already the Zend scope name —
                // `{closure:Foo::m():L}`/`{closure:FILE:L}` included.
                function: f.fn_name.clone(),
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
                // A generator body resumed by a `Generator->{m}()`
                // call isn't a userland call — Zend stamps its trace
                // frame at the internal site (`[internal function]:
                // fn(args)`). Under an engine resume (foreach /
                // iterator_*) the body frame instead shows the
                // consumer's resume site (`FILE(line): fn(args)`).
                file: if f.gen_body && self.iter_calls == 0 && self.gen_internal_resume == 0 {
                    "[internal function]".to_string()
                } else {
                    site_file.clone()
                },
                line: site_line,
                args: targs.clone(),
                named_args: targs_named.clone(),
                internal: false,
                visible: true,
                named_dispatch: false,
                gen_resume: false,
                gen_body: f.gen_body,
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
                visible: true,
                named_dispatch: false,
                gen_resume: false,
                gen_body: false,
            });
        // Zend runs FilterIterator's accept loop in internal C — its
        // `fetch` frame never reaches a PHP trace.
        if fr.function.eq_ignore_ascii_case("fetch")
            && fr
                .class
                .as_deref()
                .is_some_and(|c| self.is_a_str(c, "FilterIterator"))
        {
            fr.visible = false;
        }
        fr
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
        // Signature deprecations (implicit-nullable, optional-before-
        // required) must precede the param checks' default-value
        // fatals — Zend emits them while compiling the params.
        self.sig_deprecations(fname, decl)?;
        for p in &decl.params {
            let Some(ty) = &p.ty else { continue };
            self.cur_line = decl.line;
            match &p.default {
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

    /// get_scope(frame): the calling function's declaring class scope.
    fn callable_frame_scope(&self) -> Option<Rc<PhpClass>> {
        self.stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned())
            .or_else(|| self.const_self.clone())
    }

    /// zend_is_callable_check_class — resolve a class-name slot to its
    /// calling scope. `scope_of` overrides the frame scope for the
    /// qualifier inside `[$ce, 'X::m']` ('self'→the slot, 'parent'→the
    /// slot's parent); 'static' always reads the frame's called scope.
    /// Missing class names run the autoload chain (bug45186_2's
    /// `call_user_func(['Lazy','sm'])` regression — the class must
    /// autoload just like resolve_class does).
    /// Class lookup for `'Cls::m'` string callables: the name is taken
    /// verbatim (no scope keywords, no namespace resolution) and a
    /// throwing autoloader's exception propagates.
    pub(in crate::interp) fn str_callable_class(
        &mut self,
        cn: &str,
    ) -> Result<Option<Rc<PhpClass>>, PhpError> {
        let raw = cn.trim_start_matches('\\');
        let lw = raw.to_lowercase();
        if let Some(c) = self.classes.get(&lw) {
            return Ok(Some(c.clone()));
        }
        if !raw.is_empty() {
            self.run_autoload(raw)?;
        }
        Ok(self.classes.get(&lw).cloned())
    }

    pub(in crate::interp) fn callable_site(
        &mut self,
        cn: &str,
        scope_of: Option<&Rc<PhpClass>>,
        emit_dep: bool,
        autoload: bool,
    ) -> Result<CallableSite, SiteErr> {
        let raw = cn.trim_start_matches('\\');
        let lw = raw.to_lowercase();
        let this = self.stack.last().and_then(|f| f.this_obj.clone());
        let frame_scope = self.callable_frame_scope();
        let scope = scope_of.cloned().or_else(|| frame_scope.clone());
        match lw.as_str() {
            "self" => match scope {
                None => Err(SiteErr::Msg(
                    "cannot access \"self\" when no class scope is active".to_string(),
                )),
                Some(s) => {
                    if emit_dep {
                        let _ = self.deprecated("Use of \"self\" in callables is deprecated");
                    }
                    Ok(CallableSite {
                        ce: s,
                        bound: this,
                        strict: false,
                    })
                }
            },
            "parent" => match scope {
                None => Err(SiteErr::Msg(
                    "cannot access \"parent\" when no class scope is active".to_string(),
                )),
                Some(s) => match s
                    .decl
                    .parent
                    .as_deref()
                    .and_then(|p| self.classes.get(&p.to_lowercase()).cloned())
                {
                    // Callable validation (`is_callable`, cuf arginfo)
                    // reports this as a not-callable detail — zend's
                    // 'Cannot use "parent" ...' compile fatal is gated
                    // to the `parent::` dispatch syntax (traits/
                    // bug76773-deprecated).
                    None => Err(SiteErr::Msg(
                        "cannot access \"parent\" when current class scope has no parent"
                            .to_string(),
                    )),
                    Some(p) => {
                        if emit_dep {
                            let _ = self.deprecated("Use of \"parent\" in callables is deprecated");
                        }
                        Ok(CallableSite {
                            ce: p,
                            bound: this,
                            strict: true,
                        })
                    }
                },
            },
            "static" => {
                let called = self
                    .stack
                    .last()
                    .and_then(|f| f.called_class.clone())
                    .or(scope);
                match called {
                    None => Err(SiteErr::Msg(
                        "cannot access \"static\" when no class scope is active".to_string(),
                    )),
                    Some(s) => {
                        if emit_dep {
                            let _ = self.deprecated("Use of \"static\" in callables is deprecated");
                        }
                        Ok(CallableSite {
                            ce: s,
                            bound: this,
                            strict: true,
                        })
                    }
                }
            }
            _ => {
                let found = match self.classes.get(&lw).cloned() {
                    Some(c) => Some(c),
                    // zend_lookup_class bails on empty names without
                    // invoking autoloaders; a throwing autoloader's
                    // exception propagates (it is not a validation
                    // failure).
                    None if autoload && !raw.is_empty() => {
                        if let Err(e) = self.run_autoload(raw) {
                            return Err(SiteErr::Thrown(e));
                        }
                        self.classes.get(&lw).cloned()
                    }
                    None => None,
                };
                match found {
                    None => Err(SiteErr::Msg(format!("class \"{}\" not found", raw))),
                    Some(ce) => {
                        // fcc->object binds $this only when $this's class
                        // is-a the calling scope AND the scope is-a ce
                        // (object-context forwarding — bug45186).
                        let bound = match (&this, &frame_scope) {
                            (Some(o), Some(s)) => {
                                let oc = o.borrow().class.name().to_string();
                                if self.is_a_str(&oc, s.name())
                                    && self.is_a_str(s.name(), ce.name())
                                {
                                    this.clone()
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        Ok(CallableSite {
                            ce,
                            bound,
                            strict: true,
                        })
                    }
                }
            }
        }
    }

    /// zend_is_callable_check_func's method leg: split an `X::m`
    /// qualifier off the method slot, resolve it against `ce_org` (the
    /// array's class slot — 'Cls::m' strings pass None and resolve
    /// against the frame scope), emit the deprecated-form notice once
    /// the qualifier resolves, then run the method-table ladder:
    /// non-public found methods with __call/__callStatic reroute to
    /// the magic path; misses report `class X does not have a method
    /// "m"`; found candidates fail in order: abstract, non-static
    /// (unbound), visibility.
    pub(in crate::interp) fn callable_leg(
        &mut self,
        site: &CallableSite,
        ce_org: Option<&Rc<PhpClass>>,
        mn: &str,
        emit_dep: bool,
        autoload: bool,
    ) -> Result<(Rc<PhpClass>, String), SiteErr> {
        let mut ce = site.ce.clone();
        let mut mname = mn.to_string();
        if let Some((q, m2)) = mn.rsplit_once("::") {
            let qs = self.callable_site(q, ce_org, false, autoload)?;
            if let Some(orig) = ce_org {
                if !self.is_a_str(orig.name(), qs.ce.name()) {
                    return Err(SiteErr::Msg(format!(
                        "class {} is not a subclass of {}",
                        orig.name(),
                        qs.ce.name()
                    )));
                }
                if emit_dep {
                    let _ = self.deprecated(&format!(
                        "Callables of the form [\"{}\", \"{}\"] are deprecated",
                        orig.name(),
                        mn
                    ));
                }
            }
            ce = qs.ce;
            mname = m2.to_string();
        }
        let mut found = self.find_method_in(&ce, &mname);
        if let Some((m, dc)) = &found {
            // An inaccessible found-method with magic defined falls
            // through to the via-handler path (zend's
            // get_function_via_handler) — `[$f,'priv']` stays callable
            // via __call.
            if m.visibility != crate::ast::Visibility::Public {
                let magic = if site.bound.is_some() {
                    self.find_method_in(&ce, "__call").is_some()
                } else {
                    self.find_method_in(&ce, "__callstatic").is_some()
                };
                if magic && !self.method_access_ok(m, dc) {
                    found = None;
                }
            }
        }
        let Some((m, dc)) = found else {
            // via-handler fallback: bound calls with the slot intact
            // trampoline through __call; everything else through
            // __callStatic.
            let obj_ctx = site.bound.is_some() && ce_org.is_some_and(|o| o.name() == ce.name());
            let magic = if obj_ctx { "__call" } else { "__callstatic" };
            if self.find_method_in(&ce, magic).is_some() {
                return Ok((ce, mname));
            }
            return Err(SiteErr::Msg(format!(
                "class {} does not have a method \"{}\"",
                ce.name(),
                mname
            )));
        };
        if m.is_abstract {
            return Err(SiteErr::Msg(format!(
                "cannot call abstract method {}::{}()",
                ce.name(),
                m.decl.name
            )));
        }
        if site.bound.is_none() && !m.is_static {
            return Err(SiteErr::Msg(format!(
                "non-static method {}::{}() cannot be called statically",
                ce.name(),
                m.decl.name
            )));
        }
        if m.visibility != crate::ast::Visibility::Public && !self.method_access_ok(&m, &dc) {
            let vis = match m.visibility {
                crate::ast::Visibility::Protected => "protected",
                crate::ast::Visibility::Private => "private",
                _ => "public",
            };
            return Err(SiteErr::Msg(format!(
                "cannot access {} method {}::{}()",
                vis,
                ce.name(),
                m.decl.name
            )));
        }
        Ok((ce, mname))
    }

    /// Dispatch for a validated callable — zend calls the resolved
    /// fcc handler: the calling-scope's method on fcc->object (a
    /// qualifier pins it, so `[$this,'TestA::m']` never re-dispatches
    /// to the child's override — bug32290); statics and misses
    /// (via-magic) go through the static/dynamic fallbacks.
    fn callable_invoke(
        &mut self,
        ce: Rc<PhpClass>,
        mn: &str,
        args: CallArgs,
        bound: Option<Rc<RefCell<PhpObject>>>,
    ) -> Result<Value, PhpError> {
        match self.find_method_in(&ce, mn) {
            Some((m, dc)) if !m.is_static => match bound {
                Some(o) => {
                    self.pending_decl_class = Some(dc.clone());
                    self.pending_called_class = Some(o.borrow().class.clone());
                    self.pending_decl_site = Some(Rc::as_ptr(&m) as usize);
                    let r = self.invoke_fn(&Rc::new(m.decl.clone()), args, Some(o), Some(dc));
                    self.pending_decl_class = None;
                    self.pending_called_class = None;
                    r
                }
                None => self.static_invoke_vis(ce, mn, args, None, true),
            },
            Some(_) => self.static_invoke_vis(ce, mn, args, None, true),
            None => match bound {
                // Via-handler resolution — the receiver's __call or
                // the class's __callStatic trampoline.
                Some(o) => self.method_invoke_vis(o, mn, args),
                None => self.static_invoke_vis(ce, mn, args, None, true),
            },
        }
    }

    /// Shared class+method check for `['Cls','m']` array callables.
    fn callable_pair_ok(&mut self, cn: &str, mn: &str) -> Result<bool, PhpError> {
        match self.callable_site(cn, None, true, true) {
            Ok(site) => {
                let ce_org = site.ce.clone();
                match self.callable_leg(&site, Some(&ce_org), mn, true, true) {
                    Ok(_) => Ok(true),
                    Err(SiteErr::Thrown(e)) => Err(e),
                    Err(SiteErr::Msg(_)) => Ok(false),
                }
            }
            Err(SiteErr::Thrown(e)) => Err(e),
            Err(SiteErr::Msg(_)) => Ok(false),
        }
    }

    /// `[$obj, 'm']` — the object is the bound context (fcc->object).
    fn callable_obj_ok(&mut self, o: &Rc<RefCell<PhpObject>>, mn: &str) -> Result<bool, PhpError> {
        let ce = o.borrow().class.clone();
        let site = CallableSite {
            ce: ce.clone(),
            bound: Some(o.clone()),
            strict: false,
        };
        match self.callable_leg(&site, Some(&ce), mn, true, true) {
            Ok(_) => Ok(true),
            Err(SiteErr::Thrown(e)) => Err(e),
            Err(SiteErr::Msg(_)) => Ok(false),
        }
    }

    /// `is_callable`-family validation where a throwing autoloader's
    /// exception propagates (`is_callable('Nope::sm')` with a loader
    /// that throws — zend's exception, not a false).
    pub fn try_is_callable_value(&mut self, v: &Value) -> Result<bool, PhpError> {
        match v {
            Value::Callable(_) => Ok(true),
            Value::Str(s) => {
                let s = String::from_utf8_lossy(s).to_string();
                if self.functions.contains_key(&s.to_lowercase())
                    || builtins::is_builtin(&s.to_lowercase())
                    || builtins::builtin_params(&s.to_lowercase()).is_some()
                {
                    return Ok(true);
                }
                // 'Cls::m' splits at the LAST '::' (zend_memrchr).
                let Some((cn, mn)) = s.rsplit_once("::") else {
                    return Ok(false);
                };
                match self.callable_site(cn, None, true, true) {
                    Ok(site) => {
                        let ce_org = site.ce.clone();
                        match self.callable_leg(&site, Some(&ce_org), mn, true, true) {
                            Ok(_) => Ok(true),
                            Err(SiteErr::Thrown(e)) => Err(e),
                            Err(SiteErr::Msg(_)) => Ok(false),
                        }
                    }
                    Err(SiteErr::Thrown(e)) => Err(e),
                    Err(SiteErr::Msg(_)) => Ok(false),
                }
            }
            Value::Object(o) => {
                let cn = o.borrow().class.decl.name.clone();
                let Some(c) = self.classes.get(&cn.to_lowercase()).cloned() else {
                    return Ok(false);
                };
                Ok(self.find_method_in(&c, "__invoke").is_some())
            }
            Value::Array(a) => {
                let arr = a.borrow();
                // zend: num_elements must be exactly 2 AND live at
                // indices 0/1 (`[9=>'K',10=>'m']` fails the index check).
                if arr.len() != 2 {
                    return Ok(false);
                }
                let first = arr.get(&crate::value::ArrKey::Int(0));
                let second = arr.get(&crate::value::ArrKey::Int(1));
                let (Some(first), Some(second)) = (first, second) else {
                    return Ok(false);
                };
                let Value::Str(mn) = &second else {
                    return Ok(false);
                };
                let mn = String::from_utf8_lossy(mn).to_string();
                match &first {
                    Value::Str(cn) => self.callable_pair_ok(&String::from_utf8_lossy(cn), &mn),
                    Value::Callable(_) => Ok(mn.eq_ignore_ascii_case("__invoke")),
                    Value::Object(o) => self.callable_obj_ok(o, &mn),
                    _ => Ok(false),
                }
            }
            _ => Ok(false),
        }
    }

    /// Validation-style callable check: a throwing autoloader's
    /// exception is abandoned — the value simply is not callable.
    /// The error is stashed in `callable_probe_err` so a failing
    /// `callable` param type can re-raise it (zend propagates the
    /// autoload exception instead of emitting TypeError).
    pub fn is_callable_value(&mut self, v: &Value) -> bool {
        match self.try_is_callable_value(v) {
            Ok(b) => b,
            Err(e) => {
                // The throwable VALUE lives in pending_exception —
                // stash it with the error so a `callable` param can
                // re-raise the original exception, not a Null.
                let v = self.pending_exception.take().unwrap_or(Value::Null);
                self.pending_exception = None;
                self.callable_probe_err = Some((v, e));
                false
            }
        }
    }

    /// Consume the stashed probe error of a failing
    /// `is_callable_value`, restoring the throwable for re-raise.
    /// Zend propagates a throwing autoloader's exception through
    /// callable validation everywhere except the call_user_func
    /// family, which wraps it in its own TypeError.
    pub fn take_callable_probe_err(&mut self) -> Option<PhpError> {
        let (v, e) = self.callable_probe_err.take()?;
        if e.kind == ErrorKind::Throw {
            self.pending_exception = Some(v);
        }
        Some(e)
    }

    /// The ` in <file> on line <n>` tail of a too-few-args error. Zend
    /// drops it when the immediate caller is an internal function
    /// (array_map's driver frame) — except the VM-inlined
    /// call_user_func family, whose caller frame is the user's own.
    /// `caller_depth` says how far back the caller sits on call_trace:
    /// 1 when this callee's own frame is already pushed, 0 when the
    /// count check fires before the push (invoke_fn).
    fn arg_err_in(&self, caller_depth: usize) -> String {
        let internal_driver = self
            .call_trace
            .iter()
            .rev()
            .filter(|f| !crate::value::trace_frame_hidden(f))
            .nth(caller_depth)
            .map(|f| f.internal)
            .unwrap_or(false);
        if internal_driver {
            String::new()
        } else {
            format!(" in {} on line {}", self.diag_file(), self.cur_line)
        }
    }

    /// PHP's "given" type word in TypeError messages.
    pub fn zval_type_name(&self, v: &Value) -> String {
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

    /// Failure detail for a class+method callable pair once validation
    /// already failed — the class slot (scope kw / autoload / not
    /// found), then the method leg's ordered ladder (bug45186).
    fn callable_pair_detail(&mut self, cn: &str, mn: &str) -> String {
        match self.callable_site(cn, None, false, false) {
            Err(SiteErr::Msg(d)) => d,
            // autoload=false — Thrown cannot happen; keep the detail
            // shape zend would have reported anyway.
            Err(SiteErr::Thrown(_)) => format!("class \"{}\" not found", cn),
            Ok(site) => {
                let ce_org = site.ce.clone();
                match self.callable_leg(&site, Some(&ce_org), mn, false, false) {
                    Err(SiteErr::Msg(d)) => d,
                    Err(SiteErr::Thrown(_)) => {
                        format!("class {} does not have a method \"{}\"", ce_org.name(), mn)
                    }
                    Ok(_) => format!("class {} does not have a method \"{}\"", ce_org.name(), mn),
                }
            }
        }
    }

    /// Detail for `[$obj,'m']` once validation failed (same ladder).
    fn callable_obj_detail(&mut self, o: &Rc<RefCell<PhpObject>>, mn: &str) -> String {
        let ce = o.borrow().class.clone();
        let site = CallableSite {
            ce: ce.clone(),
            bound: Some(o.clone()),
            strict: false,
        };
        match self.callable_leg(&site, Some(&ce), mn, false, false) {
            Err(SiteErr::Msg(d)) => d,
            Err(SiteErr::Thrown(_)) | Ok(_) => {
                format!("class {} does not have a method \"{}\"", ce.name(), mn)
            }
        }
    }

    /// Zend's callback-validation error detail for internal functions
    /// (the part after `must be a valid callback`/`or null,`).
    pub fn zpp_callback_detail(&mut self, v: &Value) -> String {
        match v {
            Value::Array(a) => {
                let a = a.borrow();
                // zend order: arity first, then index presence, then
                // the member-type checks.
                if a.len() != 2 {
                    return "array callback must have exactly two members".into();
                }
                let first = a.get(&crate::value::ArrKey::Int(0));
                let second = a.get(&crate::value::ArrKey::Int(1));
                let (Some(first), Some(second)) = (first, second) else {
                    return "array callback has to contain indices 0 and 1".into();
                };
                if !matches!(
                    &first,
                    Value::Str(_) | Value::Object(_) | Value::Callable(_)
                ) {
                    return "first array member is not a valid class name or object".into();
                }
                let Value::Str(m) = &second else {
                    return "second array member is not a valid method".into();
                };
                let m = String::from_utf8_lossy(m).to_string();
                match &first {
                    Value::Str(cn) => self.callable_pair_detail(&String::from_utf8_lossy(cn), &m),
                    Value::Object(o) => self.callable_obj_detail(o, &m),
                    // A Closure IS an object to zend — a bad method name
                    // reports against it like any other object slot.
                    Value::Callable(_) => {
                        format!("class Closure does not have a method \"{}\"", m)
                    }
                    _ => "first array member is not a valid class name or object".into(),
                }
            }
            Value::Str(s) => {
                let s = String::from_utf8_lossy(s).to_string();
                match s.rsplit_once("::") {
                    // "Cls::m" strings fail on the class+method ladder
                    // like the array form (bug45186_2's
                    // `class bar does not have a method "www"`).
                    Some((cn, mn)) => match self.callable_site(cn, None, false, false) {
                        Err(SiteErr::Msg(d)) => d,
                        Err(SiteErr::Thrown(_)) => format!("class \"{}\" not found", cn),
                        Ok(site) => {
                            let ce_org = site.ce.clone();
                            match self.callable_leg(&site, Some(&ce_org), mn, false, false) {
                                Err(SiteErr::Msg(d)) => d,
                                Err(SiteErr::Thrown(_)) | Ok(_) => format!(
                                    "class {} does not have a method \"{}\"",
                                    site.ce.name(),
                                    mn
                                ),
                            }
                        }
                    },
                    None => format!("function \"{}\" not found or invalid function name", s),
                }
            }
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
                } else if let Some(pos) = m.find('\0') {
                    // `{base}@anonymous\0FILE:LINE$N` — the internal
                    // decl-site suffix never leaks into TypeErrors
                    // (union_types/anonymous_class).
                    vec![m[..pos].to_string()]
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

    /// zend coerces the frame's own arg slot — a fresh cell — never
    /// through the caller's (spread args share the source array's
    /// cell, so `*cell.borrow_mut() = v` retypes caller state).
    /// The send-time trace frame's arg IS that slot zend coerces
    /// (`f(5, 'x')`, debug_backtrace args) — repoint it too.
    fn coerce_arg_slot(
        &mut self,
        args: &mut CallArgs,
        by_name: &mut [Option<(Cell, bool, bool)>],
        i: usize,
        cv: Value,
    ) {
        let c = cell(cv);
        if let Some(slot) = args.cells.get_mut(i) {
            *slot = c.clone();
        } else if let Some(t) = by_name[i].as_mut() {
            t.0 = c.clone();
        }
        if let Some(a) = self.call_trace.last_mut().and_then(|fr| fr.args.get_mut(i)) {
            *a = c;
        }
    }

    fn bind_and_run_inner(
        &mut self,
        decl: &FunctionDecl,
        mut args: CallArgs,
        _unused: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        // Required count runs to the last non-default param: an
        // optional declared before a required is itself required
        // (`opt($a = 1, $b)` needs 2 args — zend's "implicitly required").
        let required = decl
            .params
            .iter()
            .rposition(|p| p.default.is_none() && !p.variadic)
            .map(|i| i + 1)
            .unwrap_or(0);
        // With named args, missing-required is reported per-param during
        // binding ("Argument #N ($x) not passed"); the count check below
        // is the positional-only form.
        if args.named.is_empty() && args.len() < required {
            self.stack_pop();
            let mut e = PhpError::uncaught(
                "ArgumentCountError",
                format!(
                    "Too few arguments to function {}(), {} passed{} and {} {} expected",
                    self.decl_fname(decl),
                    args.len(),
                    self.arg_err_in(1),
                    if required == decl.params.len() {
                        "exactly"
                    } else {
                        "at least"
                    },
                    required
                ),
                0,
            );
            e.thrown_line = Some(decl.line);
            return self.fail(e);
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
                    self.stack_pop();
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
                    self.stack_pop();
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
            // Probe errors belong to THIS param's type check only.
            self.callable_probe_err = None;
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
                    self.coerce_arg_slot(&mut args, &mut by_name, i, cv);
                }
            }
            // strict mode still allows the int->float widening stored
            // back for visibility in the callee.
            if ok
                && caller_strict
                && ty.iter().any(|m| m.eq_ignore_ascii_case("float"))
                && !self.ty_weak_exact(ty, &v)
            {
                if let Value::Int(n) = v {
                    self.coerce_arg_slot(&mut args, &mut by_name, i, Value::Float(n as f64));
                }
            }
            if !ok {
                // A `callable` member's resolution ran a swallowing
                // probe — an autoloader that threw has its exception
                // propagate (zend raises it, not the TypeError).
                if ty.iter().any(|m| m.eq_ignore_ascii_case("callable")) {
                    if let Some(e) = self.take_callable_probe_err() {
                        self.stack_pop();
                        return self.fail(e);
                    }
                }
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
                // A callback dispatched from inside a builtin
                // (array_walk, usort, ob handlers) traces from
                // `[internal function]` — zend's message then drops the
                // `called in ... and defined` tail too (bug24658).
                let internal_site = self
                    .call_trace
                    .last()
                    .map(|f| f.file == "[internal function]")
                    .unwrap_or(false);
                let msg = if call_alias.is_some() || internal_site {
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
                let display = if internal_site {
                    msg.clone()
                } else {
                    format!("{} and defined", msg)
                };
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
                let frame = if internal_site {
                    // The callee's own pushed frame carries the
                    // `[internal function]` site and the callback args.
                    self.call_trace
                        .last()
                        .map(crate::value::trace_frame_str)
                        .unwrap_or_default()
                } else {
                    format!(
                        "{}({}): {}({})",
                        self.diag_file(),
                        self.cur_line,
                        tname,
                        argdesc
                    )
                };
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
                self.stack_pop();
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
                            self.mark_ref(v);
                            arr.push_cell(v.clone());
                        } else {
                            arr.push(v.borrow().clone());
                        }
                    }
                    for (n, c) in &variadic_named {
                        if p.by_ref {
                            arr.is_ref = true;
                            self.mark_ref(c);
                            arr.set_cell(ArrKey::Str(n.clone().into()), c.clone());
                        } else {
                            arr.set(ArrKey::Str(n.clone().into()), c.borrow().clone());
                        }
                    }
                    // zend packs the spill args into a fresh arData at
                    // bind — a real emalloc the frame pays until teardown;
                    // the charge model must see it too (it's the figure
                    // oracle's fatals report: ht_req(n, packed)).
                    let packed = arr.entries.iter().all(|(k, _)| matches!(k, ArrKey::Int(_)));
                    let n = arr.entries.len();
                    let rc = Rc::new(RefCell::new(arr));
                    if n > 0 {
                        // The pack's emalloc runs inside the callee
                        // frame — zend attributes its OOM to the
                        // callee's decl line, not the send site.
                        let pl = self.cur_line;
                        self.cur_line = decl.line;
                        self.mem_realloc(&rc, Self::ht_req(n, packed));
                        self.cur_line = pl;
                    }
                    binds.push((p.name.clone(), cell(Value::Array(rc))));
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
                                || (self.is_ref_cell(c) && Rc::strong_count(c) > 1),
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
                            // 'value given' passes a zval copy — the
                            // param's writes stay local.
                            binds.push((p.name.clone(), cell(v.borrow().clone())));
                            continue;
                        }
                        // The callee's var becomes a Zend IS_REFERENCE
                        // over the caller's cell — write-through errors
                        // say "reference held by property"
                        // (typed_properties_055/108).
                        self.mark_ref(v);
                        binds.push((p.name.clone(), v.clone()));
                    } else {
                        binds.push((p.name.clone(), cell(v.borrow().clone())));
                    }
                } else if let Some(d) = if i < required {
                    None
                } else {
                    p.default.as_ref()
                } {
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
                    // Param defaults name their enclosing function —
                    // const_self serves `self::` binds but must not
                    // blank the enclosing fn name the way class-init
                    // const eval does ({closure:M::m():L}).
                    let pb = self.param_bind_ctx.replace(self.class_const_ctx);
                    let r = self.eval_decl_const(d, &decl.file, decl.line);
                    self.param_bind_ctx = pb;
                    self.const_self = old;
                    self.cur_line = prev_line;
                    let mut dv = match r {
                        Ok(v) => v,
                        Err(e) => {
                            self.stack_pop();
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
                            self.stack_pop();
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
                            self.stack_pop();
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
                    self.stack_pop();
                    let mut e = PhpError::uncaught(
                        "ArgumentCountError",
                        format!("{}(): Argument #{} (${}) not passed", fname, i + 1, p.name),
                        0,
                    );
                    e.thrown_line = Some(decl.line);
                    return self.fail(e);
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
        // The body is its own compile unit — loop/switch depth for
        // `break N` operand checks restarts here, not at the caller's.
        let saved_depth = std::mem::replace(&mut self.loop_depth, 0);
        let flow = self.exec_block(&decl.body);
        self.loop_depth = saved_depth;
        let ret_fname = self.decl_fname(decl);
        // `static` resolves against THIS frame's called class — after
        // the pop, `stack.last()` is the caller (static_type_return).
        let resolved_ret = decl.ret.as_ref().map(|ty| self.resolve_static(ty));
        let popped = self.stack_pop();
        // Zend decrefs the frame's CVs at unwind — the popped frame
        // is handed to bind_and_run, which runs its __destruct pass
        // after the call-trace pop so the dtor's trace attributes to
        // the caller's site (bug52361). Its declaring file is kept:
        // a body-level compile fatal (stray break/continue/goto)
        // attributes to the declaring unit, not the caller frame
        // `diag_file()` would now see.
        let popped_file = popped.as_ref().map(|f| f.file.clone());
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
            Flow::Break(_) => {
                // Compile fatal in Zend (function bodies are compiled
                // eagerly) — carry the compile-context backtrace and
                // attribute to the declaring file, whose unit died at
                // compile.
                let mut e = PhpError::compile_fatal(
                    "'break' not in the 'loop' or 'switch' context",
                    self.cur_line,
                );
                e.trace = Some(self.compile_err_frames());
                let r = self.fail(e);
                if let Some(f) = &popped_file {
                    self.last_err_file = f.clone();
                }
                r
            }
            Flow::Continue(_) => {
                let mut e = PhpError::compile_fatal(
                    "'continue' not in the 'loop' or 'switch' context",
                    self.cur_line,
                );
                e.trace = Some(self.compile_err_frames());
                let r = self.fail(e);
                if let Some(f) = &popped_file {
                    self.last_err_file = f.clone();
                }
                r
            }
            Flow::Goto(l) => {
                let mut e = PhpError::compile_fatal(
                    format!("'goto' to undefined label '{}'", l),
                    self.cur_line,
                );
                e.trace = Some(self.compile_err_frames());
                let r = self.fail(e);
                if let Some(f) = &popped_file {
                    self.last_err_file = f.clone();
                }
                r
            }
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
        // Consume any queued decl-origin anchor up front so early
        // returns (stub dispatch, arity errors) cannot leak it into
        // the next call's frame.
        let decl_site = self.pending_decl_site.take();
        // Native SPL stubs (empty body, line 0) hit spl_method even on
        // paths that bypass method dispatch — parent:: calls reach here
        // with the stub decl directly.
        if decl.body.is_empty() && decl.line == 0 {
            if let Some(o) = &this_obj {
                let cn = o.borrow().class.name().to_string();
                if self.is_a_str(&cn, "splfileinfo") {
                    if let Some(v) = self.spl_method(o, &decl.name, &args)? {
                        return Ok(v);
                    }
                }
                if self.is_a_str(&cn, "arrayiterator") || self.is_a_str(&cn, "arrayobject") {
                    if let Some(v) = self.array_iter_method(o, &decl.name, &args)? {
                        return Ok(v);
                    }
                }
            }
        }
        // Required count runs to the last non-default param: an
        // optional declared before a required is itself required
        // (`opt($a = 1, $b)` needs 2 args — zend's "implicitly required").
        let required = decl
            .params
            .iter()
            .rposition(|p| p.default.is_none() && !p.variadic)
            .map(|i| i + 1)
            .unwrap_or(0);
        if args.named.is_empty() && args.len() < required {
            // Zend verifies arity inside the callee's call frame — the
            // thrown ArgumentCountError still lists the callee
            // ([internal function] for builtin-driven callbacks) and
            // attributes the throw to the declaration line
            // (probe11/probe11c). A minimal exec frame gives
            // call_site_frame the callee identity (C->m vs plain f()).
            let mut frame = Frame::new(decl.name.clone());
            frame.this_obj = this_obj.clone();
            frame.scope_class = scope_class.clone();
            frame.decl_class = self
                .pending_decl_class
                .clone()
                .or_else(|| scope_class.clone());
            self.stack.push(frame);
            let fr = self.call_site_frame(decl, &args);
            self.call_trace.push(fr);
            let fname = self.decl_fname(decl);
            self.stack_pop();
            let mut e = PhpError::uncaught(
                "ArgumentCountError",
                format!(
                    "Too few arguments to function {}(), {} passed{} and {} {} expected",
                    fname,
                    args.len(),
                    self.arg_err_in(1),
                    if required == decl.params.len() {
                        "exactly"
                    } else {
                        "at least"
                    },
                    required
                ),
                0,
            );
            e.thrown_line = Some(decl.line);
            let r = self.fail(e);
            self.call_trace.pop();
            return r;
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
                closure_rc: None,
            })));
        }
        let dc = self.pending_decl_class.take();
        let cc = self.pending_called_class.take();
        self.invoke_fn_run(decl, args, this_obj, scope_class, dc, cc, None, decl_site)
    }

    /// Frame push + body run — the part of invoke_fn the Generator
    /// start path also uses (the yield check must not re-trip here).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::interp) fn invoke_fn_run(
        &mut self,
        decl: &Rc<FunctionDecl>,
        args: CallArgs,
        this_obj: Option<Rc<RefCell<PhpObject>>>,
        scope_class: Option<Rc<PhpClass>>,
        decl_class: Option<Rc<PhpClass>>,
        called_class: Option<Rc<PhpClass>>,
        closure_rc: Option<Rc<PhpCallable>>,
        decl_site: Option<usize>,
    ) -> Result<Value, PhpError> {
        let mut frame = Frame::new(decl.name.clone());
        frame.decl_site = decl_site.unwrap_or(Rc::as_ptr(decl) as usize);
        frame.closure_rc = closure_rc;
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
        frame.gen_body = self.pending_gen_body;
        self.pending_gen_body = false;
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
        | "shuffle" | "reset" | "end" | "next" | "prev" | "array_push" | "array_unshift"
        | "array_splice" => &[true],
        "preg_match" | "preg_match_all" => &[false, false, true],
        "preg_replace"
        | "preg_replace_callback"
        | "preg_filter"
        | "str_replace"
        | "str_ireplace" => &[false, false, false, false, true],
        "preg_replace_callback_array" => &[false, false, false, true],
        "parse_str" => &[false, true],
        "getopt" => &[false, false, true],
        "is_callable" => &[false, false, true],
        "sscanf" | "fscanf" => &[false, false],
        "exec" => &[false, true, true],
        "passthru" | "system" => &[false, true],
        "proc_open" => &[false, false, true],
        "stream_select" => &[true, true, true, false, false],
        "flock" => &[false, false, true],
        "preg_grep" => &[false],
        _ => return None,
    })
}

/// A literal `sprintf(...)` Zend compiles to rope-concat opcodes
/// instead of a call (zend_compile_func_sprintf): the format is a
/// constant string under 256 bytes, placeholders are only `%s`/`%d`
/// (`%%` emits a literal `%`), and placeholder count == value count.
/// No call exists at runtime, so the frame contributes nothing to
/// traces. Everything else — other literal builtins, non-const or
/// non-rope formats, dynamic dispatches — is a real call.
fn zend_literal_no_frame(name: &str, args: &[Expr]) -> bool {
    if name != "sprintf" {
        return false;
    }
    // Named args make it a dynamic arg-bind (sprintf's '*' variadic
    // rejects them anyway) — Zend emits a real call, not a rope.
    if args
        .iter()
        .any(|a| matches!(Interp::unmark_arg(a), Expr::Binary { op: "named", .. }))
    {
        return false;
    }
    // Compile-time-constant format — a quoted literal is `Expr::Str`,
    // an all-literal `Expr::Interp`, or a foldable `"a" . "b"` concat
    // chain (zend_compile const-folds literal concats before the
    // sprintf check, so `"%" . "s"` specializes too).
    let fmt: Vec<u8> = match args
        .first()
        .map(Interp::unmark_arg)
        .and_then(const_str_fold)
    {
        Some(v) => v,
        None => return false,
    };
    let fmt = fmt.as_slice();
    if fmt.len() >= 256 {
        return false;
    }
    let mut n = 0usize;
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] == b'%' {
            i += 1;
            if i >= fmt.len() {
                return false;
            }
            match fmt[i] {
                b's' | b'd' => n += 1,
                b'%' => {}
                _ => return false,
            }
        }
        i += 1;
    }
    n == args.len() - 1
}

/// Compile-time-constant string value of an expression node: a quoted
/// literal, an all-literal interp, or a concat of folds. Mirrors the
/// const-fold zend_compile performs before checking whether a call
/// specializes — named consts/defines are NOT folded (they resolve at
/// runtime).
fn const_str_fold(e: &Expr) -> Option<Vec<u8>> {
    match e {
        Expr::Binary {
            op: "argline", r, ..
        } => const_str_fold(r),
        Expr::Str(s) => Some(s.as_bytes().to_vec()),
        Expr::Interp(parts) => {
            let mut v = Vec::new();
            for p in parts {
                match p {
                    crate::lexer::StringPart::Lit(t) => v.extend_from_slice(t),
                    _ => return None,
                }
            }
            Some(v)
        }
        Expr::Binary { op: ".", l, r, .. } => {
            let mut v = const_str_fold(l)?;
            v.extend(const_str_fold(r)?);
            Some(v)
        }
        _ => None,
    }
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
