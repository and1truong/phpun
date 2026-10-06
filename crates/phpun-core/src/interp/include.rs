//! `include`/`require`/`eval` execution and parse-error reporting
//! at the including file.

use super::util::*;
use super::*;

impl<'a> Interp<'a> {
    // ----- include / eval -----

    pub(in crate::interp) fn include(
        &mut self,
        kind: IncludeKind,
        e: &Expr,
    ) -> Result<Value, PhpError> {
        let pathv = self.eval(e)?;
        let path_s = self.conv_str(&pathv)?;
        if kind == IncludeKind::Eval {
            return self.eval_code(&path_s);
        }
        // zend's stream wrapper refuses an empty path outright — a
        // catchable ValueError, not the warning+false open failure.
        if path_s.is_empty() {
            return self.fail(PhpError::uncaught(
                "ValueError",
                "Path must not be empty",
                self.cur_line,
            ));
        }
        // include()/require() appear in backtraces as internal-function
        // frames — even for a failed open (bug28213).
        self.call_trace.push(TraceFrame {
            function: match kind {
                IncludeKind::Include | IncludeKind::IncludeOnce => "include",
                _ => "require",
            }
            .to_string(),
            class: None,
            ty: String::new(),
            file: self.diag_file(),
            line: self.cur_line as u32,
            args: vec![cell(pathv.clone())],
            named_args: Vec::new(),
            internal: true,
        });
        let inc_pop = |it: &mut Interp| {
            it.call_trace.pop();
        };
        // Resolution: include_path entries (`.` = cwd), then the calling
        // file's dir, then cwd (PHP's stream search order).
        let p = std::path::Path::new(&path_s);
        let cands: Vec<std::path::PathBuf> = if p.is_absolute() {
            vec![p.to_path_buf()]
        } else {
            let mut v: Vec<std::path::PathBuf> = Vec::new();
            for part in self
                .ini
                .get("include_path")
                .map(|s| s.as_str())
                .unwrap_or("")
                .split(':')
            {
                if part.is_empty() {
                    continue;
                }
                v.push(std::path::Path::new(part).join(&path_s));
            }
            // The calling file's dir = the file lexically containing the
            // include call (the frame's decl file, not the entry script).
            let base = self
                .stack
                .last()
                .map(|f| f.file.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&self.cur_file);
            let dir = std::path::Path::new(base)
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_default();
            v.push(dir.join(&path_s));
            v.push(std::path::PathBuf::from(&path_s));
            v
        };
        let found = cands.iter().find(|c| c.exists()).cloned();
        let path = match found {
            Some(p) => p,
            None => {
                // PHP emits a pair: the stream failure (path as written)
                // then the generic 'Failed opening' (bug43958).
                let fname = match kind {
                    IncludeKind::Include => "include",
                    IncludeKind::IncludeOnce => "include_once",
                    IncludeKind::Require => "require",
                    _ => "require_once",
                };
                let ip = ".:/home/linuxbrew/.linuxbrew/share/pear";
                let r = self
                    .warn(&format!(
                        "{}({}): Failed to open stream: No such file or directory",
                        fname, path_s,
                    ))
                    .and_then(|_| match kind {
                        // PHP 8.5 emits the generic 'Failed opening'
                        // warning only for include*; require* goes
                        // straight to the uncaught Error (bug35176).
                        IncludeKind::Include | IncludeKind::IncludeOnce => self.warn(&format!(
                            "{}(): Failed opening '{}' for inclusion (include_path='{}')",
                            fname, path_s, ip,
                        )),
                        _ => Ok(()),
                    });
                if let Err(e) = r {
                    inc_pop(self);
                    return Err(e);
                }
                match kind {
                    IncludeKind::Include | IncludeKind::IncludeOnce => {
                        inc_pop(self);
                        return Ok(Value::Bool(false));
                    }
                    _ => {
                        inc_pop(self);
                        // require* failures raise an uncaught Error
                        // (bug35176).
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Failed opening required '{}' (include_path='{}')",
                                path_s, ip
                            ),
                            self.cur_line,
                        ));
                    }
                }
            }
        };
        let canon = path.canonicalize().unwrap_or(path);
        // Zend's include/require backtrace entries carry the RESOLVED
        // canonical path as their arg (trace_arg truncates it to 15
        // chars at render).
        if let Some(f) = self.call_trace.last_mut() {
            f.args[0] = cell(Value::str(canon.display().to_string()));
        }
        if matches!(kind, IncludeKind::IncludeOnce | IncludeKind::RequireOnce) {
            if self.included.contains(&canon) {
                inc_pop(self);
                return Ok(Value::Bool(true));
            }
            self.included.insert(canon.clone());
        }
        let src = match std::fs::read_to_string(&canon) {
            Ok(s) => s,
            Err(e) => {
                let e = e.to_string();
                let _ = self.warn(&format!(
                    "include({}): Failed to open stream: {}",
                    path_s, e
                ));
                inc_pop(self);
                return Ok(Value::Bool(false));
            }
        };
        let fname = canon.display().to_string();
        let stmts = match parser::parse_source(&src, self.ini_on("short_open_tag")) {
            Ok(s) => s,
            Err(e) => {
                match e.kind {
                    ErrorKind::Parse => {
                        // A parse error in the included file raises a
                        // catchable ParseError in Zend — attributed to
                        // the included file — like eval()'d code does.
                        let msg = e.message.clone();
                        // The throwable's own trace omits the include
                        // pseudo-frame (Zend: `#0 file(N): deep()
                        // #1 {main}` — include frames are internal).
                        inc_pop(self);
                        let v = self.exception("ParseError", &msg);
                        if let Value::Object(o) = &v {
                            if let Some(ObjectInternal::Exception {
                                file, line, thrown, ..
                            }) = &mut o.borrow_mut().internal
                            {
                                // getFile() is the bad file; getLine()
                                // is the parse error's own line inside
                                // it (the EOF line for unclosed
                                // brackets — Zend quirk).
                                *file = fname.clone();
                                *line = e.line as u32;
                                *thrown = e.line as u32;
                            }
                        }
                        self.pending_exception = Some(v);
                        return Err(PhpError {
                            trace: None,
                            thrown_line: None,
                            display_msg: None,
                            kind: ErrorKind::Throw,
                            message: "include".into(),
                            line: 0,
                        });
                    }
                    // Non-parse fatals raised while compiling the included
                    // file still attribute to the included file.
                    _ => {
                        self.last_err_file = fname.clone();
                        self.print_fatal(&e);
                    }
                }
                inc_pop(self);
                // A failed compile of the included file is fatal in Zend
                // even through include() — the script dies rather than
                // include returning false (false only covers open/read
                // failures above).
                return Err(PhpError {
                    trace: None,
                    thrown_line: None,
                    display_msg: None,
                    kind: ErrorKind::Fatal,
                    message: "\u{1}exit:255".into(),
                    line: 0,
                });
            }
        };
        // Include executes in the current scope (PHP semantics); the
        // included file's namespace starts global regardless of the
        // includer's (namespaces/ns_069). __FILE__/__DIR__ and diag
        // attribution inside its top-level code bind to the included
        // file, so the executing frame's file swaps with it.
        let saved_file = std::mem::replace(&mut self.cur_file, canon.display().to_string());
        let saved_frame_file = self
            .stack
            .last_mut()
            .map(|f| std::mem::replace(&mut f.file, self.cur_file.clone()));
        let saved_ns = std::mem::take(&mut self.globals.ns);
        // Inside a function frame the file's `namespace` decl writes this
        // slot (not globals.ns) so caller_ns() sees the file's own ns.
        self.include_ns.push((self.stack.len(), String::new()));
        // The included file's Stmt::Line markers move cur_line into its own
        // line space; restore the includer's line so a later call in the same
        // statement still reports the call-site line (gh19653_2).
        let saved_line = self.cur_line;
        // Compile-error flows pop the include pseudo-frame themselves so
        // the backtrace fill sees the same stack Zend prints.
        let mut inc_frame_popped = false;
        // The included unit is compiled separately in Zend —
        // `break`/`continue` operands in it count only ITS enclosing
        // loop/switch contexts, not the includer's.
        let saved_depth = std::mem::replace(&mut self.loop_depth, 0);
        // ...and it is a fresh op_array — a new compile-unit serial
        // for `static` decl site identity. `static` decls in the
        // included unit's own top-level code belong to ITS op_array,
        // so while it executes inside a caller's frame they key under
        // this unit (a fresh table per include execution in Zend);
        // the frame's own table stays untouched.
        let saved_unit = self.begin_unit();
        let saved_su = self
            .stack
            .last_mut()
            .map(|f| f.statics_unit.replace(self.cur_unit_id));
        // A compile diagnostic's handler runs at THIS include's callsite.
        let saved_callsite =
            self.compile_callsite
                .replace((saved_file.clone(), saved_line as u32));
        let flow = match Self::const_closure_gate(&stmts)
            .and_then(|_| self.flow_gate(&stmts))
            .and_then(|_| self.hoist_funcs(&stmts))
        {
            Err(mut e) => {
                // Compile fatals raised while compiling the included file
                // attribute to the included file (cur_file still holds it
                // here), mirroring the eval()'d-code branch. Zend attaches
                // the compile-context backtrace — the live stack minus
                // this include's own pseudo-frame — and prints the block
                // even when it is just `{main}`.
                self.last_err_file = self.cur_file.clone();
                e.trace = Some(self.compile_err_frames());
                inc_pop(self);
                inc_frame_popped = true;
                self.err_flow(e)
            }
            Ok(()) => self.exec_block(&stmts),
        };
        self.compile_callsite = saved_callsite;
        self.loop_depth = saved_depth;
        self.cur_unit_id = saved_unit;
        if let Some(su) = saved_su {
            if let Some(f) = self.stack.last_mut() {
                f.statics_unit = su;
            }
        }
        self.include_ns.pop();
        // break/continue/goto leaking out of the unit are compile fatals
        // in Zend too: same compile-context backtrace (needs our
        // pseudo-frame and the inc file's line still in place) and the
        // same attribution to the included file.
        let flow = match flow {
            Flow::Break(_) | Flow::Continue(_) | Flow::Goto(_) => {
                self.last_err_file = fname.clone();
                let mut e = match &flow {
                    Flow::Goto(l) => PhpError::compile_fatal(
                        format!("'goto' to undefined label '{}'", l),
                        self.cur_line,
                    ),
                    Flow::Continue(_) => PhpError::compile_fatal(
                        "'continue' not in the 'loop' or 'switch' context",
                        self.cur_line,
                    ),
                    _ => PhpError::compile_fatal(
                        "'break' not in the 'loop' or 'switch' context",
                        self.cur_line,
                    ),
                };
                e.trace = Some(self.compile_err_frames());
                inc_pop(self);
                inc_frame_popped = true;
                self.err_flow(e)
            }
            f => f,
        };
        if !inc_frame_popped {
            inc_pop(self);
        }
        self.cur_line = saved_line;
        self.cur_file = saved_file;
        if let Some(old) = saved_frame_file {
            if let Some(f) = self.stack.last_mut() {
                f.file = old;
            }
        }
        self.globals.ns = saved_ns;
        match flow {
            Flow::Return(v) => Ok(v),
            Flow::Normal => Ok(Value::Int(1)),
            Flow::Exit(c) => Err(PhpError {
                trace: None,
                thrown_line: None,
                display_msg: None,
                kind: ErrorKind::Fatal,
                message: format!("\u{1}exit:{}", c),
                line: 0,
            }),
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
            // break/continue/goto escaping the unit already funnelled
            // through the compile-fatal arm above and came back as
            // Exit/Throw — no remaining variants.
            Flow::Break(_) | Flow::Continue(_) | Flow::Goto(_) => unreachable!(),
        }
    }

    pub(in crate::interp) fn eval_code(&mut self, code: &str) -> Result<Value, PhpError> {
        // eval'd code has no <?php tag; strip a leading one defensively.
        let src = code.strip_prefix("<?php").unwrap_or(code).to_string();
        match parser::parse_pure(&src, self.ini_on("short_open_tag")) {
            Ok(stmts) => {
                // Same cur_line clobber as include(): `f(eval(...))` must keep
                // the call-site line for later calls in the statement.
                let saved_line = self.cur_line;
                // Zend compiles eval'd code as its own unit attributed to
                // the call site — `FILE(N) : eval()'d code` — which
                // __FILE__, decl files and every diagnostic read via
                // cur_file (a nested eval composes the context).
                let eval_ctx = format!("{}({}) : eval()'d code", self.cur_file, self.cur_line);
                let saved_file = std::mem::replace(&mut self.cur_file, eval_ctx);
                // eval'd top-level stmts execute in the caller's frame —
                // attribution (throwable file, __FILE__) reads the frame's
                // file, so it swaps to the eval context like include() does.
                let saved_frame_file = self
                    .stack
                    .last_mut()
                    .map(|f| std::mem::replace(&mut f.file, self.cur_file.clone()));
                // eval'd code is its own compile unit — `break`/`continue`
                // operands count only ITS enclosing loop/switch contexts.
                let saved_depth = std::mem::replace(&mut self.loop_depth, 0);
                // ...a fresh op_array: its own unit serial too. The
                // eval'd unit's own `static` decls key under it (fresh
                // table per eval() call, Zend-compiled op_array), even
                // though they bind into the executing frame's vars.
                let saved_unit = self.begin_unit();
                let saved_su = self
                    .stack
                    .last_mut()
                    .map(|f| f.statics_unit.replace(self.cur_unit_id));
                // Zend traces through eval'd code carry a `FILE(N):
                // eval()` frame at the call site (rendered bare — the
                // eval'd source is not an arg in backtraces).
                self.call_trace.push(TraceFrame {
                    function: "eval".to_string(),
                    class: None,
                    ty: String::new(),
                    file: saved_file.clone(),
                    line: saved_line as u32,
                    args: Vec::new(),
                    named_args: Vec::new(),
                    internal: true,
                });
                // A compile diagnostic's handler runs at THIS eval()'s callsite.
                let saved_callsite =
                    self.compile_callsite
                        .replace((saved_file.clone(), saved_line as u32));
                let flow = match Self::const_closure_gate(&stmts)
                    .and_then(|_| self.flow_gate(&stmts))
                    // eval'd code early-binds its unconditional decls
                    // like any compile unit (`eval('a(); function a(){}')`
                    // works; a collision is a compile fatal here).
                    .and_then(|_| self.hoist_funcs(&stmts))
                {
                    Err(mut e) => {
                        // Gate errors are compile fatals of the eval'd
                        // unit — attribute to the eval()'d-code context
                        // and carry the live backtrace (Zend compiles
                        // eval'd code at the call site).
                        self.last_err_file = self.cur_file.clone();
                        e.trace = Some(self.compile_err_frames());
                        self.err_flow(e)
                    }
                    Ok(()) => self.exec_block(&stmts),
                };
                self.compile_callsite = saved_callsite;
                self.loop_depth = saved_depth;
                self.cur_unit_id = saved_unit;
                if let Some(su) = saved_su {
                    if let Some(f) = self.stack.last_mut() {
                        f.statics_unit = su;
                    }
                }
                // break/continue/goto leaking out of the eval'd unit are
                // compile fatals in Zend, attributed to the eval()'d-code
                // context (cur_file/cur_line still hold it here) with the
                // compile-context backtrace — same arm as include().
                let flow = match flow {
                    Flow::Break(_) | Flow::Continue(_) | Flow::Goto(_) => {
                        self.last_err_file = self.cur_file.clone();
                        let mut e = match &flow {
                            Flow::Goto(l) => PhpError::compile_fatal(
                                format!("'goto' to undefined label '{}'", l),
                                self.cur_line,
                            ),
                            Flow::Continue(_) => PhpError::compile_fatal(
                                "'continue' not in the 'loop' or 'switch' context",
                                self.cur_line,
                            ),
                            _ => PhpError::compile_fatal(
                                "'break' not in the 'loop' or 'switch' context",
                                self.cur_line,
                            ),
                        };
                        e.trace = Some(self.compile_err_frames());
                        self.err_flow(e)
                    }
                    f => f,
                };
                self.call_trace.pop();
                self.cur_line = saved_line;
                self.cur_file = saved_file;
                if let Some(old) = saved_frame_file {
                    if let Some(f) = self.stack.last_mut() {
                        f.file = old;
                    }
                }
                match flow {
                    Flow::Return(v) => Ok(v),
                    Flow::Normal => Ok(Value::Null),
                    Flow::Exit(c) => Err(PhpError {
                        trace: None,
                        thrown_line: None,
                        display_msg: None,
                        kind: ErrorKind::Fatal,
                        message: format!("\u{1}exit:{}", c),
                        line: 0,
                    }),
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
                    Flow::Break(_) | Flow::Continue(_) | Flow::Goto(_) => unreachable!(),
                }
            }
            Err(e) => {
                let msg = e.message.clone();
                let v = self.exception("ParseError", &msg);
                if let Value::Object(o) = &v {
                    if let Some(ObjectInternal::Exception {
                        file,
                        line,
                        thrown,
                        eval_ctx,
                        ..
                    }) = &mut o.borrow_mut().internal
                    {
                        // getFile() is the composite eval context
                        // (`FILE(N) : eval()'d code`); getLine() is
                        // the error's own line inside the eval
                        // string. eval_ctx stays >0 as the marker the
                        // uncaught render keys on.
                        *file = format!("{}({}) : eval()'d code", self.cur_file, self.cur_line);
                        *line = e.line as u32;
                        *thrown = e.line as u32;
                        *eval_ctx = e.line as u32;
                    }
                }
                self.pending_exception = Some(v);
                Err(PhpError {
                    trace: None,
                    thrown_line: None,
                    display_msg: None,
                    kind: ErrorKind::Throw,
                    message: "eval".into(),
                    line: 0,
                })
            }
        }
    }
}
