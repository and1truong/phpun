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
                .any(|(_, e)| e.as_ref().is_some_and(Self::expr_contains_yield)),
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
        }));
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
        let collected = std::mem::take(&mut *items.borrow_mut());
        let mut st = state.borrow_mut();
        st.items = collected;
        st.finished = true;
        match r {
            Ok(rv) => {
                st.return_val = rv;
                Ok(())
            }
            Err(e) => Err(e),
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
                let (started, finished) = {
                    let st = state.borrow();
                    (st.started, st.finished)
                };
                if finished {
                    let v =
                        self.exception("Exception", "Cannot traverse an already closed generator");
                    return Err(self.throw(v));
                }
                if started {
                    let v = self.exception(
                        "Exception",
                        "Cannot rewind a generator that was already run",
                    );
                    return Err(self.throw(v));
                }
                self.gen_start(&state)?;
                Ok(Some(Value::Null))
            }
            "valid" => {
                self.gen_start(&state)?;
                let st = state.borrow();
                Ok(Some(Value::Bool(st.pos < st.items.len())))
            }
            "current" => {
                self.gen_start(&state)?;
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
                let st = state.borrow();
                Ok(Some(
                    st.items.get(st.pos).map(|(k, _)| k.clone()).unwrap_or(Value::Null),
                ))
            }
            "next" => {
                self.gen_start(&state)?;
                state.borrow_mut().pos += 1;
                let pos = state.borrow().pos;
                self.gen_flush_out(&state, pos);
                Ok(Some(Value::Null))
            }
            "send" => {
                let v = args.cells.first().map(|c| c.borrow().clone()).unwrap_or(Value::Null);
                {
                    let mut st = state.borrow_mut();
                    st.sends.push(v);
                    if st.started {
                        // Eager model: re-run the body so queued sends
                        // reach their yield expressions (the k-th send
                        // feeds the k-th yield expr).
                        st.started = false;
                        st.finished = false;
                        st.items.clear();
                        st.pos = 0;
                        st.pending_out.clear();
                    }
                }
                self.gen_start(&state)?;
                // The k-th send resumes at item k.
                {
                    let mut st = state.borrow_mut();
                    st.pos = st.sends.len();
                }
                let pos = state.borrow().pos;
                self.gen_flush_out(&state, pos);
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
                state.borrow_mut().finished = true;
                Err(self.throw(e))
            }
            "getreturn" => {
                // getReturn() runs the generator to completion —
                // everything still deferred past yields belongs to
                // that final resume.
                self.gen_flush_out(&state, usize::MAX);
                let st = state.borrow();
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

    /// Materialize an iterable's (key, value) pairs for `yield from`.
    pub fn yield_from_collect(
        &mut self,
        v: &Value,
    ) -> Result<Vec<crate::value::GenItem>, PhpError> {
        match v {
            Value::Array(a) => Ok(a
                .borrow()
                .iter()
                .map(|(k, c)| (key_value(k), c.clone()))
                .collect()),
            Value::Object(o) => {
                if self.obj_is_a(o, "IteratorAggregate") {
                    let it = self.method_invoke(o.clone(), "getIterator", CallArgs::empty())?;
                    return self.yield_from_collect(&it);
                }
                if self.obj_is_a(o, "Iterator")
                    || o.borrow().class.name().eq_ignore_ascii_case("generator")
                {
                    let mut out = Vec::new();
                    let _ = self.method_invoke(o.clone(), "rewind", CallArgs::empty())?;
                    loop {
                        let ok = self
                            .method_invoke(o.clone(), "valid", CallArgs::empty())
                            .map(|v| v.is_truthy())
                            .unwrap_or(false);
                        if !ok {
                            break;
                        }
                        let k = self
                            .method_invoke(o.clone(), "key", CallArgs::empty())
                            .unwrap_or(Value::Null);
                        let val = self
                            .method_invoke(o.clone(), "current", CallArgs::empty())
                            .unwrap_or(Value::Null);
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
                        let _ = self.method_invoke(o.clone(), "next", CallArgs::empty())?;
                    }
                    Ok(out)
                } else {
                    self.fail(PhpError::uncaught(
                        "TypeError",
                        "Argument #1 must be of type Traversable|array",
                        0,
                    ))
                }
            }
            _ => self.fail(PhpError::uncaught(
                "TypeError",
                "Argument #1 must be of type Traversable|array",
                0,
            )),
        }
    }
}
