//! Generators: yield detection, `GenState` batch-replay start,
//! `send`/`yield from` plumbing and the SPL iterator method bridge.

use super::util::*;
use super::*;

impl<'a> Interp<'a> {
    // ----- generators -----

    /// Whether a function body yields — scanning skips nested closures
    /// and function decls (each is its own generator context).
    pub(in crate::interp) fn decl_contains_yield(stmts: &[Stmt]) -> bool {
        stmts.iter().any(|s| Self::stmt_yield_kind(s).is_some())
    }

    /// The yield token kind (`"yield"` or `"yield from"`) a statement's
    /// own expressions carry outside any nested generator context.
    fn stmt_yield_kind(s: &Stmt) -> Option<&'static str> {
        match s {
            Stmt::Expr(e) => Self::expr_yield_kind(e),
            Stmt::Echo(es) => es.iter().find_map(Self::expr_yield_kind),
            Stmt::Return(Some(e)) => Self::expr_yield_kind(e),
            Stmt::Block(b) => Self::block_yield_kind(b),
            Stmt::If { cond, then, else_ } => Self::expr_yield_kind(cond)
                .or_else(|| Self::block_yield_kind(then))
                .or_else(|| Self::block_yield_kind(else_)),
            Stmt::While { cond, body } | Stmt::DoWhile { body, cond } => {
                Self::expr_yield_kind(cond).or_else(|| Self::block_yield_kind(body))
            }
            Stmt::For {
                init,
                cond,
                inc,
                body,
            } => init
                .iter()
                .chain(cond.iter())
                .chain(inc.iter())
                .find_map(Self::expr_yield_kind)
                .or_else(|| Self::block_yield_kind(body)),
            Stmt::Foreach { arr, val, body, .. } => Self::expr_yield_kind(arr)
                .or_else(|| match val {
                    ForeachTarget::Lvalue(e) | ForeachTarget::ByRef(e) => Self::expr_yield_kind(e),
                    _ => None,
                })
                .or_else(|| Self::block_yield_kind(body)),
            Stmt::Switch { cond, cases } => Self::expr_yield_kind(cond).or_else(|| {
                cases.iter().find_map(|(c, b)| {
                    c.as_ref()
                        .and_then(Self::expr_yield_kind)
                        .or_else(|| Self::block_yield_kind(b))
                })
            }),
            Stmt::Try {
                body,
                catches,
                finally,
            } => Self::block_yield_kind(body)
                .or_else(|| catches.iter().find_map(|c| Self::block_yield_kind(&c.body)))
                .or_else(|| finally.as_ref().and_then(|b| Self::block_yield_kind(b))),
            Stmt::Static { vars, .. } => vars
                .iter()
                .find_map(|(_, e, _)| e.as_ref().and_then(Self::expr_yield_kind)),
            Stmt::Unset(v) | Stmt::Global(v) => v.iter().find_map(Self::expr_yield_kind),
            Stmt::ConstDecl(v) => v.iter().find_map(|(_, e)| Self::expr_yield_kind(e)),
            Stmt::Declare { value, .. } => Self::expr_yield_kind(value),
            // A nested `function` decl is its own generator context
            // (its yields don't make the outer fn a generator).
            Stmt::Function(_) | Stmt::Class(_) => None,
            _ => None,
        }
    }

    fn block_yield_kind(stmts: &[Stmt]) -> Option<&'static str> {
        stmts.iter().find_map(Self::stmt_yield_kind)
    }

    /// Compile gate: a separately compiled unit's top-level
    /// `yield`/`yield from` is a fatal in Zend — include()/eval() units
    /// compile with their own function context, so a top-level yield in
    /// included code is invalid even when the includer is itself a
    /// generator body (it must not feed the outer gen's stream).
    pub(in crate::interp) fn yield_gate(stmts: &[Stmt]) -> Result<(), PhpError> {
        let mut line = 0;
        if let Some((l, kind)) = Self::yield_gate_scan(stmts, &mut line) {
            return Err(PhpError::compile_fatal(
                format!(
                    "The \"{}\" expression can only be used inside a function",
                    kind
                ),
                l,
            ));
        }
        Ok(())
    }

    /// First offending top-level yield's (line, kind). Stmt::Line
    /// markers track the scan position so a yield nested in a block
    /// reports its own line rather than the enclosing statement's.
    fn yield_gate_scan(stmts: &[Stmt], line: &mut usize) -> Option<(usize, &'static str)> {
        for s in stmts {
            match s {
                Stmt::Line(n) => *line = *n,
                // Declared functions/methods/closures are their own
                // generator contexts — their yields stay legal.
                Stmt::Function(_) | Stmt::Class(_) => continue,
                _ => {
                    let stmt_line = *line;
                    // Descend statement-level blocks first so an inner
                    // yield reports its own Line marker.
                    let bodies: Vec<&[Stmt]> = match s {
                        Stmt::Block(b) => vec![b.as_slice()],
                        Stmt::If { then, else_, .. } => {
                            vec![then.as_slice(), else_.as_slice()]
                        }
                        Stmt::While { body, .. }
                        | Stmt::DoWhile { body, .. }
                        | Stmt::For { body, .. }
                        | Stmt::Foreach { body, .. } => vec![body.as_slice()],
                        Stmt::Switch { cases, .. } => {
                            cases.iter().map(|(_, b)| b.as_slice()).collect()
                        }
                        Stmt::Try {
                            body,
                            catches,
                            finally,
                        } => {
                            let mut v: Vec<&[Stmt]> = vec![body.as_slice()];
                            v.extend(catches.iter().map(|c| c.body.as_slice()));
                            v.extend(finally.as_ref().map(|f| f.as_slice()));
                            v
                        }
                        _ => Vec::new(),
                    };
                    for b in bodies {
                        if let Some(hit) = Self::yield_gate_scan(b, line) {
                            return Some(hit);
                        }
                    }
                    if let Some(kind) = Self::stmt_yield_kind(s) {
                        return Some((stmt_line, kind));
                    }
                }
            }
        }
        None
    }

    /// The yield token kind an expression carries outside any nested
    /// generator context — `Some("yield")`/`Some("yield from")`.
    fn expr_yield_kind(e: &Expr) -> Option<&'static str> {
        match e {
            Expr::Yield { .. } => Some("yield"),
            Expr::YieldFrom(_) => Some("yield from"),
            // Nested closures/arrow fns are their own generator context.
            Expr::Closure(_) | Expr::AnonClass(_) => None,
            Expr::Assign { target, value, .. } => {
                Self::expr_yield_kind(target).or_else(|| Self::expr_yield_kind(value))
            }
            Expr::Binary { l, r, .. } => {
                Self::expr_yield_kind(l).or_else(|| Self::expr_yield_kind(r))
            }
            Expr::Unary { e, .. }
            | Expr::Clone(e)
            | Expr::ByRef(e)
            | Expr::PreInc(e)
            | Expr::PreDec(e)
            | Expr::PostInc(e)
            | Expr::PostDec(e)
            | Expr::Empty(e)
            | Expr::Print(e)
            | Expr::VarVar(e)
            | Expr::Paren(e)
            | Expr::Fcc(e)
            | Expr::Unpack(e)
            | Expr::Cast { e, .. }
            | Expr::Throw(e)
            | Expr::Include { e, .. } => Self::expr_yield_kind(e),
            Expr::Ternary { c, t, f } => Self::expr_yield_kind(c)
                .or_else(|| t.as_ref().and_then(|t| Self::expr_yield_kind(t)))
                .or_else(|| Self::expr_yield_kind(f)),
            Expr::Call { name, args } => {
                Self::expr_yield_kind(name).or_else(|| args.iter().find_map(Self::expr_yield_kind))
            }
            Expr::MethodCall {
                obj, name, args, ..
            } => Self::expr_yield_kind(obj)
                .or_else(|| match name {
                    PropName::Expr(e) => Self::expr_yield_kind(e),
                    _ => None,
                })
                .or_else(|| args.iter().find_map(Self::expr_yield_kind)),
            Expr::StaticCall { class, args, .. } => {
                Self::expr_yield_kind(class).or_else(|| args.iter().find_map(Self::expr_yield_kind))
            }
            Expr::StaticCallDyn { class, name, args } => Self::expr_yield_kind(class)
                .or_else(|| Self::expr_yield_kind(name))
                .or_else(|| args.iter().find_map(Self::expr_yield_kind)),
            Expr::Index { e, i } => Self::expr_yield_kind(e)
                .or_else(|| i.as_ref().and_then(|i| Self::expr_yield_kind(i))),
            Expr::Prop { obj, name, .. } => Self::expr_yield_kind(obj).or_else(|| match name {
                PropName::Expr(e) => Self::expr_yield_kind(e),
                _ => None,
            }),
            Expr::StaticProp { class, name } => {
                Self::expr_yield_kind(class).or_else(|| match name {
                    PropName::Expr(e) => Self::expr_yield_kind(e.as_ref()),
                    _ => None,
                })
            }
            Expr::Isset(v) => v.iter().find_map(Self::expr_yield_kind),
            Expr::List(v) => v.iter().flatten().find_map(|(k, e)| {
                k.as_ref()
                    .and_then(|k| Self::expr_yield_kind(k))
                    .or_else(|| Self::expr_yield_kind(e))
            }),
            Expr::Exit(Some(e)) => Self::expr_yield_kind(e),
            Expr::ArrayLit(items) => items.iter().find_map(|(k, v)| {
                k.as_ref()
                    .and_then(|k| Self::expr_yield_kind(k))
                    .or_else(|| Self::expr_yield_kind(v))
            }),
            Expr::Match { subject, arms } => Self::expr_yield_kind(subject).or_else(|| {
                arms.iter().find_map(|a| {
                    a.conds
                        .iter()
                        .find_map(Self::expr_yield_kind)
                        .or_else(|| Self::expr_yield_kind(&a.result))
                })
            }),
            Expr::New { class, args } => {
                Self::expr_yield_kind(class).or_else(|| args.iter().find_map(Self::expr_yield_kind))
            }
            Expr::ClassConst { class, .. } => Self::expr_yield_kind(class),
            Expr::Instanceof { obj, class } => {
                Self::expr_yield_kind(obj).or_else(|| Self::expr_yield_kind(class))
            }
            _ => None,
        }
    }

    /// Whether a closure body references `$this` (the zend
    /// uses-this-compile flag behind bindTo's unbind warning).
    /// Debug-format scan; nested `function`/`fn` decls bind their own
    /// $this so they are skipped.
    pub(in crate::interp) fn body_uses_this(stmts: &[crate::ast::Stmt]) -> bool {
        stmts.iter().any(|st| {
            !matches!(st, crate::ast::Stmt::Function(_))
                && format!("{:?}", st).contains(r#"Var("this")"#)
        })
    }

    /// Build the deferred Generator object for a yielding call.
    pub(in crate::interp) fn make_generator(&mut self, setup: GenSetup) -> Rc<RefCell<PhpObject>> {
        let GenSetup::Invoke { decl, .. } = &setup;
        let by_ref = decl.by_ref;
        let fin_q = Rc::new(RefCell::new(crate::value::GenFinData {
            fn_name: decl.name.clone(),
            file: decl.file.clone(),
            ..Default::default()
        }));
        let state = Rc::new(RefCell::new(GenState {
            setup,
            items: Vec::new(),
            pos: 0,
            started: false,
            finished: false,
            return_val: Value::Null,
            by_ref,
            auto_key: 0,
            sends: Vec::new(),
            throws: Vec::new(),
            delegate_gens: Vec::new(),
            injected_throwable: None,
            pending_out: Vec::new(),
            fin_q,
            deferred_err: None,
            dead: false,
            closed: false,
            running: false,
            live: None,
            suppress_prefix: false,
        }));
        // GC-time finally replay: the weak dies with the object —
        // unset()/overwrite then replays fin_q; unit end replays it
        // for gens still suspended (request shutdown).
        self.live_gens
            .push((Rc::downgrade(&state), state.borrow().fin_q.clone()));
        let cls = self
            .classes
            .get("generator")
            .cloned()
            .expect("Generator class registered");
        self.alloc_obj(PhpObject {
            class: cls,
            props: HashMap::new(),
            prop_order: Vec::new(),
            id: 0,
            internal: Some(ObjectInternal::Generator(state)),
            unset_props: std::collections::HashSet::new(),
        })
    }

    /// Run a not-yet-started generator body to completion, collecting
    /// every yield into `state.items`. PHP defers body execution to the
    /// first iterator access, which this mirrors (eager collection on
    /// first use).
    fn gen_start(&mut self, state: &Rc<RefCell<GenState>>) -> Result<(), PhpError> {
        let (setup, sends) = {
            let mut st = state.borrow_mut();
            if st.started {
                return Ok(());
            }
            st.started = true;
            st.running = true;
            let setup = match &st.setup {
                GenSetup::Invoke {
                    decl,
                    this_obj,
                    scope_class,
                    decl_class,
                    called_class,
                    captures,
                    closure_rc,
                    ..
                } => (
                    decl.clone(),
                    this_obj.clone(),
                    scope_class.clone(),
                    decl_class.clone(),
                    called_class.clone(),
                    captures.clone(),
                    closure_rc.clone(),
                ),
            };
            (setup, st.sends.clone())
        };
        let (decl, this_obj, scope_class, decl_class, called_class, captures, closure_rc) = setup;

        // Replay keeps the original arg cells (zend re-runs the same
        // frame): taking them once left the send()-triggered re-run
        // with an empty arg list and a fatals on required params.
        let args = {
            let st = state.borrow();
            match &st.setup {
                GenSetup::Invoke { args, .. } => args.clone(),
            }
        };
        let throws = {
            let st = state.borrow();
            st.throws.clone()
        };
        let items = Rc::new(RefCell::new(Vec::new()));
        let saved_sink = self.gen_sink.replace(items.clone());
        // Expose the in-flight collection so consumer read ops
        // reaching the object mid-run (valid/current/key) see the
        // yields produced so far.
        state.borrow_mut().live = Some(items.clone());
        let saved_sends = std::mem::replace(&mut self.gen_sends, sends.into_iter().collect());
        let saved_throws = std::mem::replace(&mut self.gen_throws, throws.into_iter().collect());
        let saved_auto = std::mem::replace(&mut self.gen_auto, 0);
        let saved_run = self.gen_run_state.replace(state.clone());
        let saved_fin_q = self.gen_fin_q.replace(state.borrow().fin_q.clone());
        let saved_fin_depth = std::mem::replace(&mut self.gen_fin_depth, 0);
        // Nested gen_start (a gen inside a running gen's body):
        // save/restore the outer run's slots like the other gen_*s —
        // an unconditional clear dropped an outer gen's pending
        // fatal/raise context.
        let saved_pf = self.gen_pending_fatal.take();
        let saved_ctx = std::mem::take(&mut self.gen_raise_ctx);
        let saved_fin_err = std::mem::replace(&mut self.gen_fin_err, false);
        // gen_replay_horizon is NOT saved: a send() re-run installs it
        // around this call specifically so the re-run's prefix bytes
        // are suppressed.
        let saved_cbase = self.gen_collect_base.take();
        let saved_yff = self.gen_yield_from_fin.take();
        // The body frame lands at call_trace[trace_base] — everything
        // above it at death time (eval()/include() pseudo-frames,
        // userland calls) is the suspended raise context the deferred
        // render prepends to the resume stack.
        let trace_base = self.call_trace.len();
        // Closure-generator captures bind as extra frame vars.
        if !captures.is_empty() {
            self.pending_gen_captures = captures;
        }
        let r = self.invoke_fn_run(
            &decl,
            args,
            this_obj,
            scope_class,
            decl_class,
            called_class,
            closure_rc,
            None,
        );
        // Output buffers the body opened past a yield leave the real
        // stack while it is suspended — Zend's buffers are global, so
        // they rematerialize as the consumer's cursor passes each
        // open tag.
        self.ob_suspend(&state.borrow().fin_q);
        self.gen_sink = saved_sink;
        self.gen_sends = saved_sends;
        self.gen_throws = saved_throws;
        self.gen_auto = saved_auto;
        self.gen_run_state = saved_run;
        self.gen_fin_q = saved_fin_q;
        self.gen_fin_depth = saved_fin_depth;
        let run_fin_err = self.gen_fin_err;
        self.gen_fin_err = saved_fin_err;
        self.gen_collect_base = saved_cbase;
        self.gen_yield_from_fin = saved_yff;
        let collected = std::mem::take(&mut *items.borrow_mut());
        {
            let mut st = state.borrow_mut();
            {
                let mut fin = st.fin_q.borrow_mut();
                fin.total = collected.len();
                // The body ran to its end — every echo it can ever
                // produce is journaled; reads may confirm all of it.
                fin.finished = true;
            }
            st.items = collected;
            st.finished = true;
            st.running = false;
            st.live = None;
        }
        match r {
            Ok(rv) => {
                self.gen_pending_fatal = saved_pf;
                self.gen_raise_ctx = saved_ctx;
                state.borrow_mut().return_val = rv;
                Ok(())
            }
            Err(e) => {
                // The body died mid-run: Zend's lazy generator raises
                // that error at the consumer's NEXT resume call —
                // after the bytes it already echoed between yields.
                // Hold it on the state; collected items stay
                // consumable until the consumer asks for the dead
                // resume (gen_raise_deferred). A fatal stashed by
                // err_flow inside the body is the real error behind
                // the exit:N sentinel that propagated out.
                let e = self.gen_pending_fatal.take().unwrap_or(e);
                self.gen_pending_fatal = saved_pf;
                // Throw deaths need their throwable here — the ambient
                // pending_exception slot gets clobbered by consumer
                // calls between death and resume.
                let throwable = if e.kind == crate::error::ErrorKind::Throw {
                    self.pending_exception.take()
                } else {
                    None
                };
                // call_trace is already unwound past the body frame —
                // the suspended raise context was snapshotted at the
                // last throw()/fail()/err_flow inside the body; an
                // error that bypassed them (a raw Err propagation)
                // falls back to whatever stack remains.
                if self.gen_raise_ctx.is_empty() {
                    self.gen_raise_ctx = self.call_trace.clone();
                }
                // Slice out everything at/below the gen's own frame.
                let raise_frames = self
                    .gen_raise_ctx
                    .get(trace_base + 1..)
                    .unwrap_or_default()
                    .to_vec();
                self.gen_raise_ctx = saved_ctx;
                let mut st = state.borrow_mut();
                // A death that happened inside a `finally` region
                // surfaces at the gen's destruction instead of the
                // deferred resume — mirror it into the shared
                // journal so a dead weak still raises it.
                if run_fin_err {
                    st.fin_q.borrow_mut().fin_err = Some((e.clone(), throwable.clone()));
                }
                st.deferred_err = Some((e, throwable, raise_frames, run_fin_err));
                st.dead = true;
                st.fin_q.borrow_mut().finished = true;
                Ok(())
            }
        }
    }

    /// SplFileInfo / DirectoryIterator native methods. SplFileInfo
    /// state is a `\0fi\0path` prop; DirectoryIterator additionally
    /// carries a DirIter internal (sorted dir entries + cursor).
    pub(in crate::interp) fn spl_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        args: &CallArgs,
    ) -> Result<Option<Value>, PhpError> {
        let lname = name.to_lowercase();
        match lname.as_str() {
            "__construct" => {
                let path_v = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let path = self.conv_str(&path_v)?.to_string();
                let flags = args.get(1).map(|c| c.borrow().to_int()).unwrap_or(0);
                let is_iter = self.obj_is_a(obj, "directoryiterator");
                if is_iter {
                    let mut entries: Vec<String> = Vec::new();
                    match std::fs::read_dir(&path) {
                        Ok(rd) => {
                            for e in rd.flatten() {
                                let n = e.file_name().to_string_lossy().to_string();
                                if n == "." || n == ".." {
                                    continue;
                                }
                                entries.push(format!("{}/{}", path.trim_end_matches('/'), n));
                            }
                            entries.sort();
                        }
                        Err(_) => {
                            return self.fail::<Option<Value>>(PhpError::uncaught(
                                "UnexpectedValueException",
                                format!(
                                    "DirectoryIterator::__construct({}): failed to open dir",
                                    path
                                ),
                                0,
                            ));
                        }
                    }
                    let mut ob = obj.borrow_mut();
                    ob.props
                        .insert("\0fi\0path".into(), cell(Value::str(&path)));
                    ob.internal = Some(ObjectInternal::DirIter {
                        entries,
                        pos: 0,
                        flags,
                        sub_path: String::new(),
                    });
                } else {
                    obj.borrow_mut()
                        .props
                        .insert("\0fi\0path".into(), cell(Value::str(&path)));
                }
                Ok(Some(Value::Null))
            }
            "rewind" => {
                if let Some(ObjectInternal::DirIter { pos, .. }) = &mut obj.borrow_mut().internal {
                    *pos = 0;
                }
                Ok(Some(Value::Null))
            }
            "valid" => Ok(Some(Value::Bool(match &obj.borrow().internal {
                Some(ObjectInternal::DirIter { entries, pos, .. }) => *pos < entries.len(),
                _ => false,
            }))),
            "current" => {
                // PHP yields SplFileInfo instances for each entry;
                // FilesystemIterator flags can switch that to the path
                // string (CURRENT_AS_PATHNAME=32) or $this (CURRENT_AS_SELF=16).
                let cur = match &obj.borrow().internal {
                    Some(ObjectInternal::DirIter {
                        entries,
                        pos,
                        flags,
                        ..
                    }) if *pos < entries.len() => Some((entries[*pos].clone(), *flags)),
                    _ => None,
                };
                match cur {
                    Some((p, flags)) if flags & 240 == 32 => Ok(Some(Value::str(&p))),
                    Some((_, flags)) if flags & 240 == 16 => {
                        Ok(Some(Value::Object(Rc::clone(obj))))
                    }
                    Some((p, _)) => {
                        let v = self.instantiate("splfileinfo", &[])?;
                        if let Value::Object(o) = &v {
                            o.borrow_mut()
                                .props
                                .insert("\0fi\0path".into(), cell(Value::str(&p)));
                        }
                        Ok(Some(v))
                    }
                    None => Ok(Some(Value::Null)),
                }
            }
            "key" => Ok(Some(match &obj.borrow().internal {
                Some(ObjectInternal::DirIter {
                    entries,
                    pos,
                    flags,
                    ..
                }) if *flags & 3840 == 256 && *pos < entries.len() => {
                    // KEY_AS_FILENAME
                    Value::str(entries[*pos].rsplit('/').next().unwrap_or("").to_string())
                }
                Some(ObjectInternal::DirIter {
                    entries,
                    pos,
                    flags,
                    ..
                }) if *flags != 0 && *pos < entries.len() => {
                    // KEY_AS_PATHNAME (flagged iterators carry real paths)
                    Value::str(&entries[*pos])
                }
                Some(ObjectInternal::DirIter { pos, .. }) => Value::Int(*pos as i64),
                _ => Value::Null,
            })),
            "next" => {
                if let Some(ObjectInternal::DirIter { pos, .. }) = &mut obj.borrow_mut().internal {
                    *pos += 1;
                }
                Ok(Some(Value::Null))
            }
            // Dots are filtered out at construct time, so the current
            // entry is never `.`/`..`.
            "isdot" => Ok(Some(Value::Bool(false))),
            "getflags" => Ok(Some(match &obj.borrow().internal {
                Some(ObjectInternal::DirIter { flags, .. }) => Value::Int(*flags),
                _ => Value::Int(0),
            })),
            "setflags" => {
                let f = args.first().map(|c| c.borrow().to_int()).unwrap_or(0);
                if let Some(ObjectInternal::DirIter { flags, .. }) = &mut obj.borrow_mut().internal
                {
                    *flags = f;
                }
                Ok(Some(Value::Null))
            }
            "haschildren" => {
                let (entry, flags) = match &obj.borrow().internal {
                    Some(ObjectInternal::DirIter {
                        entries,
                        pos,
                        flags,
                        ..
                    }) if *pos < entries.len() => (entries[*pos].clone(), *flags),
                    _ => (String::new(), 0),
                };
                if entry.is_empty() {
                    return Ok(Some(Value::Bool(false)));
                }
                let allow_links = args
                    .first()
                    .map(|c| c.borrow().is_truthy())
                    .unwrap_or(false);
                let md = std::fs::metadata(&entry).ok();
                let ld = std::fs::symlink_metadata(&entry).ok();
                let is_link = ld.is_some_and(|m| m.file_type().is_symlink());
                let r = md.is_some_and(|m| m.is_dir())
                    && (allow_links || flags & 16384 != 0 || !is_link);
                Ok(Some(Value::Bool(r)))
            }
            "getchildren" => {
                let (entry, flags, sub, cls_name) = match &obj.borrow().internal {
                    Some(ObjectInternal::DirIter {
                        entries,
                        pos,
                        flags,
                        sub_path,
                    }) if *pos < entries.len() => (
                        entries[*pos].clone(),
                        *flags,
                        sub_path.clone(),
                        obj.borrow().class.name().to_string(),
                    ),
                    _ => {
                        return Ok(Some(Value::Null));
                    }
                };
                let fname = entry.rsplit('/').next().unwrap_or("").to_string();
                let child_sub = if sub.is_empty() {
                    fname.clone()
                } else {
                    format!("{}/{}", sub, fname)
                };
                let args = vec![cell(Value::str(&entry)), cell(Value::Int(flags))];
                let v = self.instantiate_class(&cls_name, args)?;
                if let Value::Object(o) = &v {
                    if let Some(ObjectInternal::DirIter { sub_path, .. }) =
                        &mut o.borrow_mut().internal
                    {
                        *sub_path = child_sub;
                    }
                }
                Ok(Some(v))
            }
            "getsubpath" => Ok(Some(match &obj.borrow().internal {
                Some(ObjectInternal::DirIter { sub_path, .. }) => Value::str(sub_path),
                _ => Value::str(""),
            })),
            "getsubpathname" => Ok(Some(match &obj.borrow().internal {
                Some(ObjectInternal::DirIter {
                    entries,
                    pos,
                    sub_path,
                    ..
                }) if *pos < entries.len() => {
                    let fname = entries[*pos].rsplit('/').next().unwrap_or("");
                    if sub_path.is_empty() {
                        Value::str(fname)
                    } else {
                        Value::str(format!("{}/{}", sub_path, fname))
                    }
                }
                _ => Value::str(""),
            })),
            _ => {
                let path_v = obj
                    .borrow()
                    .props
                    .get("\0fi\0path")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mut path = self.conv_str(&path_v)?.to_string();
                // DirectoryIterator accessors target the current entry,
                // not the iterated root path.
                let entry = match &obj.borrow().internal {
                    Some(ObjectInternal::DirIter { entries, pos, .. }) if *pos < entries.len() => {
                        Some(entries[*pos].clone())
                    }
                    _ => None,
                };
                if let Some(e) = entry {
                    path = e;
                }
                let base = path.rsplit('/').next().unwrap_or(&path).to_string();
                let md = std::fs::metadata(&path).ok();
                let v = match lname.as_str() {
                    "getfilename" => Value::str(&base),
                    "getbasename" => {
                        let suffix = args
                            .first()
                            .map(|c| c.borrow().clone())
                            .map(|v| self.conv_str(&v).map(|s| s.to_string()))
                            .transpose()?
                            .unwrap_or_default();
                        Value::str(
                            base.strip_suffix(&suffix)
                                .filter(|_| !suffix.is_empty())
                                .unwrap_or(&base),
                        )
                    }
                    "getpathname" => Value::str(&path),
                    "getpath" => Value::str(match path.rfind('/') {
                        Some(i) => &path[..i],
                        None => "",
                    }),
                    "getextension" => Value::str(
                        base.rsplit_once('.')
                            .filter(|(h, _)| !h.is_empty())
                            .map(|(_, e)| e)
                            .unwrap_or(""),
                    ),
                    "getrealpath" => match std::fs::canonicalize(&path) {
                        Ok(p) => Value::str(p.display().to_string()),
                        Err(_) => Value::Bool(false),
                    },
                    "isfile" => Value::Bool(md.as_ref().is_some_and(|m| m.is_file())),
                    "isdir" => Value::Bool(md.as_ref().is_some_and(|m| m.is_dir())),
                    "islink" => Value::Bool(
                        std::fs::symlink_metadata(&path)
                            .map(|m| m.file_type().is_symlink())
                            .unwrap_or(false),
                    ),
                    "isreadable" | "iswritable" | "isexecutable" => Value::Bool(md.is_some()),
                    "getsize" => md
                        .as_ref()
                        .map(|m| Value::Int(m.len() as i64))
                        .unwrap_or(Value::Bool(false)),
                    "getmtime" => md
                        .as_ref()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| Value::Int(d.as_secs() as i64))
                        .unwrap_or(Value::Bool(false)),
                    "gettype" => Value::str(if md.as_ref().is_some_and(|m| m.is_dir()) {
                        "dir"
                    } else {
                        "file"
                    }),
                    "__tostring" => Value::str(&path),
                    _ => return Ok(None),
                };
                Ok(Some(v))
            }
        }
    }

    /// Re-raise the body's terminal error at the consumer's resume
    /// call — Zend's lazy body dies inside `Generator->{m}()`, after
    /// the bytes the consumer already echoed between yields, so the
    /// stored error only fires once `pos` reaches the end of the
    /// collected items.
    fn gen_raise_deferred(
        &mut self,
        state: &Rc<RefCell<GenState>>,
        method: &str,
        args: &[crate::value::Cell],
    ) -> Result<(), PhpError> {
        let dead = {
            let st = state.borrow();
            st.pos >= st.items.len() && st.deferred_err.is_some()
        };
        if !dead {
            return Ok(());
        }
        // The body's tail output belongs to this final resume.
        self.gen_flush_out(state, usize::MAX);
        // The error unwind already ran the body's finally chains —
        // the queue drains with them so shutdown doesn't replay.
        {
            let st = state.borrow();
            let mut fin = st.fin_q.borrow_mut();
            fin.bytes.clear();
            fin.fin_err = None;
        }
        let (mut e, throwable, raise_frames, _) = state.borrow_mut().deferred_err.take().unwrap();
        // A consumer-injected throwable (Generator->throw()) keeps its
        // own trace when it escapes — Zend renders the trace captured
        // at `new`, not the gen's resume stack.
        let injected = {
            let st = state.borrow();
            matches!(
                (&st.injected_throwable, &throwable),
                (Some(Value::Object(a)), Some(Value::Object(b)))
                    if std::rc::Rc::ptr_eq(a, b)
            )
        };
        if e.kind == crate::error::ErrorKind::Throw {
            // Restore the throwable captured at death — consumer calls
            // since then may have overwritten the ambient slot.
            if throwable.is_some() {
                self.pending_exception = throwable;
            }
        }
        // Foreach-internal resume: the body dies under the iteration
        // machinery — its raise carries the gen's own call frame
        // (`FILE(call_line): g()`) under the consumer's stack, while a
        // userland `Generator->{m}()` resume renders the engine
        // stack instead.
        if self.iter_calls > 0 {
            let (fn_name, call_args) = {
                let st = state.borrow();
                match &st.setup {
                    GenSetup::Invoke { decl, args, .. } => {
                        let a = args
                            .cells
                            .iter()
                            .map(|c| crate::value::trace_arg(&c.borrow()))
                            .collect::<Vec<_>>()
                            .join(", ");
                        (decl.name.clone(), a)
                    }
                }
            };
            // Frames suspended between the throw site and the gen
            // body (autoloads, nested calls) sit deepest, then the
            // gen's resume frame — citing the consumer's current
            // site (the foreach header) with the gen's original
            // call args — under the consumer's stack.
            let bare = if raise_frames.last().is_some_and(crate::value::include_frame) {
                Some(0)
            } else {
                None
            };
            let mut frames: Vec<String> = raise_frames
                .iter()
                .rev()
                .enumerate()
                .map(|(i, f)| crate::value::trace_frame_str_at(f, Some(i) == bare))
                .collect();
            frames.push(format!(
                "{}({}): {}({})",
                self.diag_file(),
                self.cur_line,
                fn_name,
                call_args
            ));
            for fr in self.call_trace.iter().rev() {
                if crate::value::trace_frame_hidden(fr) {
                    continue;
                }
                frames.push(crate::value::trace_frame_str(fr));
            }
            if e.kind == crate::error::ErrorKind::Throw {
                if !injected {
                    self.rewrite_throwable_trace(&frames);
                }
            } else {
                e.trace = Some(frames);
            }
            return Err(e);
        }
        let mut frames = self.gen_resume_frames(state, method, args, self.gen_internal_resume == 0);
        if e.kind == crate::error::ErrorKind::Throw && !injected {
            // Frames suspended between the throw site and the gen body
            // — eval()/include() pseudo-frames and userland calls —
            // lead the resume stack in Zend's render.
            if !raise_frames.is_empty() {
                let bare = if raise_frames.last().is_some_and(crate::value::include_frame) {
                    Some(0)
                } else {
                    None
                };
                let prefix: Vec<String> = raise_frames
                    .iter()
                    .rev()
                    .enumerate()
                    .map(|(i, f)| crate::value::trace_frame_str_at(f, Some(i) == bare))
                    .collect();
                frames = prefix.into_iter().chain(frames).collect();
            }
            // The uncaught render reads the Throwable's own trace —
            // swap it for the resume stack.
            self.rewrite_throwable_trace(&frames);
        } else if e.kind != crate::error::ErrorKind::Throw {
            e.trace = Some(frames);
        }
        Err(e)
    }

    /// Release the gen's suspended frame when the consumer's cursor
    /// proved the body done (`pos >= items`): Zend frees execute_data
    /// inside the resume that exhausts the stream, so the CVs' decref
    /// — locals' `__destruct`, held gens' own teardown — lands in
    /// this call's output window.
    fn gen_release_exhausted(&mut self, state: &Rc<RefCell<GenState>>) -> Result<(), PhpError> {
        let done = {
            let st = state.borrow();
            st.pos >= st.items.len()
        };
        if !done {
            return Ok(());
        }
        let cells = {
            let st = state.borrow();
            let mut fin = st.fin_q.borrow_mut();
            std::mem::take(&mut fin.suspended)
        };
        self.gen_release_cells(cells)
    }

    /// Decref the suspended frame's CVs the way Zend's execute_data
    /// teardown does — in CV order, each cell's value dies when the
    /// frame's last ref to it drops: a `__destruct` runs, and a gen
    /// internal releases into its own destruction replay at its own
    /// slot (interleaved with the plain locals' destructors).
    pub(in crate::interp) fn gen_release_cells(
        &mut self,
        cells: Vec<(String, Cell)>,
    ) -> Result<(), PhpError> {
        if cells.is_empty() {
            return Ok(());
        }
        // Zend frees the frame inside the resume call — destructors
        // cite the resume's line (`->next()` call / foreach header).
        if let Some(l) = self.gen_resume_site {
            self.cur_line = l;
        }
        // Per-object count of the released cells — a value dies when
        // the frame's LAST cell holding it decrefs.
        let mut remaining: HashMap<usize, usize> = HashMap::new();
        for (_, c) in &cells {
            if let Value::Object(o) = &*c.borrow() {
                *remaining.entry(Rc::as_ptr(o) as usize).or_insert(0) += 1;
            }
        }
        let mut terminal = None;
        for (_, c) in cells {
            // The frame's CV decrefs: a plain cell's stored value dies,
            // not just this clone — journal snapshots (delegate fins)
            // hold sibling clones of the same cell and must observe
            // the release as Null instead of resurrecting the local.
            // An is_ref cell is a shared binding (by-ref yield): the
            // frame gives up its hold but the referent stays live —
            // Zend decrefs the CV's reference, it does not null the
            // referent. Same for shared cells bound to outside storage
            // (a `static` aliases the function's statics table —
            // bug64979).
            let v = if self.is_ref_cell(&c) || self.is_shared_cell(&c) {
                c.borrow().clone()
            } else {
                std::mem::replace(&mut *c.borrow_mut(), Value::Null)
            };
            drop(c);
            let obj = match v {
                Value::Object(o) => Some(o),
                _ => None,
            };
            let Some(o) = obj else { continue };
            let key = Rc::as_ptr(&o) as usize;
            let Some(r) = remaining.get_mut(&key) else {
                continue;
            };
            *r -= 1;
            if *r != 0 {
                continue;
            }
            remaining.remove(&key);
            // `o` is the accounting clone — dies with the frame iff
            // nothing outside it still holds the value.
            if Rc::strong_count(&o) != 1 {
                continue;
            }
            let fq = match &o.borrow().internal {
                Some(ObjectInternal::Generator(st)) => Some(st.borrow().fin_q.clone()),
                _ => None,
            };
            let e = match fq {
                Some(q) => self.gen_fin_replay(&q, false),
                None => self.destruct_dying_value(&Value::Object(o)).err(),
            };
            if terminal.is_none() {
                terminal = e;
            }
        }
        match terminal {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The resume-stack frames for a deferred body death: the gen fn
    /// ran as an internal call (`[internal function]: g()`), invoked
    /// from `Generator->{method}()` at the consumer's call site,
    /// under whatever frames the consumer itself is in.
    fn gen_resume_frames(
        &mut self,
        state: &Rc<RefCell<GenState>>,
        method: &str,
        args: &[crate::value::Cell],
        include_method: bool,
    ) -> Vec<String> {
        let fn_name = {
            let st = state.borrow();
            match &st.setup {
                GenSetup::Invoke { decl, .. } => decl.name.clone(),
            }
        };
        let mut frames = vec![format!("[internal function]: {}()", fn_name)];
        if include_method {
            // Zend renders the resume-call args (`Generator->send(5)`).
            let args_str = args
                .iter()
                .map(|c| crate::value::trace_arg(&c.borrow()))
                .collect::<Vec<_>>()
                .join(", ");
            frames.push(format!(
                "{}({}): Generator->{}({})",
                self.diag_file(),
                self.cur_line,
                method,
                args_str
            ));
        }
        for fr in self.call_trace.iter().rev() {
            if crate::value::trace_frame_hidden(fr) {
                continue;
            }
            frames.push(crate::value::trace_frame_str(fr));
        }
        frames
    }

    /// Raise at a `Generator->{m}()` call. Userland calls carry the
    /// `Generator->{m}(args)` pseudo-frame Zend stamps (`#0
    /// FILE(n): Generator->send(5)`); engine-driven resumes
    /// (foreach / iterator_*) report from the real frame instead.
    fn gen_method_throw(
        &mut self,
        method: &str,
        args: &[crate::value::Cell],
        msg: &str,
    ) -> PhpError {
        self.gen_method_throw_kind("Exception", method, args, msg)
    }

    /// `gen_method_throw` for a different throwable class (TypeError
    /// for bad `Generator->throw()` args).
    fn gen_method_throw_kind(
        &mut self,
        class: &str,
        method: &str,
        args: &[crate::value::Cell],
        msg: &str,
    ) -> PhpError {
        let userland = self.iter_calls == 0 && self.gen_internal_resume == 0;
        if userland {
            self.call_trace.push(TraceFrame {
                function: method.to_string(),
                class: Some("Generator".into()),
                ty: "->".into(),
                file: self.diag_file(),
                line: self.cur_line as u32,
                args: args.to_vec(),
                named_args: Vec::new(),
                internal: false,
            });
        }
        let v = self.exception(class, msg);
        if userland {
            self.call_trace.pop();
        }
        self.throw(v)
    }

    /// Native dispatch for the `Generator` class (Iterator + send/throw/
    /// getReturn). `obj` must carry a Generator internal.
    pub(in crate::interp) fn generator_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        args: &CallArgs,
    ) -> Result<Option<Value>, PhpError> {
        // The dispatch's invocation line is the resume's line —
        // destructors the exhaust/close step runs cite it. Engine-
        // driven nested dispatches (the `yield from` drain stepping a
        // delegate, foreach/materializer internals) share the outer
        // resume's line; a userland call — even one inside another
        // gen's body — cites its own call line.
        let engine_nested = self.gen_resume_site.is_some()
            && (self.gen_collect_base.is_some()
                || self.gen_internal_resume > 0
                || self.iter_calls > 0);
        let saved_site = self.gen_resume_site;
        if !engine_nested {
            self.gen_resume_site = Some(self.cur_line);
        }
        let r = self.generator_method_body(obj, name, args);
        if !engine_nested {
            self.gen_resume_site = saved_site;
        }
        r
    }

    fn generator_method_body(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        args: &CallArgs,
    ) -> Result<Option<Value>, PhpError> {
        let state = match &obj.borrow().internal {
            Some(ObjectInternal::Generator(st)) => st.clone(),
            _ => return Ok(None),
        };
        let lname = name.to_lowercase();
        match lname.as_str() {
            "rewind" => {
                if !state.borrow().started {
                    self.gen_start(&state)?;
                    self.gen_raise_deferred(&state, "rewind", &args.cells)?;
                    return Ok(Some(Value::Null));
                }
                let (pos, len, dead, closed, running) = {
                    let st = state.borrow();
                    (st.pos, st.items.len(), st.dead, st.closed, st.running)
                };
                let engine = self.iter_calls > 0 || self.gen_internal_resume > 0;
                if running {
                    // Zend cleared DO_INIT the moment the body ran —
                    // rewind of a still-running gen reports 'already
                    // run' whether it arrives as ->rewind() or as a
                    // foreach re-init inside the body itself.
                    return Err(self.gen_method_throw(
                        "rewind",
                        &args.cells,
                        "Cannot rewind a generator that was already run",
                    ));
                }
                if closed {
                    // Engine-driven consume reports the closed state;
                    // an explicit ->rewind() reports 'already run'.
                    let msg = if engine {
                        "Cannot traverse an already closed generator"
                    } else {
                        "Cannot rewind a generator that was already run"
                    };
                    return Err(self.gen_method_throw("rewind", &args.cells, msg));
                }
                if pos > 0 || dead {
                    if dead {
                        if pos == 0 {
                            // Dead while still at the first item:
                            // rewind is a silent no-op while the
                            // death is still deferred — the buffered
                            // items serve, it surfaces at the resume
                            // past them. Once it surfaced, Zend marks
                            // the gen closed: an engine consume
                            // reports 'closed' on every retry while
                            // an explicit ->rewind() stays silent.
                            let surfaced = state.borrow().deferred_err.is_none();
                            if surfaced && engine {
                                return Err(self.gen_method_throw(
                                    "rewind",
                                    &args.cells,
                                    "Cannot traverse an already closed generator",
                                ));
                            }
                            return Ok(Some(Value::Null));
                        }
                        if engine && pos >= len {
                            // Engine-driven consume of an exhausted
                            // dead gen surfaces the body's deferred
                            // death — or reports 'closed' once
                            // consumed.
                            self.gen_raise_deferred(&state, "rewind", &args.cells)?;
                            return Err(self.gen_method_throw(
                                "rewind",
                                &args.cells,
                                "Cannot traverse an already closed generator",
                            ));
                        }
                        // Dead mid-buffer — an explicit ->rewind() or
                        // a foreach re-init both hit Zend's 'already
                        // run' gate; the silent continue only ever
                        // applied at pos==0.
                        return Err(self.gen_method_throw(
                            "rewind",
                            &args.cells,
                            "Cannot rewind a generator that was already run",
                        ));
                    }
                    let msg = if engine && pos >= len {
                        "Cannot traverse an already closed generator"
                    } else {
                        "Cannot rewind a generator that was already run"
                    };
                    return Err(self.gen_method_throw("rewind", &args.cells, msg));
                }
                Ok(Some(Value::Null))
            }
            "valid" => {
                self.gen_start(&state)?;
                self.gen_raise_deferred(&state, "valid", &args.cells)?;
                let st = state.borrow();
                // Mid-run probes see the live collection, not the
                // (still empty) committed buffer.
                let len = st
                    .live
                    .as_ref()
                    .map(|l| l.borrow().len())
                    .unwrap_or(st.items.len());
                Ok(Some(Value::Bool(st.pos < len)))
            }
            "current" => {
                self.gen_start(&state)?;
                self.gen_raise_deferred(&state, "current", &args.cells)?;
                let st = state.borrow();
                let v = st
                    .live
                    .as_ref()
                    .and_then(|l| l.borrow().get(st.pos).map(|(_, v)| v.borrow().clone()))
                    .or_else(|| {
                        st.items
                            .get(st.pos)
                            .map(|(_, v)| v.borrow().clone())
                    })
                    .unwrap_or(Value::Null);
                Ok(Some(v))
            }
            "key" => {
                self.gen_start(&state)?;
                self.gen_raise_deferred(&state, "key", &args.cells)?;
                let st = state.borrow();
                let k = st
                    .live
                    .as_ref()
                    .and_then(|l| l.borrow().get(st.pos).map(|(k, _)| k.clone()))
                    .or_else(|| st.items.get(st.pos).map(|(k, _)| k.clone()))
                    .unwrap_or(Value::Null);
                Ok(Some(k))
            }
            "next" => {
                if state.borrow().running {
                    return Err(self.gen_method_throw_kind(
                        "Error",
                        "next",
                        &args.cells,
                        "Cannot resume an already running generator",
                    ));
                }
                self.gen_start(&state)?;
                let pos = {
                    let mut st = state.borrow_mut();
                    let p = st.pos + 1;
                    st.set_pos(p);
                    p
                };
                self.gen_flush_out(&state, pos);
                // Crossing the body's end frees its suspended frame —
                // Zend frees execute_data inside the resume that
                // exhausts the gen.
                self.gen_release_exhausted(&state)?;
                self.gen_raise_deferred(&state, "next", &args.cells)?;
                Ok(Some(Value::Null))
            }
            "send" => {
                let v = args.cells.first().map(|c| c.borrow().clone()).unwrap_or(Value::Null);
                if state.borrow().running {
                    return Err(self.gen_method_throw_kind(
                        "Error",
                        "send",
                        &args.cells,
                        "Cannot resume an already running generator",
                    ));
                }
                if state.borrow().closed {
                    // send() on a killed gen is a silent no-op.
                    return Ok(Some(Value::Null));
                }
                let (prev_pos, restart) = {
                    let mut st = state.borrow_mut();
                    // send() delivers to the yield the gen is
                    // suspended at (index pos) — the re-run replays
                    // every yield, so earlier yields must not eat it.
                    let pos = st.pos;
                    st.sends.push((pos, v.clone()));
                    // A gen resumed past its end carries the body's
                    // death — Zend re-raises it at this call rather
                    // than re-running the body; a clean exhausted
                    // gen takes send() as a silent NULL.
                    (st.pos, st.started && st.pos < st.items.len())
                };
                if restart {
                    {
                        let mut st = state.borrow_mut();
                        // Eager model: re-run the body so queued sends
                        // reach their yield expressions (the k-th
                        // send feeds the k-th yield expr).
                        st.started = false;
                        st.finished = false;
                        st.items.clear();
                        st.set_pos(0);
                        st.pending_out.clear();
                        // The deferred-output journal replays from
                        // scratch with the body — stale fin bytes /
                        // yield markers from the pre-resume stream
                        // would double-emit or mis-flag yields.
                        {
                            let mut fin = st.fin_q.borrow_mut();
                            fin.bytes.clear();
                            fin.yields.clear();
                            fin.delegates.clear();
                            fin.fin_err = None;
                            for (_, d) in fin.delegate_fins.drain(..) {
                                // The re-run displaces this
                                // delegate's incarnation — its eager
                                // tail never ran.
                                d.borrow_mut().kill_tree();
                            }
                        }
                        st.deferred_err = None;
                        st.dead = false;
                        st.delegate_gens.clear();
                    }
                    // The gen's ob windows journaled the old run's
                    // post-yield tail — drop it before the re-run.
                    self.ob_gen_restart(&state.borrow().fin_q.clone());
                    // The re-run replays the prefix the consumer
                    // already echoed — suppress its bytes (Zend only
                    // produces the resume segment).
                    self.gen_replay_horizon = Some((prev_pos, state.clone()));
                    let r = self.gen_start(&state);
                    self.gen_replay_horizon = None;
                    r?;
                } else {
                    self.gen_start(&state)?;
                }
                {
                    let mut st = state.borrow_mut();
                    // This send resumes one step past what the
                    // consumer had — a send() burst can outpace the
                    // cursor further.
                    let n = st.sends.len();
                    st.set_pos((prev_pos + 1).max(n));
                }
                let pos = state.borrow().pos;
                self.gen_flush_out(&state, pos);
                self.gen_release_exhausted(&state)?;
                self.gen_raise_deferred(&state, "send", &args.cells)?;
                let st = state.borrow();
                Ok(Some(
                    st.items
                        .get(st.pos)
                        .map(|(_, v)| v.borrow().clone())
                        .unwrap_or(Value::Null),
                ))
            }
            "throw" => {
                let e = args.cells.first().map(|c| c.borrow().clone()).unwrap_or(Value::Null);
                // zpp: $exception must be a Throwable.
                let ok = matches!(&e, Value::Object(o) if self.obj_is_a(o, "throwable"));
                if !ok {
                    return Err(self.gen_method_throw_kind(
                        "TypeError",
                        "throw",
                        &args.cells,
                        &format!(
                            "Generator::throw(): Argument #1 ($exception) must be of type Throwable, {} given",
                            self.zval_type_name(&e)
                        ),
                    ));
                }
                if state.borrow().running {
                    return Err(self.gen_method_throw_kind(
                        "Error",
                        "throw",
                        &args.cells,
                        "Cannot resume an already running generator",
                    ));
                }
                // throw() into an unstarted gen resumes it to the
                // first yield first — its body (and queued finally
                // output) exists before the kill.
                self.gen_start(&state)?;
                {
                    let st = state.borrow();
                    // No suspended frame to receive the throwable — a
                    // force-closed or cursor-exhausted gen (pos past
                    // the last item = Zend's finished execute_data)
                    // bounces it out of this call verbatim, leaving the
                    // completed body and its return value intact.
                    if st.closed || st.pos >= st.items.len() {
                        drop(st);
                        return Err(self.throw(e));
                    }
                }
                // Re-run the body with the throwable queued at the
                // suspended yield: that yield expression raises it,
                // so the body's own try/catch/finally performs the
                // real unwind — a matching catch binds the exception
                // object itself, `return` ending a finally swallows
                // it, a yield inside `finally` suspends the unwind.
                let prev = {
                    let mut st = state.borrow_mut();
                    let prev = st.pos;
                    st.throws.push((prev, e.clone()));
                    st.injected_throwable = Some(e.clone());
                    st.started = false;
                    st.finished = false;
                    st.items.clear();
                    st.set_pos(0);
                    st.pending_out.clear();
                    {
                        let mut fin = st.fin_q.borrow_mut();
                        fin.bytes.clear();
                        fin.yields.clear();
                        fin.delegates.clear();
                        fin.fin_err = None;
                        for (_, d) in fin.delegate_fins.drain(..) {
                            // The re-run displaces this delegate's
                            // incarnation — its eager tail never ran.
                            d.borrow_mut().kill_tree();
                        }
                    }
                    st.deferred_err = None;
                    st.dead = false;
                    st.delegate_gens.clear();
                    prev
                };
                self.ob_gen_restart(&state.borrow().fin_q.clone());
                self.gen_throws_fired.clear();
                // The re-run replays the prefix the consumer already
                // echoed — suppress its bytes like a send() re-run.
                self.gen_replay_horizon = Some((prev, state.clone()));
                let r = self.gen_start(&state);
                self.gen_replay_horizon = None;
                r?;
                let fired_at_delegate = {
                    let st = state.borrow();
                    st.delegate_gens.iter().any(|(base, span)| {
                        prev >= *base
                            && prev < base + span
                            && self.gen_throws_fired.contains(&(prev - base))
                    })
                };
                if !self.gen_throws_fired.contains(&prev) && !fired_at_delegate {
                    // The injection couldn't land — the suspension
                    // point isn't a body-level yield (a `yield from`
                    // splice item) or the stream was exhausted: Zend
                    // force-closes — run the suspended chain's
                    // finally output, then bounce the throwable out
                    // of this call.
                    let fq = state.borrow().fin_q.clone();
                    let mut fin = std::mem::take(&mut *fq.borrow_mut());
                    let pos = fin.pos;
                    {
                        let mut f = fq.borrow_mut();
                        f.finished = true;
                        f.kill_tree();
                    }
                    self.ob_dead_gen(&fq);
                    self.gen_fin_bytes(&fin, pos);
                    // The force-close frees the suspended frame's CVs
                    // right after the finally chain (locals' __destruct
                    // runs, held gens release into their own teardown).
                    self.gen_release_cells(std::mem::take(&mut fin.suspended))?;
                    let mut st = state.borrow_mut();
                    st.finished = true;
                    st.closed = true;
                    st.items.clear();
                    st.pending_out.clear();
                    st.deferred_err = None;
                    return Err(self.throw(e));
                }
                {
                    let mut st = state.borrow_mut();
                    st.set_pos(prev + 1);
                    // An injected throwable that propagated uncaught
                    // killed the body — Zend leaves the gen closed,
                    // not dead-resumable (explicit ->rewind() reports
                    // 'already run').
                    if st.dead {
                        st.closed = true;
                        if st.injected_throwable.is_some() {
                            st.fin_q.borrow_mut().kill_tree();
                        }
                    }
                }
                self.gen_flush_out(&state, prev + 1);
                self.gen_release_exhausted(&state)?;
                self.gen_raise_deferred(&state, "throw", &args.cells)?;
                Ok(Some(
                    state
                        .borrow()
                        .items
                        .get(prev + 1)
                        .map(|(_, v)| v.borrow().clone())
                        .unwrap_or(Value::Null),
                ))
            }
            "getreturn" => {
                // getReturn() starts the body first (zend_generator
                // _ensure_initialized) — a `{ return; yield; }` gen
                // completes and reports NULL, while one that yielded
                // throws 'hasn't returned' once the run proves it
                // unfinished.
                self.gen_start(&state)?;
                // Deferred output belongs to the resumes that cross
                // each tag — getReturn doesn't advance the cursor,
                // so only bytes already due drain here (Zend emits
                // nothing extra for a still-suspended gen).
                let pos = state.borrow().pos;
                self.gen_flush_out(&state, pos);
                self.gen_raise_deferred(&state, "getReturn", &args.cells)?;
                let st = state.borrow();
                // Still inside its first drive (the live sink has no
                // item yet): ensure_initialized hits Zend's resume
                // guard — an Error. Once items exist the unfinished
                // check is what reports (Exception).
                if st.running && st.live.as_ref().is_none_or(|l| l.borrow().is_empty()) {
                    let msg = "Cannot resume an already running generator";
                    drop(st);
                    return Err(self.gen_method_throw_kind(
                        "Error",
                        "getReturn",
                        &args.cells,
                        msg,
                    ));
                }
                if st.pos < st.items.len() || st.closed || st.dead || st.running {
                    let msg = "Cannot get return value of a generator that hasn't returned";
                    drop(st);
                    return Err(self.gen_method_throw("getReturn", &args.cells, msg));
                }
                Ok(Some(st.return_val.clone()))
            }
            "__construct" => self.fail(PhpError::uncaught(
                "Error",
                "The \"Generator\" class is reserved for internal use and cannot be manually instantiated",
                0,
            )),
            _ => Ok(None),
        }
    }

    /// Materialize an iterable's (key, value) pairs. Returns the
    /// items collected plus a death to propagate afterwards — an
    /// inner-generator death mid-materialization keeps the prefix it
    /// already yielded (`yield from` streams up to the death, then
    /// dies), so the error travels beside the items rather than
    /// discarding them.
    pub(in crate::interp) fn yield_from_collect(
        &mut self,
        v: &Value,
    ) -> (Vec<crate::value::GenItem>, Option<PhpError>) {
        match v {
            Value::Array(a) => (
                a.borrow()
                    .iter()
                    .map(|(k, c)| (key_value(k), c.clone()))
                    .collect(),
                None,
            ),
            Value::Object(o) => {
                if self.obj_is_a(o, "IteratorAggregate") {
                    let it = match self.method_invoke(o.clone(), "getIterator", CallArgs::empty()) {
                        Ok(it) => it,
                        Err(e) => return (Vec::new(), Some(e)),
                    };
                    return self.yield_from_collect(&it);
                }
                if self.obj_is_a(o, "Iterator")
                    || o.borrow().class.name().eq_ignore_ascii_case("generator")
                {
                    let mut out = Vec::new();
                    let mut death = None;
                    // A delegate (re-)driven inside an outer's
                    // send()/throw() replay: its pre-first-yield
                    // bytes were already echoed by the run the
                    // consumer saw — the outer's horizon can't tag
                    // this state's emits, so mark the prefix stale.
                    if let Some(crate::value::ObjectInternal::Generator(ist)) = &o.borrow().internal
                    {
                        if let Some((k, tgt)) = &self.gen_replay_horizon {
                            let is_delegate = !std::rc::Rc::ptr_eq(tgt, ist)
                                && self.gen_collect_base.is_some_and(|b| b <= *k);
                            if is_delegate {
                                ist.borrow_mut().suppress_prefix = true;
                            }
                        }
                    }
                    if let Err(e) = self.method_invoke(o.clone(), "rewind", CallArgs::empty()) {
                        death = Some(e);
                    }
                    // A gen inner's destruction journal, snapshotted
                    // right after its start — the drain below prunes
                    // it, but the OUTER gen's force-close replays the
                    // regions the inner was suspended inside.
                    if death.is_none() {
                        if let Some(crate::value::ObjectInternal::Generator(ist)) =
                            &o.borrow().internal
                        {
                            self.gen_yield_from_fin = Some((*ist.borrow().fin_q.borrow()).clone());
                        }
                    }
                    while death.is_none() {
                        match self.method_invoke(o.clone(), "valid", CallArgs::empty()) {
                            Ok(v) if v.is_truthy() => {}
                            Ok(_) => break,
                            Err(e) => {
                                death = Some(e);
                                break;
                            }
                        }
                        let k = match self.method_invoke(o.clone(), "key", CallArgs::empty()) {
                            Ok(k) => k,
                            Err(e) => {
                                death = Some(e);
                                break;
                            }
                        };
                        let val = match self.method_invoke(o.clone(), "current", CallArgs::empty())
                        {
                            Ok(v) => v,
                            Err(e) => {
                                death = Some(e);
                                break;
                            }
                        };
                        // Storage-backed iterators expose live cells:
                        // zend's materialization keeps IS_REFERENCE
                        // bindings (a by-ref foreach's marks re-bind,
                        // writes through the copy reach the storage).
                        let c = match &o.borrow().internal {
                            Some(crate::value::ObjectInternal::ArrayIter {
                                store, pos, ..
                            }) => store
                                .borrow()
                                .arr
                                .borrow()
                                .iter()
                                .nth(*pos)
                                .map(|(_, c)| c.clone())
                                .filter(|c| self.is_ref_cell(c)),
                            Some(crate::value::ObjectInternal::Generator(st)) => {
                                let st = st.borrow();
                                st.items
                                    .get(st.pos)
                                    .map(|(_, c)| c.clone())
                                    .filter(|c| self.is_ref_cell(c))
                            }
                            _ => None,
                        }
                        .unwrap_or_else(|| cell(val));
                        out.push((k, c));
                        self.gen_collect_seen += 1;
                        // A consumer injection queued for this splice
                        // index delivers to the delegate's suspended
                        // yield — Zend's chain is live: send()/throw()
                        // on the outer lands inside the delegate, so
                        // drive the step that crosses it with the
                        // delegate's own method instead of next().
                        let base = self.gen_collect_base.unwrap_or(0);
                        let outer_idx = base + out.len() - 1;
                        // Injection methods exist only on Generator —
                        // a plain Iterator delegate has no send()/throw():
                        // zend falls back to next() for send(), and
                        // surfaces a throw() at the outer's own
                        // yield-from instead of calling the delegate.
                        let delegate_is_gen = matches!(
                            o.borrow().internal,
                            Some(crate::value::ObjectInternal::Generator(_))
                        );
                        let injected = if let Some(p) =
                            self.gen_sends.iter().position(|(i, _)| *i == outer_idx)
                        {
                            let (_, v) = self.gen_sends.remove(p).unwrap();
                            if !delegate_is_gen {
                                false
                            } else {
                                // The delegate's injected re-run
                                // replaces the tail it journaled into
                                // the outer queue from its stale pass.
                                if let Some(run) = &self.gen_run_state {
                                    run.borrow_mut()
                                        .pending_out
                                        .retain(|(t, ..)| *t < outer_idx);
                                }
                                let mut a = CallArgs::empty();
                                a.cells.push(cell(v));
                                match self.method_invoke(o.clone(), "send", a) {
                                    Ok(_) => true,
                                    Err(e) => {
                                        death = Some(e);
                                        break;
                                    }
                                }
                            }
                        } else if let Some(p) =
                            self.gen_throws.iter().position(|(i, _)| *i == outer_idx)
                        {
                            let (_, v) = self.gen_throws.remove(p).unwrap();
                            self.gen_throws_fired.push(outer_idx);
                            if !delegate_is_gen {
                                self.pending_exception = Some(v);
                                death = Some(PhpError {
                                    kind: crate::error::ErrorKind::Throw,
                                    message: "gen".into(),
                                    line: 0,
                                    trace: None,
                                    thrown_line: None,
                                    display_msg: None,
                                });
                                break;
                            }
                            // Same stale-tail drop as send() — the
                            // delegate's re-run under the throwable
                            // replaces what its earlier pass journaled.
                            if let Some(run) = &self.gen_run_state {
                                run.borrow_mut()
                                    .pending_out
                                    .retain(|(t, ..)| *t < outer_idx);
                            }
                            let mut a = CallArgs::empty();
                            a.cells.push(cell(v));
                            match self.method_invoke(o.clone(), "throw", a) {
                                Ok(_) => true,
                                Err(e) => {
                                    death = Some(e);
                                    break;
                                }
                            }
                        } else {
                            false
                        };
                        if !injected {
                            if let Err(e) = self.method_invoke(o.clone(), "next", CallArgs::empty())
                            {
                                death = Some(e);
                                break;
                            }
                        }
                    }
                    (out, death)
                } else {
                    (
                        Vec::new(),
                        self.fail::<Value>(PhpError::uncaught(
                            "TypeError",
                            "Argument #1 must be of type Traversable|array",
                            0,
                        ))
                        .err(),
                    )
                }
            }
            _ => (
                Vec::new(),
                self.fail::<Value>(PhpError::uncaught(
                    "TypeError",
                    "Argument #1 must be of type Traversable|array",
                    0,
                ))
                .err(),
            ),
        }
    }

    /// Internal materializers (iterator_to_array, iterator_count,
    /// iterator_apply) drive the iteration under the internal-resume
    /// flag — a deferred gen death inside renders the resume stack
    /// without the `Generator->{method}()` pseudo-frame.
    pub(crate) fn yield_from_collect_internal(
        &mut self,
        v: &Value,
    ) -> (Vec<crate::value::GenItem>, Option<PhpError>) {
        self.gen_internal_resume += 1;
        let r = self.yield_from_collect(v);
        self.gen_internal_resume -= 1;
        r
    }
}
