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
        if self.error_handler.is_some() && !self.in_handler {
            let h = self.error_handler.clone().unwrap();
            let args: Vec<Cell> = vec![
                cell(Value::Int(errno)),
                cell(Value::str(msg)),
                cell(Value::str(self.diag_file())),
                cell(Value::Int(self.cur_line as i64)),
            ];
            self.in_handler = true;
            let r = self.call_value(&h, CallArgs::positional(args));
            self.in_handler = false;
            match r {
                Err(e) => return Err(e),
                Ok(v) if !matches!(v, Value::Bool(false)) => return Ok(()),
                Ok(_) => {}
            }
        }
        self.diag(level, msg);
        Ok(())
    }

    pub(in crate::interp) fn warn(&mut self, msg: &str) -> Result<(), PhpError> {
        if self.silence > 0 || self.error_level & 2 == 0 {
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
        if self.silence > 0 || self.error_level & 8 == 0 {
            return Ok(());
        }
        self.emit_diag("Notice", 8, msg)
    }

    pub(in crate::interp) fn deprecated(&mut self, msg: &str) -> Result<(), PhpError> {
        if self.silence > 0 || self.error_level & 8192 == 0 {
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
        self.emit(&format!(
            "\nParse error: {} in {} on line {}\n",
            e.message, self.file, e.line
        ));
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
    }

    pub(in crate::interp) fn print_fatal(&mut self, e: &PhpError) {
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
            let msg = o
                .props
                .get("message")
                .map(|c| c.borrow().to_php_string())
                .unwrap_or_default();
            let (file, line, thrown, tr, msg, eval_ctx) = match &o.internal {
                Some(ObjectInternal::Exception {
                    file,
                    line,
                    trace,
                    thrown,
                    full_msg,
                    eval_ctx,
                    frames,
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
                ),
                _ => (
                    self.diag_file(),
                    self.cur_line as u32,
                    self.cur_line as u32,
                    "#0 {main}".to_string(),
                    msg,
                    0,
                ),
            };
            drop(o);
            if eval_ctx > 0 {
                // ParseError inside eval'd code prints the plain
                // `Parse error:` form (tests/lang/019).
                self.emit(&format!(
                    "\nParse error: {} in {}({}) : eval()'d code on line {}\n",
                    msg, file, line, eval_ctx
                ));
            } else if self.error_level & 1 != 0 {
                // error_reporting masks the uncaught display too
                // (see print_fatal) — bit-0 masks silence it.
                // Buffered output precedes the fatal, as PHP's output
                // layer would emit it (bug32828's throwing handler).
                self.flush_ob_all();
                // Zend prints `Uncaught C: msg` — no colon when msg empty.
                let colon = if msg.is_empty() { "" } else { ": " };
                if self.ini_on("html_errors") {
                    self.out.extend_from_slice(format!(
                        "<br />\n<b>Fatal error</b>:  Uncaught {}{}{} in {}:{}\nStack trace:\n{}\n  thrown in <b>{}</b> on line <b>{}</b><br />\n",
                        class, colon, msg, file, line, tr, file, thrown
                    ).as_bytes());
                } else {
                    // The PHP CLI SAPI logs the uncaught to stderr first
                    // (log_errors default on), then prints the display
                    // block to stdout — same ordering as print_fatal.
                    let log_errors = self.ini.get("log_errors").is_none_or(|v| {
                        matches!(v.to_lowercase().as_str(), "1" | "on" | "true" | "yes")
                    });
                    if log_errors {
                        self.diag_stderr(&format!(
                            "PHP Fatal error:  Uncaught {}{}{} in {}:{}\nStack trace:\n{}\n  thrown in {} on line {}\n",
                            class, colon, msg, file, line, tr, file, thrown
                        ));
                    }
                    // ob_stack is empty here (flushed above), so emit
                    // reaches out-or-stdout like a direct write did.
                    self.emit(&format!(
                        "\nFatal error: Uncaught {}{}{} in {}:{}\nStack trace:\n{}\n  thrown in {} on line {}\n",
                        class, colon, msg, file, line, tr, file, thrown
                    ));
                }
            }
        } else {
            self.print_fatal(&PhpError::fatal("Can only throw objects", self.cur_line));
        }
    }

    /// Turn an eval error into control flow. `\u{1}exit:N` is the exit
    /// sentinel; `ErrorKind::Throw` carries pending_exception.
    pub(in crate::interp) fn err_flow(&mut self, e: PhpError) -> Flow {
        if let Some(code) = e.message.strip_prefix("\u{1}exit:") {
            return Flow::Exit(code.parse().unwrap_or(0));
        }
        if e.kind == ErrorKind::Throw {
            return Flow::Throw(self.pending_exception.take().unwrap_or(Value::Null));
        }
        self.print_fatal(&e);
        Flow::Exit(255)
    }

    /// File diagnostics attribute to: the executing frame's declaring
    /// file, else the file currently being included/run (warnings inside
    /// autoloaded/library code report the library file, not the caller).
    pub(in crate::interp) fn diag_file(&self) -> String {
        self.stack
            .last()
            .map(|f| f.file.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.cur_file.clone())
    }
}
