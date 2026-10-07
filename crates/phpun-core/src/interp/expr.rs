//! Expression evaluation: `eval` and the assign/store/index/cell
//! machinery, typed slots, inc/dec, unary/binary/arith ops and casts.

use super::util::*;
use super::*;

impl<'a> Interp<'a> {
    // ----- expressions -----

    pub fn eval(&mut self, e: &Expr) -> Result<Value, PhpError> {
        match e {
            Expr::Null => Ok(Value::Null),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Int(i) => Ok(Value::Int(*i)),
            Expr::Float(f) => Ok(Value::Float(*f)),
            Expr::Str(s) => Ok(Value::str(s.clone())),
            Expr::Interp(parts) => {
                let mut s: Vec<u8> = Vec::new();
                for p in parts {
                    match p {
                        StringPart::Lit(t) => s.extend_from_slice(t),
                        StringPart::Var(name) => {
                            let v = self.var_get(name)?;
                            let cs = self.conv_bytes(&v)?;
                            s.extend_from_slice(&cs);
                        }
                        StringPart::Expr(src) => {
                            let (expr, _) = parser::parse_expr_src(src)
                                .map_err(|e| PhpError::parse(e.message, e.line))?;
                            let v = self.eval(&expr)?;
                            s.extend_from_slice(&self.conv_bytes(&v)?);
                        }
                        StringPart::DollarBraceExpr(src) => {
                            // `${expr}` — deprecated variable-variable
                            // interpolation; its deprecation + inner
                            // diagnostics already emitted at lex time
                            // (heredoc_nowdoc/flexible-heredoc-complex-*).
                            let (expr, _) = parser::parse_expr_src(src)
                                .map_err(|e| PhpError::parse(e.message, e.line))?;
                            let nv = self.eval(&expr)?;
                            let name = self.conv_str(&nv)?;
                            let v = self.var_get(&name)?;
                            s.extend_from_slice(&self.conv_bytes(&v)?);
                        }
                    }
                }
                Ok(Value::bytes(s))
            }
            Expr::Var(name) => self.var_get(name),
            Expr::VarVar(inner) => {
                let n = self.eval(inner)?;
                let name = self.conv_str(&n)?;
                self.var_get(&name)
            }
            Expr::Const(name) => self.const_read(name),
            Expr::MagicConst(m) => Ok(self.magic(*m)),
            Expr::ArrayLit(items) => {
                let mut arr = PhpArray::new();
                for (k, v) in items {
                    // `&$x` elements bind the source cell, not a copy.
                    if let Expr::ByRef(e) = v {
                        let c = self.eval_cell(e)?;
                        self.mark_ref(&c);
                        self.reg_arr_ref(&c);
                        match k {
                            Some(ke) => {
                                let kv = self.eval(ke)?;
                                self.check_offset_key(&kv)?;
                                arr.bind_cell(to_key(&kv), c);
                            }
                            None => {
                                let key = ArrKey::Int(arr.next);
                                arr.bind_cell(key, c);
                            }
                        }
                        continue;
                    }
                    // Zend evaluates the key expression before the value
                    // (namespaces/ns_077_3).
                    match k {
                        Some(ke) => {
                            let kv = self.eval(ke)?;
                            self.check_offset_key(&kv)?;
                            let val = self.eval(v)?;
                            arr.set(to_key(&kv), val);
                        }
                        None => {
                            // `...$it` spread: int keys renumber
                            // positionally, string keys set (PHP 8.1+).
                            if let Expr::Unpack(e) = v {
                                let sv = self.eval(e)?;
                                for (sk, c) in self.unpack_items(&sv)? {
                                    match sk {
                                        Some(s) => arr.set(ArrKey::Str(s), c.borrow().clone()),
                                        None => arr.push(c.borrow().clone()),
                                    }
                                }
                                continue;
                            }
                            let val = self.eval(v)?;
                            arr.push(val);
                        }
                    }
                }
                // zend: HashTable alloc + one bucket per element.
                self.mem_charge(96 + 32 * items.len() as u64);
                Ok(Value::Array(Rc::new(RefCell::new(arr))))
            }
            Expr::ByRef(e) => {
                // `&expr` outside array literals binds the target cell.
                let c = self.eval_cell(e)?;
                let v = c.borrow().clone();
                Ok(v)
            }
            Expr::List(_) => self.fail(PhpError::fatal("Cannot use list() as value", 0)),
            Expr::Assign { target, op, value } => self.assign(target, op, value),
            Expr::Binary { op, l, r } => self.binary(op, l, r),
            Expr::Unary { op, e } => self.unary(op, e),
            Expr::Ternary { c, t, f } => {
                let c = self.eval(c)?;
                if c.is_truthy() {
                    match t {
                        Some(t) => self.eval(t),
                        None => Ok(c),
                    }
                } else {
                    self.eval(f)
                }
            }
            Expr::Call { name, args } => self.call(name, args),
            Expr::Fcc(inner) => self.fcc(inner),
            Expr::Unpack(_) | Expr::FccMark => {
                self.fail(PhpError::fatal("argument unpacking/FCC outside of call", 0))
            }
            Expr::Index { e, i } => self.index_read(e, i.as_deref()),
            Expr::PreInc(t) => self.incdec(t, 1, false),
            Expr::PreDec(t) => self.incdec(t, -1, false),
            Expr::PostInc(t) => self.incdec(t, 1, true),
            Expr::PostDec(t) => self.incdec(t, -1, true),
            Expr::Isset(args) => {
                self.silence += 1;
                let mut ok = true;
                for a in args {
                    match self.isset_val_mode(a, 0) {
                        Ok(Some(_)) => {}
                        Ok(None) => {
                            ok = false;
                            break;
                        }
                        Err(e) => {
                            self.silence -= 1;
                            return Err(e);
                        }
                    }
                }
                self.silence -= 1;
                Ok(Value::Bool(ok))
            }
            Expr::Empty(e) => {
                self.silence += 1;
                let v = self.isset_val_mode(e, 1);
                self.silence -= 1;
                match v {
                    Ok(Some(v)) => Ok(Value::Bool(!v.is_truthy())),
                    Ok(None) => Ok(Value::Bool(true)),
                    Err(e) => Err(e),
                }
            }
            Expr::Print(e) => {
                let v = self.eval(e)?;
                let s = self.conv_str(&v)?;
                self.emit(&s);
                Ok(Value::Int(1))
            }
            Expr::Yield { key, val } => {
                // `function &gen()` yields the value's own cell so
                // `foreach ($gen as &$v)` writes back into it
                // (typed_properties_033/034).
                let by_ref = self.stack.last().map(|f| f.ret_by_ref).unwrap_or(false);
                let vc = match val {
                    Some(e) if by_ref => self.eval_cell(e)?,
                    Some(e) => cell(self.eval(e)?),
                    None => cell(Value::Null),
                };
                let k = match key {
                    Some(e) => self.eval(e)?,
                    None => {
                        if self.gen_sink.is_none() {
                            return self.fail(PhpError::fatal(
                                "The \"yield\" expression can only be used inside a function",
                                self.cur_line,
                            ));
                        }
                        let i = self.gen_auto;
                        self.gen_auto += 1;
                        Value::Int(i)
                    }
                };
                match &self.gen_sink {
                    Some(sink) => {
                        sink.borrow_mut().push((k, vc));
                        let idx = sink.borrow().len() - 1;
                        // A yield inside a `finally` region marks the
                        // force-close fatal — destruction replay
                        // raises 'Cannot yield from finally in a
                        // force-closed generator' here.
                        if self.gen_fin_depth > 0 {
                            if let Some(q) = &self.gen_fin_q {
                                q.borrow_mut().yields.push((idx, self.cur_line));
                            }
                        }
                        // A `Generator->throw()` queued for this yield
                        // raises the throwable as the expression's
                        // result — the body's own try/catch/finally
                        // performs the unwind.
                        if self.gen_throws.front().is_some_and(|(i, _)| *i == idx) {
                            let (_, v) = self.gen_throws.pop_front().unwrap();
                            self.gen_throws_fired.push(idx);
                            return Err(self.throw(v));
                        }
                        // Queued send()s are keyed by yield index —
                        // replayed yields before the suspended one
                        // take NULL, not the incoming send.
                        Ok(if self.gen_sends.front().is_some_and(|(i, _)| *i == idx) {
                            self.gen_sends.pop_front().unwrap().1
                        } else {
                            Value::Null
                        })
                    }
                    None => self.fail(PhpError::fatal(
                        "The \"yield\" expression can only be used inside a function",
                        self.cur_line,
                    )),
                }
            }
            Expr::YieldFrom(e) => {
                let v = self.eval(e)?;
                let sink = self.gen_sink.clone();
                match sink {
                    Some(sink) => {
                        // `yield from` splices the inner keys verbatim —
                        // duplicates and all — and doesn't touch the
                        // keyless auto counter. The materialization
                        // drives inner iterators under the foreach
                        // marking so an inner-gen death keeps its own
                        // trace; items gathered before it still reach
                        // the sink, and the death becomes this body's
                        // own (deferred-raising) death.
                        self.iter_calls += 1;
                        let base = sink.borrow().len();
                        // The inner drain's flushed stream bytes retag
                        // into THIS gen's deferred queue at `base` —
                        // a live emit would echo inner output before
                        // the consumer reached it (yield-from order).
                        let saved_cbase = self.gen_collect_base.replace(base);
                        let saved_seen = std::mem::take(&mut self.gen_collect_seen);
                        let saved_crun = self
                            .gen_collect_run
                            .replace(self.gen_run_state.clone().unwrap());
                        let (items, death) = self.yield_from_collect(&v);
                        self.gen_collect_base = saved_cbase;
                        self.gen_collect_seen = saved_seen;
                        self.gen_collect_run = saved_crun;
                        self.iter_calls -= 1;
                        let inner_len = items.len();
                        // Record the delegation window: consumer
                        // send()/throw() landing inside it routes into
                        // the delegate's own queues (Zend's chain is
                        // live), and the delegate's return value is
                        // the yield-from expression's own value.
                        let inner_ret = if let Value::Object(o) = &v {
                            match &o.borrow().internal {
                                Some(crate::value::ObjectInternal::Generator(ist)) => {
                                    if let Some(run) = &self.gen_run_state {
                                        let mut r = run.borrow_mut();
                                        r.delegate_gens.push((base, inner_len));
                                        // Live delegate journal —
                                        // killing this incarnation
                                        // displaces its delegates.
                                        r.fin_q
                                            .borrow_mut()
                                            .delegate_fins
                                            .push((base, ist.borrow().fin_q.clone()));
                                        // The eager drain above drove
                                        // the delegate's own cursor to
                                        // its production count — the
                                        // consumer-facing position is
                                        // zero until the outer's
                                        // cursor reaches `base`.
                                        ist.borrow().fin_q.borrow_mut().set_vis_tree(0);
                                    }
                                    ist.borrow().return_val.clone()
                                }
                                _ => Value::Null,
                            }
                        } else {
                            Value::Null
                        };
                        sink.borrow_mut().extend(items);
                        // A gen suspended inside `yield from` shares
                        // the OUTER gen's destruction: its destruction
                        // journal (snapshotted at its start, before
                        // the drain pruned it) merges into the
                        // parent's — tags retagged into the parent's
                        // item space and the splice range recorded, so
                        // the replay fires only while the consumer's
                        // cursor is inside the delegate's stream (Zend
                        // force-closes just the actually-suspended
                        // delegation chain — a delegate never reached
                        // replays nothing).
                        if let Some(mut inner_fin) = self.gen_yield_from_fin.take() {
                            if let Some(q) = &self.gen_fin_q {
                                inner_fin.retag(base);
                                q.borrow_mut().delegates.push(crate::value::FinDelegate {
                                    entry: base,
                                    span: inner_len,
                                    fin: inner_fin,
                                });
                            }
                        }
                        match death {
                            Some(e) => Err(e),
                            None => Ok(inner_ret),
                        }
                    }
                    None => self.fail(PhpError::fatal(
                        "The \"yield from\" expression can only be used inside a function",
                        self.cur_line,
                    )),
                }
            }
            Expr::Exit(arg) => {
                let code = if let Some(a) = arg {
                    match self.eval(a)? {
                        Value::Int(i) => i as i32,
                        Value::Str(s) => {
                            self.emit_bytes(&s);
                            0
                        }
                        _ => 0,
                    }
                } else {
                    0
                };
                Err(PhpError {
                    trace: None,
                    thrown_line: None,
                    display_msg: None,
                    kind: ErrorKind::Fatal,
                    message: format!("\u{1}exit:{}", code),
                    line: 0,
                })
            }
            Expr::Include { kind, e } => self.include(*kind, e),
            Expr::Throw(e) => {
                let v = self.eval(e)?;
                match v {
                    Value::Object(_) => Err(self.throw(v)),
                    _ => {
                        // PHP 8: "Can only throw objects"
                        let v = self.exception("Error", "Can only throw objects");
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
                }
            }
            Expr::Match { subject, arms } => {
                let sv = self.eval(subject)?;
                let mut default: Option<&Expr> = None;
                for arm in arms {
                    if arm.conds.is_empty() {
                        default = Some(&arm.result);
                        continue;
                    }
                    for c in &arm.conds {
                        let cv = self.eval(c)?;
                        crate::value::clear_cmp_depth_err();
                        // ZEND_CASE_STRICT (TMP|VAR subjects) is
                        // noncommutative — subject stays left; CONST|CV
                        // subjects emit IS_IDENTICAL which pass_two
                        // commutative-swaps when the arm ranks higher.
                        let r = compare_operand_rank(subject);
                        let (x, y) = if (r & 6) == 0 && r < compare_operand_rank(c) {
                            (&cv, &sv)
                        } else {
                            (&sv, &cv)
                        };
                        let hit = identical(x, y);
                        self.emit_cmp_notices()?;
                        if crate::value::cmp_depth_err() {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                "Nesting level too deep - recursive dependency?",
                                self.cur_line,
                            ));
                        }
                        if hit {
                            return self.eval(&arm.result);
                        }
                    }
                }
                if let Some(d) = default {
                    self.eval(d)
                } else {
                    let e = self.exception(
                        "UnhandledMatchError",
                        &format!("Unhandled match case {}", sv.to_php_string()),
                    );
                    self.pending_exception = Some(e);
                    Err(PhpError {
                        trace: None,
                        thrown_line: None,
                        display_msg: None,
                        kind: ErrorKind::Throw,
                        message: "match".into(),
                        line: 0,
                    })
                }
            }
            Expr::Closure(c) => {
                // PHP 8.5 closures in constant expressions must be
                // static and can't import use() vars; `fn` arrow bodies
                // are never const-expr material (closure_const_expr/*).
                if self.in_const_expr > 0 {
                    if c.arrow {
                        return self.fail(PhpError::compile_fatal(
                            "Constant expression contains invalid operations",
                            c.decl.line,
                        ));
                    }
                    if !c.is_static {
                        return self.fail(PhpError::compile_fatal(
                            "Closures in constant expressions must be static",
                            c.decl.line,
                        ));
                    }
                    if !c.uses.is_empty() {
                        return self.fail(PhpError::compile_fatal(
                            "Cannot use(...) variables in constant expression",
                            c.decl.line,
                        ));
                    }
                }
                // Compile-time param checks for the closure's decl —
                // `{closure:FILE:LINE}():` names it (namespaces/ns_073).
                let cfile = if c.decl.file.is_empty() {
                    self.cur_file.clone()
                } else {
                    c.decl.file.clone()
                };
                // PHP 8.5 names a closure after its enclosing scope:
                // `{closure:Class::m():L}` inside a method,
                // `{closure:fn():L}` inside a function, `{closure:FILE:L}`
                // at top level, and `{closure:{closure:...}:L}` when
                // nested (iterable_003, closure_065). Class-init
                // initializers (prop/const/static-prop defaults) have no
                // enclosing function — file-based name regardless of the
                // runtime caller's frame. Param-default evals also set
                // const_self (for `self::` binds) but DO name the
                // enclosing callee — param_bind_ctx records the ambient
                // class-init level and only counts while a nested
                // initializer hasn't bumped it.
                let enclosing = if self.const_self.is_some()
                    && self.param_bind_ctx != Some(self.class_const_ctx)
                {
                    String::new()
                } else {
                    self.stack
                        .last()
                        .map(|f| {
                            if f.fn_name.is_empty() || f.fn_name == "{main}" {
                                String::new()
                            } else if f.fn_name.starts_with("{closure:") {
                                f.fn_name.clone()
                            } else {
                                match f.trait_origin.clone().or_else(|| {
                                    f.decl_class
                                        .as_ref()
                                        .or(f.scope_class.as_ref())
                                        .map(|c| c.name().to_string())
                                }) {
                                    Some(o) => format!("{}::{}", o, f.fn_name),
                                    None => f.fn_name.clone(),
                                }
                            }
                        })
                        .unwrap_or_default()
                };
                let fname = if enclosing.is_empty() {
                    format!("{{closure:{}:{}}}", cfile, c.decl.line)
                } else if enclosing.starts_with('{') {
                    format!("{{closure:{}:{}}}", enclosing, c.decl.line)
                } else {
                    format!("{{closure:{}():{}}}", enclosing, c.decl.line)
                };
                // The closure's decl.file = the file currently executing
                // — __FILE__/__DIR__ inside it must resolve to where it
                // was defined, not where it is later invoked (autoloaders).
                let mut decl = c.decl.clone();
                if decl.file.is_empty() {
                    decl.file = cfile;
                }
                decl.name = fname.clone();
                // A closure declared lexically inside a trait method
                // keeps the trait as its __TRAIT__ origin; the decl is
                // cloned per instance so the creating context stamps it
                // here (closure_trait_const).
                if decl.decl_in.is_none() {
                    decl.decl_in = self
                        .stack
                        .last()
                        .and_then(|f| f.trait_origin.clone())
                        .or_else(|| {
                            self.const_self
                                .as_ref()
                                .filter(|c| c.decl.kind == crate::ast::ClassKind::Trait)
                                .map(|c| c.name().to_string())
                        });
                }
                if let Err(e) = self.decl_type_checks(&fname, &decl, None) {
                    return Err(self.decl_fatal_ctx(e));
                }
                let mut captures = Vec::new();
                if c.arrow {
                    // `fn` captures whole scope by value — the zval
                    // share keeps the same array until a write, when
                    // cow_split separates it (cyclic self-refs stay
                    // intact; writes can't leak out).
                    let f = self.stack.last().unwrap_or(&self.globals);
                    for (n, cellv) in f.vars.iter() {
                        captures.push((n.clone(), cell(cellv.borrow().clone()), false));
                    }
                } else {
                    for (n, by_ref) in &c.uses {
                        let cap = if *by_ref {
                            // `use (&$x)` promotes the imported var to
                            // an IS_REFERENCE cell (bug52193's `&` in
                            // var_dump of the captures table).
                            let cellv = self.var_cell(n);
                            self.mark_ref(&cellv);
                            cellv
                        } else {
                            match self.var_cell_opt(n) {
                                Some(c) => cell(c.borrow().clone()),
                                // `use ($x)` on an undefined var warns
                                // and captures null; `use (&$x)` binds
                                // silently (closure_027).
                                None => {
                                    self.warn(&format!("Undefined variable ${}", n))?;
                                    cell(Value::Null)
                                }
                            }
                        };
                        captures.push((n.clone(), cap, *by_ref));
                    }
                }
                // `static function` never binds $this; `static::`
                // keeps the creating frame's late-bound class
                // (closure_049-052). Class-init initializers bind the
                // DECLARING class's scope instead (const_self), so a
                // prop-default closure can reach private props
                // (property_initializer_scope_*).
                let is_static = c.is_static;
                // Zend seeds a closure's static-variable table at
                // creation — each instance owns its own copy of the
                // compiled defaults (closure_const_expr/static_variable).
                let mut sv = Vec::new();
                closure_static_vars(&decl.body, &mut sv);
                let mut seed_frame = Frame::new(fname.clone());
                seed_frame.fn_line = decl.line;
                seed_frame.file = decl.file.clone();
                seed_frame.ns = decl.ns.clone();
                seed_frame.trait_origin = decl.decl_in.clone();
                let callable = self.new_callable(PhpCallable {
                    id: std::cell::Cell::new(0),
                    kind: CallableKind::Closure(Rc::new(decl)),
                    captures,
                    this_obj: if is_static {
                        None
                    } else {
                        self.stack.last().and_then(|f| f.this_obj.clone())
                    },
                    scope_class: self
                        .const_self
                        .clone()
                        .or_else(|| self.stack.last().and_then(|f| f.scope_class.clone())),
                    called_class: self
                        .const_self
                        .clone()
                        .or_else(|| self.stack.last().and_then(|f| f.called_class.clone())),
                    is_static,
                });
                if !sv.is_empty() {
                    let key = format!("{}\u{0}c{}", fname, callable.id.get());
                    let mut table = std::collections::HashMap::new();
                    // The seeded defaults compile against the CLOSURE's
                    // own scope: __FUNCTION__/__METHOD__ name it and
                    // __CLASS__ sees its bound scope — evaluating in
                    // the enclosing frame would stamp the caller's
                    // context in permanently.
                    seed_frame.scope_class = callable.scope_class.clone();
                    seed_frame.called_class = callable.called_class.clone();
                    let saved_line = self.cur_line;
                    self.stack.push(seed_frame);
                    for (n, d, sline) in sv {
                        // Only literal-only defaults are bound at
                        // creation — consts, `new`, calls and anything
                        // needing a runtime env stay NULL until the
                        // `static` statement first executes
                        // (closure_const_expr/bug79778).
                        let Some(e) = d else { continue };
                        if !literal_static_init(&e, &self.engine_consts) {
                            continue;
                        }
                        // __LINE__ resolves to the `static` statement's
                        // line inside the body (probe_sv_line).
                        self.cur_line = sline;
                        let Ok(v) = self.eval_const(&e) else {
                            continue;
                        };
                        table.insert(n, cell(v));
                    }
                    self.stack.pop();
                    self.cur_line = saved_line;
                    // Wholesale replace: a recycled handle id could
                    // otherwise expose a dead closure's stale table
                    // to this fresh instance.
                    self.statics.insert(key, table);
                }
                Ok(Value::Callable(callable))
            }
            Expr::New { class, args } => {
                let name = self.class_name_of(class)?;
                let params = self
                    .classes
                    .get(&name.to_lowercase())
                    .cloned()
                    .and_then(|c| self.find_method_in(&c, "__construct"))
                    .map(|m| m.0.decl.params.clone())
                    .unwrap_or_default();
                let argvals =
                    self.arg_cells(args, &params, &format!("{}::__construct()", name), false)?;
                self.new_instance(&name, argvals)
            }
            Expr::Prop {
                obj,
                name,
                nullsafe,
            } => self.prop_read(obj, name, *nullsafe),
            Expr::MethodCall {
                obj,
                name,
                args,
                nullsafe,
            } => self.method_call(obj, name, args, *nullsafe),
            Expr::Paren(e) => self.eval(e),
            Expr::StaticProp { class, name } => self.static_prop_read(class, name),
            Expr::StaticCall { class, name, args } => {
                // `parent::$prop::get()` — parent property hook call.
                if let Expr::StaticProp {
                    class: pc,
                    name: PropName::Name(pn),
                } = class.as_ref()
                {
                    if let Expr::Const(n) = pc.as_ref() {
                        if n.eq_ignore_ascii_case("parent")
                            && (name.eq_ignore_ascii_case("get")
                                || name.eq_ignore_ascii_case("set"))
                        {
                            return self.hook_parent_call(
                                pn,
                                name.eq_ignore_ascii_case("get"),
                                args,
                            );
                        }
                    }
                }
                self.static_call(class, name, args)
            }
            Expr::StaticCallDyn { class, name, args } => {
                // `C::$var(...)`: class resolves first, then the name.
                // Non-string names are a catchable Error
                // (call_static_004).
                let cls = self.class_of(class)?;
                let nv = self.eval(name)?;
                let n = match nv {
                    Value::Str(s) => Self::nul_trunc(&crate::value::lossy(&s)),
                    _ => {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Method name must be a string",
                            0,
                        ))
                    }
                };
                let fwd = matches!(&**class, Expr::Const(_) | Expr::Str(_) | Expr::AnonClass(_));
                let argvals =
                    self.arg_cells(args, &[], &format!("{}::{{closure}}()", cls.name()), false)?;
                self.static_invoke_vis(cls, &n, argvals, None, fwd)
            }
            Expr::ClassConst { class, name } => self.class_const(class, name),
            Expr::Clone(e) => {
                let v = self.eval(e)?;
                match v {
                    Value::Object(o) => {
                        let ob = o.borrow();
                        let mut props = HashMap::new();
                        // References survive clone — the clone's prop
                        // shares the same zval and keeps the typed gate
                        // (typed_properties_081).
                        let mut shared: Vec<(String, Cell)> = Vec::new();
                        for (k, c) in ob.props.iter() {
                            if self.is_ref_cell(c) && Rc::strong_count(c) > 1 {
                                shared.push((k.clone(), c.clone()));
                            }
                            props.insert(k.clone(), cell(c.borrow().clone()));
                        }
                        let new_obj = PhpObject {
                            class: ob.class.clone(),
                            props,
                            prop_order: ob.prop_order.clone(),
                            id: 0,
                            internal: match &ob.internal {
                                Some(ObjectInternal::Exception {
                                    file,
                                    line,
                                    trace,
                                    thrown,
                                    full_msg,
                                    eval_ctx,
                                    frames,
                                }) => Some(ObjectInternal::Exception {
                                    file: file.clone(),
                                    line: *line,
                                    trace: trace.clone(),
                                    thrown: *thrown,
                                    full_msg: full_msg.clone(),
                                    eval_ctx: *eval_ctx,
                                    frames: frames.clone(),
                                }),
                                _ => None,
                            },
                            unset_props: ob.unset_props.clone(),
                        };
                        drop(ob);
                        let nv = Value::Object(self.alloc_obj(new_obj));
                        // Rebind shared cells into the clone and give it
                        // its own slot owner so type checks keep
                        // resolving against the clone's prop.
                        if let Value::Object(no) = &nv {
                            for (k, c) in shared {
                                let ptr = Rc::as_ptr(&c) as usize;
                                no.borrow_mut().props.insert(k.clone(), c);
                                if let Some(a) = self.slot_anchor.get_mut(&ptr) {
                                    if let SlotAnchor::Obj(w, sk) = a {
                                        if sk == &k
                                            && w.upgrade().is_some_and(|u| Rc::ptr_eq(&u, &o))
                                        {
                                            *a = SlotAnchor::Obj(Rc::downgrade(no), k.clone());
                                        }
                                    }
                                }
                                if let Some(owners) = self.slot_owners.get_mut(&ptr) {
                                    for (_, _, _, a) in owners.iter_mut() {
                                        // Repoint owners that anchored
                                        // the SOURCE object's prop to
                                        // the clone's slot.
                                        if let SlotAnchor::Obj(w, sk) = a {
                                            if sk == &k
                                                && w.upgrade().is_some_and(|u| Rc::ptr_eq(&u, &o))
                                            {
                                                *a = SlotAnchor::Obj(Rc::downgrade(no), k.clone());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Value::Object(no) = &nv {
                            let ncls = no.borrow().class.clone();
                            if self.find_method_in(&ncls, "__clone").is_some() {
                                self.method_invoke(no.clone(), "__clone", CallArgs::empty())?;
                            }
                        }
                        Ok(nv)
                    }
                    Value::Callable(c) => {
                        // `clone $closure` — fresh handle id; captured
                        // cells stay shared so by-ref uses still alias
                        // the outer var (closure_024).
                        let nc = self.new_callable((*c).clone());
                        Ok(Value::Callable(nc))
                    }
                    _ => {
                        let e = self.exception("Error", "Cannot clone non-object");
                        self.pending_exception = Some(e);
                        Err(PhpError {
                            trace: None,
                            thrown_line: None,
                            display_msg: None,
                            kind: ErrorKind::Throw,
                            message: "clone".into(),
                            line: 0,
                        })
                    }
                }
            }
            Expr::Cast { kind, e } => {
                let v = self.eval(e)?;
                self.cast(*kind, v)
            }
            Expr::Instanceof { obj, class } => {
                let v = self.eval(obj)?;
                let cname = self.class_name_of(class)?;
                match v {
                    Value::Object(o) => {
                        let cls = o.borrow().class.clone();
                        Ok(Value::Bool(self.is_a(&cls, &cname)))
                    }
                    // A closure literal IS a Closure object.
                    Value::Callable(_) => Ok(Value::Bool(cname.eq_ignore_ascii_case("closure"))),
                    _ => Ok(Value::Bool(false)),
                }
            }
            Expr::AnonClass(decl) => {
                self.register_class(decl.clone())?;
                Ok(Value::str(decl.name.clone()))
            }
        }
    }

    fn magic(&mut self, m: MagicConst) -> Value {
        // __FILE__/__DIR__ resolve against the DECLARING file of the
        // code that runs them — a closure defined in vendor/autoload.php
        // sees that file's dir even when invoked from elsewhere
        // (composer-style PSR-4 autoloaders depend on this).
        let decl_file = self
            .decl_file_ctx
            .clone()
            .or_else(|| {
                self.stack
                    .last()
                    .map(|f| f.file.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| self.cur_file.clone());
        match m {
            MagicConst::Line => Value::Int(self.cur_line as i64),
            MagicConst::File => Value::str(decl_file.clone()),
            MagicConst::Dir => Value::str({
                let d = std::path::Path::new(&decl_file)
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                // A pseudo-path with no dir part ("Command line code",
                // "Standard input code", a top-level eval) resolves to
                // the process cwd, like Zend's `php -r`.
                if d.is_empty() {
                    std::env::current_dir()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                } else {
                    d
                }
            }),
            MagicConst::Function => Value::str(
                self.stack
                    .last()
                    .map(|f| f.fn_name.clone())
                    .unwrap_or_default(),
            ),
            MagicConst::Method => {
                let f = self.stack.last();
                match f {
                    // Hook frame: owner is the declaring class/trait —
                    // `T::$prop::get` for trait-origin hooks (not the
                    // consuming class).
                    Some(f) if f.hook_prop.is_some() => {
                        let (_, _, _, owner) = f.hook_prop.as_ref().unwrap();
                        Value::str(format!("{}::{}", owner, f.fn_name))
                    }
                    // Inside a closure __METHOD__ is the closure's Zend
                    // name (`{closure:C::m():L}` — closure_033).
                    Some(f) if f.fn_name.starts_with("{closure:") => Value::str(f.fn_name.clone()),
                    Some(f) => {
                        // `T::m` when the method was merged from trait T
                        // (`__METHOD__` names the trait; `__CLASS__`
                        // names the consuming class).
                        let owner = f
                            .trait_origin
                            .clone()
                            .or_else(|| f.scope_class.as_ref().map(|c| c.name().to_string()));
                        match owner {
                            Some(o) => Value::str(format!("{}::{}", o, f.fn_name)),
                            None => Value::str(""),
                        }
                    }
                    None => Value::str(""),
                }
            }
            MagicConst::Class => Value::str(
                self.stack
                    .last()
                    .and_then(|f| f.scope_class.as_ref().map(|c| c.name().to_string()))
                    .or_else(|| self.const_self.as_ref().map(|c| c.name().to_string()))
                    .unwrap_or_default(),
            ),
            MagicConst::Namespace => Value::str(self.caller_ns()),
            // `__PROPERTY__` inside a hook names its prop; anywhere else
            // (methods, closures nested in a hook, top level) it is "".
            MagicConst::Property => Value::str(
                self.stack
                    .last()
                    .and_then(|f| f.hook_prop.as_ref().map(|(_, pn, _, _)| pn.clone()))
                    .unwrap_or_default(),
            ),
            // `__TRAIT__` names the trait a method was merged from;
            // "" inside class-defined methods and at top level.
            MagicConst::Trait => Value::str(
                self.stack
                    .last()
                    .and_then(|f| f.trait_origin.clone())
                    .unwrap_or_default(),
            ),
        }
    }

    /// Whether the expr is "set" — for isset()/empty() without warnings.
    /// `isset()` semantics returning the read value — `Some(v)` when the
    /// operand exists and isn't null. `??`/`empty` consume the value
    /// directly so calls and prop-getters evaluate exactly once.
    /// isset/empty/?? property semantics (mode):
    ///  0 = isset()  — last segment's __isset answers directly, no
    ///                 fetch; absent prop without __isset is just not
    ///                 set — __get never fires (bug44899).
    ///  1 = empty()  — __isset gates; truthy result then reads via
    ///                 __get for the falsy check; absent prop without
    ///                 __isset is empty, still no __get.
    ///  2 = ?? / intermediate segment — __isset gates then __get
    ///                 fetches; without __isset the read runs __get
    ///                 directly (bug71359).
    fn isset_val_mode(&mut self, e: &Expr, mode: u8) -> Result<Option<Value>, PhpError> {
        match e {
            Expr::Var(n) => Ok(match self.var_cell_opt(n) {
                Some(c) => match &*c.borrow() {
                    Value::Null => None,
                    v => Some(v.clone()),
                },
                None => None,
            }),
            Expr::Index { e, i } => {
                // `isset($this->uninitTyped['k'])` and `$x ?? y` must not
                // throw on uninitialized typed properties. The base
                // chains through isset semantics — absent segments
                // short-circuit without __get (bug71359).
                self.silence += 1;
                let base = self.isset_val_mode(e, 2);
                self.silence -= 1;
                let base = match base {
                    Ok(Some(b)) => b,
                    Ok(None) => return Ok(None),
                    Err(err) if matches!(err.kind, ErrorKind::Throw) => {
                        if err
                            .message
                            .ends_with("must not be accessed before initialization")
                        {
                            return Ok(None);
                        }
                        return Err(err);
                    }
                    Err(_) => return Ok(None),
                };
                let key = match i {
                    Some(k) => self.eval(k)?,
                    None => return Ok(None),
                };
                // ArrayAccess containers see any key type (offsetExists);
                // string bases take the isset/empty/?? dim matrix —
                // composite keys are silent there, never 'on array' Errors.
                if !matches!(&base, Value::Object(o) if self.obj_is_a(o, "ArrayAccess"))
                    && !matches!(&base, Value::Str(_))
                {
                    self.check_offset_key(&key)?;
                }
                match base {
                    Value::Array(a) => Ok(match a.borrow().get(&to_key(&key)) {
                        Some(v) => match v {
                            Value::Null => None,
                            _ => Some(v.clone()),
                        },
                        None => None,
                    }),
                    Value::Str(s) => self.str_offset_dim(&s, &key, mode),
                    Value::Object(o) => {
                        if self.obj_is_a(&o, "ArrayAccess") {
                            match self.method_invoke(
                                o.clone(),
                                "offsetExists",
                                CallArgs::positional(vec![cell(key.clone())]),
                            ) {
                                // isset() consults offsetExists alone —
                                // offsetGet is only chained by ??/empty
                                // (bug31683).
                                Ok(v) if v.is_truthy() && mode == 0 => Ok(Some(Value::Bool(true))),
                                Ok(v) if v.is_truthy() => {
                                    match self.method_invoke(
                                        o,
                                        "offsetGet",
                                        CallArgs::positional(vec![cell(key)]),
                                    ) {
                                        Ok(v) => Ok(if matches!(v, Value::Null) {
                                            None
                                        } else {
                                            Some(v)
                                        }),
                                        Err(e) => Err(e),
                                    }
                                }
                                Ok(_) => Ok(None),
                                Err(e) => Err(e),
                            }
                        } else {
                            Ok(None)
                        }
                    }
                    _ => Ok(None),
                }
            }
            Expr::Prop {
                obj,
                name,
                nullsafe,
            } => {
                // Missing/inaccessible props consult __isset first
                // (bug63462, bug44899); a re-entrant isset inside
                // __isset hits real storage only. The base evaluates
                // through isset semantics too: each segment of a chain
                // tests via __isset, then fetches via __get only when
                // set — never triggering __get on an absent segment
                // (bug71359).
                let pn = self.prop_name(name)?;
                let ov = match self.isset_val_mode(obj, 2)? {
                    Some(v) => v,
                    None => return Ok(None),
                };
                if let Value::Callable(_) = &ov {
                    // Props on a Closure warn like undeclared members
                    // of the real Closure class (closure_031).
                    self.check_prop_name(&pn)?;
                    self.warn(&format!("Undefined property: Closure::${}", pn))?;
                    return Ok(None);
                }
                if let Value::Object(o) = &ov {
                    let cls = o.borrow().class.clone();
                    // A declared prop checks its real slot unless it
                    // was unset() — only then does __isset fire
                    // (typed_properties_magic_set vs bug63462).
                    let was_unset = {
                        let ob = o.borrow();
                        ob.unset_props.contains(&pn)
                            || ob
                                .unset_props
                                .iter()
                                .any(|k| k.ends_with(&format!("\0{}", pn)))
                    };
                    let declared_live = self.decl_prop(o, &pn).is_some()
                        && !was_unset
                        && self.prop_visible(&cls, &pn);
                    let missing = match self.obj_prop_key(o, &pn) {
                        Some(_) => !self.prop_visible(&cls, &pn),
                        None => true,
                    } && !declared_live;
                    if missing {
                        // ARRAY_AS_PROPS: undeclared props resolve
                        // against storage — a non-null value ⇒ set.
                        if self.aap_active(o) {
                            let arr = self.ao_state(o).0;
                            let v = arr.borrow().get(&ArrKey::Str(Rc::from(pn.as_str())));
                            return match v {
                                Some(v) if !matches!(v, Value::Null) => {
                                    Ok(Some(if mode == 0 { Value::Bool(true) } else { v }))
                                }
                                _ => Ok(None),
                            };
                        }
                        if self.find_method_in(&cls, "__isset").is_some() {
                            let gkey = (Rc::as_ptr(o) as usize, 2u8, pn.clone());
                            if self.magic_guards.insert(gkey.clone()) {
                                let res = self.method_invoke(
                                    o.clone(),
                                    "__isset",
                                    CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                                );
                                self.magic_guards.remove(&gkey);
                                if !res?.is_truthy() {
                                    return Ok(None);
                                }
                                // isset() takes __isset's answer — no
                                // fetch (bug44899). empty()/?? then
                                // read through __get with the SAME
                                // bound name (bug75420).
                                if mode == 0 {
                                    return Ok(Some(Value::Bool(true)));
                                }
                                self.silence += 1;
                                let v = self.prop_read_value(ov.clone(), &pn, false);
                                self.silence -= 1;
                                return match v {
                                    Ok(v) => Ok(if matches!(v, Value::Null) {
                                        None
                                    } else {
                                        Some(v)
                                    }),
                                    Err(e)
                                        if matches!(e.kind, ErrorKind::Throw)
                                            && e.message.ends_with(
                                                "must not be accessed before initialization",
                                            ) =>
                                    {
                                        Ok(None)
                                    }
                                    Err(e) if matches!(e.kind, ErrorKind::Throw) => Err(e),
                                    Err(_) => Ok(None),
                                };
                            } else {
                                return Ok(None);
                            }
                        }
                        // No __isset: isset()/empty() see an absent
                        // prop — __get stays quiet; ?? still reads it
                        // (bug71359).
                        if mode != 2 {
                            return Ok(None);
                        }
                    }
                }
                self.check_prop_name(&pn)?;
                self.silence += 1;
                let v = self.prop_read_value(ov.clone(), &pn, *nullsafe);
                self.silence -= 1;
                match v {
                    Ok(v) => Ok(if matches!(v, Value::Null) {
                        None
                    } else {
                        Some(v)
                    }),
                    // A hooked get runs inside isset — its exceptions
                    // escape (write-only prop throws through the
                    // try/catch, not `false`).
                    Err(e) if matches!(e.kind, ErrorKind::Throw) => {
                        // Uninitialized typed prop reads still mean
                        // "not set" for isset — hook Errors escape.
                        if e.message
                            .ends_with("must not be accessed before initialization")
                        {
                            Ok(None)
                        } else {
                            Err(e)
                        }
                    }
                    Err(_) => Ok(None),
                }
            }
            Expr::StaticProp { class, name } => {
                // Scoped isset — `isset(A::$priv)` is false when the
                // static exists but is invisible to the current scope
                // (closure_041-046, disallows_*).
                let pn = match self.prop_name(name) {
                    Ok(pn) => pn,
                    Err(_) => return Ok(None),
                };
                let cls = match self.class_of(class) {
                    Ok(c) => c,
                    Err(_) => return Ok(None),
                };
                self.statics_init(&cls)?;
                let v = cls.statics.borrow().get(&pn).map(|c| c.borrow().clone());
                let ok = match self.find_static_prop_decl(&cls, &pn) {
                    Some((pd, dcls)) => match pd.visibility {
                        crate::ast::Visibility::Public => true,
                        _ => {
                            let scope = self
                                .stack
                                .last()
                                .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()))
                                .map(|s| s.name().to_string());
                            match (pd.visibility, scope) {
                                (crate::ast::Visibility::Private, Some(s)) => s == dcls.name(),
                                (crate::ast::Visibility::Protected, Some(s)) => {
                                    self.is_a_str(&s, dcls.name()) || self.is_a_str(dcls.name(), &s)
                                }
                                _ => false,
                            }
                        }
                    },
                    None => true,
                };
                match (v, ok) {
                    (Some(v), true) if !matches!(v, Value::Null) => Ok(Some(v)),
                    _ => Ok(None),
                }
            }
            _ => {
                let v = self.eval(e)?;
                Ok(if matches!(v, Value::Null) {
                    None
                } else {
                    Some(v)
                })
            }
        }
    }

    fn prop_read_loose(&mut self, e: &Expr) -> Result<Value, PhpError> {
        match e {
            Expr::Prop {
                obj,
                name,
                nullsafe,
            } => self.prop_read(obj, name, *nullsafe),
            _ => self.eval(e),
        }
    }

    /// Byte-faithful string coercion — strings pass through untouched;
    /// other scalars go through conv_str (their output is ASCII anyway).
    pub(in crate::interp) fn conv_bytes(&mut self, v: &Value) -> Result<Vec<u8>, PhpError> {
        if let Value::Str(s) = v {
            return Ok(s.to_vec());
        }
        Ok(self.conv_str(v)?.into_bytes())
    }

    /// Object→string with __toString, plus array warning.
    pub(in crate::interp) fn conv_str(&mut self, v: &Value) -> Result<String, PhpError> {
        match v {
            Value::Array(_) => {
                self.warn("Array to string conversion")?;
                Ok("Array".into())
            }
            Value::Object(o) => {
                let class = o.borrow().class.clone();
                // __toString may be inherited — walk the chain, not just
                // the leaf decl (AbstractString defines it for
                // UnicodeString).
                if self.find_method_in(&class, "__tostring").is_some() {
                    let r = self.method_invoke(o.clone(), "__tostring", CallArgs::empty())?;
                    Ok(r.to_php_string())
                } else {
                    let cname = class.name().to_string();
                    let e = self.exception(
                        "Error",
                        &format!("Object of class {} could not be converted to string", cname),
                    );
                    self.pending_exception = Some(e);
                    Err(PhpError {
                        trace: None,
                        thrown_line: None,
                        display_msg: None,
                        kind: ErrorKind::Throw,
                        message: "cast".into(),
                        line: 0,
                    })
                }
            }
            Value::Callable(_) => {
                let e = self.exception(
                    "Error",
                    "Object of class Closure could not be converted to string",
                );
                self.pending_exception = Some(e);
                Err(PhpError {
                    trace: None,
                    thrown_line: None,
                    display_msg: None,
                    kind: ErrorKind::Throw,
                    message: "cast".into(),
                    line: 0,
                })
            }
            Value::Float(f) => {
                let prec = self.ini_int("precision", 14);
                Ok(crate::value::format_float_prec(*f, prec))
            }
            _ => Ok(v.to_php_string()),
        }
    }

    fn const_read(&mut self, name: &str) -> Result<Value, PhpError> {
        match name {
            "self" | "static" | "parent" => {
                // Resolved as class names only in :: context; bare self is an error.
                return self.fail(PhpError::fatal(
                    format!("Cannot access \"{}\" when no class scope is active", name),
                    0,
                ));
            }
            _ => {}
        }
        let key = name.trim_start_matches('\\');
        // Error names the ns-qualified candidate for an unqualified
        // const inside a namespace (namespaces/ns_041).
        let mut miss_name = name.trim_start_matches('\\').to_string();
        if !name.contains('\\') {
            // Unqualified constant inside a namespace: `ns\NAME` first,
            // then the global constant (Zend/tests/namespaces).
            let ns = self.caller_ns();
            if !ns.is_empty() {
                miss_name = format!("{}\\{}", ns, key);
                if let Some(v) = self.constants.get(&miss_name) {
                    return Ok(v.clone());
                }
            }
        }
        if let Some(v) = self.constants.get(key).cloned() {
            // PHP 8.4+ keeps E_STRICT defined but deprecated on use.
            if key.eq_ignore_ascii_case("E_STRICT") {
                self.deprecated(
                    "Constant E_STRICT is deprecated since 8.4, the error level was removed",
                )?;
            }
            return Ok(v);
        }
        if let Some(v) = self.constants.get(name) {
            return Ok(v.clone());
        }
        let v = self.exception("Error", &format!("Undefined constant \"{}\"", miss_name));
        self.pending_exception = Some(v);
        Err(PhpError {
            trace: None,
            thrown_line: None,
            display_msg: None,
            kind: ErrorKind::Throw,
            message: "const".into(),
            line: 0,
        })
    }

    fn assign(&mut self, target: &Expr, op: &'static str, value: &Expr) -> Result<Value, PhpError> {
        // `$this` may never be an assignment target (compile fatal,
        // bug24573); plain and compound assigns both route here.
        if let Expr::Var(n) = target {
            if n == "this" {
                return self.fail(PhpError::fatal("Cannot re-assign $this", 0));
            }
        }
        if op == "=&" {
            // zend refuses the $GLOBALS table itself as a by-ref source
            // (compile error `Cannot acquire reference to $GLOBALS`) —
            // element access $GLOBALS['x'] is fine.
            if let Expr::Var(n) = value {
                if n == "GLOBALS" {
                    // Engine-side fatal — zend prints the
                    // `Stack trace:\n#0 {main}` block too.
                    return self.fail(PhpError::compile_fatal(
                        "Cannot acquire reference to $GLOBALS",
                        self.cur_line,
                    ));
                }
            }
            // By-reference assignment: bind cells.
            let src = match value {
                Expr::Call { .. } | Expr::MethodCall { .. } | Expr::StaticCall { .. } => {
                    let (c, was_ref) = self.eval_call_cell(value)?;
                    if !was_ref {
                        self.notice("Only variables should be assigned by reference")?;
                    }
                    c
                }
                _ => {
                    // By-ref dim fetches flag dim_by_ref so a string
                    // leaf errors 'Cannot create references to/from
                    // string offsets' instead of 'as an array'.
                    let was = std::mem::replace(&mut self.dim_by_ref, true);
                    let c = self.eval_cell(value);
                    self.dim_by_ref = was;
                    let c = c?;
                    // A `=&` source that is itself a typed-prop slot
                    // carries that prop's declared type into the
                    // conflict check (typed_properties_068/076).
                    let decl = match value {
                        Expr::Prop { obj, name, .. } => {
                            let ov = self.eval(obj)?;
                            match (&ov, self.prop_name(name)) {
                                (Value::Object(o), Ok(pn)) => {
                                    self.decl_prop(o, &pn).map(|(pd, dc)| {
                                        let sk =
                                            self.obj_prop_key(o, &pn).unwrap_or_else(|| pn.clone());
                                        (
                                            pd.ty.clone(),
                                            dc.name().to_string(),
                                            pn,
                                            SlotAnchor::Obj(Rc::downgrade(o), sk),
                                        )
                                    })
                                }
                                _ => None,
                            }
                        }
                        Expr::StaticProp { class, name } => {
                            match (self.member_class_of(class), self.prop_name(name)) {
                                (Ok((cls, _)), Ok(pn)) => {
                                    self.find_static_prop_decl(&cls, &pn).map(|(pd, dc)| {
                                        (
                                            pd.ty.clone(),
                                            dc.name().to_string(),
                                            pn.clone(),
                                            SlotAnchor::Statics(dc.name().to_string(), pn),
                                        )
                                    })
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some((Some(tys), dcn, dpn, anc)) = decl {
                        let p = Rc::as_ptr(&c) as usize;
                        self.typed_slots.insert(p, (c.clone(), tys, dcn, dpn));
                        self.slot_anchor.insert(p, anc);
                    }
                    c
                }
            };
            let was = std::mem::replace(&mut self.dim_by_ref, true);
            let br = self.bind_cell(target, src.clone());
            self.dim_by_ref = was;
            br?;
            return Ok(src.borrow().clone());
        }
        let needs_read = op != "=";
        // PHP evaluates the LHS lvalue chain (index exprs' side effects)
        // BEFORE the RHS: `$a[f()][g()] = rhs` calls f,g first
        // (engine_assignExecutionOrder_003). Dynamic names inside an object
        // property access ($o->{e}, $o->p[e]) are evaluated early for side
        // effects but Zend's temp register is then overwritten by the
        // assignment value — so the ACTUAL name/key becomes the RHS value
        // (engine_assignExecutionOrder_001).
        enum Late {
            Prop {
                ov: Value,
                name: Option<PropName>,
            },
            PropStr {
                ov: Value,
                pn: String,
            },
            Index {
                base: Cell,
                key: Option<Value>,
                append: bool,
            },
            Static {
                class: Box<Expr>,
                pn: String,
            },
            Keyed {
                e: Box<Expr>,
                keys: Vec<Option<Value>>,
            },
            None,
        }
        fn has_prop(e: &Expr) -> bool {
            match e {
                Expr::Prop { .. } => true,
                Expr::Index { e, .. } => has_prop(e),
                _ => false,
            }
        }
        let mut late = Late::None;
        let target_cell = match target {
            Expr::Prop { obj, name, .. } => {
                match self.eval(obj) {
                    Ok(ov) => {
                        // A {dynamic} name expr resolves now (side effects +
                        // the var-var temp is read early, matching Zend);
                        // a plain $var name reads late at write time.
                        if matches!(name, PropName::Expr(_)) {
                            let pn = self.prop_name(name)?;
                            late = Late::PropStr { ov, pn };
                        } else {
                            late = Late::Prop {
                                ov,
                                name: Some(name.clone()),
                            };
                        }
                        None
                    }
                    Err(_) => None,
                }
            }
            Expr::Index { e, i } if has_prop(e) => {
                // Prop-chain index: the container resolves early; the dim
                // expr's own value is always the key — `$o->a[${f()}]` is a
                // variable-variable, not a register quirk
                // (engine_assignExecutionOrder_001 reads $name that way).
                match self.eval_cell(e) {
                    Ok(c) => {
                        let key = match i.as_deref() {
                            Some(ie) => self.eval(ie).ok(),
                            None => None,
                        };
                        late = Late::Index {
                            base: c,
                            key,
                            append: i.is_none(),
                        };
                        None
                    }
                    Err(_) => None,
                }
            }
            Expr::Index { .. } => {
                // Zend evaluates dim exprs BEFORE the RHS (innermost first)
                // but traverses the container only at write time — so the
                // write lands on the variable's CURRENT value
                // (engine_assignExecutionOrder_003 mod() case).
                let mut dims = Vec::new();
                let mut base = target;
                while let Expr::Index { e, i } = base {
                    dims.push(i.as_deref());
                    base = e;
                }
                dims.reverse();
                let mut keys = Vec::with_capacity(dims.len());
                for d in dims {
                    match d {
                        Some(ie) => match self.eval(ie) {
                            Ok(k) => keys.push(Some(k)),
                            Err(_) => keys.push(None),
                        },
                        None => keys.push(None),
                    }
                }
                late = Late::Keyed {
                    e: Box::new(base.clone()),
                    keys,
                };
                None
            }
            Expr::StaticProp { class, name } => {
                // Static prop names evaluate BEFORE the RHS with their own
                // result (no register quirk: engine_assignExecutionOrder_001).
                match self.prop_name(name) {
                    Ok(pn) => {
                        late = Late::Static {
                            class: class.clone(),
                            pn,
                        };
                        None
                    }
                    Err(_) => None,
                }
            }
            Expr::List(_) => None,
            _ => self.eval_cell(target).ok(),
        };
        let rhs = self.eval(value)?;
        let cur = if needs_read {
            match &target_cell {
                Some(c) => c.borrow().clone(),
                None => {
                    // `??=` reads with isset() semantics (no undefined
                    // warnings); every other compound op warns
                    // (typed_properties_103). The read itself is the
                    // IS-mode fetch — an uninitialized string offset
                    // means "not set" (assign), not the "" a BP_VAR_R
                    // read produces.
                    let quiet = op == "??=";
                    if quiet {
                        self.silence += 1;
                    }
                    let c = if op == "??=" {
                        self.isset_val_mode(target, 2)
                            .map(|v| v.unwrap_or(Value::Null))
                    } else {
                        self.eval(target)
                    };
                    if quiet {
                        self.silence -= 1;
                    }
                    c.unwrap_or(Value::Null)
                }
            }
        } else {
            Value::Null
        };
        // zend ASSIGN_DIM_OP walks the dim chain for write before the
        // op combines operands — a string leaf refuses first (the rhs
        // still evaluated). `??=` is exempt: it assigns through the
        // plain path, which splices string offsets fine.
        if op != "=" && op != "??=" {
            match &late {
                Late::Keyed { e, keys } => self.str_offset_op_gate(e, keys)?,
                Late::Index { base, .. } if matches!(*base.borrow(), Value::Str(_)) => {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        "Cannot use assign-op operators with string offsets",
                        self.cur_line,
                    ));
                }
                _ => {}
            }
        }
        let newv = match op {
            "=" => rhs,
            "+=" => self.arith("+", cur, rhs)?,
            "-=" => self.arith("-", cur, rhs)?,
            "*=" => self.arith("*", cur, rhs)?,
            "/=" => self.arith("/", cur, rhs)?,
            "%=" => self.arith("%", cur, rhs)?,
            ".=" => {
                let mut l = self.conv_bytes(&cur)?;
                let mut r = self.conv_bytes(&rhs)?;
                l.append(&mut r);
                Value::bytes(l)
            }
            "??=" => {
                if matches!(cur, Value::Null) {
                    rhs
                } else {
                    return Ok(cur);
                }
            }
            "&=" | "|=" | "^=" | "<<=" | ">>=" | "**=" => {
                self.arith(&op[..op.len() - 1], cur, rhs)?
            }
            _ => {
                return self.fail(PhpError::fatal(
                    format!("unsupported assignment op {}", op),
                    0,
                ))
            }
        };
        // `$t = $GLOBALS` gets a private table: zend materializes
        // $GLOBALS reads into a zval that CoWs away on write, so the
        // copy must not keep the bound global cells or `$t['k']`
        // writes would reach the live globals.
        let mut newv = match newv {
            Value::Array(rc)
                if self
                    .globals_arr
                    .as_ref()
                    .is_some_and(|g| Rc::ptr_eq(&rc, g)) =>
            {
                let a = rc.borrow();
                Value::Array(Rc::new(RefCell::new(PhpArray {
                    entries: a
                        .entries
                        .iter()
                        .map(|(k, c)| (k.clone(), cell(c.borrow().clone())))
                        .collect(),
                    next: a.next,
                    is_ref: false,
                    iter_pos: a.iter_pos,
                })))
            }
            v => v,
        };
        match late {
            Late::Prop { ov, name } => {
                let pn = match name {
                    Some(n) => self.prop_name(&n)?,
                    None => self.conv_str(&newv)?,
                };
                newv = self.store_prop(ov, &pn, newv.clone())?;
            }
            Late::PropStr { ov, pn } => {
                newv = self.store_prop(ov, &pn, newv.clone())?;
            }
            Late::Index { base, key, append } => {
                // A clobbered (call-result) dim falls back to the RHS
                // value; a real `[]` always appends (bug21961).
                let aa_obj = {
                    let b = base.borrow();
                    match &*b {
                        Value::Object(o) if self.obj_is_a(o, "ArrayAccess") => Some(o.clone()),
                        _ => None,
                    }
                };
                if let Some(o) = aa_obj {
                    // `$o[k] = v` on ArrayAccess -> offsetSet.
                    let kv = key.clone().unwrap_or_else(|| newv.clone());
                    match self.method_invoke(
                        o,
                        "offsetSet",
                        CallArgs::positional(vec![cell(kv), cell(newv.clone())]),
                    ) {
                        Ok(_) => {}
                        Err(e) => return Err(e),
                    }
                    return Ok(newv);
                }
                if matches!(&*base.borrow(), Value::Null) {
                    self.auto_init_gate(&base)?;
                    let mut b = base.borrow_mut();
                    if matches!(*b, Value::Null) {
                        *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
                    }
                }
                {
                    let mut b = base.borrow_mut();
                    if let Value::Array(_) = &mut *b {
                        // Shared zend_array: CoW-separate before the
                        // write — an overloaded prop's fetched temp
                        // must not write through into the getter's
                        // backing store (bug32660).
                        self.cow_split(&mut b);
                        let rc = match &*b {
                            Value::Array(rc) => rc.clone(),
                            _ => unreachable!(),
                        };
                        let mut arr = rc.borrow_mut();
                        if append {
                            arr.push(newv.clone());
                        } else {
                            let key = key.clone().map(|k| to_key(&k)).unwrap_or(to_key(&newv));
                            arr.set(key, newv.clone());
                        }
                    }
                }
                if matches!(&*base.borrow(), Value::Str(_)) {
                    // $o->p[k] = v on a string leaf:
                    // zend_check_string_offset key validation then the
                    // byte splice (never array-ified).
                    let mut bytes = match &*base.borrow() {
                        Value::Str(s) => s.to_vec(),
                        _ => Vec::new(),
                    };
                    let off = self.str_offset_key(key.as_ref())?;
                    // `??=` writes like `=` once the offset shows
                    // unset — only real compound ops are refused.
                    if op != "=" && op != "??=" {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Cannot use assign-op operators with string offsets",
                            self.cur_line,
                        ));
                    }
                    if let OffWrite::Stored(byte) = self.str_offset_write(&mut bytes, off, &newv)? {
                        let mut b = base.borrow_mut();
                        if let Value::Str(s) = &mut *b {
                            *s = bytes.into();
                        }
                        return Ok(Value::bytes(vec![byte]));
                    }
                }
            }
            Late::Static { class, pn } => {
                let c = self.static_prop_named(&class, &pn)?;
                // Static prop writes coerce to the declared type like
                // instance props (typed_properties_023).
                if let Ok((cls, _)) = self.member_class_of(&class) {
                    if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &pn) {
                        newv = self.prop_typed_write_check(&pd, &dcls, newv)?;
                    }
                }
                let nv = self.typed_slot_store(&c, newv.clone())?;
                self.cell_store(&c, nv)?;
            }
            Late::Keyed { e, keys } => {
                newv = self.assign_index_path(&e, &keys, newv, op != "=" && op != "??=")?;
            }
            Late::None => match target_cell {
                Some(c) => {
                    let nv = self.typed_slot_store(&c, newv.clone())?;
                    self.cell_store(&c, nv)?;
                }
                None => self.store(target, newv.clone())?,
            },
        }
        Ok(newv)
    }

    /// Evaluate to a cell (for by-ref semantics): vars and array elements
    /// and object props alias their storage.
    pub(in crate::interp) fn eval_cell(&mut self, e: &Expr) -> Result<Cell, PhpError> {
        match e {
            Expr::Var(n) => Ok(self.var_cell(n)),
            Expr::Index { e, i } => self.index_cell(e, i.as_deref()),
            Expr::Prop {
                obj,
                name,
                nullsafe,
            } => self.prop_cell(obj, name, *nullsafe),
            Expr::VarVar(inner) => {
                let n = self.eval(inner)?;
                let name = self.conv_str(&n)?;
                Ok(self.var_cell(&name))
            }
            Expr::StaticProp { class, name } => self.static_prop_cell(class, name),
            Expr::Call { .. } | Expr::MethodCall { .. } | Expr::StaticCall { .. } => {
                Ok(self.eval_call_cell(e)?.0)
            }
            _ => {
                // Function calls returning by-ref, etc.: evaluate to temp cell.
                let v = self.eval(e)?;
                Ok(cell(v))
            }
        }
    }

    /// Evaluate a call expression, keeping the callee's returned cell when the
    /// function was declared `&name()` (returns by reference).
    pub(in crate::interp) fn eval_call_cell(&mut self, e: &Expr) -> Result<(Cell, bool), PhpError> {
        let (c, was_ref) = self.eval_call_cell_inner(e)?;
        if was_ref {
            self.mark_ref(&c);
        }
        Ok((c, was_ref))
    }

    fn eval_call_cell_inner(&mut self, e: &Expr) -> Result<(Cell, bool), PhpError> {
        self.last_ret_cell = None;
        self.last_call_by_ref = false;
        let v = self.eval(e)?;
        let declared = self.last_call_by_ref;
        match self.last_ret_cell.take() {
            Some(c) => Ok((c, true)),
            // A function declared `&` that returns a non-variable binds a temp
            // cell — the caller does not warn (the callee warned at `return`).
            None => Ok((cell(v), declared)),
        }
    }

    /// `$target =& $cell`
    /// A `=&` bind displaced this slot's previous cell: Zend decrefs
    /// it only after the new binding is visible, so its __destruct
    /// reads and writes the shared cell (gh10168). A displaced cell
    /// that still has live owners (another prop/static alias) keeps
    /// its zval — Zend frees the zval only with the cell itself, so
    /// `Test::$test =& $box->value` + `$box->value =& $tmp` leaves the
    /// old shared cell alive under the static (assign_prop_ref_with_
    /// prop_ref).
    fn destruct_displaced(&mut self, old: Option<Cell>) -> Result<(), PhpError> {
        if let Some(old) = old {
            // Release dead bookkeeping refs first, then require the
            // cell be truly orphaned — otherwise its zval stays alive.
            self.prune_typed_slot(Rc::as_ptr(&old) as usize);

            if Rc::strong_count(&old) != 1 {
                return Ok(());
            }
            let v = old.borrow().clone();
            drop(old);
            self.destruct_dying_value(&v)?;
        }
        Ok(())
    }

    fn bind_cell(&mut self, target: &Expr, src: Cell) -> Result<(), PhpError> {
        // `=&` creates Zend's IS_REFERENCE — writes through it say
        // "a reference held by property", not "property" (034/078).
        self.mark_ref(&src);
        match target {
            Expr::Var(n) => {
                let old = self.cur().vars.insert(n.clone(), src);
                self.destruct_displaced(old)
            }
            Expr::Index { e, i } => {
                let key = match i {
                    Some(ie) => Some(self.eval(ie)?),
                    None => None,
                };
                // zend binds into any indexable lvalue base —
                // `$this->jobs[$id] =& $job` (ProcessExecutor).
                match &**e {
                    Expr::Var(..)
                    | Expr::Index { .. }
                    | Expr::Prop { .. }
                    | Expr::VarVar(..)
                    | Expr::StaticProp { .. } => {
                        self.reg_arr_ref(&src);
                        let c = self.eval_cell(e)?;
                        self.bind_into_key(c, key, src)
                    }
                    _ => self.fail(PhpError::fatal("Cannot create reference to expression", 0)),
                }
            }
            Expr::Prop { obj, name, .. } => {
                // `=&` on a hooked prop without `&get` — the engine
                // reports the overloaded-object error, not the
                // indirect-modification one (get_by_ref_auto).
                if let Ok(Value::Object(o)) = self.eval(obj) {
                    if let Ok(pn) = self.prop_name(name) {
                        if let Some((_pd, hs)) = self.hooked_prop(&o, &pn) {
                            let has_ref_get = hs
                                .iter()
                                .any(|(h, _)| h.is_get && h.by_ref && h.body.is_some());
                            if !has_ref_get {
                                let v = self.exception(
                                    "Error",
                                    "Cannot assign by reference to overloaded object",
                                );
                                let e = self.throw(v);
                                return self.fail(e);
                            }
                        }
                    }
                }
                let ov = self.eval(obj)?;
                if let Value::Object(o) = &ov {
                    if let Ok(pn) = self.prop_name(name) {
                        // `=&` into an overloaded prop (missing slot +
                        // __get) still fetches through __get — then the
                        // indirect-modification notice and the
                        // cannot-assign-by-reference Error (bug32660).
                        {
                            let was_unset = {
                                let ob = o.borrow();
                                ob.unset_props.contains(&pn)
                                    || ob
                                        .unset_props
                                        .iter()
                                        .any(|k| k.ends_with(&format!("\0{}", pn)))
                            };
                            let cls = o.borrow().class.clone();
                            let declared_live = self.decl_prop(o, &pn).is_some()
                                && !was_unset
                                && self.prop_visible(&cls, &pn);
                            let inaccessible = match self.obj_prop_key(o, &pn) {
                                Some(_) => !self.prop_visible(&cls, &pn),
                                None => true,
                            };
                            if inaccessible
                                && !declared_live
                                && self.find_method_in(&cls, "__get").is_some()
                            {
                                let gkey = (Rc::as_ptr(o) as usize, 0u8, pn.clone());
                                if self.magic_guards.insert(gkey.clone()) {
                                    let res = self.method_invoke(
                                        o.clone(),
                                        "__get",
                                        CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                                    );
                                    self.magic_guards.remove(&gkey);
                                    res?;
                                }
                                self.notice(&format!(
                                    "Indirect modification of overloaded property {}::${} has no effect",
                                    cls.name(),
                                    pn
                                ))?;
                                let v = self.exception(
                                    "Error",
                                    "Cannot assign by reference to overloaded object",
                                );
                                let e = self.throw(v);
                                return self.fail(e);
                            }
                        }
                        // `=&` installs the source cell as the prop's
                        // slot itself — later writes through either name
                        // hit the same storage; a missing dynamic prop
                        // materializes a real slot (oss-fuzz-382922236).
                        // The array itself stays unmarked — zend's ref
                        // is a property of the zval, not the array; a
                        // later value-copy still CoW-separates
                        // (bug39775).
                        // Binding a ref into a TYPED prop validates the
                        // source (076/068 conflict); the shared cell
                        // then stays gated through typed_slots (071).
                        let mut merged: Option<Vec<String>> = None;
                        let mut owner_ty: Option<Vec<String>> = None;
                        if let Some((pd, dcls)) = self.decl_prop(o, &pn) {
                            if let Some(pt) = &pd.ty {
                                owner_ty = Some(pt.clone());
                                merged = Some(self.bind_typed_check(&pd, &dcls, &src)?);
                            }
                        }
                        let key = self.obj_prop_key(o, &pn).unwrap_or_else(|| pn.clone());
                        let mut ob = o.borrow_mut();
                        if !ob.prop_order.contains(&key) {
                            ob.prop_order.push(key.clone());
                        }

                        let old = ob.props.insert(key.clone(), src.clone());
                        drop(ob);
                        self.destruct_displaced(old)?;
                        if let (Some(m), Some((_, dcls))) = (merged, self.decl_prop(o, &pn)) {
                            let sptr = Rc::as_ptr(&src) as usize;
                            // The FIRST owner is the ref's holder for
                            // "held by property X of type T" messages —
                            // later merges only narrow slot_merged.
                            self.typed_slots.entry(sptr).or_insert_with(|| {
                                (src.clone(), m.clone(), dcls.name().to_string(), pn.clone())
                            });
                            self.slot_anchor
                                .entry(sptr)
                                .or_insert_with(|| SlotAnchor::Obj(Rc::downgrade(o), key.clone()));
                            self.slot_merged.insert(sptr, m);
                            // Owners keep their *declared* type — a
                            // shared write must satisfy each and yield
                            // one consistent result.
                            let ot = owner_ty.unwrap_or_default();
                            let dcn = dcls.name().to_string();
                            let owners = self.slot_owners.entry(sptr).or_default();
                            if !owners
                                .iter()
                                .any(|(_, cn, pn2, _)| cn == &dcn && pn2 == &pn)
                            {
                                owners.push((
                                    ot,
                                    dcn,
                                    pn.clone(),
                                    SlotAnchor::Obj(Rc::downgrade(o), key.clone()),
                                ));
                            }
                        }
                        return Ok(());
                    }
                }
                let c = self.eval_cell(target)?;
                *c.borrow_mut() = src.borrow().clone();
                Ok(())
            }
            Expr::StaticProp { class, name } => {
                let pn = self.prop_name(name)?;
                let (cls, _t) = self.member_class_of(class)?;
                self.statics_init(&cls)?;
                let mut merged: Option<Vec<String>> = None;
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &pn) {
                    if pd.ty.is_some() {
                        // Binding a ref to a typed static validates the
                        // source (catchable TypeError — 068/069); the
                        // cell then stays gated via typed_slots.
                        merged = Some(self.bind_typed_check(&pd, &dcls, &src)?);
                    }
                }
                let old = cls.statics.borrow_mut().insert(pn.clone(), src.clone());

                self.destruct_displaced(old)?;

                if let (Some(m), Some((pd, dcls))) = (merged, self.find_static_prop_decl(&cls, &pn))
                {
                    let sptr = Rc::as_ptr(&src) as usize;
                    self.typed_slots.entry(sptr).or_insert_with(|| {
                        (src.clone(), m.clone(), dcls.name().to_string(), pn.clone())
                    });
                    self.slot_anchor.entry(sptr).or_insert_with(|| {
                        SlotAnchor::Statics(dcls.name().to_string(), pn.clone())
                    });
                    self.slot_merged.insert(sptr, m);
                    let ot = pd.ty.clone().unwrap_or_default();
                    let dcn = dcls.name().to_string();
                    let owners = self.slot_owners.entry(sptr).or_default();
                    if !owners
                        .iter()
                        .any(|(_, cn, pn2, _)| cn == &dcn && pn2 == &pn)
                    {
                        owners.push((
                            ot,
                            dcn,
                            pn.clone(),
                            SlotAnchor::Statics(dcls.name().to_string(), pn.clone()),
                        ));
                    }
                }
                Ok(())
            }
            Expr::VarVar(..) => {
                let c = self.eval_cell(target)?;
                *c.borrow_mut() = src.borrow().clone();
                Ok(())
            }
            _ => self.fail(PhpError::fatal("Cannot create reference to expression", 0)),
        }
    }

    pub(in crate::interp) fn store(&mut self, target: &Expr, v: Value) -> Result<(), PhpError> {
        match target {
            Expr::Var(name) => {
                // `$this = x` is a compile-time fatal in Zend (bug24573).
                if name == "this" {
                    return self.fail(PhpError::fatal("Cannot re-assign $this", 0));
                }
                self.var_set(name, v)?;
                Ok(())
            }
            Expr::VarVar(inner) => {
                let n = self.eval(inner)?;
                let name = self.conv_str(&n)?;
                self.var_set(&name, v)?;
                Ok(())
            }
            Expr::Index { e, i } => self.set_index(e, i.as_deref(), v),
            Expr::StaticProp { class, name } => {
                let c = self.static_prop_cell(class, name)?;
                let (cls, _) = self.member_class_of(class)?;
                let pname = self.prop_name(name)?;
                // The stored cell may be a `=&`-bound cell shared with a
                // DIFFERENT typed prop — typed_slots gates that write
                // (typed_properties_107).
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &pname) {
                    let v2 = self.prop_typed_write_check(&pd, &dcls, v)?;
                    let nv = self.typed_slot_store(&c, v2)?;
                    self.cell_store(&c, nv)?;
                } else {
                    let nv = self.typed_slot_store(&c, v)?;
                    self.cell_store(&c, nv)?;
                }
                Ok(())
            }
            Expr::List(items) => {
                // PHP reads each [i] positionally — a missing key warns
                // "Undefined array key i" (engine_assignExecutionOrder_002).
                let mut vals: Vec<Value> = Vec::with_capacity(items.len());
                for (i, slot) in items.iter().enumerate() {
                    let vi = match &v {
                        Value::Array(a) => match a.borrow().get(&ArrKey::Int(i as i64)) {
                            Some(v) => v,
                            None => {
                                if slot.is_some() {
                                    self.warn(&format!("Undefined array key {}", i))?;
                                }
                                Value::Null
                            }
                        },
                        other => {
                            if items[i].is_some() {
                                // list() on a non-array warns "Cannot use T
                                // as array" (engine_assignExecutionOrder_002).
                                self.warn(&format!("Cannot use {} as array", other.debug_type()))?;
                            }
                            Value::Null
                        }
                    };
                    vals.push(vi);
                }
                for (i, t) in items.iter().enumerate() {
                    if let Some(t) = t {
                        self.store(t, vals[i].clone())?;
                    }
                }
                Ok(())
            }
            Expr::Prop {
                obj,
                name,
                nullsafe: _,
            } => {
                let pn = self.prop_name(name)?;
                let ov = self.eval(obj)?;
                self.store_prop(ov, &pn, v).map(|_| ())
            }
            _ => self.fail(PhpError::fatal("Cannot assign to this expression", 0)),
        }
    }

    /// Write `$ov->$pn = v` — private-slot, `__set` or dynamic-prop rules.
    /// Write `$ov->$pn = v`; returns the STORED value (typed props
    /// coerce — the assign expr yields the coerced result, 077).
    /// `$cell[] = v` / `$cell[k] = v` on a null typed slot: Zend
    /// auto-initializes an array only when `array` fits the merged
    /// member type; otherwise `Cannot auto-initialize an array inside
    /// ...` TypeError and a just-materialized slot reverts to
    /// uninitialized (typed_properties_083).
    fn auto_init_gate(&mut self, c: &Cell) -> Result<(), PhpError> {
        let ptr = Rc::as_ptr(c) as usize;
        self.prune_typed_slot(ptr);
        let gated = self
            .slot_merged
            .get(&ptr)
            .cloned()
            .or_else(|| self.typed_slots.get(&ptr).map(|(_, t, _, _)| t.clone()));
        let Some(tys) = gated else {
            return Ok(());
        };
        if tys.iter().any(|m| self.ty_member_is_a("array", m)) {
            *c.borrow_mut() = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
            return Ok(());
        }
        let (cn, pn) = self
            .typed_slots
            .get(&ptr)
            .map(|(_, _, n, p)| (n.clone(), p.clone()))
            .unwrap_or_default();
        let where_ = if self.is_ref_ptr(ptr) {
            "a reference held by property"
        } else {
            "property"
        };
        let msg = format!(
            "Cannot auto-initialize an array inside {} {}::${} of type {}",
            where_,
            cn,
            pn,
            ty_disp(&tys)
        );
        // A slot materialized for this write goes back to
        // uninitialized — reads must still raise the uninit Error.
        if self.last_fresh_cell == Some(ptr) {
            if let Some(anc) = self.slot_anchor.get(&ptr).cloned() {
                match anc {
                    SlotAnchor::Obj(w, key) => {
                        if let Some(o) = w.upgrade() {
                            o.borrow_mut().props.remove(&key);
                        }
                    }
                    SlotAnchor::Statics(ccn, ppn) => {
                        if let Some(cc) = self.classes.get(&ccn.to_lowercase()) {
                            cc.statics.borrow_mut().remove(&ppn);
                        }
                    }
                    SlotAnchor::None => {}
                }
            }
            self.typed_slots.remove(&ptr);
            self.slot_anchor.remove(&ptr);
            self.slot_owners.remove(&ptr);
            self.slot_merged.remove(&ptr);
            self.last_fresh_cell = None;
        }
        let mut e = PhpError::uncaught("TypeError", msg, 0);
        e.thrown_line = Some(self.cur_line);
        self.fail(e)
    }

    /// Dynamic property names starting with `\0` hit zend's
    /// private-name-mangle check — a catchable Error, not magic
    /// (bug52484).
    pub(in crate::interp) fn check_prop_name(&mut self, pn: &str) -> Result<(), PhpError> {
        if pn.starts_with('\0') {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot access property starting with \"\\0\"",
                0,
            ));
        }
        Ok(())
    }

    pub(in crate::interp) fn store_prop(
        &mut self,
        ov: Value,
        pn: &str,
        mut v: Value,
    ) -> Result<Value, PhpError> {
        match ov {
            Value::Object(o) => {
                if !self.in_own_hook(&o, pn) {
                    if let Some((pd, hs)) = self.hooked_prop(&o, pn) {
                        {
                            self.hook_write(&o, &pd, &hs, v.clone())?;
                            return Ok(v);
                        }
                    }
                    if let Some((pd, dcls)) = self.decl_prop(&o, pn) {
                        if let Some(sv) = pd.set_vis {
                            if !self.hook_scope_allows(&o, &dcls, pn, sv) {
                                return self.set_visibility_error(&dcls, &pd.name, sv);
                            }
                        }
                    }
                }
                if let Some((pd, dcls)) = self.decl_prop(&o, pn) {
                    if pd.readonly {
                        // readonly implies protected(set): one-time init
                        // from the declaring scope only; later writes
                        // always fail (readonly_property tests).
                        let key = self.obj_prop_key(&o, pn).unwrap_or_else(|| pn.to_string());
                        if o.borrow().props.contains_key(&key) {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Cannot modify readonly property {}::${}",
                                    dcls.name(),
                                    pd.name
                                ),
                                0,
                            ));
                        }
                        let scope = self.caller_scope_name();
                        if scope.as_deref() != Some(dcls.name()) {
                            let from = scope
                                .map(|s| format!("scope {}", s))
                                .unwrap_or_else(|| "global scope".to_string());
                            return self.fail(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Cannot modify protected(set) readonly property {}::${} from {}",
                                    dcls.name(),
                                    pd.name,
                                    from
                                ),
                                0,
                            ));
                        }
                    }
                    v = self.prop_typed_write_check(&pd, &dcls, v)?;
                }
                // Declared-prop slot resolution: private decls write
                // their mangled `\0C\0p` slot even on first write (the
                // promoted-ctor write reaches here); undeclared names
                // fall through to __set/dynamic.
                let cls = o.borrow().class.clone();
                let k = self.obj_prop_key(&o, pn).or_else(|| {
                    self.decl_prop(&o, pn).map(|(pd, dcls)| {
                        if pd.visibility == crate::ast::Visibility::Private {
                            format!("\0{}\0{}", dcls.name(), pd.name)
                        } else {
                            pd.name.clone()
                        }
                    })
                });
                // A slot the caller can't see is __set territory —
                // zend never writes it directly from an outside scope
                // (overloaded_prop_assign_op_refs).
                let k = match k {
                    Some(k) if self.prop_visible(&cls, pn) => Some(k),
                    _ => None,
                };
                // ARRAY_AS_PROPS: undeclared prop writes go into the
                // storage hash — spl write_property bypasses __set and
                // the prop table for names std doesn't know.
                if k.is_none()
                    && self.aap_active(&o)
                    && self.decl_prop(&o, pn).is_none()
                    && !o.borrow().props.contains_key(pn)
                {
                    // AAP prop writes route through spl_array_write_
                    // dimension in zend — the mid-sort guard applies.
                    if let Some(e) = self.ao_sorting_err(&o) {
                        return self.fail(e);
                    }
                    let arr = self.ao_state(&o).0;
                    let k = ArrKey::Str(Rc::from(pn));
                    // Object-backed: storage IS the prop table — the
                    // write lands a dynamic prop on the backing object.
                    if let Some(src) = self.ao_src_obj(&o) {
                        self.ao_obj_dim_write(&o, &src, &arr, k, v.clone());
                    } else {
                        arr.borrow_mut().set(k, v.clone());
                    }
                    return Ok(v);
                }
                if let Some(k) = k {
                    let mut ob = o.borrow_mut();
                    // Write into the existing slot — a `&`-bound
                    // reference must see the update (typed_properties_010).
                    if let Some(existing) = ob.props.get(&k) {
                        let existing = existing.clone();
                        drop(ob);
                        // The slot may be shared with a DIFFERENT typed
                        // prop via `=&` — that prop's type still gates
                        // the write (typed_properties_062).
                        let nv = self.typed_slot_store(&existing, v)?;
                        self.cell_store(&existing, nv.clone())?;
                        v = nv;
                    } else {
                        // A declared prop that was unset() is
                        // inaccessible — writes go through __set
                        // (typed_properties_magic_set). The in-set
                        // guard writes re-entrant assignments to real
                        // storage instead of recursing (bug63462).
                        let gkey = (Rc::as_ptr(&o) as usize, 1u8, pn.to_string());
                        if ob.unset_props.contains(&k)
                            && self.find_method_in(&cls, "__set").is_some()
                            && self.magic_guards.insert(gkey.clone())
                        {
                            drop(ob);
                            let res = self.method_invoke(
                                o.clone(),
                                "__set",
                                CallArgs::positional(vec![
                                    cell(Value::str(pn.to_string())),
                                    cell(v.clone()),
                                ]),
                            );
                            self.magic_guards.remove(&gkey);
                            res?;
                            return Ok(v);
                        }
                        if !ob.prop_order.contains(&k) {
                            ob.prop_order.push(k.clone());
                        }
                        ob.props.insert(k, cell(v.clone()));
                    }
                    Ok(v)
                } else if self.find_method_in(&cls, "__set").is_some()
                    && self
                        .magic_guards
                        .insert((Rc::as_ptr(&o) as usize, 1u8, pn.to_string()))
                {
                    let res = self.method_invoke(
                        o.clone(),
                        "__set",
                        CallArgs::positional(vec![
                            cell(Value::str(pn.to_string())),
                            cell(v.clone()),
                        ]),
                    );
                    self.magic_guards
                        .remove(&(Rc::as_ptr(&o) as usize, 1u8, pn.to_string()));
                    res?;
                    Ok(v)
                } else {
                    // Writing a DECLARED prop the scope can't see is
                    // `Cannot access private/protected property` — the
                    // declared name can't be shadowed by a dynamic prop
                    // (bug38461's re-entrant __set write; direct writes
                    // without __set too).
                    if let Some(e) = self.hidden_decl_error(&o, pn) {
                        return self.fail(e);
                    }
                    // `\0` names error on the real-storage path before
                    // any deprecation (bug52484_2).
                    self.check_prop_name(pn)?;
                    // E_DEPRECATED on first write to an undeclared prop
                    // (PHP 8.2+; stdClass is exempt).
                    let is_new = {
                        let ob = o.borrow();
                        !ob.props.contains_key(pn)
                    };
                    // stdClass (and its subclasses) plus
                    // #[AllowDynamicProperties] opt out of the
                    // deprecation (property_hooks/foreach_002).
                    let exempt = self.obj_is_a(&o, "stdclass")
                        || cls.decl.attrs.iter().any(|a| {
                            a.name
                                .rsplit('\\')
                                .next()
                                .unwrap_or(&a.name)
                                .eq_ignore_ascii_case("AllowDynamicProperties")
                        });
                    if is_new && !exempt {
                        self.deprecated(&format!(
                            "Creation of dynamic property {}::${} is deprecated",
                            cls.name(),
                            pn
                        ))?;
                    }
                    let mut ob = o.borrow_mut();
                    let pn = pn.to_string();
                    if !ob.prop_order.contains(&pn) {
                        ob.prop_order.push(pn.clone());
                    }
                    ob.props.insert(pn, cell(v.clone()));
                    Ok(v)
                }
            }
            Value::Callable(_) => {
                // Closures have no prop storage — writes are a
                // catchable Error, not a dynamic-prop create
                // (closure_022, closure_write_prop).
                let e = PhpError::uncaught(
                    "Error",
                    format!("Cannot create dynamic property Closure::${}", pn),
                    0,
                );
                self.fail(e)
            }
            _ => {
                self.warn(&format!(
                    "Attempt to assign property \"{}\" on {}",
                    pn,
                    ov.gettype()
                ))?;
                Ok(Value::Null)
            }
        }
    }

    /// `$arr[$k] = v` / `$arr[] = v`.
    fn set_index(&mut self, e: &Expr, i: Option<&Expr>, v: Value) -> Result<(), PhpError> {
        let key = match i {
            Some(ie) => Some(self.eval(ie)?),
            None => None,
        };
        self.set_index_val(e, key, v)
    }

    /// `$e[k1][k2]... = v` with already-evaluated keys, traversed at write
    /// time (Zend ASSIGN_DIM semantics): intermediate scalar levels produce
    /// "Cannot use T as array" warnings; a scalar base for a single-level
    /// write throws "Cannot use a scalar value as an array".
    /// Returns the effective stored value: string-offset writes return the
    /// byte actually stored, everything else echoes `v` (bug22592: chained
    /// `$a[i] = $a[j] = $s` only warns for the first write).
    fn assign_index_path(
        &mut self,
        e: &Expr,
        keys: &[Option<Value>],
        v: Value,
        op_assign: bool,
    ) -> Result<Value, PhpError> {
        self.last_fresh_cell = None;
        let mut c = self.eval_cell(e)?;
        let last = keys.len() - 1;
        for (n, k) in keys.iter().enumerate() {
            // Auto-init gate: writing through a typed slot that is
            // null (or a just-materialized uninit slot) must produce
            // `Cannot auto-initialize an array inside property ...`
            // unless its type accepts an array (typed_properties_083).
            if matches!(&*c.borrow(), Value::Null) {
                self.auto_init_gate(&c)?;
            }
            // ArrayAccess object offset path: `$o[k]` dispatches to
            // offsetSet/offsetGet instead of writing through a cell.
            let as_obj = {
                let b = c.borrow();
                match &*b {
                    Value::Object(o) if self.obj_is_a(o, "ArrayAccess") => Some(o.clone()),
                    _ => None,
                }
            };
            // A string dim goes through zend_check_string_offset, not
            // index_into_key: the key validates first (TypeError / []
            // / cast-warning parity), then a non-final dim is the
            // 'Cannot use string offset as an array' Error, else the
            // byte splices in. Compound assigns are refused outright.
            if matches!(*c.borrow(), Value::Str(_)) {
                let off = self.str_offset_key(k.as_ref())?;
                if n != last {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        "Cannot use string offset as an array",
                        self.cur_line,
                    ));
                }
                if op_assign {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        "Cannot use assign-op operators with string offsets",
                        self.cur_line,
                    ));
                }
                let mut bytes = {
                    let b = c.borrow();
                    match &*b {
                        Value::Str(s) => s.to_vec(),
                        _ => Vec::new(),
                    }
                };
                match self.str_offset_write(&mut bytes, off, &v)? {
                    OffWrite::Skipped => return Ok(v),
                    OffWrite::Stored(byte) => {
                        let mut b = c.borrow_mut();
                        if let Value::Str(s) = &mut *b {
                            *s = bytes.into();
                        }
                        return Ok(Value::bytes(vec![byte]));
                    }
                }
            }
            // Illegal offset types must not fall into the string-offset
            // fallback — the key Error propagates
            // (closure_array_offset_error). ArrayAccess containers see
            // any key type — offsetSet receives it raw.
            if let Some(kv) = k {
                if as_obj.is_none() {
                    self.check_offset_key(kv)?;
                }
            }
            if let Some(o) = as_obj {
                if n == last {
                    match self.method_invoke(
                        o,
                        "offsetSet",
                        CallArgs::positional(vec![
                            cell(k.clone().unwrap_or(Value::Null)),
                            cell(v.clone()),
                        ]),
                    ) {
                        Ok(_) => return Ok(v),
                        Err(e) => return Err(e),
                    }
                }
                // Internal spl storage (ArrayObject & friends) hands back
                // the real bucket cell — writes through it reach the
                // object, like zend's by-ref spl read_dimension.
                if matches!(o.borrow().internal, Some(ObjectInternal::ArrayIter { .. })) {
                    let arr = self.ao_arr(&o);
                    let k2 = to_key(&k.clone().unwrap_or(Value::Null));
                    let mut a = arr.borrow_mut();
                    c = match a.get_cell(&k2) {
                        Some(cc) => cc,
                        None => {
                            let cc = cell(Value::Null);
                            a.bind_cell(k2, cc.clone());
                            cc
                        }
                    };
                    continue;
                }
                // Userland ArrayAccess — the same write-context
                // read_dimension zend runs: a `&offsetGet` hands the
                // real storage cell back (writes through it reach the
                // object); a value return yields a throwaway cell and
                // notices only when it isn't an object.
                c = self.index_cell_object(&c, k.clone())?;
                continue;
            }
            match self.index_into_key(c.clone(), k.clone()) {
                Ok(nc) => {
                    if n == last {
                        // `$ref[k] = v` where the element cell is bound to
                        // a typed prop stays type-gated (064).
                        let nv = self.typed_slot_store(&nc, v.clone())?;
                        *nc.borrow_mut() = nv;
                        return Ok(v);
                    }
                    c = nc;
                }
                Err(e2) => {
                    // Scalar/object intermediate: the engine Error from
                    // index_into_key is the real zend diagnostic — keep
                    // it. (String dims were handled above.)
                    return Err(e2);
                }
            }
        }
        Ok(v)
    }

    /// `$a[$k]` keys: object/closure keys are a catchable Error
    /// naming the class (closure_array_key_error/offset_error).
    fn check_offset_key(&mut self, v: &Value) -> Result<(), PhpError> {
        let cn = match v {
            Value::Object(o) => Some(o.borrow().class.name().to_string()),
            Value::Callable(_) => Some("Closure".to_string()),
            _ => None,
        };
        if let Some(cn) = cn {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot access offset of type {} on array", cn),
                0,
            ));
        }
        Ok(())
    }

    /// zend_check_string_offset — validates a key used to offset into a
    /// string: int passes through; a numeric string (leading ws/sign
    /// ok) uses its value silently, a leading-int-with-junk string
    /// warns 'Illegal string offset' and uses the int part; float,
    /// bool and null cast with a 'String offset cast occurred'
    /// warning; other types (incl. non-numeric strings) are a
    /// TypeError naming the type *on string*. `None` (the `[]` dim)
    /// is the '[] operator not supported for strings' Error.
    fn str_offset_key(&mut self, k: Option<&Value>) -> Result<i64, PhpError> {
        match k {
            None => self.fail(PhpError::uncaught(
                "Error",
                "[] operator not supported for strings",
                self.cur_line,
            )),
            Some(Value::Int(i)) => Ok(*i),
            Some(Value::Str(s)) => match numeric(s) {
                Numeric::Int(i) => Ok(i),
                Numeric::Leading(f, true) => {
                    let orig = crate::value::lossy(s).into_owned();
                    self.warn(&format!("Illegal string offset \"{}\"", orig))?;
                    Ok(f as i64)
                }
                _ => self.fail(PhpError::uncaught(
                    "TypeError",
                    "Cannot access offset of type string on string",
                    self.cur_line,
                )),
            },
            Some(v @ (Value::Float(_) | Value::Bool(_) | Value::Null)) => {
                self.warn("String offset cast occurred")?;
                Ok(v.to_int())
            }
            Some(v) => {
                let ty = match v {
                    Value::Array(_) => "array".to_string(),
                    Value::Object(o) => o.borrow().class.name().to_string(),
                    Value::Callable(_) => "Closure".to_string(),
                    _ => "resource".to_string(),
                };
                self.fail(PhpError::uncaught(
                    "TypeError",
                    format!("Cannot access offset of type {} on string", ty),
                    self.cur_line,
                ))
            }
        }
    }

    /// isset/empty/?? on a string offset. zend_isset_dim_slow and
    /// zend_isempty_dim_slow accept a LONG dim, a scalar (floats go
    /// through zval_get_long_ex — the lossy-precision deprecation still
    /// fires) or a fully int-numeric string; anything else is silently
    /// not-set/empty — composite keys included. The ?? value read
    /// (FETCH_DIM_IS) instead warns 'Illegal string offset' on
    /// leading-junk numerics, silently casts scalars, yields NULL for
    /// non-numeric strings, and still raises 'on string' TypeErrors for
    /// composite keys (zend_illegal_string_offset runs with BP_VAR_R).
    /// Negative offsets wrap from the end in both paths.
    fn str_offset_dim(
        &mut self,
        s: &Rc<[u8]>,
        key: &Value,
        mode: u8,
    ) -> Result<Option<Value>, PhpError> {
        let len = s.len() as i64;
        let off = if mode == 2 {
            match key {
                Value::Int(i) => *i,
                Value::Str(str) => match numeric(str) {
                    Numeric::Int(i) => i,
                    Numeric::Leading(f, true) => {
                        let orig = crate::value::lossy(str).into_owned();
                        self.warn(&format!("Illegal string offset \"{}\"", orig))?;
                        f as i64
                    }
                    // Non-numeric string keys are NULL, not an error.
                    _ => return Ok(None),
                },
                // Scalars cast silently — no 'String offset cast
                // occurred' warning and no lossy-float deprecation.
                Value::Float(_) | Value::Bool(_) | Value::Null => key.to_int(),
                _ => {
                    let ty = match key {
                        Value::Array(_) => "array".to_string(),
                        Value::Object(o) => o.borrow().class.name().to_string(),
                        Value::Callable(_) => "Closure".to_string(),
                        _ => "resource".to_string(),
                    };
                    return self.fail(PhpError::uncaught(
                        "TypeError",
                        format!("Cannot access offset of type {} on string", ty),
                        self.cur_line,
                    ));
                }
            }
        } else {
            match key {
                Value::Int(i) => *i,
                Value::Float(f) => {
                    // zval_get_long_ex(is_strict): fractional floats warn.
                    if f.fract() != 0.0 {
                        self.deprecated(&format!(
                            "Implicit conversion from float {} to int loses precision",
                            format_float_repr(*f)
                        ))?;
                    }
                    *f as i64
                }
                Value::Bool(_) | Value::Null => key.to_int(),
                Value::Str(str) => match numeric(str) {
                    Numeric::Int(i) => i,
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            }
        };
        // Negative offsets wrap from the end (isset($s[-1]) → last
        // byte); still-out-of-range is not-set in every mode.
        let idx = if off < 0 {
            off.checked_add(len).unwrap_or(i64::MIN)
        } else {
            off
        };
        if idx >= 0 && idx < len {
            Ok(Some(Value::bytes(vec![s[idx as usize]])))
        } else {
            Ok(None)
        }
    }

    /// zend ASSIGN_DIM_OP fetches the dim chain for write before the
    /// operation combines operands — a string leaf errors 'Cannot use
    /// string offset as an array' mid-path or 'Cannot use assign-op
    /// operators with string offsets' at the final dim. The gate runs
    /// before `newv` arith; the leaf's key already validated during the
    /// current-value read. The walk is READ-ONLY: probing an append or
    /// missing key must not materialize cells — a fresh slot is never
    /// a string, and a bound NULL bucket would push the real write
    /// into the next slot (`$a[] .= 'x'` left a hole).
    fn str_offset_op_gate(&mut self, e: &Expr, keys: &[Option<Value>]) -> Result<(), PhpError> {
        let mut c = self.eval_cell(e)?;
        let last = keys.len() - 1;
        for (n, k) in keys.iter().enumerate() {
            let rc = match &*c.borrow() {
                Value::Str(_) => {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        if n != last {
                            "Cannot use string offset as an array"
                        } else {
                            "Cannot use assign-op operators with string offsets"
                        },
                        self.cur_line,
                    ));
                }
                Value::Array(rc) => rc.clone(),
                _ => return Ok(()),
            };
            let Some(k) = k else { return Ok(()) };
            let found = rc.borrow().get_cell(&to_key(k));
            match found {
                Some(nc) => c = nc,
                None => return Ok(()),
            }
        }
        Ok(())
    }

    /// The zend string-offset byte splice on an already-validated
    /// offset: below -len warns 'Illegal string offset' and writes
    /// nothing; the value converts byte-faithfully — empty is the
    /// 'Cannot assign an empty string to a string offset' Error and a
    /// multi-byte value warns 'Only the first byte...' then writes
    /// byte 0. Past-the-end offsets space-pad.
    fn str_offset_write(
        &mut self,
        s: &mut Vec<u8>,
        off: i64,
        v: &Value,
    ) -> Result<OffWrite, PhpError> {
        let len = s.len() as i64;
        // The bounds check runs before the value checks — $s[-9] = ''
        // warns Illegal only, no empty-string Error (oracle-verified).
        if off < -len {
            self.warn(&format!("Illegal string offset {}", off))?;
            return Ok(OffWrite::Skipped);
        }
        let vs = self.conv_bytes(v)?;
        if vs.is_empty() {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot assign an empty string to a string offset",
                self.cur_line,
            ));
        }
        if vs.len() > 1 {
            self.warn("Only the first byte will be assigned to the string offset")?;
        }
        let idx = if off < 0 { off + len } else { off } as usize;
        if idx >= s.len() {
            s.resize(idx + 1, b' ');
        }
        s[idx] = vs[0];
        Ok(OffWrite::Stored(vs[0]))
    }

    /// `set_index` with an already-evaluated key.
    fn set_index_val(&mut self, e: &Expr, key: Option<Value>, v: Value) -> Result<(), PhpError> {
        if let Some(k) = &key {
            // ArrayAccess containers take any key type — zend hands it
            // to offsetSet untouched (SplObjectStorage keys on objects).
            let obj_container = match e {
                Expr::Var(n) => matches!(
                    self.var_cell_opt(n).map(|c| c.borrow().clone()),
                    Some(Value::Object(o)) if self.obj_is_a(&o, "ArrayAccess")
                ),
                _ => false,
            };
            // A string container validates keys via
            // zend_check_string_offset ('on string' errors), and nested
            // containers through index_into_key's arm — the generic
            // check would misname the error 'on array'.
            let self_validating = match e {
                Expr::Var(n) => matches!(
                    self.var_cell_opt(n).map(|c| c.borrow().clone()),
                    Some(Value::Str(_))
                ),
                _ => true,
            };
            if !obj_container && !self_validating {
                self.check_offset_key(k)?;
            }
        }
        match e {
            Expr::Var(name) => {
                let arr_cell = self.var_cell(name);
                let mut b = arr_cell.borrow_mut();
                match &mut *b {
                    Value::Null => {
                        let mut arr = PhpArray::new();
                        match key {
                            Some(k) => arr.set(to_key(&k), v),
                            None => arr.push(v),
                        }
                        *b = Value::Array(Rc::new(RefCell::new(arr)));
                        // zend: new HashTable + first bucket.
                        self.mem_charge(128);
                    }
                    Value::Array(_) => {
                        // CoW: shared arrays get replaced wholesale by callers
                        // through the cell, so mutate in place via borrow_mut —
                        // PHP semantics: write through to all aliases... PHP
                        // separates unreferenced copies; our Rc aliases share.
                        // For `$a = $b; $a[0]=1` PHP copies. Handle via split.
                        self.cow_split(&mut b);
                        if let Value::Array(rc) = &mut *b {
                            let mut arr = rc.borrow_mut();
                            match key {
                                Some(k) => arr.set(to_key(&k), v),
                                None => arr.push(v),
                            }
                        }
                        // zend: bucket + amortized arData growth.
                        self.mem_charge(32);
                    }
                    Value::Str(_) => {
                        let mut bytes = match &*b {
                            Value::Str(s) => s.to_vec(),
                            _ => Vec::new(),
                        };
                        drop(b);
                        let off = self.str_offset_key(key.as_ref())?;
                        if let OffWrite::Stored(_) = self.str_offset_write(&mut bytes, off, &v)? {
                            *arr_cell.borrow_mut() =
                                Value::str(String::from_utf8_lossy(&bytes).into_owned());
                        }
                    }
                    _ => {
                        drop(b);
                        // Catchable Error: `$int[0] = x` (engine_..._002).
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Cannot use a scalar value as an array",
                            self.cur_line,
                        ));
                    }
                }
                Ok(())
            }
            // Nested lvalue bases ($a[0][1], $a->b[0], C::$a[0], $$v[0]):
            // resolve the element cell generically and write into it.
            Expr::Index { .. } | Expr::Prop { .. } | Expr::StaticProp { .. } | Expr::VarVar(..) => {
                match self.index_cell_key(e, key.clone()) {
                    Ok(c) => {
                        *c.borrow_mut() = v;
                        Ok(())
                    }
                    // String offsets can't be cells — splice the byte in place.
                    Err(e2) => match self.eval_cell(e) {
                        Ok(bc) if matches!(*bc.borrow(), Value::Str(_)) => {
                            // e's own dims may have consumed a string
                            // offset mid-path ($a['k'][0]['j']): the
                            // intermediate 'Cannot use string offset as
                            // an array' is the zend diagnostic — key
                            // never re-validates.
                            let inner_str = match e {
                                Expr::Index { e: inner, .. } => self
                                    .eval_cell(inner)
                                    .map(|ic| matches!(*ic.borrow(), Value::Str(_)))
                                    .unwrap_or(false),
                                _ => false,
                            };
                            if inner_str {
                                return Err(e2);
                            }
                            let mut bytes = match &*bc.borrow() {
                                Value::Str(s) => s.to_vec(),
                                _ => Vec::new(),
                            };
                            let off = self.str_offset_key(key.as_ref())?;
                            if let OffWrite::Stored(_) =
                                self.str_offset_write(&mut bytes, off, &v)?
                            {
                                let mut b = bc.borrow_mut();
                                if let Value::Str(s) = &mut *b {
                                    *s = bytes.into();
                                }
                            }
                            Ok(())
                        }
                        // ArrayAccess object: `$o[k] = v` -> offsetSet.
                        Ok(bc)
                            if matches!(*bc.borrow(), Value::Object(ref o)
                                if self.obj_is_a(o, "ArrayAccess")) =>
                        {
                            let o = match &*bc.borrow() {
                                Value::Object(o) => o.clone(),
                                _ => unreachable!(),
                            };
                            let kv = key.clone().unwrap_or(Value::Null);
                            match self.method_invoke(
                                o,
                                "offsetSet",
                                CallArgs::positional(vec![cell(kv), cell(v)]),
                            ) {
                                Ok(_) => Ok(()),
                                Err(e) => Err(e),
                            }
                        }
                        // Nested dim on a non-container: zend throws the
                        // real engine Error ("Cannot use a scalar value
                        // as an array" et al.) — keep it, don't downgrade.
                        Ok(_) => Err(e2),
                        _ => Err(e2),
                    },
                }
            }
            _ => self.fail(PhpError::fatal("Cannot use expression as array", 0)),
        }
    }

    /// Writable array handle for by-ref builtin args: PHP COW-separates
    /// a shared array at the callee boundary, so a builtin mutating
    /// `&$array` replaces the caller's slot with a private copy while
    /// other variables keep the old contents. is_ref arrays (true `=&`
    /// bindings) write through to every alias instead.
    pub fn arr_mut(&self, c: &Cell) -> Option<Rc<RefCell<PhpArray>>> {
        let mut b = c.borrow_mut();
        if let Value::Array(rc) = &mut *b {
            if Rc::strong_count(rc) > 1 && !rc.borrow().is_ref {
                let fresh = self.dup_array(&rc.borrow());
                *b = Value::Array(Rc::new(RefCell::new(fresh)));
            }
            if let Value::Array(rc) = &*b {
                return Some(rc.clone());
            }
        }
        None
    }

    /// PHP copy-on-write separation: a shared zend_array is replaced
    /// by a fresh table on write. IS_REFERENCE elements (cells bound
    /// by `=&`/by-ref constructs, tracked in `ref_cells`) stay shared
    /// with the source — everything else copies by value.
    pub(in crate::interp) fn cow_split(&self, v: &mut Value) {
        if let Value::Array(rc) = v {
            // Deliberately-shared tables ($GLOBALS, &-bound storage,
            // arrays under a live by-ref foreach) never separate.
            if Rc::strong_count(rc) > 1 && !rc.borrow().is_ref {
                let fresh = self.dup_array(&rc.borrow());
                *v = Value::Array(Rc::new(RefCell::new(fresh)));
            }
        }
    }

    /// The separated copy for CoW / array-copy contexts (`=`, exchange
    /// values): ref-marked cells are re-bound, the rest duplicated.
    pub fn dup_array(&self, a: &PhpArray) -> PhpArray {
        let mut copy = PhpArray {
            entries: Vec::with_capacity(a.entries.len()),
            next: a.next,
            is_ref: a.is_ref,
            iter_pos: a.iter_pos,
        };
        for (k, c) in &a.entries {
            // zend unwraps a refcount-1 IS_REFERENCE bucket on copy;
            // only cells still aliased elsewhere re-bind (a stale
            // mark from a dead foreach/binding copies by value).
            let nc = if self.is_ref_cell(c) && Rc::strong_count(c) > 1 {
                c.clone()
            } else {
                cell(c.borrow().clone())
            };
            copy.entries.push((k.clone(), nc));
        }
        copy
    }

    /// Index into `c`'s array value, taking a cell for `key`/`[]`.
    fn index_into_key(&mut self, c: Cell, key: Option<Value>) -> Result<Cell, PhpError> {
        if let Some(k) = &key {
            // Objects route to index_cell_object (ArrayAccess accepts
            // any key); string containers validate keys via their own
            // arm ('on string' errors); only true arrays reject object
            // keys here.
            if !matches!(&*c.borrow(), Value::Object(_) | Value::Str(_)) {
                self.check_offset_key(k)?;
            }
        }
        let mut b = c.borrow_mut();
        if matches!(*b, Value::Null) {
            *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
        }
        if let Value::Array(_) = &mut *b {
            // Deliberately-shared arrays ($GLOBALS, &-bound storage)
            // keep their bound cells through the split (ref_cells);
            // an ordinary shared zend_array still separates on write
            // (bug32660).
            self.cow_split(&mut b);
            let rc = match &*b {
                Value::Array(rc) => rc.clone(),
                _ => unreachable!(),
            };
            drop(b);
            let key = match key {
                Some(k) => to_key(&k),
                None => {
                    let mut arr = rc.borrow_mut();
                    let k = ArrKey::Int(arr.next);
                    let c = cell(Value::Null);
                    arr.bind_cell(k.clone(), c.clone());
                    return Ok(c);
                }
            };
            let mut arr = rc.borrow_mut();
            match arr.get_cell(&key) {
                Some(c) => Ok(c),
                None => {
                    let c = cell(Value::Null);
                    arr.bind_cell(key, c.clone());
                    Ok(c)
                }
            }
        } else if matches!(*b, Value::Object(_)) {
            drop(b);
            self.index_cell_object(&c, key)
        } else {
            let is_str = matches!(*b, Value::Str(_));
            drop(b);
            if is_str {
                // zend_check_string_offset validates the key first —
                // bad keys throw 'on string' TypeErrors or the []
                // Error; a valid offset on a non-final dim errors —
                // 'as an array' on write, 'Cannot create references'
                // in a =& context (FETCH_DIM_REF).
                self.str_offset_key(key.as_ref())?;
                self.fail(PhpError::uncaught(
                    "Error",
                    if self.dim_by_ref {
                        "Cannot create references to/from string offsets"
                    } else {
                        "Cannot use string offset as an array"
                    },
                    self.cur_line,
                ))
            } else {
                self.fail(PhpError::uncaught(
                    "Error",
                    "Cannot use a scalar value as an array",
                    self.cur_line,
                ))
            }
        }
    }

    /// `index_into_key` variant for `=&`: the slot's stored cell is
    /// swapped for `src` so the array element aliases the source.
    fn bind_into_key(&mut self, c: Cell, key: Option<Value>, src: Cell) -> Result<(), PhpError> {
        if let Some(k) = &key {
            if !matches!(&*c.borrow(), Value::Object(_)) {
                self.check_offset_key(k)?;
            }
        }
        let mut b = c.borrow_mut();
        if matches!(*b, Value::Null) {
            *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
        }
        if let Value::Object(o) = &*b {
            // `$o[k] =& $x`: spl ArrayObject storage binds `src` into the
            // named bucket; everything else — userland ArrayAccess dims
            // and the append form `$o[] =& $x` zend can't address — gets
            // the indirect-modification notice then the assign-by-ref
            // catchable Error.
            let o = o.clone();
            drop(b);
            let spl_arr = if matches!(o.borrow().internal, Some(ObjectInternal::ArrayIter { .. })) {
                Some(self.ao_arr(&o))
            } else {
                None
            };
            if let (Some(arr), Some(k)) = (spl_arr, &key) {
                let old = arr.borrow_mut().bind_cell(to_key(k), src);
                return self.destruct_displaced(old);
            }
            // zend fetches the element (BP_VAR_W read_dimension — the
            // `&offsetGet` / object-element notice rule applies) before
            // the assign-by-ref Error, so offsetGet side effects and
            // the notice both precede it.
            let _ = self.index_cell_object(&c, key.clone())?;
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot assign by reference to an array dimension of an object",
                self.cur_line,
            ));
        }
        if let Value::Str(_) = &*b {
            let k = key.map(|k| to_key(&k));
            drop(b);
            return match k {
                Some(ArrKey::Str(_)) => self.fail(PhpError::uncaught(
                    "TypeError",
                    "Cannot access offset of type string on string",
                    self.cur_line,
                )),
                _ => self.fail(PhpError::uncaught(
                    "Error",
                    "Cannot create references to/from string offsets",
                    self.cur_line,
                )),
            };
        }
        if let Value::Array(_) = &mut *b {
            self.cow_split(&mut b);
            let rc = match &*b {
                Value::Array(rc) => rc.clone(),
                _ => unreachable!(),
            };
            drop(b);
            let old;
            {
                let mut arr = rc.borrow_mut();
                old = match key {
                    Some(k) => arr.bind_cell(to_key(&k), src),
                    None => {
                        let k = ArrKey::Int(arr.next);
                        arr.bind_cell(k, src)
                    }
                };
            }
            self.destruct_displaced(old)
        } else {
            drop(b);
            self.fail(PhpError::uncaught(
                "Error",
                "Cannot use a scalar value as an array",
                self.cur_line,
            ))
        }
    }

    /// Write-context cell fetch on an ArrayAccess object (`$x =&
    /// $o['k']`, `foreach (&$o['k'])`): zend's spl read_dimension is
    /// by-ref, so `&offsetGet` hands back the storage cell through
    /// last_ret_cell. A value-returning offsetGet yields a throwaway
    /// cell — plain `=` writes route through offsetSet elsewhere.
    fn index_cell_object(&mut self, c: &Cell, key: Option<Value>) -> Result<Cell, PhpError> {
        let Value::Object(o) = c.borrow().clone() else {
            unreachable!()
        };
        self.last_ret_cell = None;
        // zend evaluates this read as BP_VAR_RW — a missing bucket is
        // created silently inside offsetGet.
        let was = std::mem::replace(&mut self.dim_by_ref, true);
        let rv = self.method_invoke(
            o.clone(),
            "offsetGet",
            CallArgs::positional(vec![cell(key.unwrap_or(Value::Null))]),
        );
        self.dim_by_ref = was;
        match self.last_ret_cell.take() {
            Some(rc) => {
                self.mark_ref(&rc);
                Ok(rc)
            }
            None => {
                // offsetGet returned by value — the element can't be
                // aliased, so writes through this fetch silently
                // no-op. zend notices the indirect modification only
                // when the fetched value isn't itself an object
                // (object-typed elements carry their own storage).
                let v = rv?;
                if !matches!(v, Value::Object(_)) {
                    let cn = o.borrow().class.name().to_string();
                    self.notice(&format!(
                        "Indirect modification of overloaded element of {} has no effect",
                        cn
                    ))?;
                }
                Ok(cell(v))
            }
        }
    }

    fn index_cell(&mut self, e: &Expr, i: Option<&Expr>) -> Result<Cell, PhpError> {
        let key = match i {
            Some(ie) => Some(self.eval(ie)?),
            None => None,
        };
        self.index_cell_key(e, key)
    }

    /// `index_cell` with an already-evaluated key.
    fn index_cell_key(&mut self, e: &Expr, key: Option<Value>) -> Result<Cell, PhpError> {
        match e {
            Expr::Var(name) => {
                let c = self.var_cell(name);
                self.index_into_key(c, key)
            }
            Expr::Index { e: inner, i: ii } => {
                let c = self.index_cell(inner, ii.as_deref())?;
                self.index_into_key(c, key)
            }
            Expr::Prop {
                obj,
                name,
                nullsafe,
            } => {
                let c = self.prop_cell(obj, name, *nullsafe)?;
                self.index_into_key(c, key)
            }
            Expr::StaticProp { class, name } => {
                let c = self.static_prop_cell(class, name)?;
                self.index_into_key(c, key)
            }
            Expr::VarVar(inner) => {
                let n = self.eval(inner)?;
                let name = self.conv_str(&n)?;
                let c = self.var_cell(&name);
                self.index_into_key(c, key)
            }
            _ => {
                // e.g. function call result index — read-only path.
                let v = match key {
                    Some(k) => self.index_read_val(e, Some(k))?,
                    None => self.index_read(e, None)?,
                };
                Ok(cell(v))
            }
        }
    }

    fn index_read(&mut self, e: &Expr, i: Option<&Expr>) -> Result<Value, PhpError> {
        // Base evaluates before the index expr (left-to-right).
        let base = self.eval(e)?;
        let key = match i {
            Some(ie) => self.eval(ie)?,
            None => {
                return self.fail(PhpError::fatal("[] used in read context", 0));
            }
        };
        self.index_read_base(base, key)
    }

    /// `index_read` for a caller that already evaluated `e` and the key.
    fn index_read_val(&mut self, e: &Expr, key: Option<Value>) -> Result<Value, PhpError> {
        let base = self.eval(e)?;
        match key {
            Some(k) => self.index_read_base(base, k),
            None => self.fail(PhpError::fatal("[] used in read context", 0)),
        }
    }

    fn index_read_base(&mut self, base: Value, key: Value) -> Result<Value, PhpError> {
        // ArrayAccess containers see any key type (offsetGet); string
        // bases validate keys via str_offset_key ('on string' errors).
        if !matches!(&base, Value::Object(o) if self.obj_is_a(o, "ArrayAccess"))
            && !matches!(&base, Value::Str(_))
        {
            self.check_offset_key(&key)?;
        }
        match base {
            Value::Array(rc) => {
                let k = to_key(&key);
                let arr = rc.borrow();
                match arr.get(&k) {
                    Some(v) => Ok(v),
                    None => {
                        let shown = match &key {
                            Value::Str(s) => format!("\"{}\"", crate::value::lossy(s)),
                            other => other.to_php_string(),
                        };
                        if self.silence == 0 {
                            self.warn(&format!("Undefined array key {}", shown))?;
                        }
                        Ok(Value::Null)
                    }
                }
            }
            Value::Str(s) => {
                // zend_check_string_offset — same key matrix as the
                // write path (' 2'/'+2' accepted, leading-int junk
                // warns, casts warn, objects/arrays TypeError).
                let idx = self.str_offset_key(Some(&key))?;
                let bytes: &[u8] = &s[..];
                let idx = if idx < 0 {
                    idx + bytes.len() as i64
                } else {
                    idx
                };
                if idx < 0 || idx as usize >= bytes.len() {
                    if self.silence == 0 {
                        self.warn(&format!("Uninitialized string offset {}", key.to_int()))?;
                    }
                    // zend reads an empty string out of an
                    // uninitialized offset, not NULL.
                    Ok(Value::str(String::new()))
                } else {
                    Ok(Value::bytes(bytes[idx as usize..idx as usize + 1].to_vec()))
                }
            }
            Value::Null => {
                if self.silence == 0 {
                    // PHP 8.5 dropped "value of type" from this message
                    // (bug25922, passByReference_003).
                    self.warn("Trying to access array offset on null")?;
                }
                Ok(Value::Null)
            }
            Value::Object(o) => {
                if self.obj_is_a(&o, "ArrayAccess") {
                    return match self.method_invoke(
                        o,
                        "offsetGet",
                        CallArgs::positional(vec![cell(key)]),
                    ) {
                        Ok(v) => Ok(v),
                        Err(e) => Err(e),
                    };
                }
                if self.silence == 0 {
                    let cn = o.borrow().class.name().to_string();
                    self.warn(&format!("Cannot use object of type {} as array", cn))?;
                }
                Ok(Value::Null)
            }
            _ => {
                if self.silence == 0 {
                    // PHP 8.5 names the scalar itself: int/float/null and
                    // the literal true|false (no "value of type").
                    let what = match &base {
                        Value::Bool(b) => b.to_string(),
                        _ => base.type_name().to_lowercase(),
                    };
                    self.warn(&format!("Trying to access array offset on {}", what))?;
                }
                Ok(Value::Null)
            }
        }
    }

    pub(in crate::interp) fn unset_index(
        &mut self,
        e: &Expr,
        i: Option<&Expr>,
    ) -> Result<(), PhpError> {
        let key = match i {
            // eval errors surface as catchable throwables like zend's
            // (an uncaught Error from the index expr is a `throw`).
            Some(ie) => match self.eval(ie) {
                Ok(v) => Some(v),
                Err(e) => return self.fail::<()>(e),
            },
            None => None,
        };
        // Peel e's own dims — `unset(root[d0][d1]...[k])` descends the
        // d-chain then unsets k; `cur` is the root container expr.
        let mut idxs: Vec<Option<&Expr>> = Vec::new();
        let mut cur = e;
        while let Expr::Index { e: b, i: ix } = cur {
            idxs.push(ix.as_deref());
            cur = b;
        }
        idxs.reverse();
        // Roots with real storage cells resolve once (prop_cell invokes
        // __get a single time for an overloaded prop); a missing plain
        // variable warns and no-ops (zend undefined-variable semantics).
        let root_cell: Option<Cell> = match cur {
            Expr::Var(name) => match self.var_cell_opt(name) {
                Some(c) => Some(c),
                None => {
                    if self.silence == 0 {
                        self.warn(&format!("Undefined variable ${}", name))?;
                    }
                    return Ok(());
                }
            },
            Expr::Prop {
                obj,
                name,
                nullsafe,
            } => Some(self.prop_cell(obj, name, *nullsafe)?),
            Expr::StaticProp { class, name } => Some(self.static_prop_cell(class, name)?),
            Expr::VarVar(inner) => {
                let n = self.eval(inner)?;
                let name = self.conv_str(&n)?;
                Some(self.var_cell(&name))
            }
            _ => None,
        };
        // Roots without storage cells evaluate once — zend runs the
        // root expression a single time and unsets inside the result
        // (`unset(ret()["a"])` calls ret() once). The value rides a
        // scratch cell through the same dim-descent as real storage.
        let root_cell = match root_cell {
            Some(c) => c,
            None => match self.eval(cur) {
                Ok(v) => cell(v),
                Err(e) => return self.fail::<()>(e),
            },
        };
        // Nested-dim unset on an spl array-object — `unset($o[k][j])`:
        // intermediate levels read live storage elements (zend's
        // indirect modification); a missing level reports the
        // "Indirect modification of overloaded element" notice instead
        // of an undefined-key warning (bug66127). Only multi-dim unsets
        // take this path — `unset($o[k])` must dispatch offsetUnset so
        // userland overrides still run.
        if !idxs.is_empty() {
            let ao_obj = match &*root_cell.borrow() {
                Value::Object(o)
                    if matches!(o.borrow().internal, Some(ObjectInternal::ArrayIter { .. })) =>
                {
                    Some(o.clone())
                }
                _ => None,
            };
            let ao = ao_obj.map(|o| {
                let arr = self.ao_arr(&o);
                (o, arr)
            });
            if let Some((o, arr)) = ao {
                let mut cur_arr = arr;
                let mut ok = true;
                for ix in &idxs {
                    let kv = match ix {
                        Some(ie) => self.eval(ie)?,
                        None => Value::Null,
                    };
                    let next = match cur_arr.borrow().get_cell(&to_key(&kv)) {
                        Some(cc) => {
                            let mut b = cc.borrow_mut();
                            // The fetched array may alias other slots
                            // (assigned by value elsewhere) — cow-split
                            // like the cell descent below does.
                            self.cow_split(&mut b);
                            match &*b {
                                Value::Array(na) => Some(na.clone()),
                                Value::Object(oo) => {
                                    if matches!(
                                        oo.borrow().internal,
                                        Some(ObjectInternal::ArrayIter { .. })
                                    ) {
                                        Some(self.ao_arr(oo))
                                    } else {
                                        None
                                    }
                                }
                                _ => None,
                            }
                        }
                        None => None,
                    };
                    match next {
                        Some(na) => cur_arr = na,
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    // The terminal dim write routes through zend's
                    // spl_array_unset_dimension — mid-sort it throws.
                    if let Some(e) = self.ao_sorting_err(&o) {
                        return self.fail(e);
                    }
                    if let Some(k) = &key {
                        // The borrow must drop before the evicted
                        // payload's dtors run — a __destruct reading
                        // this same array would re-borrow it.
                        let evicted = cur_arr.borrow_mut().unset(&to_key(k));
                        if let Some(v) = evicted {
                            self.destruct_dying_value(&v)?;
                        }
                    }
                    return Ok(());
                }
                if self.silence == 0 {
                    let cn = o.borrow().class.name().to_string();
                    self.notice(&format!(
                        "Indirect modification of overloaded element of {} has no effect",
                        cn
                    ))?;
                }
                return Ok(());
            }
        }
        // Cell-backed roots then walk dim to dim through the cells.
        // Missing intermediates are a silent no-op (zend doesn't
        // autovivify on unset); non-array containers throw zend's
        // catchable unset Errors ("Cannot unset offset in a non-array
        // variable" &c).
        let mut c = root_cell;
        for ix in idxs {
            let kv = match ix {
                Some(ie) => self.eval(ie)?,
                None => Value::Null,
            };
            match self.unset_dim_cell(&c, kv)? {
                Some(nc) => c = nc,
                None => return Ok(()),
            }
        }
        self.unset_in_cell(c, key)
    }

    /// One intermediate dim down for `unset`: plain arrays yield the
    /// bucket cell (cow-separating a shared table first, like a write),
    /// spl array-objects yield the live storage cell, other ArrayAccess
    /// objects fetch through offsetGet. A missing bucket is a silent
    /// no-op (zend doesn't autovivify on unset); scalars/strings and
    /// non-ArrayAccess objects throw the catchable unset `Error`s.
    fn unset_dim_cell(&mut self, c: &Cell, key: Value) -> Result<Option<Cell>, PhpError> {
        let v = c.borrow().clone();
        match v {
            Value::Array(_) => {
                let mut b = c.borrow_mut();
                self.cow_split(&mut b);
                let rc = match &*b {
                    Value::Array(rc) => rc.clone(),
                    _ => unreachable!(),
                };
                drop(b);
                let found = rc.borrow().get_cell(&to_key(&key));
                Ok(found)
            }
            Value::Null => Ok(None),
            Value::Object(o) => {
                if matches!(o.borrow().internal, Some(ObjectInternal::ArrayIter { .. })) {
                    let arr = self.ao_arr(&o);
                    let found = arr.borrow().get_cell(&to_key(&key));
                    match found {
                        Some(cc) => Ok(Some(cc)),
                        None => {
                            if self.silence == 0 {
                                let cn = o.borrow().class.name().to_string();
                                self.notice(&format!(
                                    "Indirect modification of overloaded element of {} has no effect",
                                    cn
                                ))?;
                            }
                            Ok(None)
                        }
                    }
                } else if self.obj_is_a(&o, "ArrayAccess") {
                    // index_cell_object owns the indirect-modification
                    // notice now (non-ref, non-object fetched element —
                    // zend's zend_fetch_dimension_address rule, which
                    // unset shares).
                    let cc = self.index_cell_object(c, Some(key))?;
                    Ok(Some(cc))
                } else {
                    let cn = o.borrow().class.name().to_string();
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot use object of type {} as array", cn),
                        self.cur_line,
                    ))
                }
            }
            Value::Str(_) => self.fail(PhpError::uncaught(
                "Error",
                "Cannot use string offset as an array",
                self.cur_line,
            )),
            Value::Callable(_) => self.fail(PhpError::uncaught(
                "Error",
                "Cannot use object of type Closure as array",
                self.cur_line,
            )),
            _ => self.fail(PhpError::uncaught(
                "Error",
                "Cannot unset offset in a non-array variable",
                self.cur_line,
            )),
        }
    }

    /// Final dim of `unset(cell[key])`: arrays cow-separate then drop the
    /// key, null is a silent no-op, strings/scalars/non-ArrayAccess
    /// objects throw zend's catchable unset `Error`s, ArrayAccess
    /// objects dispatch to offsetUnset.
    fn unset_in_cell(&mut self, c: Cell, key: Option<Value>) -> Result<(), PhpError> {
        {
            let mut b = c.borrow_mut();
            match &*b {
                Value::Array(_) => {
                    // `unset($copy[$k])` must cow-separate a shared
                    // array like a write does — PHP copies `$a = $b`
                    // lazily; mutating the shared table would corrupt
                    // the source (InputDefinition::parseArgument
                    // unsets on its own copy of getArguments()).
                    self.cow_split(&mut b);
                    if let (Value::Array(rc), Some(k)) = (&*b, &key) {
                        let k = to_key(k);
                        let rc = rc.clone();
                        drop(b);
                        // The evicted payload's last ref dies with
                        // the cell — held objects/gens destruct now
                        // (zend destroys the zval's contents). The
                        // borrow_mut must end before userland dtors
                        // run: an element by-ref aliasing this array
                        // reads it inside __destruct (bug65051).
                        let evicted = rc.borrow_mut().unset(&k);
                        if let Some(v) = evicted {
                            self.destruct_dying_value(&v)?;
                        }
                        return Ok(());
                    }
                    return Ok(());
                }
                Value::Null => return Ok(()),
                _ => {}
            }
        }
        match c.borrow().clone() {
            Value::Object(o) => {
                if self.obj_is_a(&o, "ArrayAccess") {
                    let kv = key.unwrap_or(Value::Null);
                    self.method_invoke(o, "offsetUnset", CallArgs::positional(vec![cell(kv)]))?;
                    Ok(())
                } else {
                    let cn = o.borrow().class.name().to_string();
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot use object of type {} as array", cn),
                        self.cur_line,
                    ))
                }
            }
            Value::Str(_) => self.fail(PhpError::uncaught(
                "Error",
                "Cannot unset string offsets",
                self.cur_line,
            )),
            Value::Callable(_) => self.fail(PhpError::uncaught(
                "Error",
                "Cannot use object of type Closure as array",
                self.cur_line,
            )),
            _ => self.fail(PhpError::uncaught(
                "Error",
                "Cannot unset offset in a non-array variable",
                self.cur_line,
            )),
        }
    }

    /// Catch-bind checking: strict member fit only — no __toString or
    /// scalar coercion (int still widens to float)
    /// (typed_properties_108).
    fn slot_write_strict(&mut self, tys: &[String], v: &Value) -> Option<Value> {
        if self.ty_exact(tys, v) {
            if tys.iter().any(|t| t.eq_ignore_ascii_case("float")) {
                if let Value::Int(i) = v {
                    return Some(Value::Float(*i as f64));
                }
            }
            return Some(v.clone());
        }
        None
    }

    /// The write result one typed-slot owner would produce — exact
    /// match widens `int` into a `float` member, weak files coerce;
    /// `None` when the type can't be satisfied (union_types/prop_ref_assign).
    fn slot_write_one(&mut self, tys: &[String], v: &Value) -> Result<Option<Value>, PhpError> {
        if self.ty_exact(tys, v) {
            if tys.iter().any(|t| t.eq_ignore_ascii_case("float")) {
                if let Value::Int(i) = v {
                    return Ok(Some(Value::Float(*i as f64)));
                }
            }
            return Ok(Some(v.clone()));
        }
        if !self.exec_file_strict() {
            // Object with __toString coerces into a `string` slot (107).
            if let Value::Object(o) = v {
                if tys.iter().any(|t| t.eq_ignore_ascii_case("string"))
                    && (self
                        .find_method_in(&o.borrow().class, "__tostring")
                        .is_some()
                        || self
                            .find_method_in(&o.borrow().class, "__toString")
                            .is_some())
                {
                    let sv =
                        self.method_invoke(o.clone(), "__toString", CallArgs::positional(vec![]))?;
                    let svs = self.conv_str(&sv)?;
                    return Ok(Some(Value::str(svs)));
                }
            }
            if let Some(cv) = weak_ty_coerce(tys, v) {
                self.deprecate_lossy_int(tys, v, &cv);
                return Ok(Some(cv));
            }
        }
        Ok(None)
    }

    /// An owner is stale when the prop it names no longer holds this
    /// cell — static rebinds (082), unsets, or a dead object (094).
    fn slot_anchor_alive(&self, ptr: usize, anc: &SlotAnchor) -> bool {
        match anc {
            SlotAnchor::Obj(w, key) => w
                .upgrade()
                .map(|o| {
                    o.borrow()
                        .props
                        .get(key)
                        .map(|c| Rc::as_ptr(c) as usize == ptr)
                        .unwrap_or(false)
                })
                .unwrap_or(false),
            SlotAnchor::Statics(cn, pn) => self
                .classes
                .get(&cn.to_lowercase())
                .map(|c| {
                    c.statics
                        .borrow()
                        .get(pn)
                        .map(|c2| Rc::as_ptr(c2) as usize == ptr)
                        .unwrap_or(false)
                })
                .unwrap_or(false),
            SlotAnchor::None => true,
        }
    }

    /// Drop owners whose prop no longer points at `ptr`, then rebuild
    /// the merged constraint from the live owners' declared types.
    pub(in crate::interp) fn prune_typed_slot(&mut self, ptr: usize) {
        let prim_dead = self
            .slot_anchor
            .get(&ptr)
            .map(|a| !self.slot_anchor_alive(ptr, a))
            .unwrap_or(false);
        if prim_dead {
            self.typed_slots.remove(&ptr);
            self.slot_anchor.remove(&ptr);
        }
        if let Some(os) = self.slot_owners.get(&ptr) {
            let os = os.clone();
            let dead: Vec<usize> = os
                .iter()
                .enumerate()
                .filter(|(_, (_, _, _, a))| !self.slot_anchor_alive(ptr, a))
                .map(|(i, _)| i)
                .collect();
            if !dead.is_empty() {
                let os = self.slot_owners.get_mut(&ptr).unwrap();
                for i in dead.into_iter().rev() {
                    os.remove(i);
                }
                if os.is_empty() {
                    self.slot_owners.remove(&ptr);
                }
            }
        }
        if let Some(os) = self.slot_owners.get(&ptr) {
            let os = os.clone();
            let mut acc: Vec<String> = Vec::new();
            for (i, (t, _, _, _)) in os.iter().enumerate() {
                acc = if i == 0 {
                    t.clone()
                } else {
                    self.ty_bind_merge(&acc, t).unwrap_or_default()
                };
            }
            self.slot_merged.insert(ptr, acc);
        } else if let Some((_, t, _, _)) = self.typed_slots.get(&ptr) {
            self.slot_merged.insert(ptr, t.clone());
        } else {
            self.slot_merged.remove(&ptr);
        }
    }

    /// By-ref foreach bind onto a readonly prop cell shared through an
    /// ArrayIterator — Error "Cannot acquire reference to readonly
    /// property C::$p" (typed_properties_115).
    pub(in crate::interp) fn readonly_ref_error(&mut self, c: &Cell) -> Option<Flow> {
        let (cn, pn) = self.readonly_cells.get(&(Rc::as_ptr(c) as usize))?;
        let v = self.exception(
            "Error",
            &format!("Cannot acquire reference to readonly property {cn}::${pn}"),
        );
        let e = self.throw(v);
        Some(self.err_flow(e))
    }

    fn typed_slot_store(&mut self, c: &Cell, v: Value) -> Result<Value, PhpError> {
        self.typed_slot_store_mode(c, v, false)
    }

    /// `strict` = catch-binding semantics: no __toString/scalar
    /// coercion — the value must fit the declared type as-is
    /// (typed_properties_108).
    pub(in crate::interp) fn typed_slot_store_mode(
        &mut self,
        c: &Cell,
        v: Value,
        strict: bool,
    ) -> Result<Value, PhpError> {
        let ptr = Rc::as_ptr(c) as usize;
        self.prune_typed_slot(ptr);
        let owners: Vec<(Vec<String>, String, String)> =
            if let Some(os) = self.slot_owners.get(&ptr) {
                os.iter()
                    .map(|(t, n, p, _)| (t.clone(), n.clone(), p.clone()))
                    .collect()
            } else {
                match self
                    .typed_slots
                    .get(&ptr)
                    .map(|(_, t, n, p)| (t.clone(), n.clone(), p.clone()))
                {
                    Some(o) => vec![o],
                    None => return Ok(v),
                }
            };
        let mut results: Vec<Option<Value>> = Vec::with_capacity(owners.len());
        for (tys, _, _) in &owners {
            results.push(if strict {
                self.slot_write_strict(tys, &v)
            } else {
                self.slot_write_one(tys, &v)?
            });
        }
        // A value an owner rejects outright reports that owner; only
        // all-accepted-but-divergent coercions are "inconsistent"
        // (typed_reference).
        let consistent = results.iter().all(|r| r.is_some())
            && results
                .iter()
                .map(|r| r.clone().unwrap())
                .all(|rv| Self::value_identical(&rv, results[0].as_ref().unwrap()));
        if consistent {
            return Ok(results.into_iter().next().unwrap().unwrap());
        }
        if let Some(bad) = results.iter().position(|r| r.is_none()) {
            let (tys, cn, pn) = &owners[bad];
            let where_ = if self.is_ref_ptr(ptr) {
                "reference held by property"
            } else {
                "property"
            };
            let mut e = PhpError::uncaught(
                "TypeError",
                format!(
                    "Cannot assign {} to {} {}::${} of type {}",
                    self.zval_type_name(&v),
                    where_,
                    cn,
                    pn,
                    ty_disp(tys)
                ),
                0,
            );
            e.thrown_line = Some(self.cur_line);
            return self.fail(e);
        }
        let mut e = if owners.len() == 1 {
            let (tys, cn, pn) = &owners[0];
            let where_ = if self.is_ref_ptr(ptr) {
                "reference held by property"
            } else {
                "property"
            };
            PhpError::uncaught(
                "TypeError",
                format!(
                    "Cannot assign {} to {} {}::${} of type {}",
                    self.zval_type_name(&v),
                    where_,
                    cn,
                    pn,
                    ty_disp(tys)
                ),
                0,
            )
        } else {
            let held: Vec<String> = owners
                .iter()
                .map(|(tys, cn, pn)| format!("property {}::${} of type {}", cn, pn, ty_disp(tys)))
                .collect();
            PhpError::uncaught(
                "TypeError",
                format!(
                    "Cannot assign {} to reference held by {}, as this would result in an inconsistent type conversion",
                    self.zval_type_name(&v),
                    held.join(" and ")
                ),
                0,
            )
        };
        e.thrown_line = Some(self.cur_line);
        self.fail(e)
    }

    /// Synthesized signature of a magic-method trampoline FCC:
    /// `mixed ...$arguments` (trampoline_closure_named_arguments).
    fn trampoline_decl() -> Rc<crate::ast::FunctionDecl> {
        Rc::new(crate::ast::FunctionDecl {
            name: "{trampoline}".into(),
            params: vec![crate::ast::Param {
                name: "arguments".into(),
                default: None,
                by_ref: false,
                variadic: true,
                ty: Some(vec!["mixed".into()]),
                promoted: false,
                vis: None,
                readonly: false,
                is_final: false,
                set_vis: None,
                hooks: None,
            }],
            ret: None,
            body: vec![],
            attrs: vec![],
            by_ref: false,
            line: 0,
            end_line: 0,
            file: String::new(),
            ns: String::new(),
            decl_in: None,
        })
    }

    /// Decl for the function a callable value points at — builtins
    /// synthesize one from builtin_sig. Shared by the function and
    /// method (Closure::__invoke) reflector paths.
    pub(in crate::interp) fn callable_decl(
        &mut self,
        v: &Value,
    ) -> Option<Rc<crate::ast::FunctionDecl>> {
        match v {
            Value::Str(s) => {
                let n = String::from_utf8_lossy(s).to_lowercase();
                self.functions
                    .get(&n)
                    .cloned()
                    .or_else(|| Self::builtin_decl(&n))
            }
            Value::Callable(c) => match &c.kind {
                crate::value::CallableKind::Closure(d) => Some(d.clone()),
                crate::value::CallableKind::Named(n) => self
                    .functions
                    .get(&n.to_lowercase())
                    .cloned()
                    .or_else(|| Self::builtin_decl(&n.to_lowercase())),
                crate::value::CallableKind::Method { name, obj, class } => {
                    let c = class
                        .clone()
                        .or_else(|| obj.as_ref().map(|o| o.borrow().class.clone()));
                    match c {
                        Some(c) => self
                            .find_method_in(&c, name)
                            .map(|(m, _)| Rc::new(m.decl.clone()))
                            // A magic-method trampoline (`C::undef(...)`
                            // on __callStatic / `$o->undef(...)` on
                            // __call) reflects as `mixed ...$arguments`
                            // (trampoline_closure_named_arguments).
                            .or_else(|| {
                                let magic = if obj.is_some() {
                                    "__call"
                                } else {
                                    "__callstatic"
                                };
                                self.find_method_in(&c, magic)
                                    .is_some()
                                    .then(Self::trampoline_decl)
                            }),
                        None => None,
                    }
                }
            },
            _ => None,
        }
    }

    /// Synthetic decl for an internal function, from builtin_sig +
    /// builtin_param_ty — lets reflectors report param names,
    /// required flags and declared types for builtins (bug69802_2).
    fn builtin_decl(lname: &str) -> Option<Rc<crate::ast::FunctionDecl>> {
        let sig = crate::builtins::builtin_sig(lname)?;
        Some(Rc::new(crate::ast::FunctionDecl {
            name: lname.into(),
            params: sig
                .into_iter()
                .map(|(name, req)| crate::ast::Param {
                    default: if req {
                        None
                    } else {
                        Some(crate::ast::Expr::Null)
                    },
                    ty: crate::builtins::builtin_param_ty(lname, &name),
                    name,
                    by_ref: false,
                    variadic: false,
                    promoted: false,
                    vis: None,
                    readonly: false,
                    is_final: false,
                    set_vis: None,
                    hooks: None,
                })
                .collect(),
            ret: None,
            body: vec![],
            attrs: vec![],
            by_ref: false,
            line: 0,
            end_line: 0,
            file: String::new(),
            ns: String::new(),
            decl_in: None,
        }))
    }

    /// Same-type same-value — owners must agree on the *exact* result
    /// (int(42) vs float(42.0) is inconsistent).
    fn value_identical(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => x == y,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::Object(x), Value::Object(y)) => Rc::ptr_eq(x, y),
            (Value::Array(x), Value::Array(y)) => Rc::ptr_eq(x, y),
            _ => false,
        }
    }

    fn incdec(&mut self, target: &Expr, delta: i64, post: bool) -> Result<Value, PhpError> {
        // PHP warns on undefined vars/props/keys during ++/-- (bug25547).
        let old = match target {
            Expr::Var(name) => self.var_get(name).unwrap_or(Value::Null),
            Expr::Index { e, i } => {
                let base = self.eval(e)?;
                let key = match i {
                    Some(ie) => self.eval(ie)?,
                    None => return self.fail(PhpError::fatal("[] used in read context", 0)),
                };
                // `++`/`--` on an ArrayAccess offset writes the by-ref
                // offsetGet cell directly — no offsetSet call
                // (typed_properties_065).
                if let Value::Object(o) = &base {
                    if self.obj_is_a(o, "ArrayAccess") {
                        return self.incdec_aa(o.clone(), key, delta, post);
                    }
                }
                if matches!(&base, Value::Str(_)) {
                    // zend validates the key, then refuses the incdec
                    // (FETCH_DIM_INCDEC → 'Cannot increment/decrement').
                    self.str_offset_key(Some(&key))?;
                    return self.fail(PhpError::uncaught(
                        "Error",
                        "Cannot increment/decrement string offsets",
                        self.cur_line,
                    ));
                }
                self.index_read_base(base, key).unwrap_or(Value::Null)
            }
            // ++/-- reads through __get first — its exceptions
            // propagate (the __set is never reached, bug38624).
            Expr::Prop { .. } => self.prop_read_loose(target)?,
            Expr::VarVar(inner) => {
                let n = self.eval(inner)?;
                let name = self.conv_str(&n)?;
                self.var_get(&name).unwrap_or(Value::Null)
            }
            Expr::StaticProp { class, name } => {
                self.static_prop_read(class, name).unwrap_or(Value::Null)
            }
            _ => {
                return self.fail(PhpError::fatal(
                    "Cannot increment/decrement non-variable",
                    0,
                ))
            }
        };
        // Typed `int` prop can't overflow to float — a dedicated Error
        // instead of the generic assign TypeError (typed_properties_019).
        // The target's storage cell carries the owning prop's type —
        // "property" when the target IS that prop, "a reference held
        // by property" when reached through an alias/bound ref.
        // ++/-- on an *overloaded* prop (__get/__set magic) never
        // touches the backing cell — it reads a value and calls __set
        // (typed_properties_061); skip the ref-cell overflow check.
        let prop_overloaded = match target {
            Expr::Prop { obj, name, .. } => {
                let ov = self.eval(obj)?;
                let pn = self.prop_name(name)?;
                match &ov {
                    Value::Object(o) => self.decl_prop(o, &pn).is_none(),
                    _ => false,
                }
            }
            _ => false,
        };
        if !prop_overloaded && matches!(old, Value::Int(i) if i.checked_add(delta).is_none()) {
            let ent = self.eval_cell(target).ok().map(|c| {
                // Unaliased count is 3 (storage + typed_slots + this
                // temp); a `=&` bind adds a var slot -> "a reference
                // held by" (union_types/incdec_prop).
                let shared = Rc::strong_count(&c) > 3;
                let e = self
                    .typed_slots
                    .get(&(Rc::as_ptr(&c) as usize))
                    .map(|(cc, t, n, p)| (cc.clone(), t.clone(), n.clone(), p.clone()));
                (shared, e)
            });
            if let Some((shared, Some((_, tys, cn, cpn)))) = ent {
                if tys.iter().any(|m| m.eq_ignore_ascii_case("int"))
                    && !tys.iter().any(|m| m.eq_ignore_ascii_case("float"))
                {
                    let dir = if delta > 0 { "increment" } else { "decrement" };
                    let bound = if delta > 0 { "maximal" } else { "minimal" };
                    // "a reference held by" once the slot is aliased —
                    // a `&$prop` bind makes the storage cell shared
                    // (union_types/incdec_prop).
                    let own = !shared
                        && match target {
                            Expr::Prop { name, .. } | Expr::StaticProp { name, .. } => {
                                self.prop_name(name).map(|pn| pn == cpn).unwrap_or(false)
                            }
                            _ => false,
                        };
                    let msg = if own {
                        format!(
                            "Cannot {} property {}::${} of type {} past its {} value",
                            dir,
                            cn,
                            cpn,
                            ty_disp(&tys),
                            bound
                        )
                    } else {
                        format!(
                            "Cannot {} a reference held by property {}::${} of type {} past its {} value",
                            dir,
                            cn,
                            cpn,
                            ty_disp(&tys),
                            bound
                        )
                    };
                    let mut e = PhpError::uncaught("TypeError", msg, 0);
                    e.thrown_line = Some(self.cur_line);
                    return self.fail(e);
                }
            }
        }
        let new = self.incdec_value(&old, delta)?;
        self.store(target, new.clone())?;
        Ok(if post { old } else { new })
    }

    /// `$o[k]++` on an ArrayAccess: `&offsetGet` hands back the real
    /// backing cell and ++/-- writes through it (no offsetSet);
    /// a value-returning offsetGet falls back to offsetSet
    /// (typed_properties_065).
    fn incdec_aa(
        &mut self,
        o: Rc<RefCell<PhpObject>>,
        key: Value,
        delta: i64,
        post: bool,
    ) -> Result<Value, PhpError> {
        self.last_ret_cell = None;
        let was = std::mem::replace(&mut self.dim_by_ref, true);
        let rv = self.method_invoke(
            o.clone(),
            "offsetGet",
            CallArgs::positional(vec![cell(key.clone())]),
        );
        self.dim_by_ref = was;
        let rv = rv?;
        let rc = self.last_ret_cell.take();
        if let Some(c) = &rc {
            self.mark_ref(c);
        }
        let old = rc.as_ref().map(|c| c.borrow().clone()).unwrap_or(rv);
        // int-typed backing cell can't overflow to float — dedicated
        // "past its minimal/maximal value" Error (065).
        if let (Value::Int(iv), Some(c)) = (old.clone(), rc.clone()) {
            if iv.checked_add(delta).is_none() {
                let ptr = Rc::as_ptr(&c) as usize;
                self.prune_typed_slot(ptr);
                let owners: Vec<(Vec<String>, String, String)> = self
                    .slot_owners
                    .get(&ptr)
                    .map(|os| {
                        os.iter()
                            .map(|(t, n, p, _)| (t.clone(), n.clone(), p.clone()))
                            .collect()
                    })
                    .or_else(|| {
                        self.typed_slots
                            .get(&ptr)
                            .map(|(_, t, n, p)| vec![(t.clone(), n.clone(), p.clone())])
                    })
                    .unwrap_or_default();
                for (tys, cn, cpn) in &owners {
                    if tys.iter().any(|m| m.eq_ignore_ascii_case("int"))
                        && !tys.iter().any(|m| m.eq_ignore_ascii_case("float"))
                    {
                        let dir = if delta > 0 { "increment" } else { "decrement" };
                        let bound = if delta > 0 { "maximal" } else { "minimal" };
                        let mut e = PhpError::uncaught(
                            "TypeError",
                            format!(
                                "Cannot {} a reference held by property {}::${} of type {} past its {} value",
                                dir,
                                cn,
                                cpn,
                                ty_disp(tys),
                                bound
                            ),
                            0,
                        );
                        e.thrown_line = Some(self.cur_line);
                        return self.fail(e);
                    }
                }
            }
        }
        let new = self.incdec_value(&old, delta)?;
        match rc {
            Some(c) => {
                let nv = self.typed_slot_store(&c, new.clone())?;
                *c.borrow_mut() = nv;
            }
            None => {
                self.method_invoke(
                    o,
                    "offsetSet",
                    CallArgs::positional(vec![cell(key), cell(new.clone())]),
                )?;
            }
        }
        Ok(if post { old } else { new })
    }

    /// PHP inc/dec semantics: null++ = 1, null-- = null, strings increment
    /// alphanumerically (Perl-style), numeric strings go numeric.
    fn incdec_value(&mut self, v: &Value, delta: i64) -> Result<Value, PhpError> {
        Ok(match v {
            Value::Null => {
                if delta > 0 {
                    Value::Int(1)
                } else {
                    Value::Null
                }
            }
            Value::Bool(_) => {
                // bools don't change, but PHP 8.3+ warns on inc/dec.
                let dir = if delta > 0 { "Increment" } else { "Decrement" };
                self.warn(&format!(
                    "{} on type bool has no effect, this will change in the next major version of PHP",
                    dir
                ))?;
                v.clone()
            }
            Value::Array(_) | Value::Object(_) | Value::Resource(_) | Value::Callable(_) => {
                let dir = if delta > 0 { "increment" } else { "decrement" };
                let what = match v {
                    Value::Array(_) => "array".to_string(),
                    Value::Object(o) => o.borrow().class.name().to_string(),
                    Value::Callable(_) => "Closure".to_string(),
                    _ => "resource".to_string(),
                };
                return self.fail(PhpError::uncaught(
                    "TypeError",
                    format!("Cannot {} {}", dir, what),
                    0,
                ));
            }
            // Int overflow on ++ promotes to float (postinc_basiclong_64bit).
            Value::Int(i) => match i.checked_add(delta) {
                Some(n) => Value::Int(n),
                None => Value::Float(*i as f64 + delta as f64),
            },
            Value::Float(f) => Value::Float(f + delta as f64),
            Value::Str(s) => match numeric(s) {
                Numeric::Int(i) => match i.checked_add(delta) {
                    Some(n) => Value::Int(n),
                    None => Value::Float(i as f64 + delta as f64),
                },
                Numeric::Float(f) => Value::Float(f + delta as f64),
                Numeric::Leading(_, _) | Numeric::NonNumeric => {
                    // PHP 8.3+: inc/dec on a non-well-formed numeric or
                    // non-numeric string is deprecated and uses Perl-style
                    // alphanumeric increment (never decrements).
                    if delta > 0 {
                        self.deprecated(
                            "Increment on non-numeric string is deprecated, use str_increment() instead",
                        )?;
                        Value::bytes(perl_inc(s))
                    } else {
                        self.deprecated(
                            "Decrement on non-numeric string has no effect and is deprecated",
                        )?;
                        v.clone()
                    }
                }
            },
        })
    }

    fn unary(&mut self, op: &'static str, e: &Expr) -> Result<Value, PhpError> {
        match op {
            "!" => {
                let v = self.eval(e)?;
                Ok(Value::Bool(!v.is_truthy()))
            }
            "-" => {
                let v = self.eval(e)?;
                Ok(match v {
                    // -PHP_INT_MIN overflows → float.
                    Value::Int(i) => match i.checked_neg() {
                        Some(n) => Value::Int(n),
                        None => Value::Float(-(i as f64)),
                    },
                    Value::Float(f) => Value::Float(-f),
                    Value::Str(s) => match numeric(&s) {
                        Numeric::Int(i) => match i.checked_neg() {
                            Some(n) => Value::Int(n),
                            None => Value::Float(-(i as f64)),
                        },
                        Numeric::Float(f) => Value::Float(-f),
                        Numeric::Leading(f, is_int) => {
                            self.warn("A non-numeric value encountered")?;
                            if is_int {
                                Value::Int(-(f as i64))
                            } else {
                                Value::Float(-f)
                            }
                        }
                        Numeric::NonNumeric => {
                            // Unary minus lowers to `$s * -1`.
                            return self.fail(PhpError::uncaught(
                                "TypeError",
                                "Unsupported operand types: string * int",
                                0,
                            ));
                        }
                    },
                    _ => Value::Float(-v.to_float()),
                })
            }
            "+" => {
                let v = self.eval(e)?;
                Ok(match v {
                    Value::Int(_) | Value::Float(_) => v,
                    other => match numeric(&other.to_php_bytes()) {
                        Numeric::Int(i) => Value::Int(i),
                        Numeric::Float(f) => Value::Float(f),
                        Numeric::Leading(f, is_int) => {
                            self.warn("A non-numeric value encountered")?;
                            if is_int {
                                Value::Int(f as i64)
                            } else {
                                Value::Float(f)
                            }
                        }
                        Numeric::NonNumeric => {
                            return self.fail(PhpError::uncaught(
                                "TypeError",
                                format!("Unsupported operand types: {} * int", other.type_name()),
                                0,
                            ))
                        }
                    },
                })
            }
            "~" => {
                let v = self.eval(e)?;
                match &v {
                    // ~"abc" negates bytes.
                    Value::Str(s) => Ok(Value::bytes(s.iter().map(|b| !b).collect::<Vec<u8>>())),
                    _ => Ok(Value::Int(!self.coerce_int(&v))),
                }
            }
            "@" => {
                self.silence += 1;
                let v = self.eval(e);
                self.silence -= 1;
                v
            }
            _ => unreachable!(),
        }
    }

    fn binary(&mut self, op: &'static str, l: &Expr, r: &Expr) -> Result<Value, PhpError> {
        match op {
            "&&" => {
                let lv = self.eval(l)?;
                if !lv.is_truthy() {
                    return Ok(Value::Bool(false));
                }
                let rv = self.eval(r)?;
                Ok(Value::Bool(rv.is_truthy()))
            }
            "||" => {
                let lv = self.eval(l)?;
                if lv.is_truthy() {
                    return Ok(Value::Bool(true));
                }
                let rv = self.eval(r)?;
                Ok(Value::Bool(rv.is_truthy()))
            }
            "xor" => {
                let lv = self.eval(l)?.is_truthy();
                let rv = self.eval(r)?.is_truthy();
                Ok(Value::Bool(lv ^ rv))
            }
            "??" => {
                // isset() semantics: undefined vars, missing offsets and
                // uninitialized typed props fall through to the right.
                // isset_val returns the read value so calls and getters
                // evaluate exactly once.
                match self.isset_val_mode(l, 2)? {
                    Some(v) => Ok(v),
                    None => self.eval(r),
                }
            }
            "." => {
                let (lv, rv) = self.binary_operands(l, r)?;
                let mut ls = self.conv_bytes(&lv)?;
                let rs = self.conv_bytes(&rv)?;
                ls.extend_from_slice(&rs);
                Ok(Value::bytes(ls))
            }
            "==" | "!=" | "===" | "!==" | "<" | "<=" | ">" | ">=" | "<=>" => {
                let (lv, rv) = self.binary_operands(l, r)?;
                self.compare_op(op, l, r, &lv, &rv)
            }
            "named" => self.eval(r), // named-arg marker: value passthrough
            _ => {
                let (lv, rv) = self.binary_operands(l, r)?;
                self.arith(op, lv, rv)
            }
        }
    }

    /// Operands of a binary op: Zend binds plain CVs at op-execution —
    /// i.e. after the right operand has run — so `$a . ($a=$b)` sees the
    /// assigned value. Other left expressions evaluate normally first
    /// (execution_order).
    fn binary_operands(&mut self, l: &Expr, r: &Expr) -> Result<(Value, Value), PhpError> {
        if let Expr::Var(n) = l {
            let c = self.var_cell_opt(n);
            let rv = self.eval(r)?;
            let lv = match c {
                Some(c) => c.borrow().clone(),
                None => self.eval(l)?,
            };
            return Ok((lv, rv));
        }
        let lv = self.eval(l)?;
        let rv = self.eval(r)?;
        Ok((lv, rv))
    }

    fn compare_op(
        &mut self,
        op: &str,
        l: &Expr,
        r: &Expr,
        a: &Value,
        b: &Value,
    ) -> Result<Value, PhpError> {
        // pass_two (zend_vm_set_opcode_handler) swaps the operands of the
        // COMMUTATIVE ops IS_EQUAL/IS_NOT_EQUAL/IS_IDENTICAL/
        // IS_NOT_IDENTICAL when op1's znode type ranks below op2's
        // (IS_CONST < IS_TMP_VAR < IS_VAR < IS_CV). The cyclic/protected
        // operand of zend_hash_compare is always the compare's left, so a
        // literal-left compare like `[[1,2]] == cyc()` actually runs
        // compare(cyc_result, literal) and the self-referencing array is
        // the marked one — re-entry throws "Nesting level too deep".
        // `<`/`<=`/`<=>` aren't commutative (source order kept); `>`/`>=`
        // emit as IS_SMALLER(_OR_EQUAL) on the reversed nodes.
        let (a, b) = match op {
            "==" | "!=" | "===" | "!==" if compare_operand_rank(l) < compare_operand_rank(r) => {
                (b, a)
            }
            _ => (a, b),
        };
        crate::value::clear_cmp_depth_err();
        let v = match op {
            "===" => Value::Bool(identical(a, b)),
            "!==" => Value::Bool(!identical(a, b)),
            "==" => Value::Bool(compare(a, b) == Ordering::Equal),
            "!=" => Value::Bool(compare(a, b) != Ordering::Equal),
            "<=>" => Value::Int(match compare(a, b) {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            }),
            "<" => Value::Bool(compare(a, b) == Ordering::Less),
            "<=" => Value::Bool(compare(a, b) != Ordering::Greater),
            // zend compiles `>`/`>=` as IS_SMALLER(_OR_EQUAL) with the
            // operands swapped — the RHS is the compare's protected
            // left operand for the cyclic depth check.
            ">" => Value::Bool(compare(b, a) == Ordering::Less),
            ">=" => Value::Bool(compare(b, a) != Ordering::Greater),
            _ => unreachable!(),
        };
        // Notices queued inside the compare (object→number casts)
        // already fired — emit them before a cyclic depth throw.
        self.emit_cmp_notices()?;
        if crate::value::cmp_depth_err() {
            return self.fail(PhpError::uncaught(
                "Error",
                "Nesting level too deep - recursive dependency?",
                self.cur_line,
            ));
        }
        Ok(v)
    }

    /// Arithmetic / bitwise with PHP numeric-string coercion.
    fn arith(&mut self, op: &str, l: Value, r: Value) -> Result<Value, PhpError> {
        match op {
            "&" | "|" | "^" => {
                if let (Value::Str(a), Value::Str(b)) = (&l, &r) {
                    return Ok(Value::bytes(bitwise_str(op, a, b)));
                }
                let li = match self.bit_operand(op, &l, &r) {
                    Ok(i) => i,
                    Err(e) => return self.fail(e),
                };
                let ri = match self.bit_operand(op, &r, &l) {
                    Ok(i) => i,
                    Err(e) => return self.fail(e),
                };
                return Ok(Value::Int(match op {
                    "&" => li & ri,
                    "|" => li | ri,
                    _ => li ^ ri,
                }));
            }
            // PHP: shift < 0 → ArithmeticError; >= 64 → 0.
            "<<" | ">>" => {
                // PHP checks operand types left-to-right before shifting.
                let v = match self.bit_operand(op, &l, &r) {
                    Ok(i) => i,
                    Err(e) => return self.fail(e),
                };
                let s = match self.bit_operand(op, &r, &l) {
                    Ok(i) => i,
                    Err(e) => return self.fail(e),
                };
                if s < 0 {
                    return self.fail(PhpError::uncaught(
                        "ArithmeticError",
                        "Bit shift by negative number",
                        0,
                    ));
                }
                if s >= 64 {
                    return Ok(Value::Int(if op == "<<" { 0 } else { v >> 63 }));
                }
                return Ok(Value::Int(if op == "<<" {
                    v.wrapping_shl(s as u32)
                } else {
                    v.wrapping_shr(s as u32)
                }));
            }
            _ => {}
        }

        // `array + array` is PHP's union operator: lhs keys win and rhs
        // supplies only missing keys (not arithmetic — arrays never
        // reach the numeric path).
        if op == "+" {
            if let (Value::Array(a), Value::Array(b)) = (&l, &r) {
                let mut out = a.borrow().clone();
                for (k, c) in b.borrow().entries.iter() {
                    if out.get_cell(k).is_none() {
                        out.set(k.clone(), c.borrow().clone());
                    }
                }
                return Ok(Value::Array(Rc::new(RefCell::new(out))));
            }
        }

        let (ln, warn_l) = self.num(&l);
        let (rn, warn_r) = self.num(&r);
        if warn_l {
            self.warn("A non-numeric value encountered")?;
        }
        if warn_r {
            self.warn("A non-numeric value encountered")?;
        }
        let (ln, rn) = match (ln, rn) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                return self.fail(PhpError::uncaught(
                    "TypeError",
                    format!(
                        "Unsupported operand types: {} {} {}",
                        l.type_name(),
                        op,
                        r.type_name()
                    ),
                    0,
                ))
            }
        };
        Ok(match op {
            "+" => num_bin(ln, rn, i64::checked_add, |a, b| a + b),
            "-" => num_bin(ln, rn, i64::checked_sub, |a, b| a - b),
            "*" => num_bin(ln, rn, i64::checked_mul, |a, b| a * b),
            "/" => {
                if rn.to_float() == 0.0 {
                    return self.fail(PhpError::uncaught(
                        "DivisionByZeroError",
                        "Division by zero",
                        0,
                    ));
                }
                match (ln, rn) {
                    (Num::I(a), Num::I(b)) if b != 0 => match a.checked_div(b) {
                        Some(q) if q * b == a => Value::Int(q),
                        _ => Value::Float(a as f64 / b as f64),
                    },
                    (a, b) => Value::Float(a.to_float() / b.to_float()),
                }
            }
            "%" => {
                let a = match ln {
                    Num::I(i) => i,
                    Num::F(f) => {
                        let mut werr = None;
                        let i = coerce_float(f, |m| {
                            if let Err(e) = self.warn(m) {
                                werr = Some(e);
                            }
                        });
                        if let Some(e) = werr {
                            return Err(e);
                        }
                        i
                    }
                };
                let b = match rn {
                    Num::I(i) => i,
                    Num::F(f) => {
                        let mut werr = None;
                        let i = coerce_float(f, |m| {
                            if let Err(e) = self.warn(m) {
                                werr = Some(e);
                            }
                        });
                        if let Some(e) = werr {
                            return Err(e);
                        }
                        i
                    }
                };
                if b == 0 {
                    return self.fail(PhpError::uncaught(
                        "DivisionByZeroError",
                        "Modulo by zero",
                        0,
                    ));
                }
                // i64::MIN % -1 is 0 in PHP (no overflow panic).
                Value::Int(a.wrapping_rem(b))
            }
            "**" => match (&ln, &rn) {
                // int ** int (exp >= 0) stays int when it fits
                // (sebastian/diff's footprint calc feeds an int return
                // type under strict_types).
                (Num::I(bi), Num::I(ei)) if *ei >= 0 => match bi.checked_pow(*ei as u32) {
                    Some(v) => Value::Int(v),
                    None => Value::Float(ln.to_float().powf(rn.to_float())),
                },
                _ => Value::Float(ln.to_float().powf(rn.to_float())),
            },
            _ => return self.fail(PhpError::fatal(format!("unsupported operator {}", op), 0)),
        })
    }

    /// Coerce a value to a number per PHP rules.
    /// Returns (numeric, "leading-numeric warning needed").
    fn num(&mut self, v: &Value) -> (Option<Num>, bool) {
        match v {
            Value::Int(i) => (Some(Num::I(*i)), false),
            Value::Float(f) => (Some(Num::F(*f)), false),
            Value::Bool(b) => (Some(Num::I(*b as i64)), false),
            Value::Null => (Some(Num::I(0)), false),
            Value::Str(s) => match numeric(s) {
                Numeric::Int(i) => (Some(Num::I(i)), false),
                Numeric::Float(f) => (Some(Num::F(f)), false),
                Numeric::Leading(f, is_int) => {
                    if is_int {
                        (Some(Num::I(f as i64)), true)
                    } else {
                        (Some(Num::F(f)), true)
                    }
                }
                Numeric::NonNumeric => (None, false),
            },
            _ => (None, false),
        }
    }

    /// Operand coercion for integer-only binary ops (`& | ^ << >>`):
    /// leading-numeric strings warn "A non-numeric value encountered";
    /// non-numeric strings raise a catchable TypeError.
    fn bit_operand(&mut self, op: &str, v: &Value, other: &Value) -> Result<i64, PhpError> {
        if let Value::Str(s) = v {
            match numeric(s) {
                Numeric::Int(i) => return Ok(i),
                Numeric::Float(f) => {
                    let mut werr = None;
                    let i = coerce_float(f, |m| {
                        if let Err(e) = self.warn(m) {
                            werr = Some(e);
                        }
                    });
                    if let Some(e) = werr {
                        return Err(e);
                    }
                    return Ok(i);
                }
                Numeric::Leading(f, _) => {
                    self.warn("A non-numeric value encountered")?;
                    let mut werr = None;
                    let i = coerce_float(f, |m| {
                        if let Err(e) = self.warn(m) {
                            werr = Some(e);
                        }
                    });
                    if let Some(e) = werr {
                        return Err(e);
                    }
                    return Ok(i);
                }
                Numeric::NonNumeric => {
                    return Err(PhpError::uncaught(
                        "TypeError",
                        format!(
                            "Unsupported operand types: {} {} {}",
                            v.type_name(),
                            op,
                            other.type_name()
                        ),
                        0,
                    ));
                }
            }
        }
        Ok(self.coerce_int(v))
    }

    /// Int coercion for integer-only contexts (bitwise ops, shifts, casts).
    /// Out-of-range floats emit PHP's "not representable as an int" warning;
    /// conversion wraps modulo 2^64 (zend_dtoi64), NaN/INF → 0.
    pub(in crate::interp) fn coerce_int(&mut self, v: &Value) -> i64 {
        let f = match v {
            Value::Float(f) => *f,
            Value::Str(s) => match numeric(s) {
                Numeric::Float(f) | Numeric::Leading(f, _) => f,
                _ => return v.to_int(),
            },
            _ => return v.to_int(),
        };
        coerce_float(f, |msg| {
            let _ = self.warn(msg);
        })
    }

    /// `(type)expr` cast.
    fn cast(&mut self, kind: CastKind, v: Value) -> Result<Value, PhpError> {
        Ok(match kind {
            CastKind::Int => Value::Int(self.coerce_int(&v)),
            CastKind::Float => Value::Float(v.to_float()),
            CastKind::Bool => Value::Bool(v.is_truthy()),
            CastKind::Unset => Value::Null,
            CastKind::String => match &v {
                // Identity on strings — conv_str's UTF-8 decode would
                // mangle non-UTF-8 bytes.
                Value::Str(_) => v,
                _ => Value::str(self.conv_str(&v)?),
            },
            CastKind::Array => match v {
                Value::Array(_) => v,
                Value::Null => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
                // `(array) $obj` exposes raw slots under their (possibly
                // mangled) keys — hooks are not run (dump.phpt).
                Value::Object(o) => {
                    let mut a = PhpArray::new();
                    // spl array-objects cast their STORAGE hash, not
                    // the object's own props (zend get_properties_for).
                    if matches!(
                        o.borrow().internal,
                        Some(crate::value::ObjectInternal::ArrayIter { .. })
                    ) {
                        let arr = self.ao_arr(&o);
                        for (k, c) in arr.borrow().iter() {
                            if let ArrKey::Tomb = k {
                                continue;
                            }
                            a.set(k.clone(), c.borrow().clone());
                        }
                        return Ok(Value::Array(Rc::new(RefCell::new(a))));
                    }
                    let ob = o.borrow();
                    for n in &ob.prop_order {
                        if let Some(c) = ob.props.get(n) {
                            // Int-keyed buckets decode to int keys like
                            // zend's property-HT int slots.
                            let k = match crate::value::int_prop_index(n) {
                                Some(i) => ArrKey::Int(i),
                                None => ArrKey::Str(n.clone().into()),
                            };
                            a.set(k, c.borrow().clone());
                        }
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
                other => {
                    let mut a = PhpArray::new();
                    a.push(other);
                    Value::Array(Rc::new(RefCell::new(a)))
                }
            },
            CastKind::Object => match &v {
                Value::Object(_) => v,
                _ => {
                    let mut props = HashMap::new();
                    let mut order = Vec::new();
                    match &v {
                        Value::Array(a) => {
                            for (k, c) in a.borrow().iter() {
                                let name = match k {
                                    ArrKey::Int(i) => i.to_string(),
                                    ArrKey::Str(s) => s.to_string(),
                                    ArrKey::Tomb => continue,
                                };
                                props.insert(name.clone(), cell(c.borrow().clone()));
                                order.push(name);
                            }
                        }
                        Value::Null => {}
                        _ => {
                            props.insert("scalar".into(), cell(v.clone()));
                            order.push("scalar".into());
                        }
                    }
                    let cls = self
                        .classes
                        .get("stdclass")
                        .cloned()
                        .expect("stdClass registered");
                    Value::Object(self.alloc_obj(PhpObject {
                        class: cls,
                        props,
                        prop_order: order,
                        id: 0,
                        internal: None,
                        unset_props: std::collections::HashSet::new(),
                    }))
                }
            },
        })
    }
}

enum Num {
    I(i64),
    F(f64),
}

impl Num {
    fn to_float(&self) -> f64 {
        match self {
            Num::I(i) => *i as f64,
            Num::F(f) => *f,
        }
    }
}

fn num_bin(a: Num, b: Num, fi: fn(i64, i64) -> Option<i64>, ff: fn(f64, f64) -> f64) -> Value {
    match (a, b) {
        (Num::I(x), Num::I(y)) => match fi(x, y) {
            // Integer overflow promotes to float (multiply_basiclong_64bit.phpt).
            Some(r) => Value::Int(r),
            None => Value::Float(ff(x as f64, y as f64)),
        },
        (x, y) => Value::Float(ff(x.to_float(), y.to_float())),
    }
}

/// Perl-style string increment ("a"→"b", "z"→"aa", "A9"→"B0").
fn perl_inc(s: &[u8]) -> Vec<u8> {
    let mut bytes = s.to_vec();
    let mut i = bytes.len();
    let mut carry = true;
    while carry && i > 0 {
        i -= 1;
        let c = bytes[i];
        let next = match c {
            b'a'..=b'y' | b'A'..=b'Y' => c + 1,
            b'z' => {
                bytes[i] = b'a';
                continue;
            }
            b'Z' => {
                bytes[i] = b'A';
                continue;
            }
            b'0'..=b'8' => c + 1,
            b'9' => {
                bytes[i] = b'0';
                continue;
            }
            _ => {
                carry = false;
                continue;
            }
        };
        bytes[i] = next;
        carry = false;
    }
    if carry {
        // Determine the carried character class from the first char.
        let first = bytes.first().copied().unwrap_or(b'a');
        let c = if first.is_ascii_uppercase() {
            b'A'
        } else if first.is_ascii_lowercase() {
            b'a'
        } else {
            b'1'
        };
        bytes.insert(0, c);
    }
    bytes
}

/// PHP float→int conversion (zend_dtoi64): warns on out-of-range,
/// wraps modulo 2^64; NaN/INF → 0.
fn coerce_float(f: f64, mut warn: impl FnMut(&str)) -> i64 {
    const MOD: f64 = 18446744073709551616.0; // 2^64
    if !f.is_finite() {
        warn(&format!(
            "The float {} is not representable as an int, cast occurred",
            format_float_repr(f)
        ));
        return 0;
    }
    if f >= i64::MAX as f64 || f < i64::MIN as f64 {
        warn(&format!(
            "The float {} is not representable as an int, cast occurred",
            format_float_repr(f)
        ));
        let m = f % MOD;
        let u = if m < 0.0 { m + MOD } else { m };
        return u as u64 as i64;
    }
    f as i64
}

fn bitwise_str(op: &str, a: &[u8], b: &[u8]) -> Vec<u8> {
    // `|` pads the shorter operand with NUL; `&`/`^` truncate to min length.
    let (x, y) = (a, b);
    let n = if op == "|" {
        x.len().max(y.len())
    } else {
        x.len().min(y.len())
    };
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let xi = x.get(i).copied().unwrap_or(0);
        let yi = y.get(i).copied().unwrap_or(0);
        out.push(match op {
            "&" => xi & yi,
            "|" => xi | yi,
            _ => xi ^ yi,
        });
    }
    out
}

/// `static` declarations anywhere in a body, with their default exprs
/// and the `static` keyword's line — nested function/class bodies
/// declare their own.
fn closure_static_vars(stmts: &[Stmt], out: &mut Vec<(String, Option<Expr>, usize)>) {
    use crate::ast::Stmt;
    for st in stmts {
        match st {
            Stmt::Static { vars, .. } => {
                for (n, d, vl) in vars {
                    if !out.iter().any(|(x, ..)| x == n) {
                        out.push((n.clone(), d.clone(), *vl));
                    }
                }
            }
            Stmt::Block(b) => closure_static_vars(b, out),
            Stmt::If { then, else_, .. } => {
                closure_static_vars(then, out);
                closure_static_vars(else_, out);
            }
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::For { body, .. }
            | Stmt::Foreach { body, .. } => closure_static_vars(body, out),
            Stmt::Switch { cases, .. } => {
                for (_, b) in cases {
                    closure_static_vars(b, out);
                }
            }
            Stmt::Try {
                body,
                catches,
                finally,
            } => {
                closure_static_vars(body, out);
                for c in catches {
                    closure_static_vars(&c.body, out);
                }
                if let Some(f) = finally {
                    closure_static_vars(f, out);
                }
            }
            _ => {}
        }
    }
}

/// Compile-time bindable `static` initializer: literals and ops on
/// literals only. Zend resolves user consts/`new`/calls when the
/// `static` statement runs, not at closure creation (probe_sv3), so
/// those stay NULL in the seeded table — except engine consts
/// (PHP_VERSION, ...), which Zend binds at creation (probe_sv_engine).
fn literal_static_init(e: &Expr, engine: &std::collections::HashSet<String>) -> bool {
    match e {
        Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Float(_)
        | Expr::Str(_)
        | Expr::MagicConst(_) => true,
        Expr::Const(n) => engine.contains(n.trim_start_matches('\\')),
        Expr::ArrayLit(items) => items.iter().all(|(k, v)| {
            k.as_ref()
                .map(|k| literal_static_init(k, engine))
                .unwrap_or(true)
                && literal_static_init(v, engine)
        }),
        Expr::Interp(parts) => parts
            .iter()
            .all(|p| matches!(p, crate::lexer::StringPart::Lit(_))),
        Expr::Paren(inner) | Expr::ByRef(inner) | Expr::Unary { e: inner, .. } => {
            literal_static_init(inner, engine)
        }
        Expr::Cast { e: inner, .. } => literal_static_init(inner, engine),
        Expr::Binary { l, r, .. } => {
            literal_static_init(l, engine) && literal_static_init(r, engine)
        }
        Expr::Ternary { c, t, f, .. } => {
            literal_static_init(c, engine)
                && t.as_ref()
                    .map(|t| literal_static_init(t, engine))
                    .unwrap_or(true)
                && literal_static_init(f, engine)
        }
        _ => false,
    }
}

/// str_offset_write's outcome — the byte stored, or nothing when the
/// 'Illegal string offset' warning fires (zend leaves the write out).
enum OffWrite {
    Stored(u8),
    Skipped,
}
