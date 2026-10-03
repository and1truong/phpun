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
    compare, format_backtrace_frames, format_float_repr, format_trace, identical, numeric, to_key,
    trace_arg, ArrKey, CallableKind, Cell, Numeric, ObjectInternal, PhpArray, PhpCallable,
    PhpClass, PhpObject, PhpResource, TraceFrame, Value,
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
    /// Class the running method was declared in — PHP's private
    /// property slot is keyed by the declaring class (`\0Cls\0prop`).
    decl_class: Option<Rc<PhpClass>>,
    /// File this frame's code was declared in (include resolution base).
    file: String,
    /// Namespace the running code was declared in — unqualified
    /// function/const lookups try `ns\name` before the global name.
    ns: String,
    /// Function declared `&name()` — returns bind cells, not values.
    ret_by_ref: bool,
    /// While running a property hook: (object id, prop name, is_get,
    /// owner class name) — `$this->prop` inside its own hook hits the
    /// backing slot directly; the owner names `__METHOD__`'s class part
    /// (trait origin too) (Zend/tests/property_hooks).
    hook_prop: Option<(u64, String, bool, String)>,
}

impl Frame {
    fn new(fn_name: String) -> Self {
        Self {
            vars: HashMap::new(),
            args: Vec::new(),
            fn_name,
            this_obj: None,
            scope_class: None,
            decl_class: None,
            file: String::new(),
            ns: String::new(),
            ret_by_ref: false,
            hook_prop: None,
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
    /// Top-level parentless classes registered by hoisting (early
    /// binding); their decl stmt then no-ops (namespaces/ns_060).
    early_bound_classes: HashSet<String>,
    constants: HashMap<String, Value>,
    /// Accumulated program output (display_errors prints to stdout under
    /// CLI, and the PHPT harness merges streams via 2>&1).
    pub out: String,
    /// Output buffer stack for ob_*().
    ob_stack: Vec<ObLevel>,
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
    /// static-decl sites per function scope (fn key → var → source line) —
    /// PHP fatals on a same-scope redeclaration at a different site.
    static_decls: HashMap<String, HashMap<String, usize>>,
    /// include_once/require_once registry (canonical paths).
    included: HashSet<std::path::PathBuf>,
    /// Pending exception carried across an Err(Throw) return.
    pending_exception: Option<Value>,
    /// Live call stack (user + builtin) for getTrace() snapshots.
    call_trace: Vec<TraceFrame>,
    /// Pending fatal error message for exceptions raised as PhpError.
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
    /// Declaring class of the method about to be invoked (set by
    /// invoke_method, consumed by invoke_fn to fill Frame::decl_class).
    pending_decl_class: Option<Rc<PhpClass>>,
    /// (object id, prop, is_get, owner) whose hook is about to run —
    /// consumed by invoke_fn to fill Frame::hook_prop.
    pending_hook_prop: Option<(u64, String, bool, String)>,
    /// Live object handles for PHP's var_dump `#N` id: the lowest freed
    /// slot is reused, matching Zend's object store recycling.
    obj_handles: Vec<std::rc::Weak<RefCell<PhpObject>>>,
    /// Object ptrs whose __destruct already ran (shutdown pass).
    destructed: HashSet<usize>,
    /// Nonzero while a callable is invoked from inside a builtin's
    /// internals (ob handlers) — marks its trace site internal-function.
    internal_cb: u32,
    /// File currently executing — include resolution uses its directory
    /// (PHP checks include_path, then the calling file's dir, then cwd).
    cur_file: String,
    /// Bytes emitted so far — memory_limit bookkeeping.
    pub mem_used: u64,
    /// Size of the last emit — the 'tried to allocate' figure.
    mem_last: u64,
    /// Raised once the memory_limit fatal fired — buffers are dropped
    /// at shutdown instead of flushed (bug45392).
    pub mem_exceeded: bool,
    /// Execution deadline set by set_time_limit/hard_timeout (045).
    deadline: Option<std::time::Instant>,
    /// Seconds figure for the 'Maximum execution time' message.
    deadline_secs: i64,
    /// `-d` ini settings (e.g. short_open_tag=on).
    pub ini: HashMap<String, String>,
}

/// One output-buffer level (ob_start) with its optional handler.
pub struct ObLevel {
    pub buf: String,
    pub handler: Option<Value>,
    /// Set after the handler's first invocation — PHP's
    /// PHP_OUTPUT_HANDLER_START bit is only passed once (bug24951).
    pub started: bool,
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
        constants.insert("INI_USER".into(), Value::Int(1));
        constants.insert("INI_PERDIR".into(), Value::Int(2));
        constants.insert("INI_SYSTEM".into(), Value::Int(4));
        constants.insert("INI_ALL".into(), Value::Int(7));
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
        constants.insert("PHP_OUTPUT_HANDLER_START".into(), Value::Int(1));
        constants.insert("PHP_OUTPUT_HANDLER_WRITE".into(), Value::Int(0));
        constants.insert("PHP_OUTPUT_HANDLER_CONT".into(), Value::Int(0));
        constants.insert("PHP_OUTPUT_HANDLER_CLEAN".into(), Value::Int(2));
        constants.insert("PHP_OUTPUT_HANDLER_FLUSH".into(), Value::Int(4));
        constants.insert("PHP_OUTPUT_HANDLER_FINAL".into(), Value::Int(8));
        constants.insert("PHP_OUTPUT_HANDLER_END".into(), Value::Int(8));
        constants.insert("PHP_OUTPUT_HANDLER_CLEANABLE".into(), Value::Int(16));
        constants.insert("PHP_OUTPUT_HANDLER_FLUSHABLE".into(), Value::Int(32));
        constants.insert("PHP_OUTPUT_HANDLER_REMOVABLE".into(), Value::Int(64));
        constants.insert("PHP_OUTPUT_HANDLER_STDFLAGS".into(), Value::Int(112));
        constants.insert("E_RECOVERABLE_ERROR".into(), Value::Int(4096));
        constants.insert("E_CORE_ERROR".into(), Value::Int(16));
        constants.insert("E_CORE_WARNING".into(), Value::Int(32));
        constants.insert("E_COMPILE_ERROR".into(), Value::Int(64));
        constants.insert("E_COMPILE_WARNING".into(), Value::Int(128));
        // Locale categories (glibc values).
        constants.insert("LC_CTYPE".into(), Value::Int(0));
        constants.insert("LC_NUMERIC".into(), Value::Int(1));
        constants.insert("LC_TIME".into(), Value::Int(2));
        constants.insert("LC_COLLATE".into(), Value::Int(3));
        constants.insert("LC_MONETARY".into(), Value::Int(4));
        constants.insert("LC_MESSAGES".into(), Value::Int(5));
        constants.insert("LC_ALL".into(), Value::Int(6));
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
            early_bound_classes: HashSet::new(),
            constants,
            out: String::new(),
            ob_stack: Vec::new(),
            silence: 0,
            statics: HashMap::new(),
            global_statics: HashMap::new(),
            static_decls: HashMap::new(),
            included: HashSet::new(),
            pending_exception: None,
            call_trace: Vec::new(),
            res_counter: 0,
            shutdown_fns: Vec::new(),
            error_handler: None,
            error_level: 32767,
            env_overrides: HashMap::new(),
            exception_handler: None,
            in_handler: false,
            cur_line: 1,
            pending_decl_class: None,
            pending_hook_prop: None,
            obj_handles: Vec::new(),
            destructed: HashSet::new(),
            internal_cb: 0,
            cur_file: file.to_string(),
            mem_used: 0,
            mem_last: 0,
            mem_exceeded: false,
            deadline: None,
            deadline_secs: 0,
            ini: HashMap::new(),
        };
        // Auto-globals. PHP's $_SERVER carries env + script metadata;
        // the request arrays start empty (bug24908 counts on non-empty
        // $_SERVER inside __destruct).
        {
            let mut server = PhpArray::new();
            for (k, v) in std::env::vars() {
                server.set(ArrKey::Str(k.into()), Value::str(v));
            }
            server.set(ArrKey::Str("SCRIPT_FILENAME".into()), Value::str(file));
            server.set(ArrKey::Str("PHP_SELF".into()), Value::str(file));
            server.set(ArrKey::Str("SCRIPT_NAME".into()), Value::str(file));
            server.set(ArrKey::Str("SERVER_NAME".into()), Value::str("localhost"));
            server.set(
                ArrKey::Str("SERVER_SOFTWARE".into()),
                Value::str("phpun/0.0.0"),
            );
            server.set(
                ArrKey::Str("SERVER_PROTOCOL".into()),
                Value::str("HTTP/1.1"),
            );
            server.set(ArrKey::Str("REQUEST_METHOD".into()), Value::str("GET"));
            let mut argv = PhpArray::new();
            argv.push(Value::str(file));
            server.set(
                ArrKey::Str("argv".into()),
                Value::Array(Rc::new(RefCell::new(argv))),
            );
            server.set(ArrKey::Str("argc".into()), Value::Int(1));
            it.globals.vars.insert(
                "_SERVER".into(),
                cell(Value::Array(Rc::new(RefCell::new(server)))),
            );
            let mut env = PhpArray::new();
            for (k, v) in std::env::vars() {
                env.set(ArrKey::Str(k.into()), Value::str(v));
            }
            it.globals.vars.insert(
                "_ENV".into(),
                cell(Value::Array(Rc::new(RefCell::new(env)))),
            );
            for n in ["_GET", "_POST", "_COOKIE", "_FILES", "_REQUEST", "_SESSION"] {
                it.globals.vars.insert(
                    n.into(),
                    cell(Value::Array(Rc::new(RefCell::new(PhpArray::new())))),
                );
            }
            let mut argv = PhpArray::new();
            argv.push(Value::str(file));
            it.globals.vars.insert(
                "argv".into(),
                cell(Value::Array(Rc::new(RefCell::new(argv)))),
            );
            it.globals.vars.insert("argc".into(), cell(Value::Int(1)));
        }
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
                readonly: false,
                parent: parent.map(|s| s.to_string()),
                implements: vec!["Throwable".into()],
                attrs: vec![],
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
                        ty: None,
                        is_abstract: false,
                        is_final: false,
                        set_vis: None,
                        decl_in: None,
                        hooks: None,
                        line: 0,
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
                    line: 0,
                    file: String::new(),
                    ns: String::new(),
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
            readonly: false,
            parent: None,
            implements: parents.iter().map(|s| s.to_string()).collect(),
            attrs: vec![],
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
                            line: 0,
                            file: String::new(),
                            ns: String::new(),
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
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                methods: vec![],
                props: vec![],
                consts: vec![],
            },
            false,
        );
        // DateTime — stub class whose ctor accepts an optional datetime
        // string; exists so `new DateTime(...)` type-checks
        // (compare_objects_basic2).
        reg(
            ClassDecl {
                name: "DateTime".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                methods: vec![Rc::new(MethodDecl {
                    decl: FunctionDecl {
                        name: "__construct".into(),
                        params: vec![Param {
                            name: "datetime".into(),
                            default: Some(Expr::Str("now".into())),
                            by_ref: false,
                            variadic: false,
                            ty: Some(vec!["string".into()]),
                            promoted: false,
                            vis: None,
                            readonly: false,
                            is_final: false,
                            set_vis: None,
                            hooks: None,
                        }],
                        body: vec![],
                        by_ref: false,
                        line: 0,
                        file: String::new(),
                        ns: String::new(),
                    },
                    is_static: false,
                    is_abstract: false,
                    is_final: false,
                    visibility: Visibility::Public,
                })],
                props: vec![],
                consts: vec![],
            },
            false,
        );
        // Reflection stubs — enough surface for the hooked-prop tests:
        // ReflectionClass::newInstanceWithoutConstructor builds the shell
        // without running __construct; ReflectionProperty::isInitialized
        // checks slot presence (typed props with no default start uninit).
        let mk_method = |name: &str, params: Vec<Param>| {
            Rc::new(MethodDecl {
                decl: FunctionDecl {
                    name: name.into(),
                    params,
                    body: vec![],
                    by_ref: false,
                    line: 0,
                    file: String::new(),
                    ns: String::new(),
                },
                is_static: false,
                is_abstract: false,
                is_final: false,
                visibility: Visibility::Public,
            })
        };
        let str_param = |n: &str| Param {
            name: n.into(),
            default: None,
            by_ref: false,
            variadic: false,
            ty: Some(vec!["string".into()]),
            promoted: false,
            vis: None,
            readonly: false,
            is_final: false,
            set_vis: None,
            hooks: None,
        };
        reg(
            ClassDecl {
                name: "ReflectionClass".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                methods: vec![
                    mk_method("__construct", vec![str_param("class")]),
                    mk_method("newInstanceWithoutConstructor", vec![]),
                    mk_method("getName", vec![]),
                ],
                props: vec![],
                consts: vec![],
            },
            false,
        );
        reg(
            ClassDecl {
                name: "ReflectionProperty".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                methods: vec![
                    mk_method(
                        "__construct",
                        vec![str_param("class"), str_param("property")],
                    ),
                    mk_method(
                        "isInitialized",
                        vec![Param {
                            name: "object".into(),
                            default: Some(Expr::Null),
                            by_ref: false,
                            variadic: false,
                            ty: None,
                            promoted: false,
                            vis: None,
                            readonly: false,
                            is_final: false,
                            set_vis: None,
                            hooks: None,
                        }],
                    ),
                ],
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
            ("AssertionError", "Error"),
            ("UnhandledMatchError", "Error"),
        ] {
            reg(
                throwable_class(name, Some(parent), &["message", "code", "file", "line"]),
                false,
            );
        }
    }

    /// PHP binds a compilation unit's unconditional top-level function
    /// decls before executing it (bug23279's later-declared handler).
    fn hoist_funcs(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match s {
                Stmt::Function(d) => {
                    let _ = self.decl_type_checks(&d.name, d);
                    let mut d = d.clone();
                    d.file = self.cur_file.clone();
                    self.functions.insert(d.name.to_lowercase(), Rc::new(d));
                }
                // `namespace X { stmts }` parses as
                // Block[Namespace, Block[stmts]] — decls inside are still
                // unconditional top-level for early binding (ns_085).
                Stmt::Block(v) if matches!(v.first(), Some(Stmt::Namespace(_))) => {
                    for s in &v[1..] {
                        if let Stmt::Block(inner) = s {
                            self.hoist_funcs(inner);
                        }
                    }
                }
                // Early binding: unconditional top-level classes with no
                // parent/interfaces/traits register before execution
                // (namespaces/ns_060).
                Stmt::Class(d)
                    if d.parent.is_none() && d.implements.is_empty() && d.traits.is_empty() =>
                {
                    let key = d.name.to_lowercase();
                    if !self.classes.contains_key(&key) && !self.early_bound_classes.contains(&key)
                    {
                        let mut d = (**d).clone();
                        for m in &mut d.methods {
                            let mut mm = (**m).clone();
                            mm.decl.file = self.cur_file.clone();
                            *m = Rc::new(mm);
                        }
                        if self.register_class(Rc::new(d)).is_ok() {
                            self.early_bound_classes.insert(key);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn run(&mut self, stmts: &[Stmt]) -> RunResult {
        // hard_timeout ini is the absolute deadline (045).
        let ht = self.ini_bytes("hard_timeout");
        if ht > 0 {
            self.deadline =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(ht as u64));
            self.deadline_secs = ht;
        }
        self.hoist_funcs(stmts);
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
                // set_exception_handler replaces the uncaught display
                // entirely; exit is still 255 (bug23279).
                if let Some(h) = self.exception_handler.clone() {
                    let _ = self.call_value(&h, vec![cell(v)]);
                } else {
                    self.uncaught(&v);
                }
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
        // Zend calls __destruct on live objects after shutdown functions
        // and before output buffers flush — destructors still see their
        // buffers' contents (bug30578, bug24908).
        for h in std::mem::take(&mut self.obj_handles) {
            let Some(o) = h.upgrade() else { continue };
            let key = Rc::as_ptr(&o) as usize;
            if self.destructed.contains(&key) {
                continue;
            }
            if self
                .find_method_in(&o.borrow().class, "__destruct")
                .is_some()
            {
                self.destructed.insert(key);
                let _ = self.method_invoke(o.clone(), "__destruct", vec![]);
            }
        }
        if !self.mem_exceeded {
            self.flush_ob_all();
        }
    }

    /// Convenience: parse+run a source string (used by tests and the CLI).
    /// INI integer value with default (e.g. precision=14).
    pub fn ini_int(&self, k: &str, dflt: i64) -> i64 {
        self.ini.get(k).and_then(|s| s.parse().ok()).unwrap_or(dflt)
    }

    /// INI truthiness — PHP accepts 1/On/true/yes case-insensitively.
    pub fn ini_on(&self, k: &str) -> bool {
        match self.ini.get(k).map(|s| s.to_lowercase()) {
            Some(v) => matches!(v.as_str(), "1" | "on" | "true" | "yes"),
            None => false,
        }
    }

    pub fn run_source(&mut self, src: &str) -> RunResult {
        match parser::parse_with(src, self.ini_on("short_open_tag")) {
            Ok(stmts) => self.run(&stmts),
            Err(e) => {
                // Compile-time semantic errors (hook decl checks, `parent::`
                // misuse) are E_ERROR fatals, not syntax errors.
                match e.kind {
                    ErrorKind::Parse => self.print_parse(&e),
                    _ => self.print_fatal(&e),
                }
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

    /// PHP auto-globals resolve in every scope; first access links the
    /// global cell into the local table (bug24908).
    fn is_superglobal(name: &str) -> bool {
        matches!(
            name,
            "_GET"
                | "_POST"
                | "_COOKIE"
                | "_FILES"
                | "_ENV"
                | "_SERVER"
                | "_REQUEST"
                | "_SESSION"
                | "GLOBALS"
                | "argc"
                | "argv"
        )
    }

    fn superglobal_cell(&mut self, name: &str) -> Option<Cell> {
        if name == "GLOBALS" {
            // Live view — never the seeded/lazy globals.vars cell.
            return Some(cell(self.globals_array_val()));
        }
        if !Self::is_superglobal(name) {
            return None;
        }
        let g = self
            .globals
            .vars
            .entry(name.to_string())
            .or_insert_with(|| cell(Value::Null))
            .clone();
        self.cur().vars.insert(name.to_string(), g.clone());
        Some(g)
    }

    fn var_get(&mut self, name: &str) -> Result<Value, PhpError> {
        match self.cur().vars.get(name) {
            Some(c) => Ok(c.borrow().clone()),
            None => match self.superglobal_cell(name) {
                Some(c) => Ok(c.borrow().clone()),
                None => {
                    if self.silence == 0 {
                        self.warn(&format!("Undefined variable ${}", name))?;
                    }
                    Ok(Value::Null)
                }
            },
        }
    }

    /// The cell behind a variable name — creating it on demand.
    /// Superglobals resolve to the global cell (writes propagate).
    pub fn var_cell(&mut self, name: &str) -> Cell {
        if name == "GLOBALS" {
            // $GLOBALS is a live view over the global symbol table — array
            // entries share the same Cells as globals.vars so writes alias.
            return cell(self.globals_array_val());
        }
        let is_global = self.stack.is_empty();
        let existed = self.cur().vars.contains_key(name);
        if let Some(c) = self.superglobal_cell(name) {
            return c;
        }
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
            // Entries alias globals.vars cells — writes must never CoW-split.
            a.is_ref = true;
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
    fn var_cell_opt(&mut self, name: &str) -> Option<Cell> {
        match self.stack.last().unwrap_or(&self.globals).vars.get(name) {
            Some(c) => Some(c.clone()),
            None => self.superglobal_cell(name),
        }
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
        // memory_limit>0 turns into a deferred fatal once accumulated
        // writes pass it (bug45392); checked at the next statement.
        self.mem_used += s.len() as u64;
        self.mem_last = s.len() as u64;
        if let Some(buf) = self.ob_stack.last_mut() {
            buf.buf.push_str(s);
        } else {
            self.out.push_str(s);
        }
    }

    /// set_time_limit(N): restart the counter for N seconds (0 =
    /// unlimited) (045).
    pub fn set_deadline(&mut self, secs: i64) {
        self.deadline_secs = secs;
        self.deadline = if secs <= 0 {
            None
        } else {
            Some(std::time::Instant::now() + std::time::Duration::from_secs(secs as u64))
        };
    }

    /// INI byte shorthand: `2M`, `512K`, `1G`, plain ints, -1 unlimited.
    pub fn ini_bytes(&self, k: &str) -> i64 {
        let Some(raw) = self.ini.get(k) else {
            return -1;
        };
        let s = raw.trim();
        let (num, mul) = match s.as_bytes().last() {
            Some(b'K') | Some(b'k') => (&s[..s.len() - 1], 1i64 << 10),
            Some(b'M') | Some(b'm') => (&s[..s.len() - 1], 1i64 << 20),
            Some(b'G') | Some(b'g') => (&s[..s.len() - 1], 1i64 << 30),
            _ => (s, 1),
        };
        num.trim().parse::<i64>().unwrap_or(-1) * mul
    }

    /// Shared diagnostic path: Warning/Notice/Deprecated all route through
    /// a user error handler first (PHP semantics); the handler's error —
    /// e.g. a thrown Error2Exception — propagates to the caller (038).
    /// Only a literal `false` return lets the builtin handler continue.
    fn emit_diag(&mut self, level: &str, errno: i64, msg: &str) -> Result<(), PhpError> {
        if self.error_handler.is_some() && !self.in_handler {
            let h = self.error_handler.clone().unwrap();
            let args: Vec<Cell> = vec![
                cell(Value::Int(errno)),
                cell(Value::str(msg)),
                cell(Value::str(self.file)),
                cell(Value::Int(self.cur_line as i64)),
            ];
            self.in_handler = true;
            let r = self.call_value(&h, args);
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

    fn warn(&mut self, msg: &str) -> Result<(), PhpError> {
        if self.silence > 0 || self.error_level & 2 == 0 {
            return Ok(());
        }
        self.emit_diag("Warning", 2, msg)
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
    /// html_errors=1 switches to the `<b>` docref format (bug35176).
    fn diag(&mut self, level: &str, msg: &str) {
        if self.ini_on("html_errors") {
            let msg = self.docref(msg);
            self.emit(&format!(
                "<br />\n<b>{}</b>:  {} in <b>{}</b> on line <b>{}</b><br />\n",
                level, msg, self.file, self.cur_line
            ));
        } else {
            self.emit(&format!(
                "\n{}: {} in {} on line {}\n",
                level, msg, self.file, self.cur_line
            ));
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

    #[allow(dead_code)]
    fn notice(&mut self, msg: &str) -> Result<(), PhpError> {
        if self.silence > 0 || self.error_level & 8 == 0 {
            return Ok(());
        }
        self.emit_diag("Notice", 8, msg)
    }

    fn deprecated(&mut self, msg: &str) -> Result<(), PhpError> {
        if self.silence > 0 || self.error_level & 8192 == 0 {
            return Ok(());
        }
        self.emit_diag("Deprecated", 8192, msg)
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
                let frames = e.trace.clone().unwrap_or_default();
                let mut t = String::new();
                for (i, fr) in frames.iter().enumerate() {
                    t.push_str(&format!("#{} {}\n", i, fr));
                }
                t.push_str(&format!("#{} {{main}}\n", frames.len()));
                self.emit(&format!(
                    "\nFatal error: Uncaught {}: {} in {}:{}\nStack trace:\n{}  thrown in {} on line {}\n",
                    class,
                    e.message,
                    self.file,
                    e.line,
                    t,
                    self.file,
                    e.thrown_line.unwrap_or(e.line)
                ));
            }
            // Plain fatals (compile errors, E_ERROR) print no trace.
            _ => {
                let s = format!(
                    "\nFatal error: {} in {} on line {}\n",
                    e.message, self.file, e.line
                );
                if self.mem_exceeded {
                    // Memory-exhausted: buffers are dropped, so the
                    // fatal goes straight to output (bug45392).
                    self.out.push_str(&s);
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
    fn uncaught(&mut self, v: &Value) {
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
                    self.file.to_string(),
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
            } else {
                // Buffered output precedes the fatal, as PHP's output
                // layer would emit it (bug32828's throwing handler).
                self.flush_ob_all();
                // Zend prints `Uncaught C: msg` — no colon when msg empty.
                let colon = if msg.is_empty() { "" } else { ": " };
                if self.ini_on("html_errors") {
                    self.out.push_str(&format!(
                        "<br />\n<b>Fatal error</b>:  Uncaught {}{}{} in {}:{}\nStack trace:\n{}\n  thrown in <b>{}</b> on line <b>{}</b><br />\n",
                        class, colon, msg, file, line, tr, file, thrown
                    ));
                } else {
                    self.out.push_str(&format!(
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
                thrown: self.cur_line as u32,
                full_msg: String::new(),
                eval_ctx: 0,
                frames: Rc::new(self.call_trace.clone()),
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
            trace: None,
            thrown_line: None,
            display_msg: None,
            kind: ErrorKind::Throw,
            message: "throw".into(),
            line: self.cur_line,
        }
    }

    /// Builtin call — errors become catchable throwables via `fail`.
    fn call_builtin(&mut self, name: &str, args: &[Cell]) -> Result<Option<Value>, PhpError> {
        self.call_trace.push(TraceFrame {
            function: name.to_string(),
            class: None,
            ty: String::new(),
            file: self.file.to_string(),
            line: self.cur_line as u32,
            args: args.to_vec(),
            internal: true,
        });
        let r = builtins::call(self, name, args);
        self.call_trace.pop();
        match r {
            Ok(r) => Ok(r),
            Err(e) => self.fail(e),
        }
    }

    fn fail<T>(&mut self, e: PhpError) -> Result<T, PhpError> {
        if let ErrorKind::Uncaught { class } = e.kind {
            // Internal errors raised as exceptions become real throwables so
            // userland `catch` blocks can intercept them.
            let v = self.exception(class, &e.message);
            if let Value::Object(o) = &v {
                if let Some(ObjectInternal::Exception {
                    trace,
                    thrown,
                    line,
                    full_msg,
                    ..
                }) = &mut o.borrow_mut().internal
                {
                    if let Some(frames) = &e.trace {
                        let mut t = String::new();
                        for (i, f) in frames.iter().enumerate() {
                            t.push_str(&format!("#{} {}\n", i, f));
                        }
                        t.push_str(&format!("#{} {{main}}", frames.len()));
                        *trace = t;
                    }
                    if let Some(l) = e.thrown_line {
                        *thrown = l as u32;
                        *line = l as u32;
                    }
                    if let Some(m) = &e.display_msg {
                        *full_msg = m.clone();
                    }
                }
            }
            self.pending_exception = Some(v);
            return Err(PhpError {
                trace: None,
                thrown_line: None,
                display_msg: None,
                kind: ErrorKind::Throw,
                message: e.message,
                line: e.line,
            });
        }
        Err(e)
    }

    pub fn exec_block(&mut self, stmts: &[Stmt]) -> Flow {
        for s in stmts {
            // memory_limit fires between statements (bug45392).
            let limit = self.ini_bytes("memory_limit");
            if limit > 0 && self.mem_used as i64 > limit {
                self.mem_exceeded = true;
                return self.err_flow(PhpError::fatal(
                    format!(
                        "Allowed memory size of {} bytes exhausted (tried to allocate {} bytes)",
                        limit, self.mem_last
                    ),
                    self.cur_line,
                ));
            }
            if let Some(d) = self.deadline {
                if std::time::Instant::now() > d {
                    let secs = self.deadline_secs;
                    return self.err_flow(PhpError::fatal(
                        format!(
                            "Maximum execution time of {} second{} exceeded",
                            secs,
                            if secs == 1 { "" } else { "s" }
                        ),
                        self.cur_line,
                    ));
                }
            }
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
            Stmt::Deprecated { msg, line } => {
                self.cur_line = *line;
                match self.deprecated(msg) {
                    Ok(()) => Flow::Normal,
                    Err(e) => self.err_flow(e),
                }
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
                if let Err(e) = self.decl_type_checks(&d.name, d) {
                    return self.err_flow(e);
                }
                let mut d = d.clone();
                d.file = self.cur_file.clone();
                self.functions.insert(d.name.to_lowercase(), Rc::new(d));
                Flow::Normal
            }
            Stmt::Class(d) => {
                for m in &d.methods {
                    let fname = format!("{}::{}", d.name, m.decl.name);
                    if let Err(e) = self.decl_type_checks(&fname, &m.decl) {
                        return self.err_flow(e);
                    }
                }
                let mut d = (**d).clone();
                for m in &mut d.methods {
                    let mut mm = (**m).clone();
                    mm.decl.file = self.cur_file.clone();
                    *m = Rc::new(mm);
                }
                if self.early_bound_classes.contains(&d.name.to_lowercase()) {
                    return Flow::Normal;
                }
                if let Err(e) = self.register_class(Rc::new(d)) {
                    return self.err_flow(e);
                }
                Flow::Normal
            }
            Stmt::Static { vars, line } => {
                let key = self.fn_statics_key();
                for (name, default) in vars {
                    // `static $a` redeclared at a different site in the same
                    // scope is a compile fatal (tests/lang/static_basic_002).
                    let prev = self
                        .static_decls
                        .entry(key.clone())
                        .or_default()
                        .insert(name.clone(), *line);
                    if prev.is_some_and(|l| l != *line) {
                        return self.err_flow(PhpError::fatal(
                            format!("Duplicate declaration of static variable ${}", name),
                            self.cur_line,
                        ));
                    }
                    // Statics live per-function-decl: inside a function
                    // they never fall back to the top-level table
                    // (static_variation_001).
                    let exists = {
                        let table = if self.stack.is_empty() {
                            Some(&self.global_statics)
                        } else {
                            self.statics.get(&key)
                        };
                        table.and_then(|t| t.get(name).cloned())
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
                            } else if let Err(e) = self
                                .notice("Only variable references should be returned by reference")
                            {
                                return self.err_flow(e);
                            }
                            return Flow::Return(c.borrow().clone());
                        }
                        if let Err(e) =
                            self.notice("Only variable references should be returned by reference")
                        {
                            return self.err_flow(e);
                        }
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
            Stmt::Global(names) => {
                // Bind each local name to its global cell. `$$x` resolves
                // the name dynamically (bug24396).
                for e in names {
                    let name = match e {
                        Expr::Var(n) => n.clone(),
                        // `global $$b` — the global name is $b's value.
                        Expr::VarVar(inner) => match self.eval(inner) {
                            Ok(v) => match self.conv_str(&v) {
                                Ok(s) => s,
                                Err(e) => return self.err_flow(e),
                            },
                            Err(e) => return self.err_flow(e),
                        },
                        other => match self.eval(other) {
                            Ok(v) => match self.conv_str(&v) {
                                Ok(s) => s,
                                Err(e) => return self.err_flow(e),
                            },
                            Err(e) => return self.err_flow(e),
                        },
                    };
                    let gcell = self
                        .globals
                        .vars
                        .entry(name.clone())
                        .or_insert_with(|| cell(Value::Null))
                        .clone();
                    self.cur().vars.insert(name, gcell);
                }
                Flow::Normal
            }
            Stmt::Unset(xs) => {
                for x in xs {
                    match x {
                        Expr::Var(n) => {
                            self.cur().vars.remove(n);
                        }
                        Expr::VarVar(inner) => {
                            if let Ok(n) = self.eval(inner) {
                                if let Ok(name) = self.conv_str(&n) {
                                    self.cur().vars.remove(&name);
                                }
                            }
                        }
                        Expr::Index { e, i } => {
                            let _ = self.unset_index(e, i.as_deref());
                        }
                        Expr::Prop { .. } => {
                            if let Err(e) = self.unset_prop(x) {
                                return self.err_flow(e);
                            }
                        }
                        Expr::StaticProp { class, name } => {
                            if let Ok(pn) = self.prop_name(name) {
                                if let Ok(cls) = self.class_of(class) {
                                    cls.statics.borrow_mut().remove(&pn);
                                }
                            }
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
            Stmt::Namespace(n) => {
                // Top-level scope follows `namespace` declarations —
                // unqualified calls/consts resolve relative to it.
                self.globals.ns = n.clone();
                Flow::Normal
            }
            Stmt::Use(names) => {
                // `use A;` / `use \B;` with no compound name has no
                // effect and warns (namespaces/ns_033).
                for n in names {
                    if !n.contains('\\') {
                        if let Err(e) = self.warn(&format!(
                            "The use statement with non-compound name '{}' has no effect",
                            n
                        )) {
                            return self.err_flow(e);
                        }
                    }
                }
                Flow::Normal
            }
            Stmt::ConstDecl(defs) => {
                for (n, e) in defs {
                    // TRUE/FALSE/NULL are reserved — `const NULL` is a
                    // compile-time fatal (namespaces/ns_075).
                    let short = n.rsplit('\\').next().unwrap_or(n);
                    if matches!(short.to_uppercase().as_str(), "TRUE" | "FALSE" | "NULL") {
                        return self.err_flow(PhpError::fatal(
                            format!("Cannot redeclare constant '{}'", short),
                            self.cur_line,
                        ));
                    }
                    match self.eval(e) {
                        Ok(v) => self.define_const(n, v),
                        Err(e) => return self.err_flow(e),
                    }
                }
                Flow::Normal
            }
            Stmt::Declare { .. } => Flow::Normal,
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
        if matches!(key, Some(ForeachKey::ByRef)) {
            return self.err_flow(PhpError::fatal(
                "Key element cannot be a reference",
                self.cur_line,
            ));
        }
        let src = match self.eval(arr) {
            Ok(v) => v,
            Err(e) => return self.err_flow(e),
        };
        match src {
            Value::Array(rc) => {
                let by_ref = matches!(val, ForeachTarget::ByRef(_));
                // `&$v` foreach iterates the live array — appends and
                // removals during the loop are observed (foreachLoop.009).
                let live = by_ref;
                if live {
                    // PHP separates a shared (non-reference) array when the
                    // loop takes references to its elements, so &-writes
                    // don't leak into other copies (foreachLoop.016). An
                    // is_ref array is iterated live as-is.
                    let rc = if Rc::strong_count(&rc) > 1 && !rc.borrow().is_ref {
                        let sep: Vec<(ArrKey, Cell)> = rc
                            .borrow()
                            .entries
                            .iter()
                            .map(|(k, c)| (k.clone(), cell(c.borrow().clone())))
                            .collect();
                        let nr = Rc::new(RefCell::new(PhpArray {
                            entries: sep,
                            next: rc.borrow().next,
                            is_ref: false,
                        }));
                        if let Ok(c) = self.eval_cell(arr) {
                            *c.borrow_mut() = Value::Array(nr.clone());
                        }
                        nr
                    } else {
                        rc
                    };
                    rc.borrow_mut().is_ref = true;
                    // PHP's live iterator tracks "the element after the
                    // current one in logical order" — prepends (unshift) and
                    // renumbering (shift) don't move it, tombstoned current
                    // elements still anchor it (foreachLoop.013/.015).
                    let mut last: Option<Cell> = None;
                    loop {
                        let next = {
                            let a = rc.borrow();
                            let live_at = |from: usize| -> Option<(ArrKey, Cell)> {
                                a.entries[from..]
                                    .iter()
                                    .find(|(k, _)| !matches!(k, ArrKey::Tomb))
                                    .cloned()
                            };
                            match &last {
                                None => live_at(0),
                                Some(lc) => {
                                    match a.entries.iter().position(|(_, c)| Rc::ptr_eq(c, lc)) {
                                        Some(i) => live_at(i + 1),
                                        // Current element gone entirely —
                                        // restart at the first live element.
                                        None => live_at(0),
                                    }
                                }
                            }
                        };
                        let Some((k, c)) = next else { break };
                        last = Some(c.clone());
                        if let Some(ForeachKey::Var(kn)) = key {
                            self.var_set(kn, key_value(&k));
                        }
                        match val {
                            ForeachTarget::Var(n) => self.var_set(n, c.borrow().clone()),
                            ForeachTarget::ByRef(n) => {
                                self.cur().vars.insert(n.clone(), c);
                            }
                            ForeachTarget::Lvalue(e) => {
                                let _ = self.store(e, c.borrow().clone());
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
                    return Flow::Normal;
                }
                // Snapshot (key, cell) pairs — PHP iterates a copy for
                // value-iteration but shares cells for &-iteration.
                let snapshot: Vec<(ArrKey, Cell)> = if by_ref {
                    rc.borrow().iter().cloned().collect()
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
                        ForeachTarget::Lvalue(e) => {
                            let _ = self.store(e, c.borrow().clone());
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
                // IteratorAggregate → getIterator() then iterate that
                // (its result may itself be an IteratorAggregate — loop).
                if self.obj_is_a(&o, "IteratorAggregate") {
                    let mut cur = o.clone();
                    loop {
                        let it_obj = match self.method_invoke(cur.clone(), "getIterator", vec![]) {
                            Ok(v) => v,
                            Err(e) => return self.err_flow(e),
                        };
                        match it_obj {
                            Value::Object(io) if self.obj_is_a(&io, "IteratorAggregate") => {
                                cur = io;
                            }
                            // getIterator() must return a Traversable.
                            Value::Object(io) if self.obj_is_a(&io, "Iterator") => {
                                return self.exec_foreach_iter(io, key, val, body);
                            }
                            _ => {
                                let cls_name = cur.borrow().class.name().to_string();
                                let v = self.exception(
                                    "Exception",
                                    &format!(
                                        "Objects returned by {}::getIterator() must be traversable or implement interface Iterator",
                                        cls_name
                                    ),
                                );
                                return Flow::Throw(v);
                            }
                        }
                    }
                }
                if self.obj_is_a(&o, "Iterator") {
                    if matches!(val, ForeachTarget::ByRef(_)) {
                        let v = self.exception(
                            "Error",
                            "An iterator cannot be used with foreach by reference",
                        );
                        let e = self.throw(v);
                        return self.err_flow(e);
                    }
                    return self.exec_foreach_iter(o.clone(), key, val, body);
                }
                // Plain object: iterate the property table in
                // declaration order — backed slots plus *virtual* hooked
                // props (which have no slot but still yield their get
                // value), with dynamic props appended (property_hooks/
                // foreach). unset() during the loop tombstones a slot
                // (foreachLoopObjects.004/.005).
                let cls = o.borrow().class.clone();
                let (spec, decl_names) = self.object_foreach_spec(&o);
                let mut pos = 0usize;
                let mut dyn_pos = 0usize;
                loop {
                    // After the declared spec runs out, scan prop_order
                    // live for dynamic props — ones added during the
                    // loop are seen (foreach_002); declared names hide
                    // same-named dynamics entirely.
                    let (ent, resolved_decl) = if pos < spec.len() {
                        (spec[pos].clone(), true)
                    } else {
                        let mut found = None;
                        loop {
                            let k = {
                                let ob = o.borrow();
                                ob.prop_order.get(dyn_pos).cloned()
                            };
                            let Some(k) = k else { break };
                            dyn_pos += 1;
                            let plain = k
                                .strip_prefix('\0')
                                .and_then(|r| r.split('\0').nth(1))
                                .unwrap_or(k.as_str());
                            if decl_names.contains(plain) {
                                continue;
                            }
                            if !spec.iter().any(|(_, sk, _)| sk == &k) {
                                found = Some((k.clone(), k.clone(), k.clone()));
                                break;
                            }
                        }
                        match found {
                            Some(e) => (e, false),
                            None => break,
                        }
                    };
                    pos += 1;
                    let (n, slot_key, dname) = ent;
                    // Spec entries are already scope-resolved; dynamics
                    // are runtime slots checked against the caller.
                    if !resolved_decl && !self.prop_visible(&cls, &dname) {
                        continue;
                    }
                    // Resolve this entry: hooked props (backed or
                    // virtual) read/write through their hooks; plain
                    // props read the live slot (unset() tombstones).
                    let mut writeback: Option<(PropDecl, MergedHooks, Value)> = None;
                    let c: Cell = if let Some((pd, hs)) = self.hooked_prop(&o, &dname) {
                        // Write-only *virtual* hooked props aren't in the
                        // readable property table — foreach skips them
                        // (virtualSetOnly in property_hooks/foreach).
                        // A set-only BACKED prop still has a table slot
                        // and iterates as its raw value (gh15187).
                        if !hs.iter().any(|(h, _)| h.is_get && h.body.is_some())
                            && !self.backed_for(&o, &dname, &hs)
                        {
                            continue;
                        }
                        if !hs.iter().any(|(h, _)| h.is_get && h.body.is_some()) {
                            // Set-only backed prop: iterate the raw
                            // backing slot, no hook write-back. An
                            // uninitialized typed slot isn't iterated
                            // (gh15187_2).
                            match o.borrow().props.get(&slot_key).cloned() {
                                Some(c) => c,
                                None if pd.ty.is_some() => continue,
                                None => cell(Value::Null),
                            }
                        } else if matches!(val, ForeachTarget::ByRef(_)) {
                            // By-ref binds a managed reference: virtual
                            // props read via get and write back through
                            // set; a backed prop is only bindable when a
                            // `&get` hands back its real backing cell —
                            // otherwise the reference can't be created
                            // (foreach_val_to_ref, foreach_002).
                            let backed = self.backed_for(&o, &dname, &hs);
                            let by_ref_get = hs
                                .iter()
                                .find(|(h, _)| h.is_get && h.by_ref && h.body.is_some());
                            if backed && by_ref_get.is_none() {
                                let dc = self
                                    .decl_prop(&o, &dname)
                                    .map(|(_, c)| c.name().to_string())
                                    .unwrap_or_else(|| cls.name().to_string());
                                let v = self.exception(
                                    "Error",
                                    &format!(
                                        "Cannot create reference to property {}::${}",
                                        dc, dname
                                    ),
                                );
                                let e = self.throw(v);
                                return self.err_flow(e);
                            }
                            if let Some((h, hc)) = by_ref_get {
                                self.last_ret_cell = None;
                                match self.run_hook(&o, hc, &dname, h, None) {
                                    Ok(_) => self
                                        .last_ret_cell
                                        .take()
                                        .unwrap_or_else(|| cell(Value::Null)),
                                    Err(e) => return self.err_flow(e),
                                }
                            } else if hs.iter().any(|(h, _)| !h.is_get && h.body.is_some()) {
                                let v = match self.hook_read(&o, &pd, &hs) {
                                    Ok(v) => v,
                                    Err(e) => return self.err_flow(e),
                                };
                                writeback = Some((pd, hs, v.clone()));
                                cell(v)
                            } else {
                                let dc = self
                                    .decl_prop(&o, &dname)
                                    .map(|(_, c)| c.name().to_string())
                                    .unwrap_or_else(|| cls.name().to_string());
                                let v = self.exception(
                                    "Error",
                                    &format!(
                                        "Cannot create reference to property {}::${}",
                                        dc, dname
                                    ),
                                );
                                let e = self.throw(v);
                                return self.err_flow(e);
                            }
                        } else {
                            match self.hook_read(&o, &pd, &hs) {
                                Ok(v) => cell(v),
                                Err(e) => return self.err_flow(e),
                            }
                        }
                    } else {
                        let live = { o.borrow().props.get(&slot_key).cloned() };
                        match live {
                            Some(c) => c,
                            None => continue, // tombstoned by unset()
                        }
                    };
                    if let Some(ForeachKey::Var(kn)) = key {
                        self.var_set(kn, Value::str(n.clone()));
                    }
                    match val {
                        ForeachTarget::Var(n) => self.var_set(n, c.borrow().clone()),
                        ForeachTarget::ByRef(n) => {
                            self.cur().vars.insert(n.clone(), c.clone());
                        }
                        ForeachTarget::Lvalue(e) => {
                            let _ = self.store(e, c.borrow().clone());
                        }
                        ForeachTarget::List(items) => {
                            let _ = self.foreach_list(items, &c.borrow().clone());
                        }
                    }
                    match self.exec_block(body) {
                        Flow::Break(0) | Flow::Break(1) => break,
                        Flow::Break(n) => return Flow::Break(n - 1),
                        Flow::Normal | Flow::Continue(0) | Flow::Continue(1) => {}
                        Flow::Continue(n) => return Flow::Continue(n - 1),
                        f => return f,
                    }
                    // Managed reference: a changed bound value dispatches
                    // to the set hook (property_hooks/foreach).
                    if let Some((pd, hs, old)) = writeback.take() {
                        let nv = c.borrow().clone();
                        if !crate::value::identical(&nv, &old) {
                            if let Err(e) = self.hook_write(&o, &pd, &hs, nv) {
                                return self.err_flow(e);
                            }
                        }
                    }
                }
                Flow::Normal
            }
            _ => {
                if let Err(e) = self.warn(&format!(
                    "foreach() argument must be of type array|object, {} given",
                    src.debug_type()
                )) {
                    return self.err_flow(e);
                }
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
            // PHP calls current() before key() on each iteration.
            let v = self
                .method_invoke(it.clone(), "current", vec![])
                .unwrap_or(Value::Null);
            if let Some(ForeachKey::Var(kn)) = key {
                let k = self
                    .method_invoke(it.clone(), "key", vec![])
                    .unwrap_or(Value::Null);
                self.var_set(kn, k);
            }
            match val {
                ForeachTarget::Var(n) => self.var_set(n, v),
                ForeachTarget::ByRef(n) => {
                    let c = cell(v);
                    self.cur().vars.insert(n.clone(), c);
                }
                ForeachTarget::Lvalue(e) => {
                    let _ = self.store(e, v);
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
                        ForeachTarget::Lvalue(e) => {
                            let _ = self.store(e, iv);
                        }
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
                    // `&$x` elements bind the source cell, not a copy.
                    if let Expr::ByRef(e) = v {
                        let c = self.eval_cell(e)?;
                        arr.is_ref = true;
                        match k {
                            Some(ke) => {
                                let kv = self.eval(ke)?;
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
                            let val = self.eval(v)?;
                            arr.set(to_key(&kv), val);
                        }
                        None => {
                            let val = self.eval(v)?;
                            arr.push(val);
                        }
                    }
                }
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
                // Compile-time param checks for the closure's decl —
                // `{closure:FILE:LINE}():` names it (namespaces/ns_073).
                let cfile = if c.decl.file.is_empty() {
                    self.file.to_string()
                } else {
                    c.decl.file.clone()
                };
                let fname = format!("{{closure:{}:{}}}", cfile, c.decl.line);
                let decl = c.decl.clone();
                self.decl_type_checks(&fname, &decl)?;
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
                    .map(|m| m.0.decl.params.clone())
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
                let cls = self.class_of(class)?;
                let nv = self.eval(name)?;
                let n = self.conv_str(&nv)?;
                let argvals =
                    self.arg_cells(args, &[], &format!("{}::{{closure}}()", cls.name()))?;
                self.static_invoke(cls, &n, argvals)
            }
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
                        };
                        drop(ob);
                        let nv = Value::Object(self.alloc_obj(new_obj));
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
                match f {
                    // Hook frame: owner is the declaring class/trait —
                    // `T::$prop::get` for trait-origin hooks (not the
                    // consuming class).
                    Some(f) if f.hook_prop.is_some() => {
                        let (_, _, _, owner) = f.hook_prop.as_ref().unwrap();
                        Value::str(format!("{}::{}", owner, f.fn_name))
                    }
                    Some(f) => match f
                        .scope_class
                        .as_ref()
                        .map(|c| format!("{}::{}", c.name(), f.fn_name))
                    {
                        Some(s) => Value::str(s),
                        None => Value::str(""),
                    },
                    None => Value::str(""),
                }
            }
            MagicConst::Class => Value::str(
                self.stack
                    .last()
                    .and_then(|f| f.scope_class.as_ref().map(|c| c.name().to_string()))
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
                    Value::Object(o) => {
                        if self.obj_is_a(&o, "ArrayAccess") {
                            match self.method_invoke(o, "offsetExists", vec![cell(key)]) {
                                Ok(v) => Ok(v.is_truthy()),
                                Err(e) => Err(e),
                            }
                        } else {
                            Ok(false)
                        }
                    }
                    _ => Ok(false),
                }
            }
            Expr::Prop { .. } => {
                self.silence += 1;
                let v = self.prop_read_loose(e);
                self.silence -= 1;
                match v {
                    Ok(v) => Ok(!matches!(v, Value::Null)),
                    // A hooked get runs inside isset — its exceptions
                    // escape (write-only prop throws through the
                    // try/catch, not `false`).
                    Err(e) if matches!(e.kind, ErrorKind::Throw) => {
                        // Uninitialized typed prop reads still mean
                        // "not set" for isset — hook Errors escape.
                        if e.message
                            .ends_with("must not be accessed before initialization")
                        {
                            Ok(false)
                        } else {
                            Err(e)
                        }
                    }
                    Err(_) => Ok(false),
                }
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
                self.warn("Array to string conversion")?;
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
        if let Some(v) = self.constants.get(key) {
            return Ok(v.clone());
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
            // By-reference assignment: bind cells.
            let src = match value {
                Expr::Call { .. } | Expr::MethodCall { .. } | Expr::StaticCall { .. } => {
                    let (c, was_ref) = self.eval_call_cell(value)?;
                    if !was_ref {
                        self.notice("Only variables should be assigned by reference")?;
                    }
                    c
                }
                _ => self.eval_cell(value)?,
            };
            self.bind_cell(target, src.clone())?;
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
                e: Expr,
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
                // Prop-chain index: container resolved early; a dynamic index
                // expr runs for side effects and its effective key is replaced
                // by the RHS (register quirk), a literal keeps its value.
                match self.eval_cell(e) {
                    Ok(c) => {
                        // The register quirk only hits call-result dims
                        // (engine_assignExecutionOrder_001): literals, plain
                        // vars and binary-op dims keep their evaluated value.
                        let clobber = matches!(
                            i.as_deref(),
                            Some(Expr::Call { .. })
                                | Some(Expr::MethodCall { .. })
                                | Some(Expr::StaticCall { .. })
                                | Some(Expr::New { .. })
                        );
                        let key = match i.as_deref() {
                            Some(ie) => self.eval(ie).ok().filter(|_| !clobber),
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
                    e: base.clone(),
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
                    self.silence += 1;
                    let c = self.eval(target);
                    self.silence -= 1;
                    c.unwrap_or(Value::Null)
                }
            }
        } else {
            Value::Null
        };
        let mut newv = match op {
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
        match late {
            Late::Prop { ov, name } => {
                let pn = match name {
                    Some(n) => self.prop_name(&n)?,
                    None => self.conv_str(&newv)?,
                };
                self.store_prop(ov, &pn, newv.clone())?;
            }
            Late::PropStr { ov, pn } => {
                self.store_prop(ov, &pn, newv.clone())?;
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
                    match self.method_invoke(o, "offsetSet", vec![cell(kv), cell(newv.clone())]) {
                        Ok(_) => {}
                        Err(e) => return Err(e),
                    }
                    return Ok(newv);
                }
                let mut b = base.borrow_mut();
                if matches!(*b, Value::Null) {
                    *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
                }
                if let Value::Array(rc) = &mut *b {
                    let mut arr = rc.borrow_mut();
                    if append {
                        arr.push(newv.clone());
                    } else {
                        let key = key.map(|k| to_key(&k)).unwrap_or(to_key(&newv));
                        arr.set(key, newv.clone());
                    }
                }
                drop(b);
            }
            Late::Static { class, pn } => {
                let c = self.static_prop_named(&class, &pn)?;
                *c.borrow_mut() = newv.clone();
            }
            Late::Keyed { e, keys } => {
                newv = self.assign_index_path(&e, &keys, newv)?;
            }
            Late::None => match target_cell {
                Some(c) => {
                    *c.borrow_mut() = newv.clone();
                }
                None => self.store(target, newv.clone())?,
            },
        }
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
                // Binding an array by `=&` marks it referenced: later writes
                // through copies go through, not CoW-split (Zend is_ref).
                if let Value::Array(rc) = &*src.borrow() {
                    rc.borrow_mut().is_ref = true;
                }
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
                        // `=&` installs the source cell as the prop's
                        // slot itself — later writes through either name
                        // hit the same storage; a missing dynamic prop
                        // materializes a real slot (oss-fuzz-382922236).
                        if let Value::Array(rc) = &*src.borrow() {
                            rc.borrow_mut().is_ref = true;
                        }
                        let key = self.obj_prop_key(o, &pn).unwrap_or_else(|| pn.clone());
                        let mut ob = o.borrow_mut();
                        if !ob.prop_order.contains(&key) {
                            ob.prop_order.push(key.clone());
                        }
                        ob.props.insert(key, src);
                        return Ok(());
                    }
                }
                let c = self.eval_cell(target)?;
                *c.borrow_mut() = src.borrow().clone();
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

    fn store(&mut self, target: &Expr, v: Value) -> Result<(), PhpError> {
        match target {
            Expr::Var(name) => {
                // `$this = x` is a compile-time fatal in Zend (bug24573).
                if name == "this" {
                    return self.fail(PhpError::fatal("Cannot re-assign $this", 0));
                }
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
            Expr::StaticProp { class, name } => {
                let c = self.static_prop_cell(class, name)?;
                *c.borrow_mut() = v;
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
                self.store_prop(ov, &pn, v)
            }
            _ => self.fail(PhpError::fatal("Cannot assign to this expression", 0)),
        }
    }

    /// Write `$ov->$pn = v` — private-slot, `__set` or dynamic-prop rules.
    fn store_prop(&mut self, ov: Value, pn: &str, mut v: Value) -> Result<(), PhpError> {
        match ov {
            Value::Object(o) => {
                if !self.in_own_hook(&o, pn) {
                    if let Some((pd, hs)) = self.hooked_prop(&o, pn) {
                        return self.hook_write(&o, &pd, &hs, v);
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
                let k = self.obj_prop_key(&o, pn).or_else(|| {
                    self.decl_prop(&o, pn).map(|(pd, dcls)| {
                        if pd.visibility == crate::ast::Visibility::Private {
                            format!("\0{}\0{}", dcls.name(), pd.name)
                        } else {
                            pd.name.clone()
                        }
                    })
                });
                let cls = o.borrow().class.clone();
                if let Some(k) = k {
                    let mut ob = o.borrow_mut();
                    if !ob.prop_order.contains(&k) {
                        ob.prop_order.push(k.clone());
                    }
                    ob.props.insert(k, cell(v));
                    Ok(())
                } else if cls.find_method("__set").is_some() {
                    self.method_invoke(
                        o.clone(),
                        "__set",
                        vec![cell(Value::str(pn.to_string())), cell(v)],
                    )?;
                    Ok(())
                } else {
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
                            a.rsplit('\\')
                                .next()
                                .unwrap_or(a)
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
                    ob.props.insert(pn, cell(v));
                    Ok(())
                }
            }
            _ => {
                self.warn(&format!(
                    "Attempt to assign property \"{}\" on {}",
                    pn,
                    ov.gettype()
                ))?;
                Ok(())
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
    ) -> Result<Value, PhpError> {
        let mut c = self.eval_cell(e)?;
        let last = keys.len() - 1;
        for (n, k) in keys.iter().enumerate() {
            // ArrayAccess object offset path: `$o[k]` dispatches to
            // offsetSet/offsetGet instead of writing through a cell.
            let as_obj = {
                let b = c.borrow();
                match &*b {
                    Value::Object(o) if self.obj_is_a(o, "ArrayAccess") => Some(o.clone()),
                    _ => None,
                }
            };
            if let Some(o) = as_obj {
                if n == last {
                    match self.method_invoke(
                        o,
                        "offsetSet",
                        vec![cell(k.clone().unwrap_or(Value::Null)), cell(v.clone())],
                    ) {
                        Ok(_) => return Ok(v),
                        Err(e) => return Err(e),
                    }
                }
                let iv = self
                    .method_invoke(o, "offsetGet", vec![cell(k.clone().unwrap_or(Value::Null))])
                    .unwrap_or(Value::Null);
                c = cell(iv);
                continue;
            }
            match self.index_into_key(c.clone(), k.clone()) {
                Ok(nc) => {
                    if n == last {
                        *nc.borrow_mut() = v.clone();
                        return Ok(v);
                    }
                    c = nc;
                }
                Err(_) => {
                    let is_str = matches!(*c.borrow(), Value::Str(_));
                    if is_str {
                        // String offset write (final level only).
                        let mut b = c.borrow_mut();
                        let mut bytes = match &*b {
                            Value::Str(s) => s.as_bytes().to_vec(),
                            _ => Vec::new(),
                        };
                        if matches!(*b, Value::Str(_)) {
                            let vs = self.conv_str(&v).unwrap_or_default();
                            let byte = vs.as_bytes().first().copied().unwrap_or(b' ');
                            // PHP 8: negative offsets index from the end;
                            // beyond -len stays illegal (bug22592).
                            let idx_i =
                                k.as_ref().map(|k| k.to_int()).unwrap_or(bytes.len() as i64);
                            let idx_i = if idx_i < 0 {
                                idx_i + bytes.len() as i64
                            } else {
                                idx_i
                            };
                            if idx_i < 0 {
                                drop(b);
                                let orig = k.as_ref().map(|k| k.to_int()).unwrap_or_default();
                                self.warn(&format!("Illegal string offset {}", orig))?;
                                return Ok(v);
                            }
                            let idx = idx_i as usize;
                            if idx >= bytes.len() {
                                bytes.resize(idx + 1, b' ');
                            }
                            bytes[idx] = byte;
                            if vs.len() > 1 {
                                drop(b);
                                self.warn(
                                    "Only the first byte will be assigned to the string offset",
                                )?;
                                b = c.borrow_mut();
                            }
                            if let Value::Str(s) = &mut *b {
                                *s = String::from_utf8_lossy(&bytes).into_owned().into();
                            }
                            return Ok(Value::str(String::from_utf8_lossy(&[byte]).into_owned()));
                        }
                        return Ok(v);
                    }
                    let t = c.borrow().debug_type();
                    if keys.len() == 1 {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Cannot use a scalar value as an array",
                            self.cur_line,
                        ));
                    }
                    self.warn(&format!("Cannot use {} as array", t))?;
                    return Ok(v);
                }
            }
        }
        Ok(v)
    }

    /// `set_index` with an already-evaluated key.
    fn set_index_val(&mut self, e: &Expr, key: Option<Value>, v: Value) -> Result<(), PhpError> {
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
                        // For `$a = $b; $a[0]=1` PHP copies. Handle via split.
                        // A referenced array (is_ref — elements aliased) is
                        // written through, never split.
                        if Rc::strong_count(rc) > 1 && !rc.borrow().is_ref {
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
                                // PHP 8: negative offsets index from the
                                // end; beyond -len is illegal (bug22592).
                                let orig = k.to_int();
                                let idx = if orig < 0 {
                                    orig + bytes.len() as i64
                                } else {
                                    orig
                                };
                                if idx < 0 {
                                    drop(b);
                                    self.warn(&format!("Illegal string offset {}", orig))?;
                                    return Ok(());
                                }
                                let idx = idx as usize;
                                let vs = self.conv_str(&v).unwrap_or_default();
                                let vb = vs.as_bytes();
                                if idx >= bytes.len() {
                                    bytes.resize(idx + 1, b' ');
                                }
                                bytes[idx] = vb.first().copied().unwrap_or(b' ');
                                if vs.len() > 1 {
                                    drop(b);
                                    self.warn(
                                        "Only the first byte will be assigned to the string offset",
                                    )?;
                                    return Ok(());
                                }
                            }
                            None => bytes.extend_from_slice(v.to_php_string().as_bytes()),
                        }
                        *b = Value::str(String::from_utf8_lossy(&bytes).into_owned());
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
                            let mut b = bc.borrow_mut();
                            if let Value::Str(s) = &mut *b {
                                let vs = self.conv_str(&v).unwrap_or_default();
                                let mut bytes = s.as_bytes().to_vec();
                                let orig = key
                                    .as_ref()
                                    .map(|k| k.to_int())
                                    .unwrap_or(bytes.len() as i64);
                                let idx = if orig < 0 {
                                    orig + bytes.len() as i64
                                } else {
                                    orig
                                };
                                if idx < 0 {
                                    drop(b);
                                    self.warn(&format!("Illegal string offset {}", orig))?;
                                    return Ok(());
                                }
                                let idx = idx as usize;
                                if idx >= bytes.len() {
                                    bytes.resize(idx + 1, b' ');
                                }
                                bytes[idx] = vs.as_bytes().first().copied().unwrap_or(b' ');
                                let multi = vs.len() > 1;
                                *s = String::from_utf8_lossy(&bytes).into_owned().into();
                                if multi {
                                    drop(b);
                                    self.warn(
                                        "Only the first byte will be assigned to the string offset",
                                    )?;
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
                            match self.method_invoke(o, "offsetSet", vec![cell(kv), cell(v)]) {
                                Ok(_) => Ok(()),
                                Err(e) => Err(e),
                            }
                        }
                        // Nested dim on a scalar is a Warning, not an Error
                        // (engine_assignExecutionOrder_002) — write is skipped.
                        Ok(bc) => {
                            let t = bc.borrow().debug_type();
                            drop(bc);
                            self.warn(&format!("Cannot use {} as array", t))?;
                            Ok(())
                        }
                        _ => Err(e2),
                    },
                }
            }
            _ => self.fail(PhpError::fatal("Cannot use expression as array", 0)),
        }
    }

    /// Index into `c`'s array value, taking a cell for `key`/`[]`.
    fn index_into_key(&mut self, c: Cell, key: Option<Value>) -> Result<Cell, PhpError> {
        let mut b = c.borrow_mut();
        if matches!(*b, Value::Null) {
            *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
        }
        if let Value::Array(rc) = &mut *b {
            if Rc::strong_count(rc) > 1 && !rc.borrow().is_ref {
                let fresh = rc.borrow().clone();
                *b = Value::Array(Rc::new(RefCell::new(fresh)));
            }
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
        } else {
            drop(b);
            self.fail(PhpError::fatal("Cannot use scalar value as an array", 0))
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
                            self.warn(&format!("Undefined array key {}", shown))?;
                        }
                        Ok(Value::Null)
                    }
                }
            }
            Value::Str(s) => {
                // String offset keys: a leading int is used with an
                // 'Illegal string offset' warning when followed by
                // non-numeric junk; a key with no leading int (or a
                // float-shaped one) is a TypeError (bug29566).
                let idx = match &key {
                    Value::Str(k) => {
                        let b = k.as_bytes();
                        let mut i = usize::from(b.first() == Some(&b'-'));
                        let start = i;
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                        if i == b.len() && i > start {
                            k.parse::<i64>().unwrap_or(0)
                        } else if i > start && matches!(numeric(k), Numeric::Leading(_, _)) {
                            self.warn(&format!("Illegal string offset \"{}\"", k))?;
                            k[..i].parse::<i64>().unwrap_or(0)
                        } else {
                            return self.fail(PhpError::uncaught(
                                "TypeError",
                                "Cannot access offset of type string on string",
                                self.cur_line,
                            ));
                        }
                    }
                    _ => key.to_int(),
                };
                let bytes = s.as_bytes();
                let idx = if idx < 0 {
                    idx + bytes.len() as i64
                } else {
                    idx
                };
                if idx < 0 || idx as usize >= bytes.len() {
                    if self.silence == 0 {
                        self.warn(&format!("Uninitialized string offset {}", key.to_int()))?;
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
                    // PHP 8.5 dropped "value of type" from this message
                    // (bug25922, passByReference_003).
                    self.warn("Trying to access array offset on null")?;
                }
                Ok(Value::Null)
            }
            Value::Object(o) => {
                if self.obj_is_a(&o, "ArrayAccess") {
                    return match self.method_invoke(o, "offsetGet", vec![cell(key)]) {
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

    fn unset_index(&mut self, e: &Expr, i: Option<&Expr>) -> Result<(), PhpError> {
        let key = match i {
            Some(ie) => Some(self.eval(ie)?),
            None => None,
        };
        // ArrayAccess object: `unset($o[k])` -> offsetUnset.
        if let Ok(Value::Object(o)) = self.eval(e) {
            if self.obj_is_a(&o, "ArrayAccess") {
                let kv = key.unwrap_or(Value::Null);
                return match self.method_invoke(o, "offsetUnset", vec![cell(kv)]) {
                    Ok(_) => Ok(()),
                    Err(err) => Err(err),
                };
            }
        }
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
                if let Ok(Value::Object(o)) = self.eval(inner) {
                    if self.obj_is_a(&o, "ArrayAccess") {
                        let kv = key.unwrap_or(Value::Null);
                        match self.method_invoke(o, "offsetUnset", vec![cell(kv)]) {
                            Ok(_) => return Ok(()),
                            Err(e) => return Err(e),
                        }
                    }
                }
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
        // PHP warns on undefined vars/props/keys during ++/-- (bug25547).
        let old = match target {
            Expr::Var(name) => self.var_get(name).unwrap_or(Value::Null),
            Expr::Index { e, i } => self.index_read(e, i.as_deref()).unwrap_or(Value::Null),
            Expr::Prop { .. } => self.prop_read_loose(target).unwrap_or(Value::Null),
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
        let new = self.incdec_value(&old, delta)?;
        self.store(target, new.clone())?;
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
                        )?;
                        Value::str(perl_inc(s))
                    } else {
                        self.deprecated(
                            "Decrement on non-numeric string has no effect and is deprecated",
                        )?;
                        v.clone()
                    }
                }
            },
            _ => v.clone(),
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
                    other => match numeric(&other.to_php_string()) {
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
                let (lv, rv) = self.binary_operands(l, r)?;
                let ls = self.conv_str(&lv)?;
                let rs = self.conv_str(&rv)?;
                Ok(Value::str(format!("{}{}", ls, rs)))
            }
            "==" | "!=" | "===" | "!==" | "<" | "<=" | ">" | ">=" | "<=>" => {
                let (lv, rv) = self.binary_operands(l, r)?;
                Ok(self.compare_op(op, &lv, &rv))
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
    fn coerce_int(&mut self, v: &Value) -> i64 {
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
            CastKind::String => Value::str(self.conv_str(&v)?),
            CastKind::Array => match v {
                Value::Array(_) => v,
                Value::Null => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
                // `(array) $obj` exposes raw slots under their (possibly
                // mangled) keys — hooks are not run (dump.phpt).
                Value::Object(o) => {
                    let mut a = PhpArray::new();
                    let ob = o.borrow();
                    for n in &ob.prop_order {
                        if let Some(c) = ob.props.get(n) {
                            a.set(ArrKey::Str(n.clone().into()), c.borrow().clone());
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
                    }))
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
                let mn = self.prop_name(name)?;
                let params = self
                    .find_method_in(&cls, &mn)
                    .map(|m| m.0.decl.params.clone())
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
                match v {
                    // `(expr)()` — IIFE on a closure/invokable value.
                    Value::Callable(_) | Value::Object(_) => {
                        let vals = self.arg_cells(args, &[], "")?;
                        return self.call_value(&v, vals);
                    }
                    _ => self.conv_str(&v).unwrap_or_default(),
                }
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
                            self.notice("Only variables should be passed by reference")?;
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
        // `\u{1}f` marks a source-literal unqualified call — only it may
        // fall back `ns\f` -> `f`; dynamic names are fully qualified.
        let (unqualified, lname) = match fname.strip_prefix('\u{1}') {
            Some(n) => (true, n.to_lowercase()),
            None => (false, fname.trim_start_matches('\\').to_lowercase()),
        };
        // `__HALT_COMPILER()` stops execution of the file (ns_080).
        if lname == "__halt_compiler" {
            return Err(PhpError {
                trace: None,
                thrown_line: None,
                display_msg: None,
                kind: ErrorKind::Fatal,
                message: "\u{1}exit:0".to_string(),
                line: 0,
            });
        }
        let mut decl = self.functions.get(&lname).cloned();
        // A namespaced user function outranks the global/builtin one for
        // unqualified calls (namespaces/ns_013).
        let mut ns_resolved = false;
        if decl.is_none() && unqualified {
            let ns = self.caller_ns();
            if !ns.is_empty() {
                let cand = format!("{}\\{}", ns.to_lowercase(), lname);
                decl = self.functions.get(&cand).cloned();
                ns_resolved = decl.is_some();
            }
        }
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
                            ty: None,
                            promoted: false,
                            vis: None,
                            readonly: false,
                            is_final: false,
                            set_vis: None,
                            hooks: None,
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
            &format!("{}()", fname.trim_start_matches('\u{1}')),
        )?;
        if !ns_resolved {
            if let Some(v) = self.call_builtin(&lname, &argvals)? {
                return Ok(v);
            }
        }
        let decl = match decl {
            Some(d) => d,
            None => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Call to undefined function {}()",
                        fname.trim_start_matches('\u{1}')
                    ),
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
                        let n = n.trim_start_matches('\\');
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
                // Fully-qualified dynamic names carry a leading `\`
                // (namespaces/ns_032).
                let name = s.trim_start_matches('\\').to_string();
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
        unused: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        // Callee `Stmt::Line` markers must not leak into the caller:
        // diagnostics after the call report the call-site line.
        let saved_line = self.cur_line;
        // A callback invoked from inside a builtin's own machinery
        // (internal_cb: ob handlers, sort callbacks) has call site
        // `[internal function]`; engine callbacks like the error handler
        // invoked mid-eval instead report the builtin's own call site
        // (bug32828 vs bug28213).
        let from_builtin =
            self.internal_cb > 0 && self.call_trace.last().map(|f| f.internal).unwrap_or(false);
        let (site_file, site_line) = if from_builtin {
            ("[internal function]".to_string(), 0)
        } else {
            (self.file.to_string(), saved_line as u32)
        };
        let fr = self
            .stack
            .last()
            .map(|f| TraceFrame {
                function: f.fn_name.clone(),
                class: f.scope_class.as_ref().map(|c| c.name().to_string()),
                ty: if f.this_obj.is_some() {
                    "->"
                } else if f.scope_class.is_some() {
                    "::"
                } else {
                    ""
                }
                .to_string(),
                file: site_file.clone(),
                line: site_line,
                args: args.clone(),
                internal: false,
            })
            .unwrap_or_else(|| TraceFrame {
                function: decl.name.clone(),
                class: None,
                ty: String::new(),
                file: site_file,
                line: site_line,
                args: args.clone(),
                internal: false,
            });
        self.call_trace.push(fr);
        self.last_call_by_ref = decl.by_ref;
        let r = self.bind_and_run_inner(decl, args, unused);
        // Overwrite (don't restore): the flag must describe THIS callee even
        // though nested calls overwrote it during the body.
        self.last_call_by_ref = decl.by_ref;
        self.call_trace.pop();
        self.cur_line = saved_line;
        r
    }

    /// PHP's compile-time checks on typed params (tests/lang/type_hints_*):
    /// `= null` on a non-nullable type is the implicit-nullable deprecation;
    /// a scalar literal default on a class type is a fatal.
    fn decl_type_checks(&mut self, fname: &str, decl: &FunctionDecl) -> Result<(), PhpError> {
        let builtins = [
            "int", "float", "string", "bool", "array", "object", "callable", "iterable", "mixed",
            "void", "never", "false", "true", "self", "parent", "static", "null",
        ];
        let saved = self.cur_line;
        for p in &decl.params {
            let Some(ty) = &p.ty else { continue };
            self.cur_line = decl.line;
            let nullable = ty.iter().any(|m| m.eq_ignore_ascii_case("null"));
            let null_default = match &p.default {
                Some(Expr::Null) => true,
                Some(Expr::Const(c)) => c.eq_ignore_ascii_case("null"),
                _ => false,
            };
            match &p.default {
                _ if null_default => {
                    if !nullable {
                        self.deprecated(&format!(
                            "{}(): Implicitly marking parameter ${} as nullable is deprecated, the explicit nullable type must be used instead",
                            fname, p.name
                        ))?;
                    }
                }
                Some(Expr::Int(_)) | Some(Expr::Float(_)) | Some(Expr::Str(_))
                | Some(Expr::Bool(_)) => {
                    let kind = match p.default {
                        Some(Expr::Int(_)) => "int",
                        Some(Expr::Float(_)) => "float",
                        Some(Expr::Str(_)) => "string",
                        _ => "bool",
                    };
                    let mut disp = ty.clone();
                    disp.retain(|m| !m.eq_ignore_ascii_case("null"));
                    let tn = disp.join("|");
                    if disp
                        .iter()
                        .any(|m| !builtins.contains(&m.to_lowercase().as_str()))
                    {
                        self.cur_line = saved;
                        return Err(PhpError::fatal(
                            format!(
                                "Cannot use {} as default value for parameter ${} of type {}",
                                kind, p.name, tn
                            ),
                            decl.line,
                        ));
                    }
                }
                _ => {}
            }
        }
        self.cur_line = saved;
        Ok(())
    }

    /// Scalar literal default check context ends; whether `v` satisfies a
    /// type member — scalar builtins pass (weak-mode coercion territory).
    fn param_type_match(&mut self, m: &str, v: &Value) -> bool {
        let l = m.to_lowercase();
        match l.as_str() {
            "null" => matches!(v, Value::Null),
            "int" | "float" | "string" | "bool" | "mixed" | "void" | "never" | "false" | "true"
            | "self" | "parent" | "static" => true,
            "array" => matches!(v, Value::Array(_)),
            "iterable" => {
                matches!(v, Value::Array(_))
                    || matches!(v, Value::Object(o) if self.obj_is_a(o, "Traversable"))
            }
            "callable" => matches!(
                v,
                Value::Callable(_) | Value::Str(_) | Value::Object(_) | Value::Array(_)
            ),
            "object" => matches!(v, Value::Object(_)),
            // Named class/interface — instanceof check.
            _ => match v {
                Value::Object(o) => self.obj_is_a(o, m),
                _ => false,
            },
        }
    }

    /// PHP's "given" type word in TypeError messages.
    fn zval_type_name(&self, v: &Value) -> String {
        match v {
            Value::Null => "null".into(),
            Value::Bool(_) => "bool".into(),
            Value::Int(_) => "int".into(),
            Value::Float(_) => "float".into(),
            Value::Str(_) => "string".into(),
            Value::Array(_) => "array".into(),
            Value::Object(o) => o.borrow().class.name().to_string(),
            Value::Callable(_) => "Closure".into(),
            Value::Resource(_) => "resource".into(),
        }
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
        // Enforce declared param types (tests/lang/type_hints_*.phpt).
        for (i, p) in decl.params.iter().enumerate() {
            let (Some(ty), Some(a)) = (&p.ty, args.get(i)) else {
                continue;
            };
            let v = a.borrow().clone();
            let implicit_null = !ty.iter().any(|m| m.eq_ignore_ascii_case("null"))
                && match &p.default {
                    Some(Expr::Null) => true,
                    Some(Expr::Const(c)) => c.eq_ignore_ascii_case("null"),
                    _ => false,
                };
            let ok = (implicit_null && matches!(v, Value::Null))
                || ty.iter().any(|m| self.param_type_match(m, &v));
            if !ok {
                let fname = self
                    .stack
                    .last()
                    .and_then(|f| f.decl_class.as_ref().map(|c| c.name().to_string()))
                    .map(|c| format!("{}::{}", c, decl.name))
                    .unwrap_or_else(|| decl.name.clone());
                let mut disp: Vec<String> = ty
                    .iter()
                    .filter(|m| !m.eq_ignore_ascii_case("null"))
                    .cloned()
                    .collect();
                if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) || implicit_null {
                    if disp.len() == 1 {
                        disp[0] = format!("?{}", disp[0]);
                    } else {
                        disp.push("null".into());
                    }
                }
                let given = self.zval_type_name(&v);
                // getMessage() is the short form; the uncaught display
                // appends ` and defined in FILE:M` (catchable_error_002).
                let msg = format!(
                    "{}(): Argument #{} (${}) must be of type {}, {} given, called in {} on line {}",
                    fname,
                    i + 1,
                    p.name,
                    disp.join("|"),
                    given,
                    self.file,
                    self.cur_line
                );
                let display = format!("{} and defined in {}:{}", msg, self.file, decl.line);
                let argdesc = args
                    .iter()
                    .map(|a| trace_arg(&a.borrow()))
                    .collect::<Vec<_>>()
                    .join(", ");
                // Trace frames render `->` for instance calls while the
                // message keeps `::` (namespaces/ns_071).
                let arrow = if self
                    .stack
                    .last()
                    .and_then(|f| f.this_obj.as_ref())
                    .is_some()
                {
                    "->"
                } else {
                    "::"
                };
                let tname = fname.replacen("::", arrow, 1);
                let frame = format!("{}({}): {}({})", self.file, self.cur_line, tname, argdesc);
                let call_line = self.cur_line;
                self.stack.pop();
                let mut e = PhpError::uncaught("TypeError", msg, call_line);
                e.trace = Some(vec![frame]);
                e.thrown_line = Some(decl.line);
                e.display_msg = Some(display);
                return self.fail(e);
            }
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
                    // Default exprs are evaluated at call time; an error
                    // (e.g. an undefined constant) propagates as the
                    // call's failure (namespaces/ns_077).
                    let dv = match self.eval(d) {
                        Ok(v) => v,
                        Err(e) => {
                            self.stack.pop();
                            return self.fail(e);
                        }
                    };
                    binds.push((p.name.clone(), cell(dv)));
                } else {
                    binds.push((p.name.clone(), cell(Value::Null)));
                }
            }
            let frame = self.stack.last_mut().unwrap();
            // func_get_arg(i) sees the param's CURRENT value, so frame args
            // are the bound param cells plus any extra call args.
            let mut fa: Vec<Cell> = Vec::new();
            for (i, p) in decl.params.iter().enumerate() {
                if p.variadic {
                    break;
                }
                fa.push(binds[i].1.clone());
            }
            for a in &args[fa.len().min(args.len())..] {
                fa.push(a.clone());
            }
            // Promoted ctor params: declare+assign $this->{name}
            // (error_2_exception_001).
            let is_ctor = decl.name.eq_ignore_ascii_case("__construct");
            let this_obj = frame.this_obj.clone();
            let mut promoted_writes: Vec<(Rc<RefCell<PhpObject>>, String, Value)> = Vec::new();
            for (i, p) in decl.params.iter().enumerate() {
                if p.promoted && is_ctor {
                    if let Some(obj) = &this_obj {
                        let v = binds[i].1.borrow().clone();
                        promoted_writes.push((obj.clone(), p.name.clone(), v));
                    }
                }
            }
            for (n, c) in binds {
                frame.vars.insert(n, c);
            }
            frame.args = fa;
            for (obj, pname, v) in promoted_writes {
                // Promoted assignment goes through prop write semantics —
                // hooked promoted props run their set hook (gh15438_1).
                self.store_prop(Value::Object(obj), &pname, v)?;
            }
        }
        let flow = self.exec_block(&decl.body);
        self.stack.pop();
        match flow {
            Flow::Return(v) => Ok(v),
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
            Flow::Exit(c) => Err(PhpError {
                trace: None,
                thrown_line: None,
                display_msg: None,
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
        frame.ns = decl.ns.clone();
        frame.ret_by_ref = decl.by_ref;
        if let Some(obj) = &this_obj {
            frame
                .vars
                .insert("this".to_string(), cell(Value::Object(obj.clone())));
        }
        frame.decl_class = self.pending_decl_class.take();
        frame.hook_prop = self.pending_hook_prop.take();
        frame.this_obj = this_obj;
        frame.scope_class = scope_class;
        frame.file = if decl.file.is_empty() {
            self.cur_file.clone()
        } else {
            decl.file.clone()
        };
        self.stack.push(frame);
        self.bind_and_run(decl, args, Vec::new())
    }

    // ----- classes -----

    fn register_class(&mut self, decl: Rc<ClassDecl>) -> Result<(), PhpError> {
        let mut d = (*decl).clone();
        // Synthesize PropDecls from promoted constructor params
        // (`__construct(public readonly int $x)`) — they behave as
        // declared props for visibility/type/readonly and hooks.
        if d.kind != ClassKind::Interface {
            if let Some(ctor) = d
                .methods
                .iter()
                .find(|m| m.decl.name.eq_ignore_ascii_case("__construct"))
            {
                for p in &ctor.decl.params {
                    if p.promoted && !d.props.iter().any(|x| x.name == p.name) {
                        d.props.push(PropDecl {
                            name: p.name.clone(),
                            default: None,
                            is_static: false,
                            visibility: p.vis.unwrap_or(crate::ast::Visibility::Public),
                            readonly: p.readonly,
                            ty: p.ty.clone(),
                            is_abstract: false,
                            is_final: p.is_final,
                            set_vis: p.set_vis,
                            decl_in: None,
                            hooks: p.hooks.clone(),
                            line: 0,
                        });
                    }
                }
            }
        }
        self.check_hooked_props(&d)?;
        let lname = decl.name.to_lowercase();
        match decl.kind {
            ClassKind::Interface => {
                self.interfaces.insert(lname, decl);
            }
            ClassKind::Trait => {
                self.traits.insert(lname, Rc::new(d));
            }
            _ => {
                // Apply traits: merge methods/props into the decl.
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
                                if let Some(ex) = d.props.iter().find(|x| x.name == p.name) {
                                    // "hooked" means either side —
                                    // a plain decl can't override a
                                    // hooked trait prop either.
                                    if ex.hooks.is_some() || p.hooks.is_some() {
                                        // Hooked props cannot be
                                        // conflict-resolved — zend
                                        // fatals at composition
                                        // (traits_conflict).
                                        let ex_src =
                                            ex.decl_in.clone().unwrap_or_else(|| d.name.clone());
                                        return Err(PhpError::fatal(
                                            format!(
                                                "{} and {} define the same hooked property (${}) in the composition of {}. Conflict resolution between hooked properties is currently not supported. Class was composed",
                                                ex_src, t, p.name, d.name
                                            ),
                                            self.cur_line,
                                        ));
                                    }
                                    // Identical plain-prop defs merge
                                    // silently; differing ones fatal.
                                    let compat = ex.visibility == p.visibility
                                        && ex.is_static == p.is_static
                                        && ex.readonly == p.readonly
                                        && ex.ty == p.ty
                                        && format!("{:?}", ex.default)
                                            == format!("{:?}", p.default);
                                    if !compat {
                                        let ex_src =
                                            ex.decl_in.clone().unwrap_or_else(|| d.name.clone());
                                        return Err(PhpError::fatal(
                                            format!(
                                                "{} and {} define the same property (${}) in the composition of {}. However, the definition differs and is considered incompatible. Class was composed",
                                                ex_src, t, p.name, d.name
                                            ),
                                            self.cur_line,
                                        ));
                                    }
                                    continue;
                                }
                                let mut np = p.clone();
                                // Trait origin survives the merge —
                                // `__METHOD__` prints `T::$p::get`.
                                np.decl_in = Some(t.clone());
                                d.props.push(np);
                            }
                        }
                    }
                }
                self.check_abstract_hooks(&d)?;
                self.check_final_override(&d)?;
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
        Ok(())
    }

    /// `final` props/hooks may not be overridden by a subclass.
    fn check_final_override(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        let mut an = d.parent.clone();
        while let Some(pname) = an {
            let Some(pc) = self.classes.get(&pname.to_lowercase()).cloned() else {
                break;
            };
            for cp in &d.props {
                let Some(ap) = pc.decl.props.iter().find(|x| x.name == cp.name) else {
                    continue;
                };
                if ap.is_final {
                    return Err(PhpError::fatal(
                        format!(
                            "Cannot override final property {}::${}",
                            pc.decl.name, cp.name
                        ),
                        self.cur_line,
                    ));
                }
                // Property types are invariant across inheritance
                // only for *backed* props — a virtual hook pair
                // follows per-kind signature variance instead
                // (backed_invariant vs override_add_get_contravariant).
                let backed = ap.hooks.is_none()
                    || Self::prop_is_backed(ap)
                    || cp.hooks.is_none()
                    || Self::prop_is_backed(cp);
                let norm = |t: &Option<Vec<String>>| {
                    let mut v = t.clone().unwrap_or_default();
                    v.sort();
                    v.dedup();
                    v
                };
                if backed && norm(&cp.ty) != norm(&ap.ty) {
                    let aty = ap
                        .ty
                        .as_ref()
                        .map(|m| m.join("|"))
                        .unwrap_or_else(|| "mixed".into());
                    return Err(PhpError::fatal(
                        format!(
                            "Type of {}::${} must be {} (as in class {})",
                            d.name, cp.name, aty, pc.decl.name
                        ),
                        cp.line,
                    ));
                }
                if let (Some(chs), Some(ahs)) = (&cp.hooks, &ap.hooks) {
                    for ch in chs {
                        if ahs.iter().any(|ah| ah.is_get == ch.is_get && ah.is_final) {
                            let kind = if ch.is_get { "get" } else { "set" };
                            return Err(PhpError::fatal(
                                format!(
                                    "Cannot override final property hook {}::${}::{}()",
                                    pc.decl.name, cp.name, kind
                                ),
                                self.cur_line,
                            ));
                        }
                        let Some(ah) = ahs.iter().find(|ah| ah.is_get == ch.is_get) else {
                            continue;
                        };
                        // Hook signature variance: a get's return type (the
                        // prop type) is covariant; a set's $value parameter
                        // is contravariant (type_compatibility*).
                        let fmt = |t: &Option<Vec<String>>| {
                            t.as_ref()
                                .map(|m| m.join("|"))
                                .unwrap_or_else(|| "mixed".into())
                        };
                        if ch.is_get {
                            // A parent `&get` requires the child's get to
                            // return by reference too; a child `&get`
                            // under a plain parent get is fine
                            // (interface_get_value_as_ref).
                            if ah.by_ref && !ch.by_ref {
                                return Err(PhpError::fatal(
                                    format!(
                                        "Declaration of {}::${}::get() must be compatible with & {}::${}::get()",
                                        d.name, cp.name, pc.decl.name, cp.name
                                    ),
                                    cp.line,
                                ));
                            }
                            let (cty, aty) = (fmt(&cp.ty), fmt(&ap.ty));
                            let cm = cp.ty.clone().unwrap_or_else(|| vec!["mixed".into()]);
                            let am = ap.ty.clone().unwrap_or_else(|| vec!["mixed".into()]);
                            if !self.ty_sup(&am, &cm) {
                                return Err(PhpError::fatal(
                                    format!(
                                        "Declaration of {}::${}::get(): {} must be compatible with {}::${}::get(): {}",
                                        d.name, cp.name, cty, pc.decl.name, cp.name, aty
                                    ),
                                    cp.line,
                                ));
                            }
                        } else {
                            let eff = |pd: &crate::ast::PropDecl, h: &crate::ast::PropHook| {
                                h.params
                                    .first()
                                    .and_then(|sp| sp.ty.clone())
                                    .or_else(|| pd.ty.clone())
                                    .unwrap_or_else(|| vec!["mixed".into()])
                            };
                            let cm = eff(cp, ch);
                            let am = eff(ap, ah);
                            let cn = ch
                                .params
                                .first()
                                .map(|sp| sp.name.clone())
                                .unwrap_or_else(|| "value".into());
                            let an = ah
                                .params
                                .first()
                                .map(|sp| sp.name.clone())
                                .unwrap_or_else(|| "value".into());
                            if !self.ty_sup(&cm, &am) {
                                return Err(PhpError::fatal(
                                    format!(
                                        "Declaration of {}::${}::set({} ${}): void must be compatible with {}::${}::set({} ${}): void",
                                        d.name, cp.name, cm.join("|"), cn,
                                        pc.decl.name, cp.name, am.join("|"), an
                                    ),
                                    cp.line,
                                ));
                            }
                        }
                    }
                }
            }
            an = pc.decl.parent.clone();
        }
        // Interface prop hooks also constrain the implementation's
        // signatures (get_by_ref_implemented_by_val: `&get;` in the
        // interface requires `&get` in the class).
        for iname in &d.implements {
            let Some(iface) = self.interfaces.get(&iname.to_lowercase()).cloned() else {
                continue;
            };
            for cp in &d.props {
                let Some(ap) = iface.props.iter().find(|x| x.name == cp.name) else {
                    continue;
                };
                let (Some(chs), Some(ahs)) = (&cp.hooks, &ap.hooks) else {
                    continue;
                };
                for ch in chs {
                    let Some(ah) = ahs.iter().find(|ah| ah.is_get == ch.is_get) else {
                        continue;
                    };
                    if ch.is_get && ah.by_ref && !ch.by_ref {
                        return Err(PhpError::fatal(
                            format!(
                                "Declaration of {}::${}::get() must be compatible with & {}::${}::get()",
                                d.name, cp.name, iface.name, cp.name
                            ),
                            cp.line,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// PHP 8.4 hooked-property decl checks (property_hooks tests): hooks
    /// are forbidden on static/readonly props; a default requires a
    /// *backed* prop; `set(T)` must be type-compatible; `final`/`abstract`
    /// and interface restrictions produce link-time fatals.
    fn check_hooked_props(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        let in_iface = d.kind == ClassKind::Interface;
        for p in &d.props {
            if in_iface && p.is_abstract {
                return Err(PhpError::fatal(
                    "Property in interface cannot be explicitly abstract. All interface members are implicitly abstract",
                    self.cur_line,
                ));
            }
            if in_iface && p.visibility != crate::ast::Visibility::Public {
                return Err(PhpError::fatal(
                    "Property in interface cannot be protected or private",
                    self.cur_line,
                ));
            }
            if in_iface && p.is_final {
                return Err(PhpError::fatal(
                    "Property in interface cannot be final",
                    self.cur_line,
                ));
            }
            if p.is_abstract && p.hooks.is_none() {
                return Err(PhpError::fatal(
                    "Only hooked properties may be declared abstract",
                    self.cur_line,
                ));
            }
            if p.is_abstract && p.is_final {
                return Err(PhpError::fatal(
                    "Cannot use the final modifier on an abstract property",
                    self.cur_line,
                ));
            }
            if p.is_final && p.visibility == crate::ast::Visibility::Private {
                return Err(PhpError::fatal(
                    "Property cannot be both final and private",
                    self.cur_line,
                ));
            }
            // Untyped readonly faults before the hooked-property rules
            // (the same message a plain readonly prop gets).
            if p.readonly && p.ty.is_none() {
                return Err(PhpError::fatal(
                    format!("Readonly property {}::${} must have type", d.name, p.name),
                    self.cur_line,
                ));
            }
            let Some(hs) = &p.hooks else { continue };
            // readonly classes forbid hooked props entirely, whether
            // declared or ctor-promoted (gh15419_1, gh15419_2).
            if d.readonly {
                return Err(PhpError::fatal(
                    "Hooked properties cannot be readonly",
                    self.cur_line,
                ));
            }
            if p.is_abstract && hs.iter().all(|h| h.body.is_some()) {
                return Err(PhpError::fatal(
                    format!(
                        "Abstract property {}::${} must specify at least one abstract hook",
                        d.name, p.name
                    ),
                    self.cur_line,
                ));
            }
            for h in hs {
                if let Some(v) = h.visibility {
                    let vn = match v {
                        crate::ast::Visibility::Public => "public",
                        crate::ast::Visibility::Protected => "protected",
                        crate::ast::Visibility::Private => "private",
                    };
                    return Err(PhpError::fatal(
                        format!("Cannot use the {} modifier on a property hook", vn),
                        self.cur_line,
                    ));
                }
                if h.is_final && p.visibility == crate::ast::Visibility::Private {
                    return Err(PhpError::fatal(
                        "Property hook cannot be both final and private",
                        self.cur_line,
                    ));
                }
                if h.is_final && (in_iface || h.body.is_none()) {
                    return Err(PhpError::fatal(
                        "Property hook cannot be both abstract and final",
                        self.cur_line,
                    ));
                }
                if h.body.is_none() && p.visibility == crate::ast::Visibility::Private {
                    return Err(PhpError::fatal(
                        "Property hook cannot be both abstract and private",
                        self.cur_line,
                    ));
                }
                if h.is_get && (h.has_plist || !h.params.is_empty()) {
                    return Err(PhpError::fatal(
                        format!(
                            "get hook of property {}::${} must not have a parameter list",
                            d.name, p.name
                        ),
                        self.cur_line,
                    ));
                }
                if !h.is_get {
                    if let Some(sp) = h.params.first() {
                        if sp.default.is_some() {
                            return Err(PhpError::fatal(
                                format!(
                                    "Parameter ${} of set hook {}::${} must not have a default value",
                                    sp.name, d.name, p.name
                                ),
                                self.cur_line,
                            ));
                        }
                        if sp.by_ref {
                            return Err(PhpError::fatal(
                                format!(
                                    "Parameter ${} of set hook {}::${} must not be pass-by-reference",
                                    sp.name, d.name, p.name
                                ),
                                self.cur_line,
                            ));
                        }
                        if sp.variadic {
                            return Err(PhpError::fatal(
                                format!(
                                    "Parameter ${} of set hook {}::${} must not be variadic",
                                    sp.name, d.name, p.name
                                ),
                                self.cur_line,
                            ));
                        }
                    }
                }
            }
            if p.is_static {
                return Err(PhpError::fatal(
                    "Cannot declare hooks for static property",
                    self.cur_line,
                ));
            }
            if p.readonly {
                return Err(PhpError::fatal(
                    "Hooked properties cannot be readonly",
                    self.cur_line,
                ));
            }
            if p.default.is_some()
                && !Self::prop_is_backed(p)
                && !self.chain_prop_backed(d, &p.name)
            {
                return Err(PhpError::fatal(
                    format!(
                        "Cannot specify default value for virtual hooked property {}::${}",
                        d.name, p.name
                    ),
                    self.cur_line,
                ));
            }
            // `&get` alongside `set` is legal only on a *virtual* prop —
            // when the hooks (or a plain ancestor decl) back the property
            // the engine can't reconcile the returned reference with set
            // writes (get_by_ref_virtual vs get_by_ref_backed).
            let backed = Self::prop_is_backed(p) || {
                let mut par = d.parent.clone();
                let mut found = false;
                while let Some(pname) = par {
                    let Some(pc) = self.classes.get(&pname.to_lowercase()) else {
                        break;
                    };
                    if pc.decl.props.iter().any(|p2| {
                        p2.name == p.name
                            && p2.hooks.is_none()
                            && p2.visibility != crate::ast::Visibility::Private
                    }) {
                        found = true;
                        break;
                    }
                    par = pc.decl.parent.clone();
                }
                found
            };
            if backed && hs.iter().any(|h| !h.is_get) && hs.iter().any(|h| h.is_get && h.by_ref) {
                return Err(PhpError::fatal(
                    format!(
                        "Get hook of backed property {}::{} with set hook may not return by reference",
                        d.name, p.name
                    ),
                    self.cur_line,
                ));
            }
            // A hook without a body is only legal in an interface or on
            // a prop declared `abstract`.
            let abs_ok = d.kind == ClassKind::Interface || p.is_abstract;
            if !abs_ok && hs.iter().any(|h| h.body.is_none()) {
                return Err(PhpError::fatal(
                    "Non-abstract property hook must have a body",
                    self.cur_line,
                ));
            }
            if let Some(set) = hs.iter().find(|h| !h.is_get) {
                if let Some(sp) = set.params.first() {
                    // The set $value parameter must accept every value the
                    // property type admits (param type ⊇ prop type); an
                    // untyped prop is mixed, and an untyped parameter is
                    // only legal on an untyped prop
                    // (set_value_parameter_type_variance_005).
                    let compat = match (&p.ty, &sp.ty) {
                        (None, None) => true,
                        (pty, Some(sty)) => {
                            let pty = pty.clone().unwrap_or_else(|| vec!["mixed".to_string()]);
                            self.ty_sup(sty, &pty)
                        }
                        (Some(_), None) => false,
                    };
                    if !compat {
                        return Err(PhpError::fatal(
                            format!(
                                "Type of parameter ${} of hook {}::${}::set must be compatible with property type",
                                sp.name, d.name, p.name
                            ),
                            0,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// `sup` is a supertype of `sub` when every `sub` member is admitted
    /// by some `sup` member — equal names, `mixed`, or a class/interface
    /// the member is-a (set_value_parameter_type_variance_006).
    fn ty_sup(&mut self, sup: &[String], sub: &[String]) -> bool {
        sub.iter()
            .all(|t| sup.iter().any(|s| self.ty_member_is_a(t, s)))
    }

    /// Type-member acceptance: `t` is admitted by `s` when they match by
    /// name, `s` is `mixed`, or `t`'s class/interface ancestry includes
    /// `s` (interfaces live in `self.interfaces`, not `self.classes`).
    fn ty_member_is_a(&mut self, t: &str, s: &str) -> bool {
        if s.eq_ignore_ascii_case(t) || s.eq_ignore_ascii_case("mixed") {
            return true;
        }
        if t.eq_ignore_ascii_case("null") || t.eq_ignore_ascii_case("mixed") {
            return false;
        }
        if let Some(iface) = self.interfaces.get(&t.to_lowercase()).cloned() {
            let mut stack = vec![iface];
            while let Some(f) = stack.pop() {
                for p in &f.implements {
                    if p.eq_ignore_ascii_case(s) {
                        return true;
                    }
                    if let Some(ff) = self.interfaces.get(&p.to_lowercase()).cloned() {
                        stack.push(ff);
                    }
                }
            }
            return false;
        }
        self.is_a_str(t, s)
    }

    /// An abstract hook (`get;`/`set;` in an interface or `abstract`
    /// prop) the class must implement — reported as abstract methods
    /// (`A::$p::get`) at class-decl link time. Inherited but
    /// unimplemented hooks still fault subclasses (v3-style:
    /// `abstract A implements I` leaves `I::$p::get` for `B extends A`).
    fn check_abstract_hooks(&self, d: &ClassDecl) -> Result<(), PhpError> {
        if d.is_abstract {
            return Ok(());
        }
        // Concrete hook impls available to this class: its own decl plus
        // every ancestor.
        let mut chain: Vec<&ClassDecl> = vec![d];
        let mut pn = d.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()) else {
                break;
            };
            chain.push(pc.decl.as_ref());
            pn = pc.decl.parent.clone();
        }
        let implemented = |pname: &str, is_get: bool| {
            chain.iter().any(|c| {
                c.props.iter().any(|cp| {
                    if cp.name != pname {
                        return false;
                    }
                    if let Some(ch) = &cp.hooks {
                        return ch.iter().any(|x| x.is_get == is_get && x.body.is_some());
                    }
                    // A plain prop satisfies `get` always; `set` only
                    // when writable (not readonly / private(set)).
                    if is_get {
                        true
                    } else {
                        !cp.readonly && cp.set_vis.is_none()
                    }
                })
            })
        };
        // Abstract hook decls owed by this class: its interfaces + every
        // class/interface in the ancestor chain.
        let mut sources: Vec<&ClassDecl> = Vec::new();
        for c in &chain {
            sources.push(*c);
            for i in &c.implements {
                if let Some(f) = self.interfaces.get(&i.to_lowercase()) {
                    sources.push(f.as_ref());
                }
            }
        }
        let mut missing: Vec<String> = Vec::new();
        for src in sources {
            for p in &src.props {
                let Some(hs) = &p.hooks else { continue };
                for h in hs {
                    if h.body.is_some() {
                        continue;
                    }
                    let kind = if h.is_get { "get" } else { "set" };
                    let label = format!("{}::${}::{}", src.name, p.name, kind);
                    if implemented(&p.name, h.is_get) {
                        continue;
                    }
                    // A readonly prop (implicit private(set)) can not
                    // satisfy an interface `set` — a dedicated message,
                    // not the abstract-method one.
                    if !h.is_get && src.kind == ClassKind::Interface {
                        let ro = chain.iter().any(|c| {
                            c.props.iter().any(|cp| {
                                cp.name == p.name
                                    && cp.hooks.is_none()
                                    && (cp.readonly || cp.set_vis.is_some())
                            })
                        });
                        if ro {
                            return Err(PhpError::fatal(
                                format!(
                                    "Set access level of {}::${} must be omitted (as in class {})",
                                    d.name, p.name, src.name
                                ),
                                self.cur_line,
                            ));
                        }
                    }
                    if !missing.contains(&label) {
                        missing.push(label);
                    }
                }
            }
        }
        if missing.is_empty() {
            return Ok(());
        }
        let (n, list) = (missing.len(), missing.join(", "));
        Err(PhpError::fatal(
            format!(
                "Class {} contains {} abstract method{} and must therefore be declared abstract or implement the remaining method{} ({})",
                d.name,
                n,
                if n == 1 { "" } else { "s" },
                if n == 1 { "" } else { "s" },
                list
            ),
            self.cur_line,
        ))
    }

    /// Resolve a class expression to a class name.
    fn class_name_of(&mut self, e: &Expr) -> Result<String, PhpError> {
        match e {
            Expr::Const(n) => Ok(self.resolve_class_name(n)),
            Expr::Str(s) => Ok(self.resolve_class_name(s)),
            Expr::AnonClass(d) => {
                self.register_class(d.clone())?;
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

    /// Allocate a PHP object handle id: reuse the lowest dead slot,
    /// like Zend's object store recycling freed handles.
    fn next_obj_id(&mut self, rc: &Rc<RefCell<PhpObject>>) -> u64 {
        let w = Rc::downgrade(rc);
        for (i, h) in self.obj_handles.iter_mut().enumerate() {
            if h.upgrade().is_none() {
                *h = w.clone();
                return (i + 1) as u64;
            }
        }
        self.obj_handles.push(w);
        self.obj_handles.len() as u64
    }

    /// Wrap a PhpObject in Rc and assign its handle id.
    pub fn alloc_obj(&mut self, o: PhpObject) -> Rc<RefCell<PhpObject>> {
        let rc = Rc::new(RefCell::new(o));
        let id = self.next_obj_id(&rc);
        rc.borrow_mut().id = id;
        rc
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
        // __construct (native for builtins via method_invoke's
        // interception); the ctor may be inherited (property_hooks/foreach).
        if self.find_method_in(&cls, "__construct").is_some() {
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
                            readonly: false,
                            parent: None,
                            implements: vec![],
                            attrs: vec![],
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
                    id: 0,
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
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (ci, c) in chain.iter().enumerate() {
            for p in &c.decl.props {
                if p.is_static {
                    continue;
                }
                let is_priv = p.visibility == crate::ast::Visibility::Private;
                if !is_priv {
                    // The NEAREST redecl is authoritative — defaults are
                    // never inherited (default_value_inheritance): a later
                    // decl replaces any slot a grandparent already made,
                    // but keeps the first declaration's slot position
                    // (foreachLoopObjects.002).
                    if !seen.insert(p.name.clone()) {
                        props.remove(&p.name);
                    }
                }
                let backed = if p.hooks.is_some() {
                    Self::prop_is_backed(p)
                        || chain[..=ci].iter().any(|c2| {
                            c2.decl.props.iter().any(|p2| {
                                p2.name == p.name
                                    && p2.hooks.is_none()
                                    && p2.visibility != crate::ast::Visibility::Private
                            })
                        })
                } else {
                    true
                };
                // A virtual hooked prop has no backing slot at all.
                if !backed {
                    continue;
                }
                // A typed prop without a default starts *uninitialized* —
                // no cell, but Zend still reserves its table position, so
                // a later write lands in declaration order
                // (property_hooks/foreach's backedUninitialized).
                if p.ty.is_some() && p.default.is_none() {
                    let key = if is_priv {
                        format!("\0{}\0{}", c.decl.name, p.name)
                    } else {
                        p.name.clone()
                    };
                    if !prop_order.contains(&key) {
                        prop_order.push(key);
                    }
                    continue;
                }
                let default = match &p.default {
                    Some(d) => self.eval(d).unwrap_or(Value::Null),
                    None => Value::Null,
                };
                // Private props live in a per-declaring-class slot
                // ("\0Cls\0name"), so C::$e and E::$e are distinct.
                let key = if is_priv {
                    format!("\0{}\0{}", c.decl.name, p.name)
                } else {
                    p.name.clone()
                };
                if !prop_order.contains(&key) {
                    prop_order.push(key.clone());
                }
                props.insert(key, cell(default));
            }
        }
        let internal = if self.is_throwable_name(&cls.decl.name) {
            Some(ObjectInternal::Exception {
                file: self.file.to_string(),
                line: self.cur_line as u32,
                trace: String::new(),
                thrown: self.cur_line as u32,
                full_msg: String::new(),
                eval_ctx: 0,
                frames: Rc::new(self.call_trace.clone()),
            })
        } else {
            None
        };
        Value::Object(self.alloc_obj(PhpObject {
            class: cls,
            props,
            prop_order,
            id: 0,
            internal,
        }))
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

    // ----- property hooks (PHP 8.4, Zend/tests/property_hooks) -----

    /// True while `o`'s own hook on `pn` is running — inside a hook body
    /// `$this->pn` is the backing slot, not a re-entry into the hook.
    fn in_own_hook(&self, o: &Rc<RefCell<PhpObject>>, pn: &str) -> bool {
        self.stack
            .last()
            .and_then(|f| f.hook_prop.as_ref())
            .is_some_and(|(id, n, _, _)| *id == o.borrow().id && n == pn)
    }

    /// Name of the class whose scope the current frame runs in —
    /// private props are only visible to their own declaring class.
    /// Namespace of the currently executing code — the running
    /// function's declaring namespace, or the file-level `namespace`
    /// for top-level statements (Zend/tests/namespaces).
    pub fn caller_ns(&self) -> String {
        self.stack
            .last()
            .map(|f| f.ns.clone())
            .unwrap_or_else(|| self.globals.ns.clone())
    }

    pub fn caller_scope_name(&self) -> Option<String> {
        self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|c| c.name().to_string())
        })
    }

    /// A private prop of the caller's scope class: it is a *distinct*
    /// property from same-name decls elsewhere in the chain and wins
    /// outright when the caller's scope declares it (private_override).
    fn scope_private_prop(
        &self,
        o: &Rc<RefCell<PhpObject>>,
        pn: &str,
    ) -> Option<(PropDecl, Rc<PhpClass>)> {
        let scope = self.caller_scope_name()?;
        let mut cur = Some(o.borrow().class.clone());
        while let Some(c) = cur {
            if c.name() == scope {
                if let Some(p) = c
                    .decl
                    .props
                    .iter()
                    .find(|p| p.name == pn && p.visibility == crate::ast::Visibility::Private)
                {
                    return Some((p.clone(), c.clone()));
                }
                break;
            }
            let par = c.decl.parent.clone();
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        None
    }

    /// The effective hooked prop for `pn`: the nearest PropDecl (for
    /// name/type/visibility) plus hooks merged along the chain — each
    /// hook kind resolves to the nearest decl that provides it (a plain
    /// child redecl still inherits parent hooks; per-hook origin class
    /// drives `__METHOD__` and decl_class). Private props of other
    /// scopes are skipped — they are different properties entirely.
    fn hooked_prop(&self, o: &Rc<RefCell<PhpObject>>, pn: &str) -> Option<(PropDecl, MergedHooks)> {
        if let Some((p, c)) = self.scope_private_prop(o, pn) {
            return p.hooks.as_ref().map(|hs| {
                (
                    p.clone(),
                    hs.iter().cloned().map(|h| (h, c.clone())).collect(),
                )
            });
        }
        let mut nearest: Option<PropDecl> = None;
        let mut hooks: MergedHooks = Vec::new();
        let mut cur = Some(o.borrow().class.clone());
        while let Some(c) = cur {
            for p in &c.decl.props {
                if p.name != pn {
                    continue;
                }
                if p.visibility == crate::ast::Visibility::Private {
                    continue;
                }
                if nearest.is_none() {
                    nearest = Some(p.clone());
                }
                if let Some(hs) = &p.hooks {
                    for h in hs {
                        if !hooks.iter().any(|(x, _)| x.is_get == h.is_get) {
                            hooks.push((h.clone(), c.clone()));
                        }
                    }
                }
            }
            let parent = c.decl.parent.clone();
            cur = parent.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        let pd = nearest?;
        if hooks.is_empty() {
            return None;
        }
        Some((pd, hooks))
    }

    /// Foreach iteration spec for a plain object: `(emitted key, slot
    /// key, decl name)` entries for declared props in first-declaration
    /// order — parent props first, a child redecl keeps the first decl's
    /// slot position, private props keep per-class mangled keys, virtual
    /// hooked props appear (no slot) at their decl position. Dynamic
    /// props are NOT included — the loop scans prop_order live so props
    /// added mid-iteration still appear (foreach_002). Returns the spec
    /// plus the declared-name set used to hide shadowed dynamics.
    fn object_foreach_spec(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
    ) -> (
        Vec<(String, String, String)>,
        std::collections::HashSet<String>,
    ) {
        let mut spec: Vec<(String, String, String)> = Vec::new();
        let mut decl_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut cur = Some(o.borrow().class.clone());
        let mut chain = Vec::new();
        while let Some(c) = cur {
            let par = c.decl.parent.clone();
            chain.push(c.clone());
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        chain.reverse();
        let scope = self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|c| c.name().to_string())
        });
        for c in &chain {
            for p in &c.decl.props {
                if p.is_static {
                    continue;
                }
                decl_names.insert(p.name.clone());
            }
        }
        // One entry per prop NAME: each name resolves to a single decl
        // through scope-private-first — a private decl wins only for its
        // own declaring scope, else the first non-private decl. The
        // entry emits at the RESOLVED decl's position (a child's private
        // redecl iterates at the end, after inherited protecteds), and
        // only when that resolved decl is visible — an invisible
        // resolution suppresses the name entirely (C::e skipped under
        // an E scope, but emitted under C).
        let mut emitted: Vec<(usize, usize, String, String)> = Vec::new();
        let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
        for c in &chain {
            for p in &c.decl.props {
                if p.is_static || done.contains(&p.name) {
                    continue;
                }
                done.insert(p.name.clone());
                // Resolve: first private decl matching the caller scope,
                // else the first non-private decl.
                let mut pick: Option<(usize, usize)> = None;
                for (ci, cc) in chain.iter().enumerate() {
                    for (pi, p2) in cc.decl.props.iter().enumerate() {
                        if p2.name != p.name || p2.is_static {
                            continue;
                        }
                        if p2.visibility == crate::ast::Visibility::Private {
                            if scope.as_ref().is_some_and(|sc| sc == &cc.decl.name) {
                                pick = Some((ci, pi));
                                break;
                            }
                        } else if pick.is_none() {
                            pick = Some((ci, pi));
                        }
                    }
                    if pick.as_ref().is_some_and(|&(ci, pi)| {
                        chain[ci].decl.props[pi].visibility == crate::ast::Visibility::Private
                    }) {
                        break;
                    }
                }
                let Some((ci, pi)) = pick else { continue };
                let decl = &chain[ci].decl.props[pi];
                let dcls = &chain[ci];
                let visible = match decl.visibility {
                    crate::ast::Visibility::Public => true,
                    crate::ast::Visibility::Private => {
                        scope.as_ref().is_some_and(|sc| sc == &dcls.decl.name)
                    }
                    crate::ast::Visibility::Protected => scope.as_ref().is_some_and(|sc| {
                        self.is_a_str(sc, &dcls.decl.name) || self.is_a_str(&dcls.decl.name, sc)
                    }),
                };
                if !visible {
                    continue;
                }
                // Position: a private decl emits at its own decl position;
                // a non-private prop shares the first declaration's slot.
                let (pci, ppi) = if decl.visibility == crate::ast::Visibility::Private {
                    (ci, pi)
                } else {
                    chain
                        .iter()
                        .enumerate()
                        .find_map(|(i, cc)| {
                            cc.decl
                                .props
                                .iter()
                                .enumerate()
                                .find(|(_, p3)| p3.name == decl.name && !p3.is_static)
                                .map(|(j, _)| (i, j))
                        })
                        .unwrap_or((ci, pi))
                };
                let slot_key = if decl.visibility == crate::ast::Visibility::Private {
                    format!("\0{}\0{}", dcls.decl.name, decl.name)
                } else {
                    decl.name.clone()
                };
                emitted.push((pci, ppi, decl.name.clone(), slot_key));
            }
        }
        emitted.sort_by_key(|(a, b, _, _)| (*a, *b));
        for (_, _, n, k) in emitted {
            spec.push((n.clone(), k, n));
        }
        (spec, decl_names)
    }

    /// Serialization view for get_object_vars/json_encode/var_export:
    /// per-DECL entries in parent-first order — non-private decls
    /// dedupe by name at their first position while private decls emit
    /// one entry per declaring class (both `changed`s in dump.phpt).
    /// Returns `(emitted key, slot key, decl+decl class)` entries;
    /// dynamic props appended live in insertion order carry `None` —
    /// the caller filters by visibility and resolves values.
    pub fn object_serial_entries(&self, o: &Rc<RefCell<PhpObject>>) -> Vec<SerialEntry> {
        let mut entries = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut cur = Some(o.borrow().class.clone());
        let mut chain = Vec::new();
        while let Some(c) = cur {
            let par = c.decl.parent.clone();
            chain.push(c.clone());
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        chain.reverse();
        for c in &chain {
            for p in &c.decl.props {
                if p.is_static {
                    continue;
                }
                if p.visibility == crate::ast::Visibility::Private {
                    entries.push((
                        p.name.clone(),
                        format!("\0{}\0{}", c.decl.name, p.name),
                        Some((p.clone(), c.clone())),
                    ));
                } else if seen.insert(p.name.clone()) {
                    entries.push((p.name.clone(), p.name.clone(), Some((p.clone(), c.clone()))));
                }
            }
        }
        // Dynamic props follow the declared entries in insertion order
        // (gh20479's g/h, oss-fuzz-382922236's b); mangled keys are
        // declared-private slots emitted by their own entries already.
        let emitted: std::collections::HashSet<String> =
            entries.iter().map(|(_, s, _)| s.clone()).collect();
        for k in &o.borrow().prop_order {
            if emitted.contains(k) || k.starts_with('\0') {
                continue;
            }
            entries.push((k.clone(), k.clone(), None));
        }
        entries
    }

    /// Resolve one serial entry's value: a hooked prop runs its `get`
    /// (write-only props are skipped); a plain prop yields the live
    /// slot (uninitialized slots are skipped).
    pub fn serial_entry_value(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        p: &PropDecl,
        dcls: &Rc<PhpClass>,
        slot: &str,
    ) -> Option<Value> {
        let hs: MergedHooks = if p.visibility == crate::ast::Visibility::Private {
            p.hooks
                .as_ref()
                .map(|hs| hs.iter().cloned().map(|h| (h, dcls.clone())).collect())
                .unwrap_or_default()
        } else {
            match self.hooked_prop(o, &p.name) {
                Some((_, hs)) => hs,
                None => Vec::new(),
            }
        };
        if !hs.is_empty() {
            if let Some((h, c)) = hs.iter().find(|(h, _)| h.is_get && h.body.is_some()) {
                // Serialization bypasses the caller's visibility — a
                // private hook runs in its own declaring scope (dump).
                let v = self.run_hook(o, c, &p.name, h, None).ok()?;
                return self.hook_get_typecheck(p, c, v).ok();
            }
            // A set-only hooked prop still backs a slot — serialization
            // reads it raw, like a plain prop (gh17988).
        }
        o.borrow().props.get(slot).map(|c| c.borrow().clone())
    }

    /// get_class_vars(): declared prop defaults in parent-first decl
    /// order, filtered by caller visibility — hooked props keep their
    /// raw default (no `get` run), virtual props and private props
    /// outside scope are omitted (gh15456).
    pub fn class_default_props(&mut self, cls: &Rc<PhpClass>) -> Vec<(String, Value)> {
        let mut chain = Vec::new();
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            let par = c.decl.parent.clone();
            chain.push(c.clone());
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        chain.reverse();
        let scope = self.caller_scope_name();
        let mut out: Vec<(String, Value)> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (ci, c) in chain.iter().enumerate() {
            for p in &c.decl.props {
                let visible = match p.visibility {
                    crate::ast::Visibility::Public => true,
                    crate::ast::Visibility::Private => {
                        scope.as_ref().is_some_and(|sc| sc == &c.decl.name)
                    }
                    crate::ast::Visibility::Protected => scope.as_ref().is_some_and(|sc| {
                        self.is_a_str(sc, &c.decl.name) || self.is_a_str(&c.decl.name, sc)
                    }),
                };
                if !visible {
                    continue;
                }
                if p.visibility != crate::ast::Visibility::Private && !seen.insert(p.name.clone()) {
                    continue;
                }
                // Virtual hooked props have no storage — not in vars.
                if p.hooks.is_some()
                    && !Self::prop_is_backed(p)
                    && !chain[..=ci].iter().any(|c2| {
                        c2.decl.props.iter().any(|p2| {
                            p2.name == p.name
                                && p2.hooks.is_none()
                                && p2.visibility != crate::ast::Visibility::Private
                        })
                    })
                {
                    continue;
                }
                let v = match &p.default {
                    Some(d) => self.eval(d).unwrap_or(Value::Null),
                    None => Value::Null,
                };
                out.push((p.name.clone(), v));
            }
        }
        out
    }

    /// The PropDecl owning a prop_order slot key — mangled `\0Cls\0p`
    /// private keys resolve to that class's decl, plain keys to the
    /// nearest non-private decl (var_dump's `uninitialized(T)`).
    pub fn decl_for_slot(&self, o: &Rc<RefCell<PhpObject>>, key: &str) -> Option<PropDecl> {
        let mut chain = Vec::new();
        let mut cur = Some(o.borrow().class.clone());
        while let Some(c) = cur {
            let par = c.decl.parent.clone();
            chain.push(c.clone());
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        if let Some(r) = key.strip_prefix('\0') {
            let mut it = r.split('\0');
            let cn = it.next()?;
            let pn = it.next()?;
            for c in &chain {
                if c.decl.name == cn {
                    return c
                        .decl
                        .props
                        .iter()
                        .find(|p| p.name == pn && !p.is_static)
                        .cloned();
                }
            }
            return None;
        }
        for c in &chain {
            if let Some(p) = c.decl.props.iter().find(|p| {
                p.name == key && !p.is_static && p.visibility != crate::ast::Visibility::Private
            }) {
                return Some(p.clone());
            }
        }
        None
    }

    /// The nearest PropDecl for `pn` along the chain, honoring the same
    /// private-scope rules as `hooked_prop` (hooked or plain).
    fn decl_prop(&self, o: &Rc<RefCell<PhpObject>>, pn: &str) -> Option<(PropDecl, Rc<PhpClass>)> {
        if let Some(x) = self.scope_private_prop(o, pn) {
            return Some(x);
        }
        let mut cur = Some(o.borrow().class.clone());
        while let Some(c) = cur {
            for p in &c.decl.props {
                if p.name == pn && p.visibility != crate::ast::Visibility::Private {
                    return Some((p.clone(), c.clone()));
                }
            }
            let parent = c.decl.parent.clone();
            cur = parent.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        None
    }

    /// Backedness of the *merged* runtime prop: any merged hook body
    /// referencing `$this->prop`, or any plain (unhooked) decl for it
    /// anywhere in the chain — a hooked redecl over a plain parent prop
    /// still shares its backing (parent_get_plain).
    fn backed_for(
        &self,
        o: &Rc<RefCell<PhpObject>>,
        pn: &str,
        hs: &[(crate::ast::PropHook, Rc<PhpClass>)],
    ) -> bool {
        if hs.iter().any(|(h, _)| {
            h.body
                .as_deref()
                .is_some_and(|b| Self::stmts_use_this_prop(b, pn))
        }) {
            return true;
        }
        // Ancestor decls: a plain prop or a hook whose body relies on
        // the implicit backing store (incl. `set => expr`, which the
        // parser desugars to `$this->prop = e`) makes the effective
        // property backed even when the nearest impl looks virtual —
        // the slot already exists (gh20270: parent arrow-set read).
        let mut cur = Some(o.borrow().class.clone());
        while let Some(c) = cur {
            if c.decl
                .props
                .iter()
                .any(|p| p.name == pn && (p.hooks.is_none() || Self::prop_is_backed(p)))
            {
                return true;
            }
            let par = c.decl.parent.clone();
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        false
    }

    /// Decl-time backedness via ancestors: nearest ancestor decl named
    /// `name` — a plain prop is backed; a hooked one checks its bodies;
    /// a virtual one defers deeper.
    fn chain_prop_backed(&self, d: &ClassDecl, name: &str) -> bool {
        let mut an = d.parent.clone();
        while let Some(pn) = an {
            let Some(pc) = self.classes.get(&pn.to_lowercase()) else {
                break;
            };
            if let Some(ap) = pc.decl.props.iter().find(|x| x.name == name) {
                if ap.hooks.is_none() || Self::prop_is_backed(ap) {
                    return true;
                }
            }
            an = pc.decl.parent.clone();
        }
        false
    }

    /// A hooked prop is *backed* (has a backing slot) iff some hook body
    /// references `$this->prop`; otherwise it's virtual and has none.
    fn prop_is_backed(p: &PropDecl) -> bool {
        p.hooks.as_ref().is_some_and(|hs| {
            hs.iter().any(|h| {
                h.body
                    .as_ref()
                    .is_some_and(|b| Self::stmts_use_this_prop(b, &p.name))
            })
        })
    }

    fn stmts_use_this_prop(v: &[Stmt], pn: &str) -> bool {
        v.iter().any(|s| Self::stmt_uses_this_prop(s, pn))
    }

    fn stmt_uses_this_prop(s: &Stmt, pn: &str) -> bool {
        match s {
            Stmt::Echo(v) | Stmt::Unset(v) | Stmt::Global(v) => {
                v.iter().any(|e| Self::expr_uses_this_prop(e, pn))
            }
            Stmt::Expr(e) => Self::expr_uses_this_prop(e, pn),
            Stmt::Block(b) => Self::stmts_use_this_prop(b, pn),
            Stmt::If { cond, then, else_ } => {
                Self::expr_uses_this_prop(cond, pn)
                    || Self::stmts_use_this_prop(then, pn)
                    || Self::stmts_use_this_prop(else_, pn)
            }
            Stmt::While { cond, body } | Stmt::DoWhile { body, cond } => {
                Self::expr_uses_this_prop(cond, pn) || Self::stmts_use_this_prop(body, pn)
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
                    .any(|e| Self::expr_uses_this_prop(e, pn))
                    || Self::stmts_use_this_prop(body, pn)
            }
            Stmt::Return(Some(e)) | Stmt::Break(Some(e)) | Stmt::Continue(Some(e)) => {
                Self::expr_uses_this_prop(e, pn)
            }
            Stmt::Switch { cond, cases } => {
                Self::expr_uses_this_prop(cond, pn)
                    || cases.iter().any(|(c, b)| {
                        c.as_ref().is_some_and(|e| Self::expr_uses_this_prop(e, pn))
                            || Self::stmts_use_this_prop(b, pn)
                    })
            }
            Stmt::Foreach { arr, body, .. } => {
                Self::expr_uses_this_prop(arr, pn) || Self::stmts_use_this_prop(body, pn)
            }
            Stmt::Try {
                body,
                catches,
                finally,
            } => {
                Self::stmts_use_this_prop(body, pn)
                    || catches
                        .iter()
                        .any(|c| Self::stmts_use_this_prop(&c.body, pn))
                    || finally
                        .as_ref()
                        .is_some_and(|f| Self::stmts_use_this_prop(f, pn))
            }
            Stmt::Function(d) => {
                d.params
                    .iter()
                    .filter_map(|p| p.default.as_ref())
                    .any(|e| Self::expr_uses_this_prop(e, pn))
                    || Self::stmts_use_this_prop(&d.body, pn)
            }
            Stmt::Static { vars, .. } => vars
                .iter()
                .filter_map(|(_, d)| d.as_ref())
                .any(|e| Self::expr_uses_this_prop(e, pn)),
            Stmt::Declare { value, .. } => Self::expr_uses_this_prop(value, pn),
            _ => false,
        }
    }

    fn expr_uses_this_prop(e: &Expr, pn: &str) -> bool {
        match e {
            Expr::Prop { obj, name, .. } => {
                (matches!(obj.as_ref(), Expr::Var(v) if v == "this")
                    && matches!(name, PropName::Name(n) if n == pn))
                    || Self::expr_uses_this_prop(obj, pn)
                    || matches!(name, PropName::Expr(x) if Self::expr_uses_this_prop(x, pn))
            }
            Expr::Assign { target, value, .. } => {
                Self::expr_uses_this_prop(target, pn) || Self::expr_uses_this_prop(value, pn)
            }
            Expr::Binary { l, r, .. } => {
                Self::expr_uses_this_prop(l, pn) || Self::expr_uses_this_prop(r, pn)
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
            | Expr::Cast { e, .. }
            | Expr::Throw(e)
            | Expr::Include { e, .. } => Self::expr_uses_this_prop(e, pn),
            Expr::Ternary { c, t, f } => {
                Self::expr_uses_this_prop(c, pn)
                    || t.as_ref().is_some_and(|t| Self::expr_uses_this_prop(t, pn))
                    || Self::expr_uses_this_prop(f, pn)
            }
            Expr::Call { name, args } => {
                Self::expr_uses_this_prop(name, pn)
                    || args.iter().any(|a| Self::expr_uses_this_prop(a, pn))
            }
            Expr::StaticCallDyn { class, name, args } => {
                Self::expr_uses_this_prop(class, pn)
                    || Self::expr_uses_this_prop(name, pn)
                    || args.iter().any(|a| Self::expr_uses_this_prop(a, pn))
            }
            Expr::Index { e, i } => {
                Self::expr_uses_this_prop(e, pn)
                    || i.as_ref().is_some_and(|i| Self::expr_uses_this_prop(i, pn))
            }
            Expr::Isset(v) => v.iter().any(|e| Self::expr_uses_this_prop(e, pn)),
            Expr::Exit(Some(e)) => Self::expr_uses_this_prop(e, pn),
            Expr::ArrayLit(items) => items.iter().any(|(k, v)| {
                k.as_ref().is_some_and(|k| Self::expr_uses_this_prop(k, pn))
                    || Self::expr_uses_this_prop(v, pn)
            }),
            Expr::List(items) => items
                .iter()
                .flatten()
                .any(|e| Self::expr_uses_this_prop(e, pn)),
            Expr::Match { subject, arms } => {
                Self::expr_uses_this_prop(subject, pn)
                    || arms.iter().any(|a| {
                        a.conds.iter().any(|c| Self::expr_uses_this_prop(c, pn))
                            || Self::expr_uses_this_prop(&a.result, pn)
                    })
            }
            Expr::Closure(c) => {
                c.decl
                    .params
                    .iter()
                    .filter_map(|p| p.default.as_ref())
                    .any(|e| Self::expr_uses_this_prop(e, pn))
                    || Self::stmts_use_this_prop(&c.decl.body, pn)
            }
            Expr::New { class, args } => {
                Self::expr_uses_this_prop(class, pn)
                    || args.iter().any(|a| Self::expr_uses_this_prop(a, pn))
            }
            Expr::MethodCall {
                obj, name, args, ..
            } => {
                Self::expr_uses_this_prop(obj, pn)
                    || matches!(name, PropName::Expr(x) if Self::expr_uses_this_prop(x, pn))
                    || args.iter().any(|a| Self::expr_uses_this_prop(a, pn))
            }
            Expr::StaticProp { class, name } => {
                Self::expr_uses_this_prop(class, pn)
                    || matches!(name, PropName::Expr(x) if Self::expr_uses_this_prop(x, pn))
            }
            Expr::StaticCall { class, args, .. } => {
                Self::expr_uses_this_prop(class, pn)
                    || args.iter().any(|a| Self::expr_uses_this_prop(a, pn))
            }
            Expr::ClassConst { class, .. } => Self::expr_uses_this_prop(class, pn),
            Expr::Instanceof { obj, class } => {
                Self::expr_uses_this_prop(obj, pn) || Self::expr_uses_this_prop(class, pn)
            }
            Expr::Interp(parts) => parts.iter().any(|p| match p {
                // Interpolated `{$expr}` parts are source strings — a
                // substring check for `$this->prop` is close enough for
                // the backed-prop heuristic.
                crate::lexer::StringPart::Expr(s) => {
                    s.contains(&format!("this->{pn}")) || s.contains(&format!("this->${pn}"))
                }
                _ => false,
            }),
            _ => false,
        }
    }

    /// Caller-scope check for a hook's effective visibility (the hook's
    /// own `private get`/`protected set` or the prop's).
    /// The class a *protected* prop is scoped to — the FURTHEST
    /// ancestor in the object's chain declaring it (GH-19044: the check
    /// uses the prototype's scope, so sibling subclasses descending
    /// from that ancestor can access each other's instances).
    fn prop_scope_class(&self, o: &Rc<RefCell<PhpObject>>, pn: &str) -> Option<Rc<PhpClass>> {
        let mut found = None;
        let mut cur = Some(o.borrow().class.clone());
        while let Some(c) = cur {
            if c.decl
                .props
                .iter()
                .any(|p| p.name == pn && p.visibility != crate::ast::Visibility::Private)
            {
                found = Some(c.clone());
            }
            let par = c.decl.parent.clone();
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        found
    }

    fn hook_scope_allows(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        dcls: &Rc<PhpClass>,
        pn: &str,
        vis: crate::ast::Visibility,
    ) -> bool {
        let scope = self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|s| s.decl.name.clone())
        });
        match (vis, scope) {
            (crate::ast::Visibility::Public, _) => true,
            (crate::ast::Visibility::Private, Some(s)) => s == dcls.name(),
            (crate::ast::Visibility::Protected, Some(s)) => {
                let pcls = self.prop_scope_class(o, pn).unwrap_or_else(|| dcls.clone());
                self.is_a_str(&s, pcls.name()) || self.is_a_str(pcls.name(), &s)
            }
            _ => false,
        }
    }

    fn hook_visibility_error<T>(
        &mut self,
        dcls: &Rc<PhpClass>,
        pname: &str,
        vis: crate::ast::Visibility,
    ) -> Result<T, PhpError> {
        self.fail(PhpError::uncaught(
            "Error",
            format!(
                "Cannot access {} property {}::${}",
                match vis {
                    crate::ast::Visibility::Private => "private",
                    crate::ast::Visibility::Protected => "protected",
                    crate::ast::Visibility::Public => "public",
                },
                dcls.name(),
                pname
            ),
            0,
        ))
    }

    /// Run a `get`/`set` hook body: a method-like frame whose `hook_prop`
    /// marker lets `$this->prop` hit the backing slot directly. `dcls` is
    /// the class that declared this hook — it becomes the frame's
    /// decl_class and names `__METHOD__`'s scope (via decl_in for traits).
    fn run_hook(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        dcls: &Rc<PhpClass>,
        pname: &str,
        hook: &crate::ast::PropHook,
        arg: Option<Cell>,
    ) -> Result<Value, PhpError> {
        let kind = if hook.is_get { "get" } else { "set" };
        let params = if hook.is_get {
            Vec::new()
        } else if hook.params.is_empty() {
            vec![crate::ast::Param {
                name: "value".into(),
                default: None,
                by_ref: false,
                variadic: false,
                ty: None,
                promoted: false,
                vis: None,
                readonly: false,
                is_final: false,
                set_vis: None,
                hooks: None,
            }]
        } else {
            hook.params.clone()
        };
        // PHP names hooks `$prop::set` — `__METHOD__` then composes the
        // declaring class into `C::$prop::set` (backed_implicit_get).
        let decl = Rc::new(FunctionDecl {
            name: format!("${}::{}", pname, kind),
            params,
            body: hook.body.clone().unwrap_or_default(),
            by_ref: hook.by_ref,
            line: self.cur_line,
            file: self.cur_file.clone(),
            ns: String::new(),
        });
        let owner = decl_owner(dcls, pname);
        let args = arg.into_iter().collect::<Vec<Cell>>();
        self.pending_decl_class = Some(dcls.clone());
        self.pending_hook_prop = Some((o.borrow().id, pname.to_string(), hook.is_get, owner));
        let scope = o.borrow().class.clone();
        let r = self.invoke_fn(&decl, args, Some(o.clone()), Some(scope));
        self.pending_decl_class = None;
        self.pending_hook_prop = None;
        r
    }

    /// Read through a hooked prop: `get` hook, backing slot, or the
    /// write-only error.
    fn hook_read(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        p: &PropDecl,
        hs: &[(crate::ast::PropHook, Rc<PhpClass>)],
    ) -> Result<Value, PhpError> {
        let get = hs.iter().find(|(h, _)| h.is_get && h.body.is_some());
        // Visibility: an explicit `private get`/`protected get` wins,
        // else the prop's own visibility governs reads.
        let vis = get.and_then(|(h, _)| h.visibility).unwrap_or(p.visibility);
        let dcls = &get
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| o.borrow().class.clone());
        if !self.hook_scope_allows(o, dcls, &p.name, vis) {
            return self.hook_visibility_error(dcls, &p.name, vis);
        }
        if let Some((h, c)) = get {
            let v = self.run_hook(o, c, &p.name, h, None)?;
            return self.hook_get_typecheck(p, dcls, v);
        }
        if self.backed_for(o, &p.name, hs) {
            return Ok(o
                .borrow()
                .props
                .get(&p.name)
                .map(|c| c.borrow().clone())
                .unwrap_or(Value::Null));
        }
        self.fail(PhpError::uncaught(
            "Error",
            format!("Property {}::${} is write-only", dcls.name(), p.name),
            0,
        ))
    }

    /// Strict type membership for the type-check-then-weakly-coerce
    /// pattern (unlike `param_type_match`, scalars don't loosely pass).
    fn ty_exact(&mut self, tys: &[String], v: &Value) -> bool {
        tys.iter().any(|t| {
            let l = t.to_lowercase();
            match l.as_str() {
                "null" => matches!(v, Value::Null),
                "int" => matches!(v, Value::Int(_)),
                "float" => matches!(v, Value::Float(_) | Value::Int(_)),
                "string" => matches!(v, Value::Str(_)),
                "bool" => matches!(v, Value::Bool(_)),
                _ => self.param_type_match(t, v),
            }
        })
    }

    /// Zend's "Implicit conversion from float X to int loses precision"
    /// Deprecated on a lossy float->int weak coercion.
    fn deprecate_lossy_int(&mut self, tys: &[String], v: &Value, c: &Value) {
        if !matches!(c, Value::Int(_)) || !tys.iter().any(|t| t.eq_ignore_ascii_case("int")) {
            return;
        }
        if let Value::Float(f) = v {
            if f.fract() != 0.0 {
                let _ = self.emit_diag(
                    "Deprecated",
                    8192,
                    &format!(
                        "Implicit conversion from float {} to int loses precision",
                        format_float_repr(*f)
                    ),
                );
            }
        }
    }

    /// A `get` hook's return is coerced to the property's declared type
    /// in weak mode ("C::$p::get(): Return value must be of type int,
    /// string returned" TypeError otherwise).
    fn hook_get_typecheck(
        &mut self,
        p: &PropDecl,
        dcls: &Rc<PhpClass>,
        v: Value,
    ) -> Result<Value, PhpError> {
        let Some(tys) = &p.ty else { return Ok(v) };
        if self.ty_exact(tys, &v) {
            return Ok(v);
        }
        // weak-mode coercion for scalar targets
        if let Some(c) = weak_ty_coerce(tys, &v) {
            self.deprecate_lossy_int(tys, &v, &c);
            return Ok(c);
        }
        let want = tys.join("|");
        let got = self.zval_type_name(&v);
        self.fail(PhpError::uncaught(
            "TypeError",
            format!(
                "{}::${}::get(): Return value must be of type {}, {} returned",
                dcls.name(),
                p.name,
                want,
                got
            ),
            0,
        ))
    }

    /// Typed-property write check (plain and backing writes): the
    /// assigned value is coerced in weak mode, else a catchable
    /// `Cannot assign T to property C::$p of type U` TypeError.
    fn prop_typed_write_check(
        &mut self,
        p: &PropDecl,
        dcls: &Rc<PhpClass>,
        v: Value,
    ) -> Result<Value, PhpError> {
        let Some(tys) = &p.ty else { return Ok(v) };
        if self.ty_exact(tys, &v) {
            return Ok(v);
        }
        if let Some(c) = weak_ty_coerce(tys, &v) {
            self.deprecate_lossy_int(tys, &v, &c);
            return Ok(c);
        }
        let mut e = PhpError::uncaught(
            "TypeError",
            format!(
                "Cannot assign {} to property {}::${} of type {}",
                self.zval_type_name(&v),
                dcls.name(),
                p.name,
                tys.join("|")
            ),
            0,
        );
        e.thrown_line = Some(self.cur_line);
        self.fail(e)
    }

    /// The value passed to a `set` hook is checked against the hook's
    /// `$value` parameter type — the declared prop type for the `set =>
    /// expr` shorthand — under weak coercion (gh17988's `string(2) "42"`).
    fn hook_set_arg_check(
        &mut self,
        p: &PropDecl,
        dcls: &Rc<PhpClass>,
        tys: Vec<String>,
        v: Value,
    ) -> Result<Value, PhpError> {
        if self.ty_exact(&tys, &v) {
            return Ok(v);
        }
        if let Some(c) = weak_ty_coerce(&tys, &v) {
            self.deprecate_lossy_int(&tys, &v, &c);
            return Ok(c);
        }
        let mut e = PhpError::uncaught(
            "TypeError",
            format!(
                "{}::${}::set(): Argument #1 ($value) must be of type {}, {} given, called in {} on line {}",
                dcls.name(),
                p.name,
                tys.join("|"),
                self.zval_type_name(&v),
                self.file,
                self.cur_line
            ),
            0,
        );
        e.thrown_line = Some(p.line);
        self.fail(e)
    }

    /// Write through a hooked prop: `set` hook, backing slot, or the
    /// read-only error. `private(set)` narrows the write side.
    fn hook_write(
        &mut self,
        o: &Rc<RefCell<PhpObject>>,
        p: &PropDecl,
        hs: &[(crate::ast::PropHook, Rc<PhpClass>)],
        mut v: Value,
    ) -> Result<(), PhpError> {
        let set = hs.iter().find(|(h, _)| !h.is_get && h.body.is_some());
        let dcls = &set
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| o.borrow().class.clone());
        if let Some(sv) = p.set_vis {
            if !self.hook_scope_allows(o, dcls, &p.name, sv) {
                return self.set_visibility_error(dcls, &p.name, sv);
            }
        }
        let vis = set.and_then(|(h, _)| h.visibility).unwrap_or(p.visibility);
        if !self.hook_scope_allows(o, dcls, &p.name, vis) {
            return self.hook_visibility_error(dcls, &p.name, vis);
        }
        if let Some((h, c)) = set {
            let arg_tys = h
                .params
                .first()
                .and_then(|pp| pp.ty.clone())
                .or_else(|| p.ty.clone());
            if let Some(tys) = arg_tys {
                v = self.hook_set_arg_check(p, dcls, tys, v)?;
            }
            self.run_hook(o, c, &p.name, h, Some(cell(v)))?;
            return Ok(());
        }
        if self.backed_for(o, &p.name, hs) {
            v = self.prop_typed_write_check(p, dcls, v)?;
            let mut ob = o.borrow_mut();
            if !ob.prop_order.contains(&p.name) {
                ob.prop_order.push(p.name.clone());
            }
            ob.props.insert(p.name.clone(), cell(v));
            return Ok(());
        }
        self.fail(PhpError::uncaught(
            "Error",
            format!("Property {}::${} is read-only", dcls.name(), p.name),
            0,
        ))
    }

    /// unserialize(): writing into a *virtual* hooked prop aborts the
    /// whole unserialize with warnings (property_hooks/unserialize).
    pub fn unserial_prop_virtual(&mut self, o: &Rc<RefCell<PhpObject>>, pn: &str) -> bool {
        match self.hooked_prop(o, pn) {
            Some((_, hs)) => !self.backed_for(o, pn, &hs),
            None => false,
        }
    }

    /// `parent::$prop::get()/set()` inside a hook — runs the parent
    /// class's hook for the same prop+kind, or reads/writes the
    /// parent's plain prop (parent_property_hook tests).
    fn hook_parent_call(
        &mut self,
        pn: &str,
        is_get: bool,
        args: &[Expr],
    ) -> Result<Value, PhpError> {
        let kind = if is_get { "get" } else { "set" };
        // Borrow-free snapshot of the caller frame's hook context (the
        // outside/different-prop/different-kind rules are parse-time).
        let (f_this, f_dcls) = {
            let f = self.stack.last();
            (
                f.and_then(|f| f.this_obj.clone()),
                f.and_then(|f| f.decl_class.clone()),
            )
        };
        let Some(o) = f_this else {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot use \"parent\" when no class scope is active",
                0,
            ));
        };
        let dcls = f_dcls.unwrap_or_else(|| o.borrow().class.clone());
        let Some(parent_name) = dcls.decl.parent.clone() else {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot use \"parent\" when current class scope has no parent",
                0,
            ));
        };
        let Some(parent) = self.classes.get(&parent_name.to_lowercase()).cloned() else {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot use \"parent\" when current class scope has no parent",
                0,
            ));
        };
        // Resolve the prop on the parent chain: per-kind merged hooks.
        let mut nearest: Option<(PropDecl, Rc<PhpClass>)> = None;
        let mut hooks: MergedHooks = Vec::new();
        let mut cur = Some(parent.clone());
        while let Some(c) = cur {
            for p in &c.decl.props {
                if p.name != pn {
                    continue;
                }
                if nearest.is_none() {
                    nearest = Some((p.clone(), c.clone()));
                }
                if let Some(hs) = &p.hooks {
                    for h in hs {
                        if !hooks.iter().any(|(x, _)| x.is_get == h.is_get) {
                            hooks.push((h.clone(), c.clone()));
                        }
                    }
                }
            }
            let par = c.decl.parent.clone();
            cur = par.and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        let Some((pd, pcls)) = nearest else {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Undefined property {}::${}", parent.name(), pn),
                0,
            ));
        };
        // private parent prop is invisible to the child scope
        if pd.visibility == crate::ast::Visibility::Private && pcls.name() != dcls.name() {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot access private property {}::${}", pcls.name(), pn),
                0,
            ));
        }
        let hook = hooks
            .iter()
            .find(|(h, _)| h.is_get == is_get && h.body.is_some())
            .cloned();
        // A user hook tolerates extra args (user-function semantics);
        // the implicit hook of a *plain* parent prop is an internal
        // function with a strict arg count (parent_superfluous_args).
        if hook.is_none() {
            let want = if is_get { 0 } else { 1 };
            if args.len() != want {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "{}::${}::{}() expects exactly {} argument{}, {} given",
                        parent_name,
                        pn,
                        kind,
                        want,
                        if want == 1 { "" } else { "s" },
                        args.len()
                    ),
                    0,
                ));
            }
        }
        // Named args bind against the hook's params (or the implicit
        // set's `$value` for a plain parent prop); unknown names are a
        // catchable Error (gh20270).
        let pnames: Vec<String> = match &hook {
            Some((h, _)) => h.params.iter().map(|p| p.name.clone()).collect(),
            None => {
                if is_get {
                    Vec::new()
                } else {
                    vec!["value".to_string()]
                }
            }
        };
        let argvals = {
            let mut vs: Vec<Value> = Vec::new();
            for a in args {
                match a {
                    Expr::Binary {
                        op: "named", l, r, ..
                    } => {
                        let n = match self.eval(l)? {
                            Value::Str(s) => s.to_string(),
                            v => v.to_php_string(),
                        };
                        match pnames.iter().position(|p| *p == n) {
                            Some(i) => {
                                while vs.len() <= i {
                                    vs.push(Value::Null);
                                }
                                vs[i] = self.eval(r)?;
                            }
                            None => {
                                return self.fail(PhpError::uncaught(
                                    "Error",
                                    format!("Unknown named parameter ${}", n),
                                    0,
                                ));
                            }
                        }
                    }
                    _ => vs.push(self.eval(a)?),
                }
            }
            vs
        };
        if let Some((h, c)) = hook {
            return self.run_hook(&o, &c, pn, &h, argvals.into_iter().next().map(cell));
        }
        // Plain parent prop: get reads the slot, set writes it (and —
        // like a plain assignment — the implicit set returns the value).
        if is_get {
            if !o.borrow().props.contains_key(pn) {
                // Implicit get reads the shared backing slot; an
                // uninitialized typed prop is a catchable Error naming
                // the decl visible from the object (parent_get_plain_…).
                if let Some((tpd, tdcls)) = self.decl_prop(&o, pn) {
                    if tpd.ty.is_some() && tpd.default.is_none() {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Typed property {}::${} must not be accessed before initialization",
                                tdcls.name(),
                                pn
                            ),
                            0,
                        ));
                    }
                }
            }
            Ok(o.borrow()
                .props
                .get(pn)
                .map(|c| c.borrow().clone())
                .unwrap_or(Value::Null))
        } else {
            let v = argvals.into_iter().next().unwrap_or(Value::Null);
            let mut ob = o.borrow_mut();
            if !ob.prop_order.iter().any(|k| k == pn) {
                ob.prop_order.push(pn.to_string());
            }
            ob.props.insert(pn.to_string(), cell(v.clone()));
            Ok(v)
        }
    }

    /// `private(set)`/`protected(set)` violation message — distinct from
    /// the read-side "Cannot access" (asymmetric_visibility).
    fn set_visibility_error<T>(
        &mut self,
        dcls: &Rc<PhpClass>,
        pname: &str,
        sv: crate::ast::Visibility,
    ) -> Result<T, PhpError> {
        let visname = match sv {
            crate::ast::Visibility::Private => "private",
            crate::ast::Visibility::Protected => "protected",
            crate::ast::Visibility::Public => "public",
        };
        let from = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()))
            .map(|c| format!("scope {}", c.name()))
            .unwrap_or_else(|| "global scope".to_string());
        self.fail(PhpError::uncaught(
            "Error",
            format!(
                "Cannot modify {}(set) property {}::${} from {}",
                visname,
                dcls.name(),
                pname,
                from
            ),
            0,
        ))
    }

    /// Native bodies for the ReflectionClass/ReflectionProperty stubs.
    /// The reflected class/prop names live under `\0rc\0` prop keys.
    fn reflection_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        args: &[Cell],
    ) -> Result<Option<Value>, PhpError> {
        let lname = name.to_lowercase();
        match lname.as_str() {
            "__construct" => {
                let mut ob = obj.borrow_mut();
                let cls = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let prop = args
                    .get(1)
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                ob.props.insert("\0rc\0class".into(), cell(cls));
                ob.props.insert("\0rc\0prop".into(), cell(prop));
                Ok(Some(Value::Null))
            }
            "getname" => Ok(Some(
                obj.borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null),
            )),
            "newinstancewithoutconstructor" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?;
                Ok(Some(self.instantiate(&cn.to_lowercase(), &[])))
            }
            "isinitialized" => {
                let pn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?;
                let target = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                if let Value::Object(o) = target {
                    Ok(Some(Value::Bool(self.obj_prop_key(&o, &pn).is_some())))
                } else {
                    Ok(Some(Value::Bool(false)))
                }
            }
            _ => Ok(None),
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
                if !self.in_own_hook(&o, &pn) {
                    if let Some((pd, hs)) = self.hooked_prop(&o, &pn) {
                        return self.hook_read(&o, &pd, &hs);
                    }
                }
                if let Some(k) = self.obj_prop_key(&o, &pn) {
                    return Ok(o.borrow().props.get(&k).unwrap().borrow().clone());
                }
                // Typed prop whose slot was never initialized → Error
                // (not __get, not a warning): parent_get_plain_typed_uninitialized.
                if let Some((tpd, tdcls)) = self.decl_prop(&o, &pn) {
                    if tpd.ty.is_some() && tpd.default.is_none() {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Typed property {}::${} must not be accessed before initialization",
                                tdcls.name(),
                                pn
                            ),
                            0,
                        ));
                    }
                }
                // __get magic
                if cls.find_method("__get").is_some() {
                    return self.method_invoke(o.clone(), "__get", vec![cell(Value::str(pn))]);
                }
                self.warn(&format!("Undefined property: {}::${}", cls.name(), pn))?;
                Ok(Value::Null)
            }
            Value::Null => {
                if nullsafe {
                    return Ok(Value::Null);
                }
                self.warn(&format!("Attempt to read property \"{}\" on null", pn))?;
                Ok(Value::Null)
            }
            other => {
                if self.silence == 0 {
                    self.warn(&format!(
                        "Attempt to read property \"{}\" on {}",
                        pn,
                        other.gettype()
                    ))?;
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
                // Hooks intercept the cell path entirely — `[]`, `&`,
                // `++`/`--` are "indirect modification", unless `&get`
                // exists: the by-ref get's returned cell is used.
                if !self.in_own_hook(&o, &pn) {
                    if let Some((pd, hs)) = self.hooked_prop(&o, &pn) {
                        let by_ref_get = hs
                            .iter()
                            .find(|(h, _)| h.is_get && h.by_ref && h.body.is_some());
                        let dcls = by_ref_get
                            .map(|(_, c)| c.clone())
                            .unwrap_or_else(|| o.borrow().class.clone());
                        if let Some((h, c)) = by_ref_get {
                            let vis = h.visibility.unwrap_or(pd.visibility);
                            if !self.hook_scope_allows(&o, c, &pd.name, vis) {
                                return self.hook_visibility_error(c, &pd.name, vis);
                            }
                            self.last_ret_cell = None;
                            let v = self.run_hook(&o, c, &pd.name, h, None)?;
                            return Ok(self.last_ret_cell.take().unwrap_or_else(|| cell(v)));
                        }
                        // `$obj->hooked[k] = v`: zend fetches the hooked
                        // prop once; when the result is an object the
                        // index op applies to the object itself (objects
                        // pass by handle), e.g. ArrayAccess offsetSet
                        // (object_in_hook.phpt). Arrays/scalars still
                        // need `&get` for the write to land.
                        if let Some((gh, gc)) = hs
                            .iter()
                            .find(|(h, _)| h.is_get && h.body.is_some())
                            .map(|(h, c)| (h.clone(), c.clone()))
                        {
                            let vis = gh.visibility.unwrap_or(pd.visibility);
                            if !self.hook_scope_allows(&o, &gc, &pd.name, vis) {
                                return self.hook_visibility_error(&gc, &pd.name, vis);
                            }
                            let gv = self.run_hook(&o, &gc, &pd.name, &gh, None)?;
                            if matches!(gv, Value::Object(_)) {
                                return Ok(cell(gv));
                            }
                        }
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Indirect modification of {}::${} is not allowed",
                                dcls.name(),
                                pd.name
                            ),
                            0,
                        ));
                    }
                }
                let key = self.obj_prop_key(&o, &pn);
                if key.is_none() {
                    if let Some((tpd, tdcls)) = self.decl_prop(&o, &pn) {
                        if tpd.ty.is_some() && tpd.default.is_none() {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Typed property {}::${} must not be accessed before initialization",
                                    tdcls.name(),
                                    pn
                                ),
                                0,
                            ));
                        }
                    }
                }
                let key = key.unwrap_or_else(|| pn.clone());
                let mut ob = o.borrow_mut();
                if !ob.props.contains_key(&key) {
                    if !ob.prop_order.contains(&key) {
                        ob.prop_order.push(key.clone());
                    }
                    ob.props.insert(key.clone(), cell(Value::Null));
                }
                Ok(ob.props.get(&key).unwrap().clone())
            }
            _ => self.fail(PhpError::fatal(
                format!("Attempt to assign property \"{}\" on non-object", pn),
                0,
            )),
        }
    }

    /// Resolve a property access to its storage key on `o`.
    /// Private slots are `\0DeclaringClass\0name`; a method sees the
    /// private slot of its own declaring class, then public/protected
    /// and dynamic props under the plain name.
    fn obj_prop_key(&mut self, o: &Rc<RefCell<PhpObject>>, pn: &str) -> Option<String> {
        let dc = self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|c| c.name().to_string())
        });
        if let Some(dc) = dc {
            let k = format!("\0{}\0{}", dc, pn);
            if o.borrow().props.contains_key(&k) {
                return Some(k);
            }
        }
        if o.borrow().props.contains_key(pn) {
            return Some(pn.to_string());
        }
        None
    }

    fn unset_prop(&mut self, e: &Expr) -> Result<(), PhpError> {
        if let Expr::Prop { obj, name, .. } = e {
            let pn = self.prop_name(name)?;
            let ov = self.eval(obj)?;
            if let Value::Object(o) = ov {
                if !self.in_own_hook(&o, &pn) {
                    if let Some((pd, hs)) = self.hooked_prop(&o, &pn) {
                        let _dcls = &hs[0].1;
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Cannot unset hooked property {}::${}",
                                o.borrow().class.name(),
                                pd.name
                            ),
                            0,
                        ));
                    }
                }
                let cls = o.borrow().class.clone();
                if let Some(k) = self.obj_prop_key(&o, &pn) {
                    o.borrow_mut().props.remove(&k);
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
                    .map(|m| m.0.decl.params.clone())
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
    /// `dc` is the declaring class — used as the private-prop scope.
    fn invoke_method(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        m: &Rc<MethodDecl>,
        args: Vec<Cell>,
        dc: Rc<PhpClass>,
    ) -> Result<Value, PhpError> {
        let scope = obj.borrow().class.clone();
        self.pending_decl_class = Some(dc);
        let r = if m.is_static {
            self.invoke_fn(&Rc::new(m.decl.clone()), args, None, Some(scope))
        } else {
            self.invoke_fn(&Rc::new(m.decl.clone()), args, Some(obj), Some(scope))
        };
        self.pending_decl_class = None;
        r
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
            // Native method only when the resolved method is a builtin
            // registration (line 0 — userland always runs, even an empty
            // body: `__construct(public $x) {}` still promotes).
            // find_method_in walks the parent chain so inherited stubs
            // (Exception::getTrace on a userland subclass) resolve
            // (tests/lang/038, error_2_exception_001).
            let stub = self
                .find_method_in(&cls, name)
                .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
                .unwrap_or(false);
            if stub {
                if let Some(v) = self.throwable_method(&obj, name, &args) {
                    return Ok(v);
                }
            }
        }
        // Reflection stubs are native: constructor stores the target
        // name, methods act on it (gh15438_2).
        if cls.name().eq_ignore_ascii_case("reflectionclass")
            || cls.name().eq_ignore_ascii_case("reflectionproperty")
        {
            let stub = self
                .find_method_in(&cls, name)
                .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
                .unwrap_or(false);
            if stub {
                if let Some(v) = self.reflection_method(&obj, name, &args)? {
                    return Ok(v);
                }
            }
        }
        match self.find_method_in(&cls, name) {
            Some((m, dc)) => self.invoke_method(obj, &m, args, dc),
            None => {
                if let Some((m, dc)) = self.find_method_in(&cls, "__call") {
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
                        dc,
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
            "gettrace" => {
                let mut arr = PhpArray::new();
                let frames: Vec<TraceFrame> = match &ob.internal {
                    Some(ObjectInternal::Exception { frames, .. }) => (**frames).clone(),
                    _ => Vec::new(),
                };
                // PHP orders innermost call first (tests/lang/038);
                // internal-function call sites carry no file/line.
                for fr in frames.iter().rev() {
                    let mut f = PhpArray::new();
                    if fr.file != "[internal function]" {
                        f.set(ArrKey::Str("file".into()), Value::str(fr.file.clone()));
                        f.set(ArrKey::Str("line".into()), Value::Int(fr.line as i64));
                    }
                    f.set(
                        ArrKey::Str("function".into()),
                        Value::str(fr.function.clone()),
                    );
                    if let Some(c) = &fr.class {
                        f.set(ArrKey::Str("class".into()), Value::str(c.clone()));
                        f.set(ArrKey::Str("type".into()), Value::str(fr.ty.clone()));
                    }
                    let mut a = PhpArray::new();
                    for av in &fr.args {
                        a.push(av.borrow().clone());
                    }
                    f.set(
                        ArrKey::Str("args".into()),
                        Value::Array(Rc::new(RefCell::new(a))),
                    );
                    arr.push(Value::Array(Rc::new(RefCell::new(f))));
                }
                Some(Value::Array(Rc::new(RefCell::new(arr))))
            }
            "gettraceasstring" => match &ob.internal {
                Some(ObjectInternal::Exception { trace, .. }) if !trace.is_empty() => {
                    Some(Value::str(trace.clone()))
                }
                Some(ObjectInternal::Exception { frames, .. }) if !frames.is_empty() => {
                    Some(Value::str(format_trace(frames)))
                }
                _ => Some(Value::str("#0 {main}")),
            },
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

    fn static_prop_read(&mut self, class: &Expr, name: &PropName) -> Result<Value, PhpError> {
        let name = self.prop_name(name)?;
        let cls = self.class_of(class)?;
        self.statics_init(&cls);
        let v = cls.statics.borrow().get(&name).map(|c| c.borrow().clone());
        match v {
            Some(v) => Ok(v),
            None => self.fail(PhpError::uncaught(
                "Error",
                format!("Undefined static property {}::${}", cls.name(), name),
                0,
            )),
        }
    }

    fn static_prop_cell(&mut self, class: &Expr, name: &PropName) -> Result<Cell, PhpError> {
        let name = self.prop_name(name)?;
        self.static_prop_named(class, &name)
    }

    fn static_prop_named(&mut self, class: &Expr, name: &str) -> Result<Cell, PhpError> {
        let cls = self.class_of(class)?;
        self.statics_init(&cls);
        let found = cls.statics.borrow().get(name).cloned();
        match found {
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
            .map(|m| m.0.decl.params.clone())
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
            Some((m, dc)) => {
                // Forwarding call: a non-static method invoked statically
                // still receives $this when the caller's $this is an
                // instance of the callee's class (bug21961).
                let this_obj = if m.is_static {
                    None
                } else {
                    self.stack
                        .last()
                        .and_then(|f| f.this_obj.clone())
                        .filter(|o| {
                            let cname = o.borrow().class.name().to_string();
                            self.is_a_str(&cname, cls.name())
                        })
                };
                self.pending_decl_class = Some(dc);
                let r = self.invoke_fn(&Rc::new(m.decl.clone()), args, this_obj, Some(cls.clone()));
                self.pending_decl_class = None;
                r
            }
            None => {
                if let Some((m, dc)) = self.find_method_in(&cls, "__callstatic") {
                    let mut arr = PhpArray::new();
                    for a in &args {
                        arr.push(a.borrow().clone());
                    }
                    self.pending_decl_class = Some(dc);
                    let r = self.invoke_fn(
                        &Rc::new(m.decl.clone()),
                        vec![
                            cell(Value::str(name)),
                            cell(Value::Array(Rc::new(RefCell::new(arr)))),
                        ],
                        None,
                        Some(cls.clone()),
                    );
                    self.pending_decl_class = None;
                    return r;
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
    /// Visibility + declaring class for a property name (var_dump marks
    /// `["n":protected]` and `["n":"Cls":private]`).
    pub fn prop_visibility(
        &mut self,
        cls: &Rc<PhpClass>,
        name: &str,
    ) -> (crate::ast::Visibility, String) {
        // Private slots are stored mangled ("\0Cls\0name"); the declaring
        // class is encoded directly in the key.
        if let Some(rest) = name.strip_prefix('\0') {
            if let Some((dcls, _)) = rest.split_once('\0') {
                return (crate::ast::Visibility::Private, dcls.to_string());
            }
        }
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            for p in &c.decl.props {
                if p.name == name && !p.is_static {
                    return (p.visibility, c.name().to_string());
                }
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        (crate::ast::Visibility::Public, String::new())
    }

    /// Whether prop `name` on `cls` is readable from the current scope
    /// (foreach over objects iterates only visible props).
    pub fn prop_visible(&mut self, cls: &Rc<PhpClass>, name: &str) -> bool {
        let (vis, dcls) = self.prop_visibility(cls, name);
        let scope = self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|s| s.decl.name.clone())
        });
        match (vis, scope) {
            (crate::ast::Visibility::Public, _) => true,
            (crate::ast::Visibility::Private, Some(s)) => s == dcls,
            (crate::ast::Visibility::Protected, Some(s)) => {
                self.is_a_str(&s, &dcls) || self.is_a_str(&dcls, &s)
            }
            _ => false,
        }
    }

    /// Declared class/interface/trait names for get_declared_*().
    pub fn declared_names(&self, kind: crate::ast::ClassKind) -> Vec<String> {
        self.classes
            .values()
            .filter(|c| c.decl.kind == kind)
            .map(|c| c.name().to_string())
            .collect()
    }

    /// Zend backtrace text for debug_print_backtrace(): innermost-first
    /// frames, no `{main}` line (bug28213).
    pub fn format_backtrace(&self) -> String {
        let frames: Vec<TraceFrame> = self
            .call_trace
            .iter()
            .rev()
            .skip_while(|f| f.internal)
            .cloned()
            .collect();
        format_backtrace_frames(&frames)
    }

    /// debug_backtrace() array — same frames as format_backtrace().
    pub fn backtrace(&self) -> Vec<TraceFrame> {
        self.call_trace
            .iter()
            .rev()
            .skip_while(|f| f.internal)
            .cloned()
            .collect()
    }

    /// is-a check between two class-name strings.
    fn is_a_str(&mut self, a: &str, b: &str) -> bool {
        match self.classes.get(&a.to_lowercase()).cloned() {
            Some(c) => self.is_a(&c, b),
            None => a.eq_ignore_ascii_case(b),
        }
    }

    pub fn find_method_in(
        &mut self,
        cls: &Rc<PhpClass>,
        name: &str,
    ) -> Option<(Rc<MethodDecl>, Rc<PhpClass>)> {
        let lname = name.to_lowercase();
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            if let Some(m) = c.decl.find_method(&lname) {
                return Some((m, c));
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
            file: self.file.to_string(),
            line: self.cur_line as u32,
            args: vec![cell(pathv.clone())],
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
        let stmts = match parser::parse(&src) {
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
        // includer's (namespaces/ns_069).
        let saved_file = std::mem::replace(&mut self.cur_file, canon.display().to_string());
        let saved_ns = std::mem::take(&mut self.globals.ns);
        self.hoist_funcs(&stmts);
        let flow = self.exec_block(&stmts);
        inc_pop(self);
        self.cur_file = saved_file;
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
                }
            }
            Err(e) => {
                // ` on line N` inside bracket messages is padded-file
                // relative — unshift it (syntax_errors).
                let msg = Self::unshift_line_ref(&e.message);
                let v = self.exception("ParseError", &msg);
                if let Value::Object(o) = &v {
                    if let Some(ObjectInternal::Exception { eval_ctx, .. }) =
                        &mut o.borrow_mut().internal
                    {
                        // `<?php\n` prepend shifts inner lines by one.
                        *eval_ctx = e.line.saturating_sub(1) as u32;
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

    /// Rewrites ` on line N` inside an error message to N-1 — eval'd
    /// code is parsed behind a `<?php\n` pad that shifts every line.
    fn unshift_line_ref(msg: &str) -> String {
        let Some(p) = msg.find(" on line ") else {
            return msg.to_string();
        };
        let tail = &msg[p + 9..];
        let digits: usize = tail
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .map(|c| c.len_utf8())
            .sum();
        let Ok(n) = tail[..digits].parse::<usize>() else {
            return msg.to_string();
        };
        format!(
            "{}{}{}",
            &msg[..p + 9],
            n.saturating_sub(1),
            &tail[digits..]
        )
    }

    /// Flush all output buffers at script end, innermost first so each
    /// level's handler output lands in its parent's buffer (bug24951).
    fn flush_ob_all(&mut self) {
        while !self.ob_stack.is_empty() {
            let r = self.ob_invoke(8);
            self.ob_stack.pop();
            if let Ok(Some(s)) = r {
                self.emit(&s);
            }
        }
    }

    /// Invoke the top level's handler with `mode | START` on first call,
    /// clearing the buffer first (bug24951 flag semantics:
    /// START=1, CLEAN=2, FLUSH=4, FINAL=8). Returns the handler's output
    /// — or the raw buffer when there is no handler.
    fn ob_invoke(&mut self, mode: i64) -> Result<Option<String>, PhpError> {
        let (handler, buf, already) = match self.ob_stack.last_mut() {
            Some(l) => {
                let buf = std::mem::take(&mut l.buf);
                let st = l.started;
                l.started = true;
                (l.handler.clone(), buf, st)
            }
            None => return Ok(None),
        };
        match handler {
            Some(h) => {
                let m = mode | if already { 0 } else { 1 };
                // A handler throwing inside ob_end_clean propagates as an
                // uncaught exception (bug32828).
                self.internal_cb += 1;
                let out = self.call_value(&h, vec![cell(Value::str(buf)), cell(Value::Int(m))]);
                self.internal_cb -= 1;
                Ok(Some(out?.to_php_string()))
            }
            None => Ok(Some(buf)),
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
    pub fn ob_push(&mut self, handler: Option<Value>) {
        self.ob_stack.push(ObLevel {
            buf: String::new(),
            handler,
            started: false,
        });
    }
    /// ob_end_clean: handler(mode=CLEAN|FINAL) result discarded, pop.
    pub fn ob_end_clean(&mut self) -> Result<(), PhpError> {
        self.ob_invoke(10)?;
        self.ob_stack.pop();
        Ok(())
    }
    /// ob_end_flush: handler(mode=FINAL) result emitted to parent, pop.
    pub fn ob_end_flush(&mut self) -> Result<(), PhpError> {
        let r = self.ob_invoke(8)?;
        self.ob_stack.pop();
        if let Some(s) = r {
            self.emit(&s);
        }
        Ok(())
    }
    /// ob_flush: handler(mode=FLUSH) result emitted to the PARENT level
    /// (the level is briefly popped so emit can't feed back into it),
    /// buffer cleared, level stays open (bug24951).
    pub fn ob_flush(&mut self) -> Result<(), PhpError> {
        if let Some(s) = self.ob_invoke(4)? {
            let level = self.ob_stack.pop();
            self.emit(&s);
            if let Some(l) = level {
                self.ob_stack.push(l);
            }
        }
        Ok(())
    }
    /// ob_clean: handler(mode=CLEAN) result discarded, buffer cleared.
    pub fn ob_clean(&mut self) -> Result<(), PhpError> {
        self.ob_invoke(2)?;
        Ok(())
    }
    /// ob_get_clean: raw buffer, NO handler invocation, pop.
    pub fn ob_get_clean(&mut self) -> Value {
        self.ob_stack
            .pop()
            .map(|l| Value::str(l.buf))
            .unwrap_or(Value::Bool(false))
    }
    /// ob_get_flush: handler(mode=FINAL) result emitted, RAW buffer
    /// returned, level popped.
    pub fn ob_get_flush(&mut self) -> Result<Value, PhpError> {
        let raw = self.ob_stack.last().map(|l| l.buf.clone());
        let r = self.ob_invoke(8)?;
        self.ob_stack.pop();
        if let Some(s) = r {
            self.emit(&s);
        }
        Ok(raw.map(Value::str).unwrap_or(Value::Bool(false)))
    }
    pub fn ob_top(&self) -> Option<&String> {
        self.ob_stack.last().map(|l| &l.buf)
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
    /// True while executing inside a function/method call frame.
    pub fn in_call(&self) -> bool {
        !self.stack.is_empty()
    }

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
    pub fn warn_pub(&mut self, msg: &str) -> Result<(), PhpError> {
        self.warn(msg)
    }

    /// Diagnostic at a caller-selected E_USER_* level (trigger_error).
    /// Respects error_reporting masking + the silence (@) counter.
    pub fn emit_diag_pub(&mut self, level: i64, msg: &str) -> Result<(), PhpError> {
        if self.silence > 0 || self.error_level & level == 0 {
            return Ok(());
        }
        let (name, errno) = match level {
            512 => ("Warning", 512),
            16384 => ("Deprecated", 16384),
            // E_USER_ERROR=256 is uncatchable in PHP 8.4+ and aborts.
            256 => return self.fail(PhpError::fatal(msg.to_string(), self.cur_line)),
            _ => ("Notice", level),
        };
        self.emit_diag(name, errno, msg)
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

/// `__METHOD__` scope name for a hook on `dcls`: a trait-origin hook
/// keeps its trait name via `decl_in`, else the declaring class.
fn decl_owner(dcls: &Rc<PhpClass>, pname: &str) -> String {
    dcls.decl
        .props
        .iter()
        .find(|p| p.name == pname)
        .and_then(|p| p.decl_in.clone())
        .unwrap_or_else(|| dcls.name().to_string())
}

/// Hooks merged along a chain (hook, declaring class), nearest first.
type MergedHooks = Vec<(PropHook, Rc<PhpClass>)>;
/// `(emitted key, slot key, decl+decl class)` — `None` decl means a
/// dynamic property (property-hooks serialization views).
type SerialEntry = (String, String, Option<(PropDecl, Rc<PhpClass>)>);

fn cell(v: Value) -> Cell {
    Rc::new(RefCell::new(v))
}

fn key_value(k: &ArrKey) -> Value {
    match k {
        ArrKey::Int(i) => Value::Int(*i),
        ArrKey::Str(s) => Value::str(s.to_string()),
        ArrKey::Tomb => Value::Null,
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

/// Weak-mode scalar coercion used by typed-property writes and hook
/// type checks ("C::$p: Return value must be of type int" family).
fn weak_ty_coerce(tys: &[String], v: &Value) -> Option<Value> {
    for t in tys {
        let coerced = match (t.as_str(), v) {
            ("int", Value::Str(s)) => {
                let tr = s.trim();
                let base = if let Some(h) = tr.strip_prefix("0x") {
                    i64::from_str_radix(h, 16).ok()
                } else if let Some(o) = tr.strip_prefix("0o") {
                    i64::from_str_radix(o, 8).ok()
                } else if let Some(b) = tr.strip_prefix("0b") {
                    i64::from_str_radix(b, 2).ok()
                } else {
                    tr.parse::<i64>().ok()
                };
                base.map(Value::Int)
            }
            ("int", Value::Float(f)) => Some(Value::Int(*f as i64)),
            ("int", Value::Bool(b)) => Some(Value::Int(*b as i64)),
            ("string", Value::Int(i)) => Some(Value::str(i.to_string())),
            ("string", Value::Float(f)) => Some(Value::str(format_float_repr(*f))),
            ("string", Value::Bool(b)) => Some(Value::str(if *b { "1" } else { "" })),
            ("float", Value::Int(i)) => Some(Value::Float(*i as f64)),
            ("float", Value::Str(s)) => s.trim().parse::<f64>().ok().map(Value::Float),
            ("float", Value::Bool(b)) => Some(Value::Float(if *b { 1.0 } else { 0.0 })),
            ("bool", _) => Some(Value::Bool(v.is_truthy())),
            _ => None,
        };
        if let Some(c) = coerced {
            return Some(c);
        }
    }
    None
}
