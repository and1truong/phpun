//! Generators: yield detection, `GenState` batch-replay start,
//! `send`/`yield from` plumbing and the SPL iterator method bridge.

use super::util::*;
use super::*;

impl<'a> Interp<'a> {
    // ----- generators -----

    /// Whether a function body yields — scanning skips nested closures
    /// and function decls (each is its own generator context).
    pub(in crate::interp) fn decl_contains_yield(stmts: &[Stmt]) -> bool {
        stmts.iter().any(Self::stmt_contains_yield)
    }

    fn stmt_contains_yield(s: &Stmt) -> bool {
        match s {
            Stmt::Expr(e) => Self::expr_contains_yield(e),
            Stmt::Echo(es) => es.iter().any(Self::expr_contains_yield),
            Stmt::Return(Some(e)) => Self::expr_contains_yield(e),
            Stmt::Block(b) => Self::decl_contains_yield(b),
            Stmt::If { cond, then, else_ } => {
                Self::expr_contains_yield(cond)
                    || Self::decl_contains_yield(then)
                    || Self::decl_contains_yield(else_)
            }
            Stmt::While { cond, body } | Stmt::DoWhile { body, cond } => {
                Self::expr_contains_yield(cond) || Self::decl_contains_yield(body)
            }
            Stmt::For {
                init,
                cond,
                inc,
                body,
            } => {
                init.iter()
                    .chain(cond.iter())
                    .chain(inc.iter())
                    .any(Self::expr_contains_yield)
                    || Self::decl_contains_yield(body)
            }
            Stmt::Foreach { arr, val, body, .. } => {
                Self::expr_contains_yield(arr)
                    || matches!(val, ForeachTarget::Lvalue(e) if Self::expr_contains_yield(e))
                    || Self::decl_contains_yield(body)
            }
            Stmt::Switch { cond, cases } => {
                Self::expr_contains_yield(cond)
                    || cases.iter().any(|(c, b)| {
                        c.as_ref().is_some_and(Self::expr_contains_yield)
                            || Self::decl_contains_yield(b)
                    })
            }
            Stmt::Try {
                body,
                catches,
                finally,
            } => {
                Self::decl_contains_yield(body)
                    || catches.iter().any(|c| Self::decl_contains_yield(&c.body))
                    || finally
                        .as_ref()
                        .is_some_and(|b| Self::decl_contains_yield(b))
            }
            Stmt::Static { vars, .. } => vars
                .iter()
                .any(|(_, e, _)| e.as_ref().is_some_and(Self::expr_contains_yield)),
            Stmt::Unset(v) | Stmt::Global(v) => v.iter().any(Self::expr_contains_yield),
            Stmt::ConstDecl(v) => v.iter().any(|(_, e)| Self::expr_contains_yield(e)),
            Stmt::Declare { value, .. } => Self::expr_contains_yield(value),
            // A nested `function` decl is its own generator context
            // (its yields don't make the outer fn a generator).
            Stmt::Function(_) | Stmt::Class(_) => false,
            _ => false,
        }
    }

    fn expr_contains_yield(e: &Expr) -> bool {
        match e {
            Expr::Yield { .. } | Expr::YieldFrom(_) => true,
            // Nested closures/arrow fns are their own generator context.
            Expr::Closure(_) | Expr::AnonClass(_) => false,
            Expr::Assign { target, value, .. } => {
                Self::expr_contains_yield(target) || Self::expr_contains_yield(value)
            }
            Expr::Binary { l, r, .. } => {
                Self::expr_contains_yield(l) || Self::expr_contains_yield(r)
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
            | Expr::Include { e, .. } => Self::expr_contains_yield(e),
            Expr::Ternary { c, t, f } => {
                Self::expr_contains_yield(c)
                    || t.as_ref().is_some_and(|t| Self::expr_contains_yield(t))
                    || Self::expr_contains_yield(f)
            }
            Expr::Call { name, args } => {
                Self::expr_contains_yield(name) || args.iter().any(Self::expr_contains_yield)
            }
            Expr::MethodCall {
                obj, name, args, ..
            } => {
                Self::expr_contains_yield(obj)
                    || matches!(name, PropName::Expr(e) if Self::expr_contains_yield(e))
                    || args.iter().any(Self::expr_contains_yield)
            }
            Expr::StaticCall { class, args, .. } => {
                Self::expr_contains_yield(class) || args.iter().any(Self::expr_contains_yield)
            }
            Expr::StaticCallDyn { class, name, args } => {
                Self::expr_contains_yield(class)
                    || Self::expr_contains_yield(name)
                    || args.iter().any(Self::expr_contains_yield)
            }
            Expr::Index { e, i } => {
                Self::expr_contains_yield(e)
                    || i.as_ref().is_some_and(|i| Self::expr_contains_yield(i))
            }
            Expr::Prop { obj, name, .. } => {
                Self::expr_contains_yield(obj)
                    || matches!(name, PropName::Expr(e) if Self::expr_contains_yield(e))
            }
            Expr::StaticProp { class, name } => {
                Self::expr_contains_yield(class)
                    || matches!(name, PropName::Expr(e) if Self::expr_contains_yield(e))
            }
            Expr::Isset(v) => v.iter().any(Self::expr_contains_yield),
            Expr::List(v) => v.iter().flatten().any(Self::expr_contains_yield),
            Expr::Exit(Some(e)) => Self::expr_contains_yield(e),
            Expr::ArrayLit(items) => items.iter().any(|(k, v)| {
                k.as_ref().is_some_and(Self::expr_contains_yield) || Self::expr_contains_yield(v)
            }),
            Expr::Match { subject, arms } => {
                Self::expr_contains_yield(subject)
                    || arms.iter().any(|a| {
                        a.conds.iter().any(Self::expr_contains_yield)
                            || Self::expr_contains_yield(&a.result)
                    })
            }
            Expr::New { class, args } => {
                Self::expr_contains_yield(class) || args.iter().any(Self::expr_contains_yield)
            }
            Expr::ClassConst { class, .. } => Self::expr_contains_yield(class),
            Expr::Instanceof { obj, class } => {
                Self::expr_contains_yield(obj) || Self::expr_contains_yield(class)
            }
            _ => false,
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
            pending_out: Vec::new(),
            fin_q,
            deferred_err: None,
            dead: false,
            closed: false,
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
        let items = Rc::new(RefCell::new(Vec::new()));
        let saved_sink = self.gen_sink.replace(items.clone());
        let saved_sends = std::mem::replace(&mut self.gen_sends, sends.into_iter().collect());
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
        );
        self.gen_sink = saved_sink;
        self.gen_sends = saved_sends;
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
            st.items = collected;
            st.finished = true;
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
        // A `$gen->throw()` parked at a yield inside `finally`: the
        // unwind continues at each resume — suspending again on the
        // next finally-yield, surfacing the throwable verbatim once
        // the consumer passes the parked point.
        {
            let st = state.borrow();
            let fq = st.fin_q.clone();
            let mut fq = fq.borrow_mut();
            if let Some((v, i)) = fq.injected.take() {
                let parked = fq.at_fin_yield(st.pos);
                if parked {
                    fq.injected = Some((v, st.pos));
                } else if st.pos > i {
                    drop(fq);
                    drop(st);
                    // The injected throwable surfaced — the unwind it
                    // was mid-way through is complete, so the gen is
                    // closed (its post-finally items never run).
                    {
                        let mut st = state.borrow_mut();
                        st.finished = true;
                        st.closed = true;
                        st.items.clear();
                        st.pending_out.clear();
                    }
                    // The injected death is the gen's own — the body's
                    // pending error never ran past that yield.
                    state.borrow_mut().deferred_err = None;
                    state.borrow().fin_q.borrow_mut().fin_err = None;
                    return Err(self.throw(v));
                } else {
                    fq.injected = Some((v, i));
                }
            }
        }
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
            let mut frames: Vec<String> = raise_frames
                .iter()
                .rev()
                .enumerate()
                .map(|(i, f)| crate::value::trace_frame_str_at(f, i))
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
                self.rewrite_throwable_trace(&frames);
            } else {
                e.trace = Some(frames);
            }
            return Err(e);
        }
        let mut frames = self.gen_resume_frames(state, method, args, self.gen_internal_resume == 0);
        if e.kind == crate::error::ErrorKind::Throw {
            // Frames suspended between the throw site and the gen body
            // — eval()/include() pseudo-frames and userland calls —
            // lead the resume stack in Zend's render.
            if !raise_frames.is_empty() {
                let prefix: Vec<String> = raise_frames
                    .iter()
                    .rev()
                    .enumerate()
                    .map(|(i, f)| crate::value::trace_frame_str_at(f, i))
                    .collect();
                frames = prefix.into_iter().chain(frames).collect();
            }
            // The uncaught render reads the Throwable's own trace —
            // swap it for the resume stack.
            self.rewrite_throwable_trace(&frames);
        } else {
            e.trace = Some(frames);
        }
        Err(e)
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
        let v = self.exception("Exception", msg);
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
                let (pos, len, dead, closed) = {
                    let st = state.borrow();
                    (st.pos, st.items.len(), st.dead, st.closed)
                };
                let engine = self.iter_calls > 0 || self.gen_internal_resume > 0;
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
                            // rewind is a silent no-op — the buffered
                            // items serve, the death surfaces at the
                            // resume past them.
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
                Ok(Some(Value::Bool(st.pos < st.items.len())))
            }
            "current" => {
                self.gen_start(&state)?;
                self.gen_raise_deferred(&state, "current", &args.cells)?;
                let st = state.borrow();
                Ok(Some(
                    st.items
                        .get(st.pos)
                        .map(|(_, v)| v.borrow().clone())
                        .unwrap_or(Value::Null),
                ))
            }
            "key" => {
                self.gen_start(&state)?;
                self.gen_raise_deferred(&state, "key", &args.cells)?;
                let st = state.borrow();
                Ok(Some(
                    st.items.get(st.pos).map(|(k, _)| k.clone()).unwrap_or(Value::Null),
                ))
            }
            "next" => {
                self.gen_start(&state)?;
                let pos = {
                    let mut st = state.borrow_mut();
                    let p = st.pos + 1;
                    st.set_pos(p);
                    p
                };
                self.gen_flush_out(&state, pos);
                self.gen_raise_deferred(&state, "next", &args.cells)?;
                Ok(Some(Value::Null))
            }
            "send" => {
                let v = args.cells.first().map(|c| c.borrow().clone()).unwrap_or(Value::Null);
                if state.borrow().closed {
                    // send() on a killed gen is a silent no-op.
                    return Ok(Some(Value::Null));
                }
                let (prev_pos, restart) = {
                    let mut st = state.borrow_mut();
                    st.sends.push(v);
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
                        st.fin_q.borrow_mut().bytes.clear();
                        st.deferred_err = None;
                        st.dead = false;
                    }
                    // The re-run replays the prefix the consumer
                    // already echoed — suppress its bytes (Zend only
                    // produces the resume segment).
                    self.gen_replay_horizon = Some(prev_pos);
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
                // throw() into an unstarted gen resumes it to the
                // first yield first — its body (and queued finally
                // output) exists before the kill.
                self.gen_start(&state)?;
                let (at_fin, next_fin) = {
                    let st = state.borrow();
                    let fq = st.fin_q.borrow();
                    (fq.at_fin_yield(st.pos), fq.next_fin_yield(st.pos))
                };
                if at_fin {
                    // Suspended AT a yield inside `finally`: the
                    // injected throwable lands on the suspended yield
                    // expression itself — the rest of the finally
                    // never runs and the throwable surfaces at this
                    // throw() call.
                    let fq = state.borrow().fin_q.clone();
                    std::mem::take(&mut *fq.borrow_mut());
                    let mut st = state.borrow_mut();
                    st.finished = true;
                    st.closed = true;
                    st.items.clear();
                    st.pending_out.clear();
                    st.deferred_err = None;
                    return Err(self.throw(e));
                }
                if let Some(idx) = next_fin {
                    // A `finally` yield ahead of the suspension
                    // point: the unwind echoes the queued output up
                    // to that journal point, delivers its item from
                    // throw() and parks the throwable — it
                    // re-surfaces at the consumer's next resume
                    // (gen_raise_deferred).
                    self.gen_flush_out(&state, idx);
                    let v = {
                        let mut st = state.borrow_mut();
                        st.set_pos(idx);
                        st.items
                            .get(idx)
                            .map(|(_, v)| v.borrow().clone())
                            .unwrap_or(Value::Null)
                    };
                    state.borrow().fin_q.borrow_mut().injected = Some((e, idx));
                    return Ok(Some(v));
                }
                {
                    // Closing a suspended generator runs the finally
                    // chains of the try-regions enclosing its
                    // suspension point BEFORE the throwable
                    // propagates — replay the suspended delegation
                    // chain's queued bytes (innermost first).
                    let fq = state.borrow().fin_q.clone();
                    let fin = std::mem::take(&mut *fq.borrow_mut());
                    let pos = fin.pos;
                    self.gen_fin_bytes(&fin, pos);
                    // Zend's closed generator: the kill discards the
                    // buffered item stream — subsequent reads report
                    // exhausted (valid() false, current()/key() null),
                    // send()/next() stay silent.
                    let mut st = state.borrow_mut();
                    st.finished = true;
                    st.closed = true;
                    st.items.clear();
                    st.pending_out.clear();
                    st.deferred_err = None;
                }
                Err(self.throw(e))
            }
            "getreturn" => {
                // getReturn() starts the body first (zend_generator
                // _ensure_initialized) — a `{ return; yield; }` gen
                // completes and reports NULL, while one that yielded
                // throws 'hasn't returned' once the run proves it
                // unfinished.
                self.gen_start(&state)?;
                self.gen_flush_out(&state, usize::MAX);
                self.gen_raise_deferred(&state, "getReturn", &args.cells)?;
                let st = state.borrow();
                if st.pos < st.items.len() || st.closed || st.dead {
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
                        if let Err(e) = self.method_invoke(o.clone(), "next", CallArgs::empty()) {
                            death = Some(e);
                            break;
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
