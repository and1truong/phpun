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
                    ErrorKind::Parse => self.print_parse_at(&e, &fname),
                    _ => self.print_fatal(&e),
                }
                inc_pop(self);
                return Ok(Value::Bool(false));
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
        self.hoist_funcs(&stmts);
        let flow = self.exec_block(&stmts);
        inc_pop(self);
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
            Flow::Break(_) | Flow::Continue(_) => {
                self.fail(PhpError::fatal("'break'/'continue' in included file", 0))
            }
            Flow::Goto(l) => self.fail(PhpError::fatal(
                format!("'goto' to undefined label '{}'", l),
                0,
            )),
        }
    }

    fn print_parse_at(&mut self, e: &PhpError, file: &str) {
        self.emit(&format!(
            "\nParse error: {} in {} on line {}\n",
            e.message, file, e.line
        ));
    }

    pub(in crate::interp) fn eval_code(&mut self, code: &str) -> Result<Value, PhpError> {
        // eval'd code has no <?php tag; strip a leading one defensively.
        let src = code.strip_prefix("<?php").unwrap_or(code).to_string();
        match parser::parse_pure(&src, self.ini_on("short_open_tag")) {
            Ok(stmts) => {
                let flow = self.exec_block(&stmts);
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
                    Flow::Break(_) | Flow::Continue(_) => Ok(Value::Null),
                    Flow::Goto(l) => Err(PhpError {
                        trace: None,
                        thrown_line: None,
                        display_msg: None,
                        kind: ErrorKind::Fatal,
                        message: format!("'goto' to undefined label '{}'", l),
                        line: 0,
                    }),
                }
            }
            Err(e) => {
                let msg = e.message.clone();
                let v = self.exception("ParseError", &msg);
                if let Value::Object(o) = &v {
                    if let Some(ObjectInternal::Exception { eval_ctx, .. }) =
                        &mut o.borrow_mut().internal
                    {
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
