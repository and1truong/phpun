use std::fmt;

/// A structured error produced by the lexer, parser, or interpreter.
///
/// `kind` maps to the PHP error level the message would be reported under
/// (`Parse error`, `Fatal error`, `Warning`, ...). Line numbers are 1-based.
#[derive(Debug, Clone, PartialEq)]
pub struct PhpError {
    pub kind: ErrorKind,
    pub message: String,
    pub line: usize,
    /// Formatted `#N` stack frames for uncaught-throwable printing
    /// (e.g. `file.php(12): f(Object(A))`). None → bare `#0 {main}`.
    pub trace: Option<Vec<String>>,
    /// Line the `thrown in` footer attributes to (defaults to `line`).
    /// Param-type TypeErrors attribute to the declaration line.
    pub thrown_line: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// `Parse error: ...`
    Parse,
    /// Uncaught throwable (Error / Exception subclasses). Prints the
    /// `Uncaught <Class>: <msg>` + stack-trace block PHP emits.
    Uncaught {
        class: &'static str,
    },
    /// Non-fatal engine/runtime errors surfaced as `Fatal error:`.
    Fatal,
    /// A `throw` unwinding through eval — the exception object travels via
    /// Interp::pending_exception, this kind is just the marker.
    Throw,
    Warning,
    Notice,
    Deprecated,
}

impl PhpError {
    pub fn parse(message: impl Into<String>, line: usize) -> Self {
        Self {
            kind: ErrorKind::Parse,
            message: message.into(),
            line,
            trace: None,
            thrown_line: None,
        }
    }

    pub fn fatal(message: impl Into<String>, line: usize) -> Self {
        Self {
            kind: ErrorKind::Fatal,
            message: message.into(),
            line,
            trace: None,
            thrown_line: None,
        }
    }

    pub fn uncaught(class: &'static str, message: impl Into<String>, line: usize) -> Self {
        Self {
            kind: ErrorKind::Uncaught { class },
            message: message.into(),
            line,
            trace: None,
            thrown_line: None,
        }
    }

    pub fn warning(message: impl Into<String>, line: usize) -> Self {
        Self {
            kind: ErrorKind::Warning,
            message: message.into(),
            line,
            trace: None,
            thrown_line: None,
        }
    }
}

impl fmt::Display for PhpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for PhpError {}
