//! Diagnostics: the shared Warning/Notice/Deprecated path, stderr
//! logging, html_errors docref, fatal/uncaught printing and the
//! error -> `Flow` bridge.

use super::util::*;
use super::*;

impl<'a> Interp<'a> {
    /// Shared diagnostic path: Warning/Notice/Deprecated all route through
    /// a user error handler first (PHP semantics); the handler's error —
    /// e.g. a thrown Error2Exception — propagates to the caller (038).
    /// Only a literal `false` return lets the builtin handler continue.
    pub(in crate::interp) fn emit_diag(
        &mut self,
        level: &str,
        errno: i64,
        msg: &str,
    ) -> Result<(), PhpError> {
        // \u{1} in a compile-time diagnostic is the source file — the
        // parser doesn't know it (`{closure:\u{1}:L}`).
        let owned;
        let msg = if msg.contains('\u{1}') {
            owned = msg.replace('\u{1}', &self.diag_file());
            owned.as_str()
        } else {
            msg
        };
        self.emit_diag_x(level, errno, msg).map(|_| ())
    }

    /// emit_diag + reports whether a user handler swallowed it —
    /// trigger_error(E_USER_ERROR) fatals only when unhandled.
    pub(in crate::interp) fn emit_diag_x(
        &mut self,
        level: &str,
        errno: i64,
        msg: &str,
    ) -> Result<bool, PhpError> {
        // \u{1} in a compile-time diagnostic is the source file — the
        // parser doesn't know it (`{closure:\u{1}:L}`).
        let owned;
        let msg = if msg.contains('\u{1}') {
            owned = msg.replace('\u{1}', &self.diag_file());
            owned.as_str()
        } else {
            msg
        };
        if self.error_handler.is_some() && !self.in_handler {
            let h = self.error_handler.clone().unwrap();
            let args: Vec<Cell> = vec![
                cell(Value::Int(errno)),
                cell(Value::str(msg)),
                cell(Value::str(self.diag_file())),
                cell(Value::Int(self.cur_line as i64)),
            ];
            // The handler runs at the CALLER's position for a compile
            // diagnostic — Zend reports the handler-call frame at the
            // eval()/include() callsite, not the in-unit diagnostic
            // line (the $file/$line args still carry the unit's own).
            let saved_site = self.compile_callsite.clone().map(|(f, l)| {
                let sf = std::mem::replace(&mut self.cur_file, f);
                let sl = std::mem::replace(&mut self.cur_line, l as usize);
                (sf, sl)
            });
            self.in_handler = true;
            let r = self.call_value(&h, CallArgs::positional(args));
            self.in_handler = false;
            if let Some((f, l)) = saved_site {
                self.cur_file = f;
                self.cur_line = l;
            }
            match r {
                // A diagnostic while a dim throwable is armed: zend's
                // op keeps running with EG(exception) pending, so the
                // handler's throw REPLACES the armed pair — it doesn't
                // abort the op on the diagnostic's error path.
                Err(e) if e.kind == ErrorKind::Throw && self.dim_throw.is_some() => {
                    let tv = self.pending_exception.take().unwrap_or(Value::Null);
                    let (live, gate) = self
                        .dim_throw
                        .as_ref()
                        .map(|(_, _, l, g)| (*l, *g))
                        .unwrap_or((false, false));
                    self.dim_throw = Some((tv, e, live, gate));
                    return Ok(true);
                }
                Err(e) => return Err(e),
                Ok(v) if !matches!(v, Value::Bool(false)) => return Ok(true),
                Ok(_) => {}
            }
        }
        // `@` silences the built-in display, never the handler —
        // PHP 8 dispatches it with a reduced error_reporting mask.
        if self.silence == 0 {
            self.diag(level, msg);
        }
        Ok(false)
    }

    /// Any active suppression: `@`'s silence or an isset/empty/??
    /// quiet read.
    pub(in crate::interp) fn is_quiet(&self) -> bool {
        self.isset_quiet > 0 || (self.silence > 0 && self.error_handler.is_none())
    }

    pub(in crate::interp) fn warn(&mut self, msg: &str) -> Result<(), PhpError> {
        if self.isset_quiet > 0 {
            return Ok(());
        }
        if self.silence > 0 {
            return if self.error_handler.is_some() {
                self.emit_diag("Warning", 2, msg)
            } else {
                Ok(())
            };
        }
        if self.error_level & 2 == 0 {
            return Ok(());
        }
        self.emit_diag("Warning", 2, msg)
    }

    /// Bypass isset/empty/?? quiet — zend still surfaces offset-key
    /// cast warnings there; only `@` silences them.
    pub(in crate::interp) fn warn_ns(&mut self, msg: &str) -> Result<(), PhpError> {
        if self.silence > 0 {
            return if self.error_handler.is_some() {
                self.emit_diag("Warning", 2, msg)
            } else {
                Ok(())
            };
        }
        if self.error_level & 2 == 0 {
            return Ok(());
        }
        self.emit_diag("Warning", 2, msg)
    }

    /// PHP CLI also logs a `PHP <Level>:` line to stderr (log_errors is
    /// on by default) — buffered separately so it lands after stdout in
    /// the merged PHPT stream.
    /// html_errors=1 switches to the `<b>` docref format (bug35176).
    fn diag(&mut self, level: &str, msg: &str) {
        // PHP logs the `PHP <Level>:` line to stderr first, then writes
        // the display line to stdout — the order is observable on a
        // merged 2>&1 stream.
        self.log_diag(level, msg);
        if self.ini_on("html_errors") {
            let msg = self.docref(msg);
            self.emit(&format!(
                "<br />\n<b>{}</b>:  {} in <b>{}</b> on line <b>{}</b><br />\n",
                level,
                msg,
                self.diag_file(),
                self.cur_line
            ));
        } else {
            self.emit(&format!(
                "\n{}: {} in {} on line {}\n",
                level,
                msg,
                self.diag_file(),
                self.cur_line
            ));
        }
    }

    /// stderr copy of a diagnostic (`PHP Warning: ...`); log_errors
    /// defaults on and error_log to a file would change the destination,
    /// which we don't model yet.
    fn log_diag(&mut self, level: &str, msg: &str) {
        let log_errors = self
            .ini
            .get("log_errors")
            .is_none_or(|v| matches!(v.to_lowercase().as_str(), "1" | "on" | "true" | "yes"));
        if !log_errors {
            return;
        }
        self.diag_stderr(&format!(
            "PHP {}:  {} in {} on line {}\n",
            level,
            msg,
            self.diag_file(),
            self.cur_line
        ));
    }

    /// Route a diagnostic to stderr — streamed in live_io mode so it
    /// interleaves with stdout like real PHP, captured otherwise.
    pub fn diag_stderr(&mut self, s: &str) {
        // Inside a generator run, stderr diag bytes defer with
        // stdout's — Zend emits both at the resume that produced
        // them, so they must not overtake consumer output.
        if self.gen_run_state.is_some() {
            let done = self
                .gen_sink
                .as_ref()
                .map(|s| s.borrow().len())
                .unwrap_or(0);
            if self.gen_horizon_suppresses(done) {
                return;
            }
            if done > 0 {
                let is_fin = self.gen_fin_depth > 0;
                self.gen_buf_out(done - 1, s.as_bytes(), true, is_fin);
                return;
            }
        }
        if self.live_io {
            eprint!("{}", s);
        } else {
            self.err_buf.push_str(s);
        }
    }

    /// html_errors docref: `fn(args): rest` becomes
    /// `fn(args) [<a href='{root}function.{slug}.html'>...</a>]: rest`.
    fn docref(&self, msg: &str) -> String {
        let Some(p) = msg.find("): ") else {
            return msg.to_string();
        };
        let Some(open) = msg.find('(') else {
            return msg.to_string();
        };
        let fname = &msg[..open];
        if fname.is_empty()
            || !fname.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || open > p
        {
            return msg.to_string();
        }
        let args = &msg[open + 1..p];
        let rest = &msg[p + 3..];
        let strip_q = |s: &str| s.trim_matches('"').to_string();
        let root = self
            .ini
            .get("docref_root")
            .map(|s| strip_q(s))
            .unwrap_or_default();
        let ext = self
            .ini
            .get("docref_ext")
            .map(|s| strip_q(s))
            .unwrap_or_else(|| ".html".into());
        let slug = fname.to_lowercase().replace('_', "-");
        format!(
            "{}({}) [<a href='{}function.{}{}'>function.{}{}</a>]: {}",
            fname, args, root, slug, ext, slug, ext, rest
        )
    }

    pub(in crate::interp) fn notice(&mut self, msg: &str) -> Result<(), PhpError> {
        if self.isset_quiet > 0 {
            return Ok(());
        }
        if self.silence > 0 {
            return if self.error_handler.is_some() {
                self.emit_diag("Notice", 8, msg)
            } else {
                Ok(())
            };
        }
        if self.error_level & 8 == 0 {
            return Ok(());
        }
        self.emit_diag("Notice", 8, msg)
    }

    /// Flush notices queued by a just-run comparison (object→number
    /// casts) through the normal E_NOTICE path, in order.
    pub(in crate::interp) fn emit_cmp_notices(&mut self) -> Result<(), PhpError> {
        for m in crate::value::take_cmp_notices() {
            self.notice(&m)?;
        }
        Ok(())
    }

    pub(in crate::interp) fn deprecated(&mut self, msg: &str) -> Result<(), PhpError> {
        if self.isset_quiet > 0 {
            return Ok(());
        }
        if self.silence > 0 {
            return if self.error_handler.is_some() {
                self.emit_diag("Deprecated", 8192, msg)
            } else {
                Ok(())
            };
        }
        if self.error_level & 8192 == 0 {
            return Ok(());
        }
        self.emit_diag("Deprecated", 8192, msg)
    }

    /// Bypass isset/empty/?? quiet — offset-key casts still emit.
    pub(in crate::interp) fn deprecated_ns(&mut self, msg: &str) -> Result<(), PhpError> {
        if self.silence > 0 {
            return if self.error_handler.is_some() {
                self.emit_diag("Deprecated", 8192, msg)
            } else {
                Ok(())
            };
        }
        if self.error_level & 8192 == 0 {
            return Ok(());
        }
        self.emit_diag("Deprecated", 8192, msg)
    }

    /// error_reporting([$level]) — returns previous level. Setting
    /// through the function also writes the ini string back (zend's
    /// ini handler keeps PG(error_reporting) in sync).
    pub fn error_reporting(&mut self, level: Option<i64>) -> i64 {
        let prev = self.error_level;
        if let Some(l) = level {
            self.error_level = l;
            self.ini.insert("error_reporting".into(), l.to_string());
        }
        prev
    }

    pub(in crate::interp) fn print_parse(&mut self, e: &PhpError) {
        let log_errors = self
            .ini
            .get("log_errors")
            .is_none_or(|v| matches!(v.to_lowercase().as_str(), "1" | "on" | "true" | "yes"));
        if log_errors {
            self.diag_stderr(&format!(
                "PHP Parse error:  {} in {} on line {}\n",
                e.message, self.file, e.line
            ));
        }
        self.emit(&format!(
            "\nParse error: {} in {} on line {}\n",
            e.message, self.file, e.line
        ));
    }

    pub(in crate::interp) fn print_fatal(&mut self, e: &PhpError) {
        // A fatal raised while a generator body runs must not be
        // swallowed by the yield output-deferral — flush pending bytes
        // and print with the deferral lifted.
        let run = self.gen_run_state.take();
        if let Some(run) = &run {
            self.gen_flush_out(run, usize::MAX);
        }
        self.print_fatal_inner(e);
        self.gen_run_state = run;
    }

    fn print_fatal_inner(&mut self, e: &PhpError) {
        match e.kind {
            ErrorKind::Uncaught { ref class } => {
                // Zend's display path checks PG(error_reporting) &
                // E_ERROR — error_reporting(0) (or a mask missing
                // bit 0, like -42) silences even uncaught fatals.
                if self.error_level & 1 == 0 {
                    return;
                }
                let frames = e.trace.clone().unwrap_or_default();
                let mut t = String::new();
                for (i, fr) in frames.iter().enumerate() {
                    t.push_str(&format!("#{} {}\n", i, fr));
                }
                t.push_str(&format!("#{} {{main}}\n", frames.len()));
                let ef = if self.last_err_file.is_empty() {
                    self.file.to_string()
                } else {
                    self.last_err_file.clone()
                };
                let dmsg = e.display_msg.clone().unwrap_or_else(|| e.message.clone());
                // stderr log line precedes the stdout display block (same
                // ordering as PHP's error path — see diag()).
                let log_errors = self.ini.get("log_errors").is_none_or(|v| {
                    matches!(v.to_lowercase().as_str(), "1" | "on" | "true" | "yes")
                });
                if log_errors {
                    self.diag_stderr(&format!(
                        "PHP Fatal error:  Uncaught {}: {} in {}:{}\nStack trace:\n{}  thrown in {} on line {}\n",
                        class,
                        dmsg,
                        ef,
                        e.line,
                        t,
                        ef,
                        e.thrown_line.unwrap_or(e.line)
                    ));
                }
                self.emit(&format!(
                    "\nFatal error: Uncaught {}: {} in {}:{}\nStack trace:\n{}  thrown in {} on line {}\n",
                    class,
                    dmsg,
                    ef,
                    e.line,
                    t,
                    ef,
                    e.thrown_line.unwrap_or(e.line)
                ));
            }
            // Plain fatals (E_ERROR) print no trace; compile fatals
            // (duplicate named args, positional-after-named, ...) carry a
            // `Stack trace:\n#0 {main}` block like the engine's.
            _ => {
                let ef = if self.last_err_file.is_empty() {
                    self.file.to_string()
                } else {
                    self.last_err_file.clone()
                };
                let backtraces = self.ini.get("fatal_error_backtraces").is_none_or(|v| {
                    !matches!(v.to_lowercase().as_str(), "0" | "off" | "false" | "no" | "")
                });
                let tr = match &e.trace {
                    Some(frames) if backtraces => {
                        let mut t = String::from("Stack trace:\n");
                        for (i, fr) in frames.iter().enumerate() {
                            t.push_str(&format!("#{} {}\n", i, fr));
                        }
                        t.push_str(&format!("#{} {{main}}\n", frames.len()));
                        t
                    }
                    _ => String::new(),
                };
                let s = format!(
                    "\nFatal error: {} in {} on line {}\n{}",
                    e.message, ef, e.line, tr
                );
                let log_errors = self.ini.get("log_errors").is_none_or(|v| {
                    matches!(v.to_lowercase().as_str(), "1" | "on" | "true" | "yes")
                });
                if log_errors {
                    self.diag_stderr(&format!(
                        "PHP Fatal error:  {} in {} on line {}\n{}",
                        e.message, ef, e.line, tr
                    ));
                }
                if self.mem_exceeded {
                    // Memory-exhausted: buffers are dropped, so the
                    // fatal goes straight to output (bug45392).
                    self.out.extend_from_slice(s.as_bytes());
                } else {
                    self.emit(&s);
                }
            }
        }
    }

    /// Print the uncaught-exception fatal for a Throwable value. Written
    /// straight to `out` — reaching it means the script is ending, so it
    /// must not be re-fed to an ob handler that may throw again
    /// (bug32828).
    pub(in crate::interp) fn uncaught(&mut self, v: &Value) {
        if let Value::Object(o) = v {
            let o = o.borrow();
            let class = o.class.name().to_string();
            // Only the exact builtin ParseError class takes the plain
            // `Parse error:` render — subclasses (and any other
            // Throwable) render the `Uncaught X:` block.
            let is_parse_err = class == "ParseError";
            let msg = o
                .props
                .get("message")
                .map(|c| c.borrow().to_php_string())
                .unwrap_or_default();
            let (file, line, thrown, tr, msg, eval_ctx, mut next) = match &o.internal {
                Some(ObjectInternal::Exception {
                    file,
                    line,
                    trace,
                    thrown,
                    full_msg,
                    eval_ctx,
                    frames,
                    previous,
                }) => (
                    file.clone(),
                    *line,
                    *thrown,
                    if !trace.is_empty() {
                        trace.clone()
                    } else if frames.is_empty() {
                        "#0 {main}".to_string()
                    } else {
                        format_trace(frames)
                    },
                    if full_msg.is_empty() {
                        msg
                    } else {
                        full_msg.clone()
                    },
                    *eval_ctx,
                    previous.clone(),
                ),
                _ => (
                    self.diag_file(),
                    self.cur_line as u32,
                    self.cur_line as u32,
                    "#0 {main}".to_string(),
                    msg,
                    0,
                    None,
                ),
            };
            drop(o);
            // Zend renders a `previous` chain innermost-first: the
            // deepest throwable gets the `Uncaught X:` block and each
            // enclosing level follows as `\nNext X:` (p3/p4).
            let mut chain: Vec<(String, String, String, u32, String)> =
                vec![(class, msg.clone(), file.clone(), line, tr.clone())];
            while let Some(Value::Object(co)) = next {
                let co = co.borrow();
                let cls = co.class.name().to_string();
                let m = co
                    .props
                    .get("message")
                    .map(|c| c.borrow().to_php_string())
                    .unwrap_or_default();
                let (f, l, tr2, dm, p) = match &co.internal {
                    Some(ObjectInternal::Exception {
                        file,
                        line,
                        trace,
                        full_msg,
                        frames,
                        previous,
                        ..
                    }) => (
                        file.clone(),
                        *line,
                        if !trace.is_empty() {
                            trace.clone()
                        } else if frames.is_empty() {
                            "#0 {main}".to_string()
                        } else {
                            format_trace(frames)
                        },
                        if full_msg.is_empty() {
                            m
                        } else {
                            full_msg.clone()
                        },
                        previous.clone(),
                    ),
                    _ => (
                        self.diag_file(),
                        self.cur_line as u32,
                        "#0 {main}".to_string(),
                        m,
                        None,
                    ),
                };
                chain.push((cls, dm, f, l, tr2));
                next = p;
            }
            let mut blocks = String::new();
            for (i, (cls, m, f, l, tr)) in chain.iter().rev().enumerate() {
                let colon = if m.is_empty() { "" } else { ": " };
                if i > 0 {
                    blocks.push('\n');
                }
                let head = if i == 0 { "Uncaught" } else { "Next" };
                blocks.push_str(&format!(
                    "{} {}{}{} in {}:{}\nStack trace:\n{}\n",
                    head, cls, colon, m, f, l, tr
                ));
            }
            let thrown_html = format!("  thrown in <b>{}</b> on line <b>{}</b>", file, thrown);
            blocks.push_str(&format!("  thrown in {} on line {}", file, thrown));
            if eval_ctx > 0 {
                // ParseError inside eval'd code prints the plain
                // `Parse error:` form (tests/lang/019) — `file` is
                // already the `FILE(N) : eval()'d code` composite and
                // eval_ctx the line inside the eval string. The CLI
                // SAPI still logs the `PHP Parse error:` header to
                // stderr first, same as a top-level parse error.
                let log_errors = self.ini.get("log_errors").is_none_or(|v| {
                    matches!(v.to_lowercase().as_str(), "1" | "on" | "true" | "yes")
                });
                if log_errors {
                    self.diag_stderr(&format!(
                        "PHP Parse error:  {} in {} on line {}\n",
                        msg, file, eval_ctx
                    ));
                }
                self.emit(&format!(
                    "\nParse error: {} in {} on line {}\n",
                    msg, file, eval_ctx
                ));
            } else if is_parse_err {
                // Any uncaught ParseError renders Zend's plain
                // `Parse error:` form — the message carries its own
                // position and the in-clause attributes to the bad
                // file — never the 'Uncaught ParseError:' block.
                let log_errors = self.ini.get("log_errors").is_none_or(|v| {
                    matches!(v.to_lowercase().as_str(), "1" | "on" | "true" | "yes")
                });
                if log_errors {
                    self.diag_stderr(&format!(
                        "PHP Parse error:  {} in {} on line {}\n",
                        msg, file, line
                    ));
                }
                self.emit(&format!(
                    "\nParse error: {} in {} on line {}\n",
                    msg, file, line
                ));
            } else if self.error_level & 1 != 0 {
                // error_reporting masks the uncaught display too
                // (see print_fatal) — bit-0 masks silence it.
                // Buffered output precedes the fatal, as PHP's output
                // layer would emit it (bug32828's throwing handler).
                self.flush_ob_all();
                if self.ini_on("html_errors") {
                    let head = blocks
                        .strip_suffix(&format!("  thrown in {} on line {}", file, thrown))
                        .unwrap_or(&blocks);
                    self.out.extend_from_slice(
                        format!(
                            "<br />\n<b>Fatal error</b>:  {}{}<br />\n",
                            head, thrown_html
                        )
                        .as_bytes(),
                    );
                } else {
                    // The PHP CLI SAPI logs the uncaught to stderr first
                    // (log_errors default on), then prints the display
                    // block to stdout — same ordering as print_fatal.
                    let log_errors = self.ini.get("log_errors").is_none_or(|v| {
                        matches!(v.to_lowercase().as_str(), "1" | "on" | "true" | "yes")
                    });
                    if log_errors {
                        self.diag_stderr(&format!("PHP Fatal error:  {}\n", blocks));
                    }
                    // ob_stack is empty here (flushed above), so emit
                    // reaches out-or-stdout like a direct write did.
                    self.emit(&format!("\nFatal error: {}\n", blocks));
                }
            }
        } else {
            self.print_fatal(&PhpError::fatal("Can only throw objects", self.cur_line));
        }
    }

    /// Turn an eval error into control flow. `\u{1}exit:N` is the exit
    /// sentinel; `ErrorKind::Throw` carries pending_exception.
    pub(in crate::interp) fn err_flow(&mut self, mut e: PhpError) -> Flow {
        if let Some(code) = e.message.strip_prefix("\u{1}exit:") {
            return Flow::Exit(code.parse().unwrap_or(0));
        }
        if e.kind == ErrorKind::Throw {
            return Flow::Throw(self.pending_exception.take().unwrap_or(Value::Null));
        }
        // Raise sites that never passed fail() leave last_err_file unset
        // — attribute the fatal to the executing code unit (the frame's
        // file, else the file being included/eval'd) like Zend instead
        // of falling back to the main script.
        if self.last_err_file.is_empty() {
            self.last_err_file = self.diag_file();
        }
        // Zend attaches the live backtrace to runtime fatals: uncaught
        // throwables and compile fatals carry one even when it's just
        // `{main}`; plain E_ERRORs only when a real frame remains.
        if e.trace.is_none() {
            let frames = self.fatal_frames();
            if e.kind != ErrorKind::Fatal || !frames.is_empty() {
                e.trace = Some(frames);
            }
        }
        if self.gen_run_state.is_some() {
            // A fatal inside a running gen body belongs to the dead
            // resume, not the raise site: gen_start picks this up into
            // deferred_err, so the display lands once — after the
            // consumer's echoed bytes, with the resume-stack trace —
            // instead of printing early at the raise site.
            self.gen_pending_fatal = Some(e);
            self.gen_raise_ctx = self.call_trace.clone();
            return Flow::Exit(255);
        }
        self.print_fatal(&e);
        Flow::Exit(255)
    }

    /// Frames for a compile-family fatal's `Stack trace` block: Zend
    /// reports these while the unit is being *compiled* — the compiling
    /// context's own frame (the innermost include/require pseudo-frame,
    /// or the `eval()` frame of the eval'd unit) is excluded, together
    /// with anything pushed above it. An eval'd unit's own compile
    /// fatal drops its eval frame (`#0 {main}` at top level); a unit
    /// included FROM eval'd code keeps it (`#0 FILE(N): eval()`).
    pub(crate) fn compile_err_frames(&self) -> Vec<String> {
        let upto = self
            .call_trace
            .iter()
            .rposition(|f| crate::value::include_frame(f) || (f.internal && f.function == "eval"))
            .unwrap_or(self.call_trace.len());
        let frames: Vec<&crate::value::TraceFrame> = self.call_trace[..upto]
            .iter()
            .rev()
            .filter(|f| !crate::value::trace_frame_hidden(f))
            .collect();
        // The innermost surviving frame is still the executing unit in
        // Zend — an include there renders bare `include()` (a compile
        // error inside `eval` nested in an include shows `#0 f(2):
        // include()`), deeper includes keep their path argument.
        let bare = if frames
            .first()
            .is_some_and(|f| crate::value::include_frame(f))
        {
            Some(0)
        } else {
            None
        };
        frames
            .iter()
            .enumerate()
            .map(|(i, f)| crate::value::trace_frame_str_at(f, Some(i) == bare))
            .collect()
    }

    /// Declaration/linking-time fatals (Cannot redeclare, abstract
    /// method, class-const redefinition, variance, ...) are
    /// compile-class errors in Zend: they always print a `Stack
    /// trace:` block — unlike plain runtime E_ERRORs which show no
    /// trace. These sites all fire at EXEC time (conditional decls),
    /// so the block carries the live call chain — `#0 {main}` at top
    /// level, the real frames inside a function call.
    pub(in crate::interp) fn decl_fatal_ctx(&mut self, mut e: PhpError) -> PhpError {
        if matches!(e.kind, ErrorKind::Fatal) {
            let frames: Vec<&crate::value::TraceFrame> = self
                .call_trace
                .iter()
                .rev()
                .filter(|f| !crate::value::trace_frame_hidden(f))
                .collect();
            // Only the INNERMOST (executing) include renders bare
            // 'include()'; dormant include frames deeper in the chain
            // keep their path argument — 'include(\'path\')' (m8c).
            let bare = if frames
                .first()
                .is_some_and(|f| crate::value::include_frame(f))
            {
                Some(0)
            } else {
                None
            };
            e.trace = Some(
                frames
                    .iter()
                    .enumerate()
                    .map(|(i, f)| crate::value::trace_frame_str_at(f, Some(i) == bare))
                    .collect(),
            );
        }
        e
    }

    /// File diagnostics attribute to: the executing frame's declaring
    /// file, else the file currently being included/run (warnings inside
    /// autoloaded/library code report the library file, not the caller).
    /// Inside eval'd code cur_file is the `FILE(N) : eval()'d code`
    /// context — Zend attributes every diagnostic there.
    pub(in crate::interp) fn diag_file(&self) -> String {
        if self.cur_file.contains("eval()'d code") {
            return self.cur_file.clone();
        }
        self.stack
            .last()
            .map(|f| f.file.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.cur_file.clone())
    }
}
