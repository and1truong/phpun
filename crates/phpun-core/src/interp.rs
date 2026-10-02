//! Tree-walking interpreter. Correctness-first PHP 8.5 semantics.
//!
//! Variables and array elements live in shared cells (`Rc<RefCell<Value>>`)
//! so `&$x` references, `global $x`, `static $x`, `foreach (&$v)` and
//! by-ref params all alias the same storage, like PHP's zval references.

use crate::ast::*;
use crate::builtins;
use crate::error::{ErrorKind, PhpError};
use crate::lexer::StringPart;
use crate::parser;
use crate::value::{
    compare, format_float_repr, identical, numeric, to_key, ArrKey, CallableKind, Cell, Numeric,
    ObjectInternal, PhpArray, PhpCallable, PhpClass, PhpObject, PhpResource, Value,
};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Internal control-flow signals.
pub enum Flow {
    Normal,
    Break(u32),
    Continue(u32),
    Return(Value),
    /// `throw` propagating an exception object.
    Throw(Value),
    Exit(i32),
}

pub struct Frame {
    vars: HashMap<String, Cell>,
    /// Actual call args for func_get_args().
    args: Vec<Cell>,
    /// Enclosing function name (for `static`/`__FUNCTION__`).
    fn_name: String,
    /// `$this` in method calls.
    this_obj: Option<Rc<RefCell<PhpObject>>>,
    /// Class context for self::/static::/parent::.
    scope_class: Option<Rc<PhpClass>>,
    /// Function declared `&name()` — returns bind cells, not values.
    ret_by_ref: bool,
}

impl Frame {
    fn new(fn_name: String) -> Self {
        Self {
            vars: HashMap::new(),
            args: Vec::new(),
            fn_name,
            this_obj: None,
            scope_class: None,
            ret_by_ref: false,
        }
    }
}

pub struct Interp<'a> {
    pub file: &'a str,
    globals: Frame,
    stack: Vec<Frame>,
    pub functions: HashMap<String, Rc<FunctionDecl>>,
    classes: HashMap<String, Rc<PhpClass>>,
    /// Traits by name — their methods are copied into using classes.
    traits: HashMap<String, Rc<ClassDecl>>,
    interfaces: HashMap<String, Rc<ClassDecl>>,
    constants: HashMap<String, Value>,
    /// Accumulated program output (display_errors prints to stdout under
    /// CLI, and the PHPT harness merges streams via 2>&1).
    pub out: String,
    /// Output buffer stack for ob_*().
    ob_stack: Vec<String>,
    /// While >0, warnings are suppressed (implements `??`, `isset`,
    /// `empty`, `@`).
    silence: u32,
    /// Cell returned by the last `&fn()` call (returnByReference tests).
    last_ret_cell: Option<Cell>,
    /// The last invoked function was declared `&name()` (returns by ref).
    last_call_by_ref: bool,
    /// Insertion order of global vars (for $GLOBALS ordering).
    globals_order: Vec<String>,
    /// Shared PhpArray backing $GLOBALS — same cells as globals.vars.
    globals_arr: Option<Rc<RefCell<PhpArray>>>,
    /// Function-scoped static storage: fn name → var → cell.
    statics: HashMap<String, HashMap<String, Cell>>,
    /// Global static vars (`static` at top level).
    global_statics: HashMap<String, Cell>,
    /// include_once/require_once registry (canonical paths).
    included: HashSet<std::path::PathBuf>,
    /// Pending exception carried across an Err(Throw) return.
    pending_exception: Option<Value>,
    /// Pending fatal error message for exceptions raised as PhpError.
    obj_counter: u64,
    res_counter: u64,
    shutdown_fns: Vec<(Value, Vec<Cell>)>,
    error_handler: Option<Value>,
    /// error_reporting() level mask (E_* bits).
    error_level: i64,
    /// putenv() overrides read back by getenv() (no real process-env mutation).
    env_overrides: HashMap<String, String>,
    exception_handler: Option<Value>,
    in_handler: bool,
    /// Current line estimate for error messages (best-effort).
    pub cur_line: usize,
}

/// Result of a top-level program run.
pub struct RunResult {
    pub exit_code: i32,
    /// Set when a fatal error terminated execution.
    pub fatal: Option<PhpError>,
}

impl<'a> Interp<'a> {
    pub fn new(file: &'a str) -> Self {
        let mut constants = HashMap::new();
        constants.insert("PHP_EOL".into(), Value::str("\n"));
        constants.insert("PHP_VERSION".into(), Value::str("8.5.11-phpun"));
        constants.insert("PHP_MAJOR_VERSION".into(), Value::Int(8));
        constants.insert("PHP_MINOR_VERSION".into(), Value::Int(5));
        constants.insert("PHP_OS".into(), Value::str("Linux"));
        constants.insert("PHP_OS_FAMILY".into(), Value::str("Linux"));
        constants.insert("PHP_SAPI".into(), Value::str("cli"));
        constants.insert("DIRECTORY_SEPARATOR".into(), Value::str("/"));
        constants.insert("PHP_INT_MAX".into(), Value::Int(i64::MAX));
        constants.insert("PHP_INT_MIN".into(), Value::Int(i64::MIN));
        constants.insert("PHP_INT_SIZE".into(), Value::Int(8));
        constants.insert("PHP_FLOAT_EPSILON".into(), Value::Float(f64::EPSILON));
        constants.insert("PHP_FLOAT_MAX".into(), Value::Float(f64::MAX));
        constants.insert("PHP_FLOAT_MIN".into(), Value::Float(f64::MIN_POSITIVE));
        constants.insert("NAN".into(), Value::Float(f64::NAN));
        constants.insert("INF".into(), Value::Float(f64::INFINITY));
        constants.insert("M_PI".into(), Value::Float(std::f64::consts::PI));
        constants.insert("PHP_EOL".into(), Value::str("\n"));
        constants.insert("E_ERROR".into(), Value::Int(1));
        constants.insert("E_WARNING".into(), Value::Int(2));
        constants.insert("E_PARSE".into(), Value::Int(4));
        constants.insert("E_NOTICE".into(), Value::Int(8));
        constants.insert("E_DEPRECATED".into(), Value::Int(8192));
        constants.insert("E_ALL".into(), Value::Int(32767));
        constants.insert("E_STRICT".into(), Value::Int(2048));
        constants.insert("E_USER_ERROR".into(), Value::Int(256));
        constants.insert("E_USER_WARNING".into(), Value::Int(512));
        constants.insert("E_USER_NOTICE".into(), Value::Int(1024));
        constants.insert("E_USER_DEPRECATED".into(), Value::Int(16384));
        constants.insert("E_RECOVERABLE_ERROR".into(), Value::Int(4096));
        constants.insert("E_CORE_ERROR".into(), Value::Int(16));
        constants.insert("E_CORE_WARNING".into(), Value::Int(32));
        constants.insert("E_COMPILE_ERROR".into(), Value::Int(64));
        constants.insert("E_COMPILE_WARNING".into(), Value::Int(128));
        let mut it = Self {
            file,
            globals: Frame::new(String::new()),
            last_ret_cell: None,
            last_call_by_ref: false,
            globals_order: Vec::new(),
            globals_arr: None,
            stack: Vec::new(),
            functions: HashMap::new(),
            classes: HashMap::new(),
            traits: HashMap::new(),
            interfaces: HashMap::new(),
            constants,
            out: String::new(),
            ob_stack: Vec::new(),
            silence: 0,
            statics: HashMap::new(),
            global_statics: HashMap::new(),
            included: HashSet::new(),
            pending_exception: None,
            obj_counter: 0,
            res_counter: 0,
            shutdown_fns: Vec::new(),
            error_handler: None,
            error_level: 32767,
            env_overrides: HashMap::new(),
            exception_handler: None,
            in_handler: false,
            cur_line: 1,
        };
        it.register_builtin_classes();
        it
    }

    /// Builtin exception classes + interfaces needed by try/catch.
    fn register_builtin_classes(&mut self) {
        fn throwable_class(name: &str, parent: Option<&str>, props: &[&str]) -> ClassDecl {
            ClassDecl {
                name: name.into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                parent: parent.map(|s| s.to_string()),
                implements: vec!["Throwable".into()],
                traits: vec![],
                methods: vec![
                    method("__construct", &["message", "code", "previous"]),
                    method("getMessage", &[]),
                    method("getCode", &[]),
                    method("getFile", &[]),
                    method("getLine", &[]),
                    method("getTrace", &[]),
                    method("getTraceAsString", &[]),
                    method("getPrevious", &[]),
                    method("__toString", &[]),
                ],
                props: props
                    .iter()
                    .map(|p| PropDecl {
                        name: p.to_string(),
                        default: None,
                        is_static: false,
                        visibility: Visibility::Protected,
                        readonly: false,
                    })
                    .collect(),
                consts: vec![],
            }
        }
        fn method(name: &str, _params: &[&str]) -> Rc<MethodDecl> {
            Rc::new(MethodDecl {
                decl: FunctionDecl {
                    name: name.into(),
                    params: vec![],
                    body: vec![],
                    by_ref: false,
                },
                is_static: false,
                is_abstract: false,
                is_final: false,
                visibility: Visibility::Public,
            })
        }
        let mut reg = |d: ClassDecl, is_iface: bool| {
            let c = Rc::new(PhpClass {
                decl: Rc::new(d),
                statics: RefCell::new(HashMap::new()),
                statics_init: RefCell::new(true),
            });
            if is_iface {
                self.interfaces
                    .insert(c.name().to_lowercase(), c.decl.clone());
            } else {
                self.classes.insert(c.name().to_lowercase(), c);
            }
        };
        // Throwable interface + Error/Exception hierarchy.
        let iface = |name: &str, parents: &[&str], methods: &[&str]| ClassDecl {
            name: name.into(),
            kind: ClassKind::Interface,
            is_abstract: false,
            is_final: false,
            parent: None,
            implements: parents.iter().map(|s| s.to_string()).collect(),
            traits: vec![],
            methods: methods
                .iter()
                .map(|m| {
                    Rc::new(MethodDecl {
                        decl: FunctionDecl {
                            name: m.to_string(),
                            params: vec![],
                            body: vec![],
                            by_ref: false,
                        },
                        is_static: false,
                        is_abstract: true,
                        is_final: false,
                        visibility: Visibility::Public,
                    })
                })
                .collect(),
            props: vec![],
            consts: vec![],
        };
        reg(iface("Throwable", &[], &[]), true);
        reg(iface("Stringable", &[], &["__toString"]), true);
        reg(iface("Traversable", &[], &[]), true);
        reg(
            iface(
                "Iterator",
                &["Traversable"],
                &["current", "key", "next", "rewind", "valid"],
            ),
            true,
        );
        reg(
            iface("IteratorAggregate", &["Traversable"], &["getIterator"]),
            true,
        );
        reg(iface("Countable", &[], &["count"]), true);
        reg(
            iface(
                "ArrayAccess",
                &[],
                &["offsetExists", "offsetGet", "offsetSet", "offsetUnset"],
            ),
            true,
        );
        reg(
            iface("BackedEnum", &["UnitEnum"], &["from", "tryFrom"]),
            true,
        );
        reg(iface("UnitEnum", &[], &["cases"]), true);
        // stdClass — the universal empty object.
        reg(
            ClassDecl {
                name: "stdClass".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                parent: None,
                implements: vec![],
                traits: vec![],
                methods: vec![],
                props: vec![],
                consts: vec![],
            },
            false,
        );
        reg(
            throwable_class("Exception", None, &["message", "code", "file", "line"]),
            false,
        );
        reg(
            throwable_class("Error", None, &["message", "code", "file", "line"]),
            false,
        );
        for (name, parent) in [
            ("ErrorException", "Exception"),
            ("RuntimeException", "Exception"),
            ("LogicException", "Exception"),
            ("InvalidArgumentException", "LogicException"),
            ("LengthException", "LogicException"),
            ("OutOfRangeException", "LogicException"),
            ("UnexpectedValueException", "RuntimeException"),
            ("OutOfBoundsException", "RuntimeException"),
            ("DomainException", "LogicException"),
            ("RangeException", "RuntimeException"),
            ("UnderflowException", "RuntimeException"),
            ("OverflowException", "RuntimeException"),
            ("ValueError", "Error"),
            ("TypeError", "Error"),
            ("ArgumentCountError", "TypeError"),
            ("ArithmeticError", "Error"),
            ("DivisionByZeroError", "ArithmeticError"),
            ("CompileError", "Error"),
            ("ParseError", "CompileError"),
            ("UnhandledMatchError", "Error"),
        ] {
            reg(
                throwable_class(name, Some(parent), &["message", "code", "file", "line"]),
                false,
            );
        }
    }

    pub fn run(&mut self, stmts: &[Stmt]) -> RunResult {
        let flow = self.exec_block(stmts);
        let result = self.finish(flow);
        self.run_shutdown();
        result
    }

    fn finish(&mut self, flow: Flow) -> RunResult {
        match flow {
            Flow::Exit(code) => RunResult {
                exit_code: code,
                fatal: None,
            },
            Flow::Normal | Flow::Return(_) => RunResult {
                exit_code: 0,
                fatal: None,
            },
            Flow::Throw(v) => {
                self.uncaught(&v);
                RunResult {
                    exit_code: 255,
                    fatal: Some(PhpError::fatal("uncaught exception", 0)),
                }
            }
            Flow::Break(_) | Flow::Continue(_) => {
                let e =
                    PhpError::fatal("'break' or 'continue' outside of loop or switch context", 0);
                self.print_fatal(&e);
                RunResult {
                    exit_code: 255,
                    fatal: Some(e),
                }
            }
        }
    }

    fn run_shutdown(&mut self) {
        let fns = std::mem::take(&mut self.shutdown_fns);
        for (f, args) in fns {
            let _ = self.call_value(&f, args);
        }
        self.flush_ob_all();
    }

    /// Convenience: parse+run a source string (used by tests and the CLI).
    pub fn run_source(&mut self, src: &str) -> RunResult {
        match parser::parse(src) {
            Ok(stmts) => self.run(&stmts),
            Err(e) => {
                self.print_parse(&e);
                RunResult {
                    exit_code: 255,
                    fatal: Some(e),
                }
            }
        }
    }

    fn cur(&mut self) -> &mut Frame {
        self.stack.last_mut().unwrap_or(&mut self.globals)
    }

    fn var_get(&mut self, name: &str) -> Result<Value, PhpError> {
        match self.cur().vars.get(name) {
            Some(c) => Ok(c.borrow().clone()),
            None => {
                if self.silence == 0 {
                    self.warn(&format!("Undefined variable ${}", name));
                }
                Ok(Value::Null)
            }
        }
    }

    /// The cell behind a variable name — creating it on demand.
    pub fn var_cell(&mut self, name: &str) -> Cell {
        if name == "GLOBALS" {
            // $GLOBALS is a live view over the global symbol table — array
            // entries share the same Cells as globals.vars so writes alias.
            return cell(self.globals_array_val());
        }
        let is_global = self.stack.is_empty();
        let existed = self.cur().vars.contains_key(name);
        let c = self
            .cur()
            .vars
            .entry(name.to_string())
            .or_insert_with(|| Rc::new(RefCell::new(Value::Null)))
            .clone();
        if is_global && !existed {
            self.globals_order.push(name.to_string());
        }
        c
    }

    /// Shared array backing $GLOBALS, synced both directions with globals.vars.
    fn globals_array_val(&mut self) -> Value {
        let arr = self
            .globals_arr
            .get_or_insert_with(|| Rc::new(RefCell::new(PhpArray::new())))
            .clone();
        // vars -> array (preserve global insertion order)
        let mut names: Vec<String> = self.globals_order.clone();
        for n in self.globals.vars.keys() {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
        {
            let mut a = arr.borrow_mut();
            for n in names {
                if let Some(c) = self.globals.vars.get(&n) {
                    a.set_cell(ArrKey::Str(n.into()), c.clone());
                }
            }
        }
        // array -> vars (writes through $GLOBALS create real globals)
        let pairs: Vec<(String, Cell)> = {
            let a = arr.borrow();
            a.entries
                .iter()
                .filter_map(|(k, c)| match k {
                    ArrKey::Str(s) => Some((s.to_string(), c.clone())),
                    _ => None,
                })
                .collect()
        };
        for (n, c) in pairs {
            if !self.globals.vars.contains_key(&n) {
                self.globals.vars.insert(n.clone(), c);
                self.globals_order.push(n);
            }
        }
        Value::Array(arr)
    }

    /// Peek without creating.
    fn var_cell_opt(&self, name: &str) -> Option<Cell> {
        let f = self.stack.last().unwrap_or(&self.globals);
        f.vars.get(name).cloned()
    }

    fn var_set(&mut self, name: &str, v: Value) {
        match self.var_cell_opt(name) {
            Some(c) => *c.borrow_mut() = v,
            None => {
                self.cur()
                    .vars
                    .insert(name.to_string(), Rc::new(RefCell::new(v)));
            }
        }
    }

    fn fn_statics_key(&self) -> String {
        self.stack
            .last()
            .map(|f| f.fn_name.clone())
            .unwrap_or_else(|| "\u{0}global".into())
    }

    /// Emit output through the output-buffer stack.
    pub fn emit(&mut self, s: &str) {
        if let Some(buf) = self.ob_stack.last_mut() {
            buf.push_str(s);
        } else {
            self.out.push_str(s);
        }
    }

    fn warn(&mut self, msg: &str) {
        if self.silence > 0 || self.error_level & 2 == 0 {
            return;
        }
        if self.error_handler.is_some() && !self.in_handler {
            let h = self.error_handler.clone().unwrap();
            let args: Vec<Cell> = vec![
                cell(Value::Int(2)),
                cell(Value::str(msg)),
                cell(Value::str(self.file)),
                cell(Value::Int(self.cur_line as i64)),
            ];
            self.in_handler = true;
            let r = self.call_value(&h, args);
            self.in_handler = false;
            if let Ok(v) = r {
                if v.is_truthy() {
                    return;
                }
            }
        }
        self.diag("Warning", msg);
    }

    /// Public wrapper so builtins can share PHP's float→int coercion.
    pub fn coerce_int_pub(&mut self, v: &Value) -> i64 {
        self.coerce_int(v)
    }

    /// getenv(): putenv() overrides win over the process environment.
    pub fn getenv_pub(&self, name: &str) -> Option<String> {
        self.env_overrides
            .get(name)
            .cloned()
            .or_else(|| std::env::var(name).ok())
    }

    /// putenv("K=V") → true on success.
    pub fn putenv_pub(&mut self, s: &str) -> bool {
        match s.split_once('=') {
            Some((k, v)) => {
                self.env_overrides.insert(k.to_string(), v.to_string());
                true
            }
            None => false,
        }
    }

    /// PHP CLI also logs a `PHP <Level>:` line to stderr, but PHPT EXPECT
    /// sections only contain the display_errors output: `\n<Level>: msg`.
    fn diag(&mut self, level: &str, msg: &str) {
        self.emit(&format!(
            "\n{}: {} in {} on line {}\n",
            level, msg, self.file, self.cur_line
        ));
    }

    #[allow(dead_code)]
    fn notice(&mut self, msg: &str) {
        if self.silence > 0 || self.error_level & 8 == 0 {
            return;
        }
        self.diag("Notice", msg);
    }

    fn deprecated(&mut self, msg: &str) {
        if self.silence > 0 || self.error_level & 8192 == 0 {
            return;
        }
        self.diag("Deprecated", msg);
    }

    /// error_reporting([$level]) — returns previous level.
    pub fn error_reporting(&mut self, level: Option<i64>) -> i64 {
        let prev = self.error_level;
        if let Some(l) = level {
            self.error_level = l;
        }
        prev
    }

    fn print_parse(&mut self, e: &PhpError) {
        self.emit(&format!(
            "\nParse error: {} in {} on line {}\n",
            e.message, self.file, e.line
        ));
    }

    fn print_fatal(&mut self, e: &PhpError) {
        match e.kind {
            ErrorKind::Uncaught { ref class } => {
                self.emit(&format!(
                    "\nFatal error: Uncaught {}: {} in {}:{}\nStack trace:\n#0 {{main}}\n  thrown in {} on line {}\n",
                    class, e.message, self.file, e.line, self.file, e.line
                ));
            }
            _ => {
                self.emit(&format!(
                    "\nFatal error: {} in {} on line {}\n",
                    e.message, self.file, e.line
                ));
            }
        }
    }

    /// Print the uncaught-exception fatal for a Throwable value.
    fn uncaught(&mut self, v: &Value) {
        if let Value::Object(o) = v {
            let o = o.borrow();
            let class = o.class.name().to_string();
            let msg = o
                .props
                .get("message")
                .map(|c| c.borrow().to_php_string())
                .unwrap_or_default();
            let (file, line) = match &o.internal {
                Some(ObjectInternal::Exception { file, line, .. }) => (file.clone(), *line),
                _ => (self.file.to_string(), self.cur_line as u32),
            };
            self.emit(&format!(
                "\nFatal error: Uncaught {}: {} in {}:{}\nStack trace:\n#0 {{main}}\n  thrown in {} on line {}\n",
                class, msg, file, line, file, line
            ));
        } else {
            self.print_fatal(&PhpError::fatal("Can only throw objects", self.cur_line));
        }
    }

    /// Turn an eval error into control flow. `\u{1}exit:N` is the exit
    /// sentinel; `ErrorKind::Throw` carries pending_exception.
    fn err_flow(&mut self, e: PhpError) -> Flow {
        if let Some(code) = e.message.strip_prefix("\u{1}exit:") {
            return Flow::Exit(code.parse().unwrap_or(0));
        }
        if e.kind == ErrorKind::Throw {
            return Flow::Throw(self.pending_exception.take().unwrap_or(Value::Null));
        }
        self.print_fatal(&e);
        Flow::Exit(255)
    }

    /// Build a throwable object (used for internal errors).
    pub fn exception(&mut self, class: &str, msg: &str) -> Value {
        let resolved = self
            .resolve_class(class)
            .unwrap_or_else(|| "Exception".into());
        let obj = self.instantiate(&resolved, &[]);
        if let Value::Object(o) = &obj {
            let mut o = o.borrow_mut();
            o.props.insert("message".into(), cell(Value::str(msg)));
            o.props.insert("code".into(), cell(Value::Int(0)));
            o.internal = Some(ObjectInternal::Exception {
                file: self.file.to_string(),
                line: self.cur_line as u32,
                trace: String::new(),
            });
            if !o.prop_order.contains(&"message".into()) {
                o.prop_order.push("message".into());
                o.prop_order.push("code".into());
            }
        }
        obj
    }

    /// Raise `throw $v` as an error result.
    fn throw(&mut self, v: Value) -> PhpError {
        self.pending_exception = Some(v);
        PhpError {
            kind: ErrorKind::Throw,
            message: "throw".into(),
            line: self.cur_line,
        }
    }

    /// Builtin call — errors become catchable throwables via `fail`.
    fn call_builtin(&mut self, name: &str, args: &[Cell]) -> Result<Option<Value>, PhpError> {
        match builtins::call(self, name, args) {
            Ok(r) => Ok(r),
            Err(e) => self.fail(e),
        }
    }

    fn fail<T>(&mut self, e: PhpError) -> Result<T, PhpError> {
        if let ErrorKind::Uncaught { class } = e.kind {
            // Internal errors raised as exceptions become real throwables so
            // userland `catch` blocks can intercept them.
            let v = self.exception(class, &e.message);
            self.pending_exception = Some(v);
            return Err(PhpError {
                kind: ErrorKind::Throw,
                message: e.message,
                line: e.line,
            });
        }
        Err(e)
    }

    pub fn exec_block(&mut self, stmts: &[Stmt]) -> Flow {
        for s in stmts {
            match self.exec(s) {
                Flow::Normal => {}
                f => return f,
            }
        }
        Flow::Normal
    }

    fn exec(&mut self, s: &Stmt) -> Flow {
        match s {
            Stmt::Line(l) => {
                self.cur_line = *l;
                Flow::Normal
            }
            Stmt::Inline(t) => {
                self.emit(t);
                Flow::Normal
            }
            Stmt::Echo(args) => {
                for a in args {
                    match self.eval(a) {
                        Ok(v) => match self.conv_str(&v) {
                            Ok(s) => self.emit(&s),
                            Err(e) => return self.err_flow(e),
                        },
                        Err(e) => return self.err_flow(e),
                    }
                }
                Flow::Normal
            }
            Stmt::Expr(e) => match self.eval(e) {
                Ok(_) => Flow::Normal,
                Err(e) => self.err_flow(e),
            },
            Stmt::Block(b) => self.exec_block(b),
            Stmt::If { cond, then, else_ } => match self.eval(cond) {
                Ok(c) => {
                    if c.is_truthy() {
                        self.exec_block(then)
                    } else {
                        self.exec_block(else_)
                    }
                }
                Err(e) => self.err_flow(e),
            },
            Stmt::While { cond, body } => self.exec_while(cond, body, false),
            Stmt::DoWhile { body, cond } => self.exec_while(cond, body, true),
            Stmt::For {
                init,
                cond,
                inc,
                body,
            } => {
                for e in init {
                    if let Err(e) = self.eval(e) {
                        return self.err_flow(e);
                    }
                }
                loop {
                    if !cond.is_empty() {
                        match self.eval(&cond[0]) {
                            Ok(c) if !c.is_truthy() => break,
                            Err(e) => return self.err_flow(e),
                            _ => {}
                        }
                    }
                    match self.exec_block(body) {
                        Flow::Break(0) | Flow::Break(1) => break,
                        Flow::Break(n) => return Flow::Break(n - 1),
                        Flow::Continue(0) | Flow::Continue(1) => {}
                        Flow::Continue(n) => return Flow::Continue(n - 1),
                        Flow::Normal => {}
                        f => return f,
                    }
                    for e in inc {
                        if let Err(e) = self.eval(e) {
                            return self.err_flow(e);
                        }
                    }
                }
                Flow::Normal
            }
            Stmt::Foreach {
                arr,
                key,
                val,
                body,
            } => self.exec_foreach(arr, key, val, body),
            Stmt::Switch { cond, cases } => {
                let cv = match self.eval(cond) {
                    Ok(v) => v,
                    Err(e) => return self.err_flow(e),
                };
                // Find first matching case (loose ==); default is fallback.
                let mut start: Option<usize> = None;
                let mut default_idx: Option<usize> = None;
                for (i, (c, _)) in cases.iter().enumerate() {
                    match c {
                        Some(ce) => {
                            if start.is_none() {
                                match self.eval(ce) {
                                    Ok(v) => {
                                        if compare(&cv, &v) == Ordering::Equal {
                                            start = Some(i);
                                        }
                                    }
                                    Err(e) => return self.err_flow(e),
                                }
                            }
                        }
                        None => default_idx = Some(i),
                    }
                }
                let start = start.or(default_idx);
                if let Some(si) = start {
                    // Run all cases from `start`, stopping at Break.
                    for (_, body) in &cases[si..] {
                        match self.exec_block(body) {
                            Flow::Break(0) | Flow::Break(1) => return Flow::Normal,
                            Flow::Break(n) => return Flow::Break(n - 1),
                            Flow::Normal => {}
                            f => return f,
                        }
                    }
                }
                Flow::Normal
            }
            Stmt::Function(d) => {
                self.functions
                    .insert(d.name.to_lowercase(), Rc::new(d.clone()));
                Flow::Normal
            }
            Stmt::Class(d) => {
                self.register_class(d.clone());
                Flow::Normal
            }
            Stmt::Static(vars) => {
                let key = self.fn_statics_key();
                for (name, default) in vars {
                    let exists = {
                        let table = if self.stack.is_empty() {
                            &self.global_statics
                        } else {
                            self.statics.get(&key).unwrap_or(&self.global_statics)
                        };
                        table.get(name).cloned()
                    };
                    let cellv = match exists {
                        Some(c) => c,
                        None => {
                            let v = match default {
                                Some(d) => self.eval(d).unwrap_or(Value::Null),
                                None => Value::Null,
                            };
                            let c = cell(v);
                            if self.stack.is_empty() {
                                self.global_statics.insert(name.clone(), c.clone());
                            } else {
                                self.statics
                                    .entry(key.clone())
                                    .or_default()
                                    .insert(name.clone(), c.clone());
                            }
                            c
                        }
                    };
                    self.cur().vars.insert(name.clone(), cellv);
                }
                Flow::Normal
            }
            Stmt::Return(e) => {
                let ret_by_ref = self.stack.last().map(|f| f.ret_by_ref).unwrap_or(false);
                if ret_by_ref {
                    if let Some(e) = e {
                        // `function &f() { return $x; }` — the returned cell is
                        // bound, not copied (returnByReference tests).
                        let is_lval = matches!(
                            e,
                            Expr::Var(_)
                                | Expr::Index { .. }
                                | Expr::Prop { .. }
                                | Expr::VarVar(_)
                                | Expr::StaticProp { .. }
                        );
                        if is_lval {
                            let c = match self.eval_cell(e) {
                                Ok(c) => c,
                                Err(e) => return self.err_flow(e),
                            };
                            self.last_ret_cell = Some(c.clone());
                            return Flow::Return(c.borrow().clone());
                        }
                        if matches!(
                            e,
                            Expr::Call { .. } | Expr::MethodCall { .. } | Expr::StaticCall { .. }
                        ) {
                            // `return &f()` chains through when callee returns
                            // by reference (returnByReference.006/009).
                            let (c, was_ref) = match self.eval_call_cell(e) {
                                Ok(t) => t,
                                Err(e) => return self.err_flow(e),
                            };
                            if was_ref {
                                self.last_ret_cell = Some(c.clone());
                            } else {
                                self.notice(
                                    "Only variable references should be returned by reference",
                                );
                            }
                            return Flow::Return(c.borrow().clone());
                        }
                        self.notice("Only variable references should be returned by reference");
                    }
                }
                let v = match e {
                    Some(e) => match self.eval(e) {
                        Ok(v) => v,
                        Err(e) => return self.err_flow(e),
                    },
                    None => Value::Null,
                };
                Flow::Return(v)
            }
            Stmt::Break(e) => {
                let n = match e {
                    Some(e) => self.eval(e).map(|v| v.to_int()).unwrap_or(1).max(1) as u32,
                    None => 1,
                };
                Flow::Break(n)
            }
            Stmt::Continue(e) => {
                let n = match e {
                    Some(e) => self.eval(e).map(|v| v.to_int()).unwrap_or(1).max(1) as u32,
                    None => 1,
                };
                Flow::Continue(n)
            }
            Stmt::Global(name) => {
                // Bind the local name to the global cell.
                let gcell = self
                    .globals
                    .vars
                    .entry(name.clone())
                    .or_insert_with(|| cell(Value::Null))
                    .clone();
                self.cur().vars.insert(name.clone(), gcell);
                Flow::Normal
            }
            Stmt::Unset(xs) => {
                for x in xs {
                    match x {
                        Expr::Var(n) => {
                            self.cur().vars.remove(n);
                        }
                        Expr::Index { e, i } => {
                            let _ = self.unset_index(e, i.as_deref());
                        }
                        Expr::Prop { .. } => {
                            let _ = self.unset_prop(x);
                        }
                        _ => {}
                    }
                }
                Flow::Normal
            }
            Stmt::Try {
                body,
                catches,
                finally,
            } => {
                let flow = self.exec_block(body);
                let out = match flow {
                    Flow::Throw(v) => {
                        let mut result = Flow::Throw(v.clone());
                        for c in catches {
                            if self.catch_matches(&v, &c.types) {
                                if let Some(var) = &c.var {
                                    self.var_set(var, v.clone());
                                }
                                result = self.exec_block(&c.body);
                                break;
                            }
                        }
                        result
                    }
                    f => f,
                };
                if let Some(fb) = finally {
                    match self.exec_block(fb) {
                        Flow::Normal => out,
                        f => f,
                    }
                } else {
                    out
                }
            }
            Stmt::Declare { .. } | Stmt::Namespace(_) | Stmt::Use(_) => Flow::Normal,
        }
    }

    fn catch_matches(&mut self, v: &Value, types: &[String]) -> bool {
        if types.is_empty() {
            return false;
        }
        if let Value::Object(o) = v {
            let cls = o.borrow().class.clone();
            for t in types {
                if self.is_a(&cls, t) {
                    return true;
                }
            }
            false
        } else {
            false
        }
    }

    /// `class X` is-a `name` (name = class or interface), parents included.
    fn is_a(&mut self, cls: &Rc<PhpClass>, name: &str) -> bool {
        let lname = name.trim_start_matches('\\').to_lowercase();
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            if c.name().eq_ignore_ascii_case(&lname) {
                return true;
            }
            for i in &c.decl.implements {
                if i.eq_ignore_ascii_case(&lname) {
                    return true;
                }
                if let Some(iface) = self.interfaces.get(&i.to_lowercase()) {
                    let mut stack = vec![iface.clone()];
                    while let Some(f) = stack.pop() {
                        if f.name.eq_ignore_ascii_case(&lname) {
                            return true;
                        }
                        for p in &f.implements {
                            if let Some(ff) = self.interfaces.get(&p.to_lowercase()) {
                                stack.push(ff.clone());
                            }
                        }
                    }
                }
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        false
    }

    fn exec_while(&mut self, cond: &Expr, body: &[Stmt], do_first: bool) -> Flow {
        if do_first {
            match self.exec_block(body) {
                Flow::Break(0) | Flow::Break(1) => return Flow::Normal,
                Flow::Break(n) => return Flow::Break(n - 1),
                Flow::Normal | Flow::Continue(_) => {}
                f => return f,
            }
        }
        loop {
            match self.eval(cond) {
                Ok(c) if !c.is_truthy() => break,
                Err(e) => return self.err_flow(e),
                _ => {}
            }
            match self.exec_block(body) {
                Flow::Break(0) | Flow::Break(1) => break,
                Flow::Break(n) => return Flow::Break(n - 1),
                Flow::Continue(0) | Flow::Continue(1) => continue,
                Flow::Continue(n) => return Flow::Continue(n - 1),
                Flow::Normal => {}
                f => return f,
            }
        }
        Flow::Normal
    }

    fn exec_foreach(
        &mut self,
        arr: &Expr,
        key: &Option<ForeachKey>,
        val: &ForeachTarget,
        body: &[Stmt],
    ) -> Flow {
        let src = match self.eval(arr) {
            Ok(v) => v,
            Err(e) => return self.err_flow(e),
        };
        match src {
            Value::Array(rc) => {
                let by_ref = matches!(val, ForeachTarget::ByRef(_));
                // Snapshot (key, cell) pairs — PHP iterates a copy for
                // value-iteration but shares cells for &-iteration.
                let snapshot: Vec<(ArrKey, Cell)> = if by_ref {
                    rc.borrow().entries.clone()
                } else {
                    rc.borrow()
                        .entries
                        .iter()
                        .map(|(k, c)| (k.clone(), cell(c.borrow().clone())))
                        .collect()
                };
                for (idx, (k, c)) in snapshot.into_iter().enumerate() {
                    self.cur_line = idx;
                    if let Some(ForeachKey::Var(kn)) = key {
                        self.var_set(kn, key_value(&k));
                    }
                    match val {
                        ForeachTarget::Var(n) => self.var_set(n, c.borrow().clone()),
                        ForeachTarget::ByRef(n) => {
                            self.cur().vars.insert(n.clone(), c);
                        }
                        ForeachTarget::List(items) => {
                            if self.foreach_list(items, &c.borrow().clone()).is_err() {}
                        }
                    }
                    match self.exec_block(body) {
                        Flow::Break(0) | Flow::Break(1) => break,
                        Flow::Break(n) => return Flow::Break(n - 1),
                        Flow::Continue(0) | Flow::Continue(1) => continue,
                        Flow::Continue(n) => return Flow::Continue(n - 1),
                        Flow::Normal => {}
                        f => return f,
                    }
                }
                Flow::Normal
            }
            Value::Object(o) => {
                // IteratorAggregate → getIterator() then iterate that.
                if self.obj_is_a(&o, "IteratorAggregate") {
                    let it_obj = self
                        .method_invoke(o.clone(), "getIterator", vec![])
                        .unwrap_or(Value::Null);
                    match it_obj {
                        Value::Object(io) => return self.exec_foreach_iter(io, key, val, body),
                        _ => return Flow::Normal,
                    }
                }
                if self.obj_is_a(&o, "Iterator") {
                    return self.exec_foreach_iter(o.clone(), key, val, body);
                }
                // Plain object: iterate its props.
                let snapshot: Vec<(String, Cell)> = {
                    let ob = o.borrow();
                    ob.prop_order
                        .iter()
                        .filter_map(|n| ob.props.get(n).map(|c| (n.clone(), c.clone())))
                        .collect()
                };
                for (k, c) in snapshot {
                    if let Some(ForeachKey::Var(kn)) = key {
                        self.var_set(kn, Value::str(k.clone()));
                    }
                    match val {
                        ForeachTarget::Var(n) => self.var_set(n, c.borrow().clone()),
                        ForeachTarget::ByRef(n) => {
                            self.cur().vars.insert(n.clone(), c);
                        }
                        ForeachTarget::List(items) => {
                            let _ = self.foreach_list(items, &c.borrow().clone());
                        }
                    }
                    match self.exec_block(body) {
                        Flow::Break(0) | Flow::Break(1) => break,
                        Flow::Break(n) => return Flow::Break(n - 1),
                        Flow::Continue(0) | Flow::Continue(1) => continue,
                        Flow::Continue(n) => return Flow::Continue(n - 1),
                        Flow::Normal => {}
                        f => return f,
                    }
                }
                Flow::Normal
            }
            _ => {
                self.warn(&format!(
                    "foreach() argument must be of type array|object, {} given",
                    src.gettype()
                ));
                Flow::Normal
            }
        }
    }

    /// foreach over an Iterator: rewind → valid → current/key → next.
    fn exec_foreach_iter(
        &mut self,
        it: Rc<RefCell<PhpObject>>,
        key: &Option<ForeachKey>,
        val: &ForeachTarget,
        body: &[Stmt],
    ) -> Flow {
        let _ = self.method_invoke(it.clone(), "rewind", vec![]);
        loop {
            let ok = self
                .method_invoke(it.clone(), "valid", vec![])
                .map(|v| v.is_truthy())
                .unwrap_or(false);
            if !ok {
                break;
            }
            if let Some(ForeachKey::Var(kn)) = key {
                let k = self
                    .method_invoke(it.clone(), "key", vec![])
                    .unwrap_or(Value::Null);
                self.var_set(kn, k);
            }
            let v = self
                .method_invoke(it.clone(), "current", vec![])
                .unwrap_or(Value::Null);
            match val {
                ForeachTarget::Var(n) => self.var_set(n, v),
                ForeachTarget::ByRef(n) => {
                    let c = cell(v);
                    self.cur().vars.insert(n.clone(), c);
                }
                ForeachTarget::List(items) => {
                    let _ = self.foreach_list(items, &v);
                }
            }
            match self.exec_block(body) {
                Flow::Break(0) | Flow::Break(1) => break,
                Flow::Break(n) => return Flow::Break(n - 1),
                Flow::Continue(0) | Flow::Continue(1) => {}
                Flow::Continue(n) => return Flow::Continue(n - 1),
                Flow::Normal => {}
                f => return f,
            }
            let _ = self.method_invoke(it.clone(), "next", vec![]);
        }
        Flow::Normal
    }

    fn foreach_list(&mut self, items: &[Option<ForeachTarget>], v: &Value) -> Result<(), PhpError> {
        if let Value::Array(a) = v {
            let a = a.borrow();
            for (i, t) in items.iter().enumerate() {
                if let Some(t) = t {
                    let iv = a.get(&ArrKey::Int(i as i64)).unwrap_or(Value::Null);
                    match t {
                        ForeachTarget::Var(n) => self.var_set(n, iv),
                        ForeachTarget::ByRef(n) => self.var_set(n, iv),
                        ForeachTarget::List(sub) => {
                            self.foreach_list(sub, &iv)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // ----- expressions -----

    pub fn eval(&mut self, e: &Expr) -> Result<Value, PhpError> {
        match e {
            Expr::Null => Ok(Value::Null),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Int(i) => Ok(Value::Int(*i)),
            Expr::Float(f) => Ok(Value::Float(*f)),
            Expr::Str(s) => Ok(Value::str(s.clone())),
            Expr::Interp(parts) => {
                let mut s = String::new();
                for p in parts {
                    match p {
                        StringPart::Lit(t) => s.push_str(t),
                        StringPart::Var(name) => {
                            let v = self.var_get(name)?;
                            let cs = self.conv_str(&v)?;
                            s.push_str(&cs);
                        }
                        StringPart::Expr(src) => {
                            let expr = parser::parse_expr_src(src)
                                .map_err(|e| PhpError::parse(e.message, e.line))?;
                            let v = self.eval(&expr)?;
                            s.push_str(&self.conv_str(&v)?);
                        }
                    }
                }
                Ok(Value::str(s))
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
                    let val = self.eval(v)?;
                    match k {
                        Some(ke) => {
                            let kv = self.eval(ke)?;
                            arr.set(to_key(&kv), val);
                        }
                        None => arr.push(val),
                    }
                }
                Ok(Value::Array(Rc::new(RefCell::new(arr))))
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
            Expr::Index { e, i } => self.index_read(e, i.as_deref()),
            Expr::PreInc(t) => self.incdec(t, 1, false),
            Expr::PreDec(t) => self.incdec(t, -1, false),
            Expr::PostInc(t) => self.incdec(t, 1, true),
            Expr::PostDec(t) => self.incdec(t, -1, true),
            Expr::Isset(args) => {
                self.silence += 1;
                let mut ok = true;
                for a in args {
                    match self.isset_eval(a) {
                        Ok(true) => {}
                        Ok(false) => {
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
                let v = self.isset_eval(e);
                self.silence -= 1;
                match v {
                    Ok(true) => {
                        self.silence += 1;
                        let v = self.eval(e);
                        self.silence -= 1;
                        match v {
                            Ok(v) => Ok(Value::Bool(!v.is_truthy())),
                            Err(e) => Err(e),
                        }
                    }
                    Ok(false) => Ok(Value::Bool(true)),
                    Err(e) => Err(e),
                }
            }
            Expr::Print(e) => {
                let v = self.eval(e)?;
                let s = self.conv_str(&v)?;
                self.emit(&s);
                Ok(Value::Int(1))
            }
            Expr::Exit(arg) => {
                let code = if let Some(a) = arg {
                    match self.eval(a)? {
                        Value::Int(i) => i as i32,
                        Value::Str(s) => {
                            self.emit(&s);
                            0
                        }
                        _ => 0,
                    }
                } else {
                    0
                };
                Err(PhpError {
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
                        if identical(&sv, &cv) {
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
                        kind: ErrorKind::Throw,
                        message: "match".into(),
                        line: 0,
                    })
                }
            }
            Expr::Closure(c) => {
                let mut captures = Vec::new();
                if c.arrow {
                    // `fn` captures whole scope by value.
                    let f = self.stack.last().unwrap_or(&self.globals);
                    for (n, cellv) in f.vars.iter() {
                        captures.push((n.clone(), cell(cellv.borrow().clone())));
                    }
                } else {
                    for (n, by_ref) in &c.uses {
                        let cap = if *by_ref {
                            self.var_cell(n)
                        } else {
                            let v = self
                                .var_cell_opt(n)
                                .map(|c| c.borrow().clone())
                                .unwrap_or(Value::Null);
                            cell(v)
                        };
                        captures.push((n.clone(), cap));
                    }
                }
                Ok(Value::Callable(Rc::new(PhpCallable {
                    kind: CallableKind::Closure(Rc::new(c.decl.clone())),
                    captures,
                    this_obj: self.stack.last().and_then(|f| f.this_obj.clone()),
                    scope_class: self.stack.last().and_then(|f| f.scope_class.clone()),
                })))
            }
            Expr::New { class, args } => {
                let name = self.class_name_of(class)?;
                let params = self
                    .classes
                    .get(&name.to_lowercase())
                    .cloned()
                    .and_then(|c| self.find_method_in(&c, "__construct"))
                    .map(|m| m.decl.params.clone())
                    .unwrap_or_default();
                let argvals = self.arg_cells(args, &params, &format!("{}::__construct()", name))?;
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
            Expr::StaticProp { class, name } => self.static_prop_read(class, name),
            Expr::StaticCall { class, name, args } => self.static_call(class, name, args),
            Expr::ClassConst { class, name } => self.class_const(class, name),
            Expr::Clone(e) => {
                let v = self.eval(e)?;
                match v {
                    Value::Object(o) => {
                        let ob = o.borrow();
                        let mut props = HashMap::new();
                        for (k, c) in ob.props.iter() {
                            props.insert(k.clone(), cell(c.borrow().clone()));
                        }
                        let new_obj = PhpObject {
                            class: ob.class.clone(),
                            props,
                            prop_order: ob.prop_order.clone(),
                            id: self.next_obj_id(),
                            internal: match &ob.internal {
                                Some(ObjectInternal::Exception { file, line, trace }) => {
                                    Some(ObjectInternal::Exception {
                                        file: file.clone(),
                                        line: *line,
                                        trace: trace.clone(),
                                    })
                                }
                                _ => None,
                            },
                        };
                        drop(ob);
                        let nv = Value::Object(Rc::new(RefCell::new(new_obj)));
                        // __clone magic
                        if let Value::Object(no) = &nv {
                            if no.borrow().class.find_method("__clone").is_some() {
                                self.method_invoke(no.clone(), "__clone", vec![])?;
                            }
                        }
                        Ok(nv)
                    }
                    _ => {
                        let e = self.exception("Error", "Cannot clone non-object");
                        self.pending_exception = Some(e);
                        Err(PhpError {
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
                    _ => Ok(Value::Bool(false)),
                }
            }
            Expr::AnonClass(decl) => {
                self.register_class(decl.clone());
                Ok(Value::str(decl.name.clone()))
            }
        }
    }

    fn magic(&mut self, m: MagicConst) -> Value {
        match m {
            MagicConst::Line => Value::Int(self.cur_line as i64),
            MagicConst::File => Value::str(self.file),
            MagicConst::Dir => Value::str(
                std::path::Path::new(self.file)
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ),
            MagicConst::Function => Value::str(
                self.stack
                    .last()
                    .map(|f| f.fn_name.clone())
                    .unwrap_or_default(),
            ),
            MagicConst::Method => {
                let f = self.stack.last();
                match f.and_then(|f| {
                    f.scope_class
                        .as_ref()
                        .map(|c| format!("{}::{}", c.name(), f.fn_name))
                }) {
                    Some(s) => Value::str(s),
                    None => Value::str(""),
                }
            }
            MagicConst::Class => Value::str(
                self.stack
                    .last()
                    .and_then(|f| f.scope_class.as_ref().map(|c| c.name().to_string()))
                    .unwrap_or_default(),
            ),
            MagicConst::Namespace => Value::str(""),
        }
    }

    /// Whether the expr is "set" — for isset()/empty() without warnings.
    fn isset_eval(&mut self, e: &Expr) -> Result<bool, PhpError> {
        match e {
            Expr::Var(n) => Ok(match self.var_cell_opt(n) {
                Some(c) => !matches!(*c.borrow(), Value::Null),
                None => false,
            }),
            Expr::Index { e, i } => {
                let base = self.eval(e)?;
                let key = match i {
                    Some(k) => self.eval(k)?,
                    None => return Ok(false),
                };
                match base {
                    Value::Array(a) => Ok(match a.borrow().get(&to_key(&key)) {
                        Some(v) => !matches!(v, Value::Null),
                        None => false,
                    }),
                    Value::Str(s) => {
                        let i = key.to_int();
                        Ok(i >= 0 && (i as usize) < s.len())
                    }
                    _ => Ok(false),
                }
            }
            Expr::Prop { .. } => {
                self.silence += 1;
                let v = self.prop_read_loose(e);
                self.silence -= 1;
                Ok(match v {
                    Ok(v) => !matches!(v, Value::Null),
                    Err(_) => false,
                })
            }
            _ => {
                let v = self.eval(e)?;
                Ok(!matches!(v, Value::Null))
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

    /// Object→string with __toString, plus array warning.
    fn conv_str(&mut self, v: &Value) -> Result<String, PhpError> {
        match v {
            Value::Array(_) => {
                self.warn("Array to string conversion");
                Ok("Array".into())
            }
            Value::Object(o) => {
                let class = o.borrow().class.clone();
                if class.find_method("__tostring").is_some() {
                    let r = self.method_invoke(o.clone(), "__tostring", vec![])?;
                    Ok(r.to_php_string())
                } else {
                    let cname = class.name().to_string();
                    let e = self.exception(
                        "Error",
                        &format!("Object of class {} could not be converted to string", cname),
                    );
                    self.pending_exception = Some(e);
                    Err(PhpError {
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
                    kind: ErrorKind::Throw,
                    message: "cast".into(),
                    line: 0,
                })
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
        if let Some(v) = self.constants.get(key) {
            return Ok(v.clone());
        }
        if let Some(v) = self.constants.get(name) {
            return Ok(v.clone());
        }
        let v = self.exception(
            "Error",
            &format!("Undefined constant \"{}\"", name.trim_start_matches('\\')),
        );
        self.pending_exception = Some(v);
        Err(PhpError {
            kind: ErrorKind::Throw,
            message: "const".into(),
            line: 0,
        })
    }

    fn assign(&mut self, target: &Expr, op: &'static str, value: &Expr) -> Result<Value, PhpError> {
        if op == "=&" {
            // By-reference assignment: bind cells.
            let src = match value {
                Expr::Call { .. } | Expr::MethodCall { .. } | Expr::StaticCall { .. } => {
                    let (c, was_ref) = self.eval_call_cell(value)?;
                    if !was_ref {
                        self.notice("Only variables should be assigned by reference");
                    }
                    c
                }
                _ => self.eval_cell(value)?,
            };
            self.bind_cell(target, src.clone())?;
            return Ok(src.borrow().clone());
        }
        let needs_read = op != "=";
        let rhs = self.eval(value)?;
        let cur = if needs_read {
            self.silence += 1;
            let c = self.eval(target);
            self.silence -= 1;
            c.unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let newv = match op {
            "=" => rhs,
            "+=" => self.arith("+", cur, rhs)?,
            "-=" => self.arith("-", cur, rhs)?,
            "*=" => self.arith("*", cur, rhs)?,
            "/=" => self.arith("/", cur, rhs)?,
            "%=" => self.arith("%", cur, rhs)?,
            ".=" => {
                let l = self.conv_str(&cur)?;
                let r = self.conv_str(&rhs)?;
                Value::str(format!("{}{}", l, r))
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
        self.store(target, newv.clone())?;
        Ok(newv)
    }

    /// Evaluate to a cell (for by-ref semantics): vars and array elements
    /// and object props alias their storage.
    fn eval_cell(&mut self, e: &Expr) -> Result<Cell, PhpError> {
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
    fn eval_call_cell(&mut self, e: &Expr) -> Result<(Cell, bool), PhpError> {
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
    fn bind_cell(&mut self, target: &Expr, src: Cell) -> Result<(), PhpError> {
        match target {
            Expr::Var(n) => {
                self.cur().vars.insert(n.clone(), src);
                Ok(())
            }
            Expr::Index { e, i } => {
                let key = match i {
                    Some(ie) => Some(self.eval(ie)?),
                    None => None,
                };
                match &**e {
                    Expr::Var(n) => {
                        let arr_cell = self.var_cell(n);
                        let mut b = arr_cell.borrow_mut();
                        match &mut *b {
                            Value::Null => {
                                let mut arr = PhpArray::new();
                                match key {
                                    Some(k) => arr.bind_cell(to_key(&k), src),
                                    None => arr.bind_cell(ArrKey::Int(arr.next), src),
                                }
                                *b = Value::Array(Rc::new(RefCell::new(arr)));
                            }
                            Value::Array(rc) => {
                                let mut arr = rc.borrow_mut();
                                match key {
                                    Some(k) => arr.bind_cell(to_key(&k), src),
                                    None => {
                                        let k = ArrKey::Int(arr.next);
                                        arr.bind_cell(k, src);
                                    }
                                }
                            }
                            _ => {
                                drop(b);
                                return self.fail(PhpError::fatal(
                                    "Cannot use scalar value as an array",
                                    0,
                                ));
                            }
                        }
                        Ok(())
                    }
                    _ => self.fail(PhpError::fatal("Cannot create reference to expression", 0)),
                }
            }
            Expr::Prop { .. } | Expr::VarVar(..) => {
                let c = self.eval_cell(target)?;
                *c.borrow_mut() = src.borrow().clone();
                Ok(())
            }
            _ => self.fail(PhpError::fatal("Cannot create reference to expression", 0)),
        }
    }

    fn store(&mut self, target: &Expr, v: Value) -> Result<(), PhpError> {
        match target {
            Expr::Var(name) => {
                self.var_set(name, v);
                Ok(())
            }
            Expr::VarVar(inner) => {
                let n = self.eval(inner)?;
                let name = self.conv_str(&n)?;
                self.var_set(&name, v);
                Ok(())
            }
            Expr::Index { e, i } => self.set_index(e, i.as_deref(), v),
            Expr::List(items) => {
                let vals = match v {
                    Value::Array(a) => {
                        let a = a.borrow();
                        (0..items.len())
                            .map(|i| a.get(&ArrKey::Int(i as i64)).unwrap_or(Value::Null))
                            .collect::<Vec<_>>()
                    }
                    _ => vec![Value::Null; items.len()],
                };
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
                match ov {
                    Value::Object(o) => {
                        let cls = o.borrow().class.clone();
                        if o.borrow().props.contains_key(&pn) || self.declared_prop(&cls, &pn) {
                            o.borrow_mut().props.insert(pn, cell(v));
                            Ok(())
                        } else if cls.find_method("__set").is_some() {
                            self.method_invoke(
                                o.clone(),
                                "__set",
                                vec![cell(Value::str(pn)), cell(v)],
                            )?;
                            Ok(())
                        } else {
                            let mut ob = o.borrow_mut();
                            if !ob.prop_order.contains(&pn) {
                                ob.prop_order.push(pn.clone());
                            }
                            ob.props.insert(pn, cell(v));
                            Ok(())
                        }
                    }
                    _ => {
                        self.warn(&format!(
                            "Attempt to assign property \"{}\" on {}",
                            pn,
                            ov.gettype()
                        ));
                        Ok(())
                    }
                }
            }
            _ => self.fail(PhpError::fatal("Cannot assign to this expression", 0)),
        }
    }

    fn declared_prop(&self, cls: &Rc<PhpClass>, name: &str) -> bool {
        cls.decl
            .props
            .iter()
            .any(|p| p.name == name && !p.is_static)
    }

    /// `$arr[$k] = v` / `$arr[] = v`.
    fn set_index(&mut self, e: &Expr, i: Option<&Expr>, v: Value) -> Result<(), PhpError> {
        let key = match i {
            Some(ie) => Some(self.eval(ie)?),
            None => None,
        };
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
                    }
                    Value::Array(rc) => {
                        // CoW: shared arrays get replaced wholesale by callers
                        // through the cell, so mutate in place via borrow_mut —
                        // PHP semantics: write through to all aliases... PHP
                        // separates unreferenced copies; our Rc aliases share.
                        // For `$a = $b; $a[0]=1` PHP copies. Handle via split:
                        if Rc::strong_count(rc) > 1 {
                            let fresh = rc.borrow().clone();
                            *b = Value::Array(Rc::new(RefCell::new(fresh)));
                            if let Value::Array(rc) = &mut *b {
                                let mut arr = rc.borrow_mut();
                                match key {
                                    Some(k) => arr.set(to_key(&k), v),
                                    None => arr.push(v),
                                }
                            }
                        } else {
                            let mut arr = rc.borrow_mut();
                            match key {
                                Some(k) => arr.set(to_key(&k), v),
                                None => arr.push(v),
                            }
                        }
                    }
                    Value::Str(s) => {
                        let mut bytes = s.as_bytes().to_vec();
                        match key {
                            Some(k) => {
                                let idx = k.to_int() as usize;
                                let vs = self.conv_str(&v).unwrap_or_default();
                                let vb = vs.as_bytes();
                                if idx >= bytes.len() {
                                    bytes.resize(idx + 1, b' ');
                                }
                                bytes[idx] = vb.first().copied().unwrap_or(b' ');
                            }
                            None => bytes.extend_from_slice(v.to_php_string().as_bytes()),
                        }
                        *b = Value::str(String::from_utf8_lossy(&bytes).into_owned());
                    }
                    _ => {
                        drop(b);
                        return self
                            .fail(PhpError::fatal("Cannot use scalar value as an array", 0));
                    }
                }
                Ok(())
            }
            Expr::Index { e: inner, i: ii } => {
                // Nested: $a[0][1] = v — ensure inner is array then recurse.
                let inner_cell = self.index_cell(inner, ii.as_deref())?;
                let mut b = inner_cell.borrow_mut();
                if matches!(*b, Value::Null) {
                    *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
                }
                if let Value::Array(rc) = &mut *b {
                    let mut arr = rc.borrow_mut();
                    match key {
                        Some(k) => arr.set(to_key(&k), v),
                        None => arr.push(v),
                    }
                    Ok(())
                } else {
                    drop(b);
                    self.fail(PhpError::fatal("Cannot use scalar value as an array", 0))
                }
            }
            _ => self.fail(PhpError::fatal("Cannot use expression as array", 0)),
        }
    }

    /// Cell for `$e[$i]` — creates the array/key when assigning.
    fn index_cell(&mut self, e: &Expr, i: Option<&Expr>) -> Result<Cell, PhpError> {
        match e {
            Expr::Var(name) => {
                let arr_cell = self.var_cell(name);
                let mut b = arr_cell.borrow_mut();
                if matches!(*b, Value::Null) {
                    *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
                }
                match &mut *b {
                    Value::Array(rc) => {
                        // $GLOBALS entries must alias the shared array — no CoW.
                        if name != "GLOBALS" && Rc::strong_count(rc) > 1 {
                            let fresh = rc.borrow().clone();
                            *b = Value::Array(Rc::new(RefCell::new(fresh)));
                        }
                        let rc = match &*b {
                            Value::Array(rc) => rc.clone(),
                            _ => unreachable!(),
                        };
                        drop(b);
                        let key = match i {
                            Some(ie) => to_key(&self.eval(ie)?),
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
                    }
                    _ => {
                        drop(b);
                        self.fail(PhpError::fatal("Cannot use scalar value as an array", 0))
                    }
                }
            }
            Expr::Index { e: inner, i: ii } => {
                let c = self.index_cell(inner, ii.as_deref())?;
                let mut b = c.borrow_mut();
                if matches!(*b, Value::Null) {
                    *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
                }
                if let Value::Array(rc) = &mut *b {
                    if Rc::strong_count(rc) > 1 {
                        let fresh = rc.borrow().clone();
                        *b = Value::Array(Rc::new(RefCell::new(fresh)));
                    }
                    let rc = match &*b {
                        Value::Array(rc) => rc.clone(),
                        _ => unreachable!(),
                    };
                    drop(b);
                    let key = match i {
                        Some(ie) => to_key(&self.eval(ie)?),
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
                } else {
                    drop(b);
                    self.fail(PhpError::fatal("Cannot use scalar value as an array", 0))
                }
            }
            _ => {
                // e.g. function call result index — read-only path.
                let v = self.index_read(e, i)?;
                Ok(cell(v))
            }
        }
    }

    fn index_read(&mut self, e: &Expr, i: Option<&Expr>) -> Result<Value, PhpError> {
        let base = self.eval(e)?;
        let key = match i {
            Some(ie) => self.eval(ie)?,
            None => {
                return self.fail(PhpError::fatal("[] used in read context", 0));
            }
        };
        match base {
            Value::Array(rc) => {
                let k = to_key(&key);
                let arr = rc.borrow();
                match arr.get(&k) {
                    Some(v) => Ok(v),
                    None => {
                        let shown = match &key {
                            Value::Str(s) => format!("\"{}\"", s),
                            other => other.to_php_string(),
                        };
                        if self.silence == 0 {
                            self.warn(&format!("Undefined array key {}", shown));
                        }
                        Ok(Value::Null)
                    }
                }
            }
            Value::Str(s) => {
                let idx = key.to_int();
                let bytes = s.as_bytes();
                let idx = if idx < 0 {
                    idx + bytes.len() as i64
                } else {
                    idx
                };
                if idx < 0 || idx as usize >= bytes.len() {
                    if self.silence == 0 {
                        self.warn(&format!("Uninitialized string offset {}", key.to_int()));
                    }
                    Ok(Value::Null)
                } else {
                    Ok(Value::str(
                        String::from_utf8_lossy(&bytes[idx as usize..idx as usize + 1])
                            .into_owned(),
                    ))
                }
            }
            Value::Null => {
                if self.silence == 0 {
                    // PHP 8.5 wording: "Trying to access array offset on null"
                    self.warn("Trying to access array offset on null");
                }
                Ok(Value::Null)
            }
            Value::Object(o) => {
                // ArrayAccess? basic prop fallback — try get
                let _ = o;
                if self.silence == 0 {
                    self.warn(&format!("Cannot use object of type {} as array", ""));
                }
                Ok(Value::Null)
            }
            _ => {
                if self.silence == 0 {
                    self.warn(&format!(
                        "Trying to access array offset on {}",
                        base.type_name().to_lowercase()
                    ));
                }
                Ok(Value::Null)
            }
        }
    }

    fn unset_index(&mut self, e: &Expr, i: Option<&Expr>) -> Result<(), PhpError> {
        let key = match i {
            Some(ie) => Some(self.eval(ie)?),
            None => None,
        };
        match e {
            Expr::Var(name) => {
                if let Some(c) = self.var_cell_opt(name) {
                    let mut b = c.borrow_mut();
                    if let Value::Array(rc) = &mut *b {
                        if let Some(k) = key {
                            rc.borrow_mut().unset(&to_key(&k));
                        }
                    }
                }
                Ok(())
            }
            Expr::Index { e: inner, i: ii } => {
                if let Ok(c) = self.index_cell(inner, ii.as_deref()) {
                    let mut b = c.borrow_mut();
                    if let Value::Array(rc) = &mut *b {
                        if let Some(k) = key {
                            rc.borrow_mut().unset(&to_key(&k));
                        }
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn incdec(&mut self, target: &Expr, delta: i64, post: bool) -> Result<Value, PhpError> {
        let old = match target {
            Expr::Var(name) => {
                self.silence += 1;
                let v = self.var_get(name).unwrap_or(Value::Null);
                self.silence -= 1;
                v
            }
            Expr::Index { e, i } => {
                self.silence += 1;
                let v = self.index_read(e, i.as_deref()).unwrap_or(Value::Null);
                self.silence -= 1;
                v
            }
            Expr::Prop { .. } => {
                self.silence += 1;
                let v = self.prop_read_loose(target).unwrap_or(Value::Null);
                self.silence -= 1;
                v
            }
            _ => {
                return self.fail(PhpError::fatal(
                    "Cannot increment/decrement non-variable",
                    0,
                ))
            }
        };
        let new = self.incdec_value(&old, delta);
        self.store(target, new.clone())?;
        Ok(if post { old } else { new })
    }

    /// PHP inc/dec semantics: null++ = 1, null-- = null, strings increment
    /// alphanumerically (Perl-style), numeric strings go numeric.
    fn incdec_value(&mut self, v: &Value, delta: i64) -> Value {
        match v {
            Value::Null => {
                if delta > 0 {
                    Value::Int(1)
                } else {
                    Value::Null
                }
            }
            Value::Bool(_) => v.clone(), // bools don't change
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
                        );
                        Value::str(perl_inc(s))
                    } else {
                        self.deprecated(
                            "Decrement on non-numeric string is deprecated, use str_decrement() instead",
                        );
                        v.clone()
                    }
                }
            },
            _ => v.clone(),
        }
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
                            self.warn("A non-numeric value encountered");
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
                    other => match numeric(&other.to_php_string()) {
                        Numeric::Int(i) => Value::Int(i),
                        Numeric::Float(f) => Value::Float(f),
                        Numeric::Leading(f, is_int) => {
                            self.warn("A non-numeric value encountered");
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
                    Value::Str(s) => Ok(Value::str(String::from_utf8_lossy(
                        &s.bytes().map(|b| !b).collect::<Vec<u8>>(),
                    ))),
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
                self.silence += 1;
                let lv = self.eval(l);
                self.silence -= 1;
                match lv? {
                    Value::Null => self.eval(r),
                    v => Ok(v),
                }
            }
            "." => {
                let lv = self.eval(l)?;
                let rv = self.eval(r)?;
                let ls = self.conv_str(&lv)?;
                let rs = self.conv_str(&rv)?;
                Ok(Value::str(format!("{}{}", ls, rs)))
            }
            "==" | "!=" | "===" | "!==" | "<" | "<=" | ">" | ">=" | "<=>" => {
                let lv = self.eval(l)?;
                let rv = self.eval(r)?;
                Ok(self.compare_op(op, &lv, &rv))
            }
            "named" => self.eval(r), // named-arg marker: value passthrough
            _ => {
                let lv = self.eval(l)?;
                let rv = self.eval(r)?;
                self.arith(op, lv, rv)
            }
        }
    }

    fn compare_op(&self, op: &str, a: &Value, b: &Value) -> Value {
        // NaN is unordered: every ordered comparison is false, <=> is -1.
        let nan = matches!((a, b), (Value::Float(f), _) | (_, Value::Float(f)) if f.is_nan());
        if nan {
            return match op {
                "===" | "!==" => Value::Bool((op == "!==") != identical(a, b)),
                "==" | "!=" => Value::Bool(op == "!="),
                "<=>" => Value::Int(-1),
                _ => Value::Bool(false),
            };
        }
        match op {
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
            ">" => Value::Bool(compare(a, b) == Ordering::Greater),
            ">=" => Value::Bool(compare(a, b) != Ordering::Less),
            _ => unreachable!(),
        }
    }

    /// Arithmetic / bitwise with PHP numeric-string coercion.
    fn arith(&mut self, op: &str, l: Value, r: Value) -> Result<Value, PhpError> {
        match op {
            "&" | "|" | "^" => {
                if let (Value::Str(a), Value::Str(b)) = (&l, &r) {
                    return Ok(Value::str(bitwise_str(op, a, b)));
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

        let (ln, warn_l) = self.num(&l);
        let (rn, warn_r) = self.num(&r);
        if warn_l {
            self.warn("A non-numeric value encountered");
        }
        if warn_r {
            self.warn("A non-numeric value encountered");
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
                    Num::F(f) => coerce_float(f, |m| self.warn(m)),
                };
                let b = match rn {
                    Num::I(i) => i,
                    Num::F(f) => coerce_float(f, |m| self.warn(m)),
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
            "**" => Value::Float(ln.to_float().powf(rn.to_float())),
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
                Numeric::Float(f) => return Ok(coerce_float(f, |m| self.warn(m))),
                Numeric::Leading(f, _) => {
                    self.warn("A non-numeric value encountered");
                    return Ok(coerce_float(f, |m| self.warn(m)));
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
    fn coerce_int(&mut self, v: &Value) -> i64 {
        let f = match v {
            Value::Float(f) => *f,
            Value::Str(s) => match numeric(s) {
                Numeric::Float(f) | Numeric::Leading(f, _) => f,
                _ => return v.to_int(),
            },
            _ => return v.to_int(),
        };
        coerce_float(f, |msg| self.warn(msg))
    }

    /// `(type)expr` cast.
    fn cast(&mut self, kind: CastKind, v: Value) -> Result<Value, PhpError> {
        Ok(match kind {
            CastKind::Int => Value::Int(self.coerce_int(&v)),
            CastKind::Float => Value::Float(v.to_float()),
            CastKind::Bool => Value::Bool(v.is_truthy()),
            CastKind::Unset => Value::Null,
            CastKind::String => Value::str(self.conv_str(&v)?),
            CastKind::Array => match v {
                Value::Array(_) => v,
                Value::Null => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
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
                            for (k, c) in a.borrow().entries.iter() {
                                let name = match k {
                                    ArrKey::Int(i) => i.to_string(),
                                    ArrKey::Str(s) => s.to_string(),
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
                    Value::Object(Rc::new(RefCell::new(PhpObject {
                        class: cls,
                        props,
                        prop_order: order,
                        id: self.next_obj_id(),
                        internal: None,
                    })))
                }
            },
        })
    }

    // ----- calls -----

    fn call(&mut self, name: &Expr, args: &[Expr]) -> Result<Value, PhpError> {
        // Resolve callee name/value.
        let fname = match name {
            Expr::Str(s) => s.to_string(),
            Expr::Var(_) | Expr::VarVar(_) => {
                let v = self.eval(name)?;
                match v {
                    Value::Callable(_) | Value::Object(_) => {
                        // $closure() / $obj->__invoke()
                        let vals = self.arg_cells(args, &[], "")?;
                        return self.call_value(&v, vals);
                    }
                    _ => self.conv_str(&v).unwrap_or_default(),
                }
            }
            Expr::StaticProp { class, name } => {
                // `C::$var()` — dynamic static method call.
                let cls = self.class_of(class)?;
                let mn = {
                    let v = self.eval(&Expr::Var(name.clone()))?;
                    self.conv_str(&v)?
                };
                let params = self
                    .find_method_in(&cls, &mn)
                    .map(|m| m.decl.params.clone())
                    .unwrap_or_default();
                let vals = self.arg_cells(args, &params, &format!("{}()", mn))?;
                return self.static_invoke(cls, &mn, vals);
            }
            Expr::Prop { .. } | Expr::MethodCall { .. } | Expr::Index { .. } => {
                let v = self.eval(name)?;
                let vals = self.arg_cells(args, &[], "")?;
                return self.call_value(&v, vals);
            }
            _ => {
                let v = self.eval(name)?;
                self.conv_str(&v).unwrap_or_default()
            }
        };
        self.call_named(&fname, args)
    }

    /// Evaluate args into cells (by-ref params alias caller storage).
    /// `named` params collected as (name, cell) too.
    fn arg_cells(
        &mut self,
        args: &[Expr],
        decl: &[Param],
        ctx: &str,
    ) -> Result<Vec<Cell>, PhpError> {
        let mut out = Vec::with_capacity(args.len());
        for (i, a) in args.iter().enumerate() {
            // named argument wrapper
            let a = match a {
                Expr::Binary { op: "named", r, .. } => r.as_ref(),
                _ => a,
            };
            let by_ref = decl.get(i).map(|p| p.by_ref).unwrap_or(false);
            if by_ref {
                match a {
                    Expr::Var(_) | Expr::Index { .. } | Expr::Prop { .. } | Expr::VarVar(_) => {
                        match self.eval_cell(a) {
                            Ok(c) => out.push(c),
                            Err(_) => {
                                return self.fail(PhpError::fatal(
                                    "Only variables should be passed by reference",
                                    0,
                                ))
                            }
                        }
                    }
                    Expr::Assign {
                        op: "=&", target, ..
                    } => {
                        // `f($x =& v)` binds the target by reference
                        // (passByReference_010); plain `=` throws Error below.
                        self.eval(a)?;
                        out.push(self.eval_cell(target)?);
                    }
                    Expr::Call { .. } | Expr::MethodCall { .. } | Expr::StaticCall { .. } => {
                        // `f(g())`: binds only when g() returns by reference,
                        // otherwise a notice and pass by value (passByReference_004/007).
                        let (c, was_ref) = self.eval_call_cell(a)?;
                        if !was_ref {
                            self.notice("Only variables should be passed by reference");
                        }
                        out.push(c);
                    }
                    _ => {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "{}: Argument #{} (${}) could not be passed by reference",
                                ctx,
                                i + 1,
                                decl.get(i).map(|p| p.name.as_str()).unwrap_or("")
                            ),
                            0,
                        ))
                    }
                }
            } else {
                match a {
                    Expr::Var(_) | Expr::Index { .. } | Expr::Prop { .. } => {
                        // Even by-value args evaluated once; fresh cell wraps
                        // a clone so callee can't alias caller storage.
                        {
                            let v = self.eval(a)?;
                            out.push(cell(v))
                        }
                    }
                    _ => {
                        let v = self.eval(a)?;
                        out.push(cell(v))
                    }
                }
            }
        }
        Ok(out)
    }

    /// Call a named function (builtin or user-defined).
    fn call_named(&mut self, fname: &str, args: &[Expr]) -> Result<Value, PhpError> {
        let lname = fname.trim_start_matches('\\').to_lowercase();
        let decl = self.functions.get(&lname).cloned();
        // Synthetic params carrying builtin by-ref flags so call results in
        // by-ref slots emit "Only variables should be passed by reference"
        // (passByReference_012, array_shift(array_shift($a))).
        let builtin_params: Vec<Param> = if decl.is_none() {
            builtin_byref(&lname)
                .map(|flags| {
                    flags
                        .iter()
                        .map(|by_ref| Param {
                            name: String::new(),
                            default: None,
                            by_ref: *by_ref,
                            variadic: false,
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
            &format!("{}()", fname),
        )?;
        if let Some(v) = self.call_builtin(&lname, &argvals)? {
            return Ok(v);
        }
        let decl = match decl {
            Some(d) => d,
            None => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Call to undefined function {}()", fname),
                    0,
                ))
            }
        };
        self.invoke_fn(&decl, argvals, None, None)
    }

    /// Call any callable-ish Value: Callable, string name, [obj,'m'], obj
    /// with __invoke.
    pub fn call_value(&mut self, v: &Value, args: Vec<Cell>) -> Result<Value, PhpError> {
        match v {
            Value::Callable(c) => {
                match &c.kind {
                    CallableKind::Closure(decl) => {
                        let mut frame_args = Vec::new();
                        let mut frame = Frame::new("{closure}".into());
                        frame.ret_by_ref = decl.by_ref;
                        for (n, cap) in &c.captures {
                            frame.vars.insert(n.clone(), cap.clone());
                        }
                        frame.this_obj = c.this_obj.clone();
                        frame.scope_class = c.scope_class.clone();
                        let decl = decl.clone();
                        self.stack.push(frame);
                        // bind params manually (frame already pushed for captures)

                        self.bind_and_run(&decl, args, frame_args.split_off(0))
                    }
                    CallableKind::Named(n) => {
                        if let Some(v) = self.call_builtin(&n.to_lowercase(), &args)? {
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
                            Some(cls) => self.static_invoke(cls.clone(), name, args),
                            None => self.fail(PhpError::fatal("bad callable", 0)),
                        },
                    },
                }
            }
            Value::Str(s) => {
                let name = s.to_string();
                // "Class::method" string callables
                if let Some((cls, m)) = name.split_once("::") {
                    if let Some(c) = self.resolve_class(cls) {
                        let cls = self.classes.get(&c.to_lowercase()).cloned();
                        if let Some(cls) = cls {
                            return self.static_invoke(cls, m, args);
                        }
                    }
                }
                if let Some(v) = self.call_builtin(&name.to_lowercase(), &args)? {
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
                // [$obj, 'method'] or ['Class', 'method']
                let a = a.borrow();
                let o0 = a.get(&ArrKey::Int(0));
                let m = a.get(&ArrKey::Int(1));
                match (o0, m) {
                    (Some(t), Some(mv)) => {
                        let mname = mv.to_php_string();
                        match t {
                            Value::Object(o) => self.method_invoke(o.clone(), &mname, args),
                            Value::Str(cn) => {
                                let cls = self
                                    .resolve_class(&cn)
                                    .and_then(|c| self.classes.get(&c.to_lowercase()).cloned());
                                match cls {
                                    Some(cls) => self.static_invoke(cls, &mname, args),
                                    None => self.fail(PhpError::uncaught(
                                        "Error",
                                        format!("Class \"{}\" not found", cn),
                                        0,
                                    )),
                                }
                            }
                            _ => self.fail(PhpError::fatal("invalid callable array", 0)),
                        }
                    }
                    _ => self.fail(PhpError::fatal("invalid callable array", 0)),
                }
            }
            Value::Object(o) => {
                if o.borrow().class.find_method("__invoke").is_some() {
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

    /// Params binding + body run for a pushed frame context (closures).
    fn bind_and_run(
        &mut self,
        decl: &FunctionDecl,
        args: Vec<Cell>,
        _unused: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        // Restore caller line after the callee runs — the callee's Stmt::Line
        // markers overwrite cur_line, but diagnostics after the call report
        // the call site's line (passByReference_007).
        let saved_line = self.cur_line;
        self.last_call_by_ref = decl.by_ref;
        let r = self.bind_and_run_inner(decl, args, _unused);
        // Overwrite (don't restore): the flag must describe THIS callee even
        // though nested calls overwrote it during the body.
        self.last_call_by_ref = decl.by_ref;
        self.cur_line = saved_line;
        r
    }

    fn bind_and_run_inner(
        &mut self,
        decl: &FunctionDecl,
        args: Vec<Cell>,
        _unused: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        let required = decl
            .params
            .iter()
            .filter(|p| p.default.is_none() && !p.variadic)
            .count();
        if args.len() < required {
            self.stack.pop();
            return self.fail(PhpError::uncaught(
                "ArgumentCountError",
                format!("Too few arguments to function, {} passed", args.len()),
                0,
            ));
        }
        {
            // Compute param bindings first (defaults may eval exprs that
            // need &mut self).
            let mut binds: Vec<(String, Cell)> = Vec::new();
            for (i, p) in decl.params.iter().enumerate() {
                if p.variadic {
                    let mut arr = PhpArray::new();
                    for v in &args[i.min(args.len())..] {
                        arr.push(v.borrow().clone());
                    }
                    binds.push((
                        p.name.clone(),
                        cell(Value::Array(Rc::new(RefCell::new(arr)))),
                    ));
                } else if let Some(v) = args.get(i) {
                    if p.by_ref {
                        binds.push((p.name.clone(), v.clone()));
                    } else {
                        binds.push((p.name.clone(), cell(v.borrow().clone())));
                    }
                } else if let Some(d) = &p.default {
                    let dv = self.eval(d).unwrap_or(Value::Null);
                    binds.push((p.name.clone(), cell(dv)));
                } else {
                    binds.push((p.name.clone(), cell(Value::Null)));
                }
            }
            let frame = self.stack.last_mut().unwrap();
            for (n, c) in binds {
                frame.vars.insert(n, c);
            }
            frame.args = args;
        }
        let flow = self.exec_block(&decl.body);
        self.stack.pop();
        match flow {
            Flow::Return(v) => Ok(v),
            Flow::Throw(v) => {
                self.pending_exception = Some(v);
                Err(PhpError {
                    kind: ErrorKind::Throw,
                    message: "throw".into(),
                    line: 0,
                })
            }
            Flow::Exit(c) => Err(PhpError {
                kind: ErrorKind::Fatal,
                message: format!("\u{1}exit:{}", c),
                line: 0,
            }),
            Flow::Break(_) | Flow::Continue(_) => self.fail(PhpError::fatal(
                "'break' or 'continue' outside of loop or switch context",
                0,
            )),
            Flow::Normal => Ok(Value::Null),
        }
    }

    fn invoke_fn(
        &mut self,
        decl: &Rc<FunctionDecl>,
        args: Vec<Cell>,
        this_obj: Option<Rc<RefCell<PhpObject>>>,
        scope_class: Option<Rc<PhpClass>>,
    ) -> Result<Value, PhpError> {
        let required = decl
            .params
            .iter()
            .filter(|p| p.default.is_none() && !p.variadic)
            .count();
        if args.len() < required {
            return self.fail(PhpError::uncaught(
                "ArgumentCountError",
                format!(
                    "Too few arguments to function {}(), {} passed in {} on line {} and {} {} expected",
                    decl.name,
                    args.len(),
                    self.file,
                    self.cur_line,
                    if required == decl.params.len() { "exactly" } else { "at least" },
                    required
                ),
                0,
            ));
        }
        let mut frame = Frame::new(decl.name.clone());
        frame.ret_by_ref = decl.by_ref;
        if let Some(obj) = &this_obj {
            frame
                .vars
                .insert("this".to_string(), cell(Value::Object(obj.clone())));
        }
        frame.this_obj = this_obj;
        frame.scope_class = scope_class;
        self.stack.push(frame);
        self.bind_and_run(decl, args, Vec::new())
    }

    // ----- classes -----

    fn register_class(&mut self, decl: Rc<ClassDecl>) {
        let lname = decl.name.to_lowercase();
        match decl.kind {
            ClassKind::Interface => {
                self.interfaces.insert(lname, decl);
            }
            ClassKind::Trait => {
                self.traits.insert(lname, decl);
            }
            _ => {
                // Apply traits: merge methods/props into the decl.
                let mut d = (*decl).clone();
                if !d.traits.is_empty() {
                    for t in d.traits.clone() {
                        if let Some(td) = self.traits.get(&t.to_lowercase()) {
                            for m in &td.methods {
                                if !d
                                    .methods
                                    .iter()
                                    .any(|x| x.decl.name.eq_ignore_ascii_case(&m.decl.name))
                                {
                                    d.methods.push(m.clone());
                                }
                            }
                            for p in &td.props {
                                if !d.props.iter().any(|x| x.name == p.name) {
                                    d.props.push(p.clone());
                                }
                            }
                        }
                    }
                }
                self.classes.insert(
                    lname,
                    Rc::new(PhpClass {
                        decl: Rc::new(d),
                        statics: RefCell::new(HashMap::new()),
                        statics_init: RefCell::new(false),
                    }),
                );
            }
        }
    }

    /// Resolve a class expression to a class name.
    fn class_name_of(&mut self, e: &Expr) -> Result<String, PhpError> {
        match e {
            Expr::Const(n) => Ok(self.resolve_class_name(n)),
            Expr::Str(s) => Ok(self.resolve_class_name(s)),
            Expr::AnonClass(d) => {
                self.register_class(d.clone());
                Ok(d.name.clone())
            }
            _ => {
                let v = self.eval(e)?;
                match v {
                    Value::Object(o) => Ok(o.borrow().class.name().to_string()),
                    other => Ok(self.resolve_class_name(&other.to_php_string())),
                }
            }
        }
    }

    /// `self`/`static`/`parent`/leading-\ name resolution → concrete name.
    pub fn resolve_class_name(&mut self, n: &str) -> String {
        let lname = n.trim_start_matches('\\');
        match lname.to_lowercase().as_str() {
            "self" | "static" => self
                .stack
                .last()
                .and_then(|f| f.scope_class.as_ref().map(|c| c.name().to_string()))
                .unwrap_or_else(|| lname.to_string()),
            "parent" => self
                .stack
                .last()
                .and_then(|f| f.scope_class.clone())
                .and_then(|c| c.decl.parent.clone())
                .unwrap_or_else(|| lname.to_string()),
            _ => lname.to_string(),
        }
    }

    /// name → registered class name (handles nothing special yet — no autoload).
    fn resolve_class(&mut self, name: &str) -> Option<String> {
        let n = name.trim_start_matches('\\');
        if self.classes.contains_key(&n.to_lowercase()) {
            Some(n.to_string())
        } else {
            None
        }
    }

    fn next_obj_id(&mut self) -> u64 {
        self.obj_counter += 1;
        self.obj_counter
    }

    /// `new X(args)` — instantiate + call __construct.
    fn new_instance(&mut self, name: &str, args: Vec<Cell>) -> Result<Value, PhpError> {
        let lname = name.to_lowercase();
        let cls = match self.classes.get(&lname) {
            Some(c) => c.clone(),
            None => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Class \"{}\" not found", name),
                    0,
                ))
            }
        };
        if cls.decl.kind == ClassKind::Interface {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot instantiate interface {}", cls.name()),
                0,
            ));
        }
        if cls.decl.is_abstract {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot instantiate abstract class {}", cls.name()),
                0,
            ));
        }
        let obj = self.instantiate(&lname, &[]);
        // __construct (native for builtins via method_invoke's interception)
        if cls.decl.find_method("__construct").is_some() {
            if let Value::Object(o) = &obj {
                self.method_invoke(o.clone(), "__construct", args)?;
            }
        }
        Ok(obj)
    }

    /// Build the object shell: init props along the whole parent chain.
    pub fn instantiate(&mut self, lname: &str, _args: &[Value]) -> Value {
        let cls = self.classes.get(&lname.to_lowercase()).cloned();
        let cls = match cls {
            Some(c) => c,
            None => {
                return Value::Object(Rc::new(RefCell::new(PhpObject {
                    class: Rc::new(PhpClass {
                        decl: Rc::new(ClassDecl {
                            name: lname.into(),
                            kind: ClassKind::Class,
                            is_abstract: false,
                            is_final: false,
                            parent: None,
                            implements: vec![],
                            traits: vec![],
                            methods: vec![],
                            props: vec![],
                            consts: vec![],
                        }),
                        statics: RefCell::new(HashMap::new()),
                        statics_init: RefCell::new(true),
                    }),
                    props: HashMap::new(),
                    prop_order: vec![],
                    id: self.next_obj_id(),
                    internal: None,
                })))
            }
        };
        // Collect decl chain (self + parents, parent-first for prop order).
        let mut chain = vec![cls.clone()];
        let mut cur = cls.clone();
        while let Some(p) = cur.decl.parent.clone() {
            if let Some(pc) = self.classes.get(&p.to_lowercase()).cloned() {
                chain.push(pc.clone());
                cur = pc;
            } else {
                break;
            }
        }
        chain.reverse();
        let mut props = HashMap::new();
        let mut prop_order = Vec::new();
        for c in &chain {
            for p in &c.decl.props {
                if p.is_static {
                    continue;
                }
                let default = match &p.default {
                    Some(d) => self.eval(d).unwrap_or(Value::Null),
                    None => Value::Null,
                };
                if !prop_order.contains(&p.name) {
                    prop_order.push(p.name.clone());
                }
                props.insert(p.name.clone(), cell(default));
            }
        }
        let internal = if self.is_throwable_name(&cls.decl.name) {
            Some(ObjectInternal::Exception {
                file: self.file.to_string(),
                line: self.cur_line as u32,
                trace: String::new(),
            })
        } else {
            None
        };
        Value::Object(Rc::new(RefCell::new(PhpObject {
            class: cls,
            props,
            prop_order,
            id: self.next_obj_id(),
            internal,
        })))
    }

    fn is_throwable_name(&mut self, name: &str) -> bool {
        let ln = name.to_lowercase();
        let mut cur = self.classes.get(&ln).cloned();
        while let Some(c) = cur {
            if c.decl
                .implements
                .iter()
                .any(|i| i.eq_ignore_ascii_case("throwable"))
            {
                return true;
            }
            if c.name().eq_ignore_ascii_case("throwable") {
                return true;
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        false
    }

    fn prop_name(&mut self, n: &PropName) -> Result<String, PhpError> {
        match n {
            PropName::Name(s) => Ok(s.clone()),
            PropName::Var(v) => {
                let val = self.var_get(v)?;
                self.conv_str(&val)
            }
            PropName::Expr(e) => {
                let v = self.eval(e)?;
                self.conv_str(&v)
            }
        }
    }

    fn prop_read(
        &mut self,
        obj: &Expr,
        name: &PropName,
        nullsafe: bool,
    ) -> Result<Value, PhpError> {
        let ov = self.eval(obj)?;
        let pn = self.prop_name(name)?;
        match ov {
            Value::Null if nullsafe => Ok(Value::Null),
            Value::Object(o) => {
                let cls = o.borrow().class.clone();
                if let Some(c) = o.borrow().props.get(&pn) {
                    return Ok(c.borrow().clone());
                }
                // __get magic
                if cls.find_method("__get").is_some() {
                    return self.method_invoke(o.clone(), "__get", vec![cell(Value::str(pn))]);
                }
                self.warn(&format!("Undefined property: {}::${}", cls.name(), pn));
                Ok(Value::Null)
            }
            Value::Null => {
                if nullsafe {
                    return Ok(Value::Null);
                }
                self.warn(&format!("Attempt to read property \"{}\" on null", pn));
                Ok(Value::Null)
            }
            other => {
                if self.silence == 0 {
                    self.warn(&format!(
                        "Attempt to read property \"{}\" on {}",
                        pn,
                        other.gettype()
                    ));
                }
                Ok(Value::Null)
            }
        }
    }

    fn prop_cell(
        &mut self,
        obj: &Expr,
        name: &PropName,
        _nullsafe: bool,
    ) -> Result<Cell, PhpError> {
        let pn = self.prop_name(name)?;
        let ov = self.eval(obj)?;
        match ov {
            Value::Object(o) => {
                let mut ob = o.borrow_mut();
                if !ob.props.contains_key(&pn) {
                    if !ob.prop_order.contains(&pn) {
                        ob.prop_order.push(pn.clone());
                    }
                    ob.props.insert(pn.clone(), cell(Value::Null));
                }
                Ok(ob.props.get(&pn).unwrap().clone())
            }
            _ => self.fail(PhpError::fatal(
                format!("Attempt to assign property \"{}\" on non-object", pn),
                0,
            )),
        }
    }

    fn unset_prop(&mut self, e: &Expr) -> Result<(), PhpError> {
        if let Expr::Prop { obj, name, .. } = e {
            let pn = self.prop_name(name)?;
            let ov = self.eval(obj)?;
            if let Value::Object(o) = ov {
                let cls = o.borrow().class.clone();
                if o.borrow().props.contains_key(&pn) {
                    o.borrow_mut().props.remove(&pn);
                } else if cls.find_method("__unset").is_some() {
                    self.method_invoke(o.clone(), "__unset", vec![cell(Value::str(pn))])?;
                }
            }
        }
        Ok(())
    }

    fn method_call(
        &mut self,
        obj: &Expr,
        name: &PropName,
        args: &[Expr],
        nullsafe: bool,
    ) -> Result<Value, PhpError> {
        let mn = self.prop_name(name)?;
        let ov = self.eval(obj)?;
        match ov {
            Value::Null if nullsafe => Ok(Value::Null),
            Value::Object(o) => {
                let params = self
                    .find_method_in(&o.borrow().class.clone(), &mn)
                    .map(|m| m.decl.params.clone())
                    .unwrap_or_default();
                let argvals = self.arg_cells(args, &params, &format!("{}()", mn))?;
                // method_invoke handles builtin (Throwable), __call, undefined.
                self.method_invoke(o.clone(), &mn, argvals)
            }
            Value::Null => {
                if nullsafe {
                    return Ok(Value::Null);
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!("Call to a member function {}() on null", mn),
                    0,
                ))
            }
            other => self.fail(PhpError::uncaught(
                "Error",
                format!("Call to a member function {}() on {}", mn, other.gettype()),
                0,
            )),
        }
    }

    /// `$obj->method()` dispatch to a resolved decl.
    fn invoke_method(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        m: &Rc<MethodDecl>,
        args: Vec<Cell>,
        _cls: Rc<PhpClass>,
    ) -> Result<Value, PhpError> {
        let scope = obj.borrow().class.clone();
        if m.is_static {
            return self.invoke_fn(&Rc::new(m.decl.clone()), args, None, Some(scope));
        }
        self.invoke_fn(&Rc::new(m.decl.clone()), args, Some(obj), Some(scope))
    }

    /// Calls a method by name through an object cell (magic methods, __call).
    pub fn method_invoke(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        name: &str,
        args: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        // Builtin exception methods implemented natively.
        if let Value::Object(_) = Value::Object(obj.clone()) {}
        let is_throwable = {
            let ob = obj.borrow();
            matches!(ob.internal, Some(ObjectInternal::Exception { .. }))
                || self.is_throwable_name(&ob.class.decl.name)
        };
        let cls = obj.borrow().class.clone();
        if is_throwable {
            // Native method only when the resolved method is a builtin stub
            // (empty body); a userland override always runs.
            let stub = cls
                .find_method(name)
                .map(|m| m.decl.body.is_empty())
                .unwrap_or(false);
            if stub {
                if let Some(v) = self.throwable_method(&obj, name, &args) {
                    return Ok(v);
                }
            }
        }
        match cls.find_method(name) {
            Some(m) => self.invoke_method(obj, &m, args, cls),
            None => {
                if let Some(m) = cls.find_method("__call") {
                    let mut arr = PhpArray::new();
                    for a in &args {
                        arr.push(a.borrow().clone());
                    }
                    return self.invoke_method(
                        obj,
                        &m,
                        vec![
                            cell(Value::str(name)),
                            cell(Value::Array(Rc::new(RefCell::new(arr)))),
                        ],
                        cls,
                    );
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!("Call to undefined method {}::{}()", cls.name(), name),
                    0,
                ))
            }
        }
    }

    /// Native implementations of Throwable methods.
    fn throwable_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        _args: &[Cell],
    ) -> Option<Value> {
        let ob = obj.borrow();
        let lname = name.to_lowercase();
        match lname.as_str() {
            "getmessage" => Some(
                ob.props
                    .get("message")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null),
            ),
            "getcode" => Some(
                ob.props
                    .get("code")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Int(0)),
            ),
            "getfile" => match &ob.internal {
                Some(ObjectInternal::Exception { file, .. }) => Some(Value::str(file.clone())),
                _ => Some(Value::str(self.file)),
            },
            "getline" => match &ob.internal {
                Some(ObjectInternal::Exception { line, .. }) => Some(Value::Int(*line as i64)),
                _ => Some(Value::Int(self.cur_line as i64)),
            },
            "gettrace" => Some(Value::Array(Rc::new(RefCell::new(PhpArray::new())))),
            "gettraceasstring" => Some(Value::str("#0 {main}")),
            "getprevious" => Some(Value::Null),
            "__tostring" => {
                let msg = ob
                    .props
                    .get("message")
                    .map(|c| c.borrow().to_php_string())
                    .unwrap_or_default();
                let (file, line) = match &ob.internal {
                    Some(ObjectInternal::Exception { file, line, .. }) => (file.clone(), *line),
                    _ => (self.file.to_string(), self.cur_line as u32),
                };
                Some(Value::str(format!(
                    "{}: {} in {}:{}\nStack trace:\n#0 {{main}}",
                    ob.class.name(),
                    msg,
                    file,
                    line
                )))
            }
            "__construct" => {
                // Builtin ctor: props from args message/code.
                drop(ob);
                let mut ob = obj.borrow_mut();
                let msg = _args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::str(""));
                let code = _args
                    .get(1)
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Int(0));
                ob.props.insert("message".into(), cell(msg));
                ob.props.insert("code".into(), cell(code));
                if !ob.prop_order.contains(&"message".into()) {
                    ob.prop_order.push("message".into());
                    ob.prop_order.push("code".into());
                }
                Some(Value::Null)
            }
            _ => None,
        }
    }

    fn static_prop_read(&mut self, class: &Expr, name: &str) -> Result<Value, PhpError> {
        let cls = self.class_of(class)?;
        self.statics_init(&cls);
        let v = cls.statics.borrow().get(name).map(|c| c.borrow().clone());
        match v {
            Some(v) => Ok(v),
            None => self.fail(PhpError::uncaught(
                "Error",
                format!("Undefined static property {}::${}", cls.name(), name),
                0,
            )),
        }
    }

    fn static_prop_cell(&mut self, class: &Expr, name: &str) -> Result<Cell, PhpError> {
        let cls = self.class_of(class)?;
        self.statics_init(&cls);
        let c = cls.statics.borrow().get(name).cloned();
        match c {
            Some(c) => Ok(c),
            None => self.fail(PhpError::uncaught(
                "Error",
                format!("Undefined static property {}::${}", cls.name(), name),
                0,
            )),
        }
    }

    /// Lazily initialize static prop defaults.
    fn statics_init(&mut self, cls: &Rc<PhpClass>) {
        if *cls.statics_init.borrow() {
            return;
        }
        *cls.statics_init.borrow_mut() = true;
        for p in &cls.decl.props {
            if !p.is_static {
                continue;
            }
            let default = match &p.default {
                Some(d) => self.eval(d).unwrap_or(Value::Null),
                None => Value::Null,
            };
            cls.statics
                .borrow_mut()
                .insert(p.name.clone(), cell(default));
        }
    }

    fn static_call(&mut self, class: &Expr, name: &str, args: &[Expr]) -> Result<Value, PhpError> {
        let cls = self.class_of(class)?;
        let params = self
            .find_method_in(&cls, name)
            .map(|m| m.decl.params.clone())
            .unwrap_or_default();
        let argvals = self.arg_cells(args, &params, &format!("{}()", name))?;
        self.static_invoke(cls, name, argvals)
    }

    fn static_invoke(
        &mut self,
        cls: Rc<PhpClass>,
        name: &str,
        args: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        // Throwable methods are instance-only; look up incl. parents.
        match self.find_method_in(&cls, name) {
            Some(m) => self.invoke_fn(&Rc::new(m.decl.clone()), args, None, Some(cls.clone())),
            None => {
                if let Some(m) = self.find_method_in(&cls, "__callstatic") {
                    let mut arr = PhpArray::new();
                    for a in &args {
                        arr.push(a.borrow().clone());
                    }
                    return self.invoke_fn(
                        &Rc::new(m.decl.clone()),
                        vec![
                            cell(Value::str(name)),
                            cell(Value::Array(Rc::new(RefCell::new(arr)))),
                        ],
                        None,
                        Some(cls.clone()),
                    );
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!("Call to undefined method {}::{}()", cls.name(), name),
                    0,
                ))
            }
        }
    }

    /// Method lookup walking parent chain (uses registered classes).
    fn find_method_in(&mut self, cls: &Rc<PhpClass>, name: &str) -> Option<Rc<MethodDecl>> {
        let lname = name.to_lowercase();
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            if let Some(m) = c.decl.find_method(&lname) {
                return Some(m);
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        None
    }

    fn class_of(&mut self, e: &Expr) -> Result<Rc<PhpClass>, PhpError> {
        let name = self.class_name_of(e)?;
        match self.classes.get(&name.to_lowercase()) {
            Some(c) => Ok(c.clone()),
            None => self.fail(PhpError::uncaught(
                "Error",
                format!("Class \"{}\" not found", name),
                0,
            )),
        }
    }

    fn class_const(&mut self, class: &Expr, name: &str) -> Result<Value, PhpError> {
        let cname = self.class_name_of(class)?;
        if name == "class" {
            return Ok(Value::str(cname));
        }
        let cls = match self.classes.get(&cname.to_lowercase()) {
            Some(c) => c.clone(),
            None => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Class \"{}\" not found", cname),
                    0,
                ))
            }
        };
        // Walk chain for the const.
        let mut cur = Some(cls);
        while let Some(c) = cur {
            for (n, e) in &c.decl.consts {
                if n == name {
                    return self.eval(e);
                }
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        self.fail(PhpError::uncaught(
            "Error",
            format!("Undefined constant {}", name),
            0,
        ))
    }

    // ----- include / eval -----

    fn include(&mut self, kind: IncludeKind, e: &Expr) -> Result<Value, PhpError> {
        let pathv = self.eval(e)?;
        let path_s = self.conv_str(&pathv)?;
        if kind == IncludeKind::Eval {
            return self.eval_code(&path_s);
        }
        // Resolution: relative to current file's dir, then cwd.
        let p = std::path::Path::new(&path_s);
        let cands: Vec<std::path::PathBuf> = if p.is_absolute() {
            vec![p.to_path_buf()]
        } else {
            let dir = std::path::Path::new(self.file)
                .parent()
                .map(|d| d.to_path_buf())
                .unwrap_or_default();
            vec![dir.join(&path_s), std::path::PathBuf::from(&path_s)]
        };
        let found = cands.iter().find(|c| c.exists()).cloned();
        let path = match found {
            Some(p) => p,
            None => {
                self.warn(&format!(
                    "{}({}): Failed opening '{}' for inclusion (include_path='.:/home/linuxbrew/.linuxbrew/share/pear')",
                    match kind {
                        IncludeKind::Include | IncludeKind::IncludeOnce => "include",
                        _ => "require",
                    },
                    self.file,
                    path_s,
                ));
                match kind {
                    IncludeKind::Include | IncludeKind::IncludeOnce => return Ok(Value::Bool(false)),
                    _ => {
                        return self.fail(PhpError::fatal(
                            format!(
                                "Failed opening required '{}' (include_path='.:/home/linuxbrew/.linuxbrew/share/pear')",
                                path_s
                            ),
                            0,
                        ))
                    }
                }
            }
        };
        let canon = path.canonicalize().unwrap_or(path);
        if matches!(kind, IncludeKind::IncludeOnce | IncludeKind::RequireOnce) {
            if self.included.contains(&canon) {
                return Ok(Value::Bool(true));
            }
            self.included.insert(canon.clone());
        }
        let src = match std::fs::read_to_string(&canon) {
            Ok(s) => s,
            Err(e) => {
                self.warn(&format!(
                    "include({}): Failed to open stream: {}",
                    path_s, e
                ));
                return Ok(Value::Bool(false));
            }
        };
        // Include executes in the current scope (PHP semantics).
        let saved_file = self.file.to_string();
        let fname = canon.display().to_string();
        let stmts = match parser::parse(&src) {
            Ok(s) => s,
            Err(e) => {
                self.print_parse_at(&e, &fname);
                return Ok(Value::Bool(false));
            }
        };
        // self.file is &'a — swap not possible; record include file for messages
        let _ = saved_file;
        let flow = self.exec_block(&stmts);
        match flow {
            Flow::Return(v) => Ok(v),
            Flow::Normal => Ok(Value::Int(1)),
            Flow::Exit(c) => Err(PhpError {
                kind: ErrorKind::Fatal,
                message: format!("\u{1}exit:{}", c),
                line: 0,
            }),
            Flow::Throw(v) => {
                self.pending_exception = Some(v);
                Err(PhpError {
                    kind: ErrorKind::Throw,
                    message: "throw".into(),
                    line: 0,
                })
            }
            Flow::Break(_) | Flow::Continue(_) => {
                self.fail(PhpError::fatal("'break'/'continue' in included file", 0))
            }
        }
    }

    fn print_parse_at(&mut self, e: &PhpError, file: &str) {
        self.emit(&format!(
            "\nParse error: {} in {} on line {}\n",
            e.message, file, e.line
        ));
    }

    fn eval_code(&mut self, code: &str) -> Result<Value, PhpError> {
        // eval'd code has no <?php tag; strip a leading one defensively.
        let src = code.strip_prefix("<?php").map(|s| s.to_string());
        let src = src.unwrap_or_else(|| format!("<?php\n{}", code));
        match parser::parse(&src) {
            Ok(stmts) => {
                let flow = self.exec_block(&stmts);
                match flow {
                    Flow::Return(v) => Ok(v),
                    Flow::Normal => Ok(Value::Null),
                    Flow::Exit(c) => Err(PhpError {
                        kind: ErrorKind::Fatal,
                        message: format!("\u{1}exit:{}", c),
                        line: 0,
                    }),
                    Flow::Throw(v) => {
                        self.pending_exception = Some(v);
                        Err(PhpError {
                            kind: ErrorKind::Throw,
                            message: "throw".into(),
                            line: 0,
                        })
                    }
                    Flow::Break(_) | Flow::Continue(_) => Ok(Value::Null),
                }
            }
            Err(e) => {
                let v = self.exception("ParseError", &e.message);
                self.pending_exception = Some(v);
                Err(PhpError {
                    kind: ErrorKind::Throw,
                    message: "eval".into(),
                    line: 0,
                })
            }
        }
    }

    /// Flush all output buffers at script end.
    fn flush_ob_all(&mut self) {
        while let Some(buf) = self.ob_stack.pop() {
            self.out.push_str(&buf);
        }
    }

    /// String conversion for builtins (__toString-aware, never errors → "" on failure).
    pub fn to_string_of(&mut self, v: &Value) -> String {
        self.conv_str(v).unwrap_or_else(|_| v.to_php_string())
    }
    /// Variable lookup for compact() — reads current scope quietly.
    pub fn lookup_var(&mut self, name: &str) -> Option<Value> {
        self.var_cell_opt(name).map(|c| c.borrow().clone())
    }
    pub fn error_handler(&self) -> Option<Value> {
        self.error_handler.clone()
    }
    pub fn exception_handler(&self) -> Option<Value> {
        self.exception_handler.clone()
    }
    /// `$obj instanceof X` helper for builtins.
    pub fn obj_is_a(&mut self, o: &Rc<RefCell<PhpObject>>, name: &str) -> bool {
        let cls = o.borrow().class.clone();
        self.is_a(&cls, name)
    }
    /// class-name-string is-a check for is_subclass_of.
    pub fn obj_is_a_str(&mut self, cls_name: &str, name: &str) -> bool {
        match self.classes.get(&cls_name.to_lowercase()).cloned() {
            Some(c) => self.is_a(&c, name),
            None => false,
        }
    }
    // public helpers for builtins
    pub fn ob_push(&mut self) {
        self.ob_stack.push(String::new());
    }
    pub fn ob_pop(&mut self) -> Option<String> {
        self.ob_stack.pop()
    }
    pub fn ob_top(&self) -> Option<&String> {
        self.ob_stack.last()
    }
    pub fn ob_len(&self) -> usize {
        self.ob_stack.len()
    }
    pub fn register_shutdown(&mut self, f: Value, args: Vec<Cell>) {
        self.shutdown_fns.push((f, args));
    }
    pub fn set_error_handler(&mut self, f: Option<Value>) {
        self.error_handler = f;
    }
    pub fn set_exception_handler(&mut self, f: Option<Value>) {
        self.exception_handler = f;
    }
    pub fn cur_frame(&mut self) -> Option<&Frame> {
        self.stack.last()
    }
    /// Args of the currently-executing function.
    pub fn frame_args(&self) -> &[Cell] {
        self.stack.last().map(|f| f.args.as_slice()).unwrap_or(&[])
    }
    pub fn define_const(&mut self, name: &str, v: Value) {
        self.constants.insert(name.to_string(), v);
    }
    pub fn const_defined(&self, name: &str) -> bool {
        self.constants.contains_key(name)
            || self.constants.contains_key(name.trim_start_matches('\\'))
    }
    pub fn const_get(&self, name: &str) -> Option<Value> {
        self.constants
            .get(name)
            .or_else(|| self.constants.get(name.trim_start_matches('\\')))
            .cloned()
    }
    pub fn next_res_id(&mut self) -> u64 {
        self.res_counter += 1;
        self.res_counter
    }
    pub fn set_resource(&mut self, _r: PhpResource) {}
    pub fn lookup_class(&self, name: &str) -> Option<Rc<PhpClass>> {
        self.classes
            .get(&name.trim_start_matches('\\').to_lowercase())
            .cloned()
    }
    pub fn obj_new_id(&mut self) -> u64 {
        self.next_obj_id()
    }
    pub fn instantiate_class(&mut self, name: &str, args: Vec<Cell>) -> Result<Value, PhpError> {
        self.new_instance(name, args)
    }
    pub fn call_closure(
        &mut self,
        c: &Rc<PhpCallable>,
        args: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        self.call_value(&Value::Callable(c.clone()), args)
    }
    pub fn var_name_set(&mut self, name: &str, v: Value) {
        self.var_set(name, v);
    }
    pub fn warn_pub(&mut self, msg: &str) {
        self.warn(msg);
    }
    pub fn invoke_callable_str(&mut self, name: &str, args: Vec<Cell>) -> Result<Value, PhpError> {
        self.call_value(&Value::str(name), args)
    }
    pub fn prop_set(&mut self, obj: &Rc<RefCell<PhpObject>>, name: &str, v: Value) {
        obj.borrow_mut().props.insert(name.to_string(), cell(v));
    }
    pub fn obj_class_name(&self, o: &Rc<RefCell<PhpObject>>) -> String {
        o.borrow().class.name().to_string()
    }
}

fn cell(v: Value) -> Cell {
    Rc::new(RefCell::new(v))
}

fn key_value(k: &ArrKey) -> Value {
    match k {
        ArrKey::Int(i) => Value::Int(*i),
        ArrKey::Str(s) => Value::str(s.to_string()),
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
fn perl_inc(s: &str) -> String {
    let mut bytes = s.as_bytes().to_vec();
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
    String::from_utf8_lossy(&bytes).into_owned()
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

fn bitwise_str(op: &str, a: &str, b: &str) -> String {
    // `|` pads the shorter operand with NUL; `&`/`^` truncate to min length.
    let (x, y) = (a.as_bytes(), b.as_bytes());
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
    String::from_utf8_lossy(&out).into_owned()
}

/// By-ref flags for builtin parameters (only slots that accept references are
/// `true`). Used to warn on non-variable args in by-ref positions and to alias
/// real cells for mutating builtins like array_pop/sort/preg_match.
fn builtin_byref(name: &str) -> Option<&'static [bool]> {
    Some(match name {
        "array_pop" | "array_shift" | "array_walk" | "sort" | "rsort" | "asort" | "arsort"
        | "ksort" | "krsort" | "usort" | "uasort" | "uksort" | "natsort" | "natcasesort"
        | "shuffle" | "reset" | "end" | "next" | "prev" | "current" | "pos" | "each"
        | "array_push" | "array_unshift" | "array_splice" | "array_multisort" => &[true],
        "preg_match" | "preg_match_all" => &[false, false, true, true],
        "preg_replace"
        | "preg_replace_callback"
        | "preg_replace_callback_array"
        | "str_replace"
        | "str_ireplace" => &[false, false, false, true],
        "parse_str" => &[false, true],
        "sscanf" | "fscanf" => &[false, false],
        "exec" => &[false, true, true],
        "passthru" | "system" => &[false, true],
        "preg_filter" | "preg_grep" => &[false],
        _ => return None,
    })
}
