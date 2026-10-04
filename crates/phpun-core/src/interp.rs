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
    trace_arg, ArrKey, CallableKind, Cell, GenSetup, GenState, Numeric, ObjectInternal, PhpArray,
    PhpCallable, PhpClass, PhpObject, PhpResource, TraceFrame, Value,
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

/// Weak slot in the shared object-store handle space — objects and
/// closures draw ids from the same vector, like Zend's EG(objects_store).
enum ObjHandle {
    Obj(std::rc::Weak<RefCell<PhpObject>>),
    Callable(std::rc::Weak<PhpCallable>),
}

impl ObjHandle {
    fn alive(&self) -> bool {
        match self {
            Self::Obj(w) => w.upgrade().is_some(),
            Self::Callable(w) => w.upgrade().is_some(),
        }
    }
}

/// Evaluated call arguments: positional cells (call order) plus named
/// entries the callee binds by param name (Zend/tests/named_params).
pub struct CallArgs {
    pub cells: Vec<Cell>,
    /// `(name, cell, by_ref_ok, from_traversable)` — by_ref_ok marks
    /// entries whose source expression was refable (`ref: $x`);
    /// literals bind by value with a warning on by-ref params
    /// (named_params/call_user_func). from_traversable marks cells
    /// produced by unpacking a Traversable — by-ref params bind them
    /// by value with a different warning (named_params/unpack).
    pub named: Vec<(String, Cell, bool, bool)>,
    /// Positional indexes (into `cells`) produced by Traversable unpack.
    pub trav_cells: Vec<usize>,
    /// Positional indexes that must bind by value even on by-ref
    /// params — `call_user_func`-family forwards never create
    /// references, so zend warns "must be passed by reference, value
    /// given" (closure_invoke_ref_warning).
    pub nonref_cells: Vec<usize>,
}

impl CallArgs {
    pub fn positional(cells: Vec<Cell>) -> Self {
        Self {
            cells,
            named: Vec::new(),
            trav_cells: Vec::new(),
            nonref_cells: Vec::new(),
        }
    }
    pub fn empty() -> Self {
        Self::positional(Vec::new())
    }
}

/// Reads (len/get/iter/index) treat the arg list as its positional cells.
impl std::ops::Deref for CallArgs {
    type Target = [Cell];
    fn deref(&self) -> &[Cell] {
        &self.cells
    }
}

pub struct Frame {
    vars: HashMap<String, Cell>,
    /// Actual call args for func_get_args().
    args: Vec<Cell>,
    /// Enclosing function name (for `static`/`__FUNCTION__`).
    fn_name: String,
    /// `$this` in method calls.
    this_obj: Option<Rc<RefCell<PhpObject>>>,
    /// Class context for self::/parent:: (the method's declaring class).
    scope_class: Option<Rc<PhpClass>>,
    /// Late-static-binding class — `static::`/`new static`/`get_called_class`
    /// resolve here; falls back to scope_class when unset.
    called_class: Option<Rc<PhpClass>>,
    /// Class the running method was declared in — PHP's private
    /// property slot is keyed by the declaring class (`\0Cls\0prop`).
    decl_class: Option<Rc<PhpClass>>,
    /// Declaration line — closures render in traces as
    /// `{closure:FILE:LINE}` (typed_properties_055).
    fn_line: usize,
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
    /// Trait the running method was merged from (`use T`) — drives
    /// `__TRAIT__` and the owner part of `__METHOD__`.
    trait_origin: Option<String>,
    /// The callable this frame executes (closure frames) —
    /// `Closure::getCurrent()` returns it (closure_get_current).
    closure_rc: Option<Rc<PhpCallable>>,
    /// Name diagnostics report for this call — `[$closure,'__invoke']`
    /// runs as `Closure::__invoke` (closure_invoke_ref_warning).
    call_alias: Option<String>,
}

impl Frame {
    fn new(fn_name: String) -> Self {
        Self {
            vars: HashMap::new(),
            args: Vec::new(),
            fn_name,
            this_obj: None,
            scope_class: None,
            called_class: None,
            decl_class: None,
            fn_line: 0,
            file: String::new(),
            ns: String::new(),
            ret_by_ref: false,
            hook_prop: None,
            trait_origin: None,
            closure_rc: None,
            call_alias: None,
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
    pub traits: HashMap<String, Rc<ClassDecl>>,
    /// Synthesized classes for direct `T::$s`/`T::m()` trait member
    /// access (deprecated but functional in PHP) — one per trait so
    /// statics share storage across accesses.
    trait_statics: HashMap<String, Rc<PhpClass>>,
    /// ClassDecls mid-registration — `is_a` ancestry checks during
    /// signature verification resolve against these by name before the
    /// class lands in `classes` (ret-covariance needs `B extends A`
    /// while B is still linking).
    linking: Vec<Rc<ClassDecl>>,
    /// Class names whose autoloader callback is currently running —
    /// Zend's in-linking guard: a re-entrant lookup of the same name
    /// no-ops instead of recursing forever (autoload(D) → `D extends C`
    /// → autoload(C) while C's own autoload is still in flight).
    autoloading: std::collections::HashSet<String>,
    /// Class lnames whose inheritance signature check deferred on a
    /// compared type that was still loading — Zend's delayed variance
    /// obligations, re-verified after each class finishes linking
    /// (class_order_autoload*).
    variance_obligations: Vec<String>,
    /// Reentrancy guard: a class registering while the deferred pass
    /// itself runs does not spawn a nested pass (error9 ordering — the
    /// autoloaded class's own code runs before the recheck resumes).
    in_variance_pass: bool,
    /// Fatal raised inside an autoload a signature probe triggered —
    /// the probe reports it to the checking context instead of
    /// degrading to "could not check" (cascading variance failures
    /// must surface the original fatal once).
    sig_fatal: Option<PhpError>,
    /// Decls currently mid-registration (register_class entered, the
    /// decl not yet on `linking`/`classes`) — type probes resolve
    /// them as class-likes so a check sees `C extends B` by name
    /// while C's own dependencies still autoload
    /// (class_order_autoload1; infinite_recursion).
    declaring: Vec<Rc<ClassDecl>>,
    /// (class, method) pairs of internal methods whose declared return
    /// type is *tentative* — incompatible overrides get a Deprecated
    /// notice, not a fatal (internal_parent/*).
    tentative: HashSet<(String, String)>,
    pub interfaces: HashMap<String, Rc<ClassDecl>>,
    /// Declaration order of classes/interfaces/traits (lc names), for
    /// get_declared_*().
    pub decl_order: Vec<String>,
    /// class_alias() display names (kind, lowercased alias) appended to
    /// get_declared_{classes,interfaces,traits} output (Zend lists
    /// aliases lowercased, right after real decls).
    pub decl_aliases: Vec<(crate::ast::ClassKind, String)>,
    /// Enum case singletons keyed `"cls\0case"` — `E::Foo === E::Foo`.
    enum_cases: std::collections::HashMap<String, Value>,
    /// Classes whose const initializers were already link-evaluated.
    consts_linked: std::collections::HashSet<String>,
    /// Top-level parentless classes registered by hoisting (early
    /// binding); their decl stmt then no-ops (namespaces/ns_060).
    early_bound_classes: HashSet<String>,
    constants: HashMap<String, Value>,
    /// Accumulated program output (display_errors prints to stdout under
    /// CLI, and the PHPT harness merges streams via 2>&1).
    pub out: Vec<u8>,
    /// PHP CLI logs every diagnostic to stderr as `PHP <Level>: msg` when
    /// log_errors is on (default); the harness merges stderr after stdout.
    pub err_buf: String,
    /// File a const-expr lexically belongs to while it's being evaluated
    /// (prop/const/param defaults, attr args): __FILE__/__DIR__ bind to
    /// the declaring file, not the accessing file.
    decl_file_ctx: Option<String>,
    /// Headers queued by header()/setcookie() — `phpun serve` emits them
    /// into the HTTP response; CLI ignores them (like php-cli).
    pub out_headers: Vec<String>,
    /// Response status code set via http_response_code() or the third
    /// arg of header() — serve mode reads it (200 default).
    pub resp_code: i64,
    /// Set by json_encode/json_decode for json_last_error().
    pub last_json_error: i64,
    /// Set by the preg_* builtins for preg_last_error().
    pub last_preg_error: i64,
    /// Raw request body for php://input — serve mode fills it.
    pub php_input: std::rc::Rc<Vec<u8>>,
    /// Real upload tmp paths created this request — is_uploaded_file()
    /// and move_uploaded_file() check membership.
    pub uploads: Vec<std::path::PathBuf>,
    /// Output buffer stack for ob_*().
    ob_stack: Vec<ObLevel>,
    /// While >0, warnings are suppressed (implements `??`, `isset`,
    /// `empty`, `@`).
    silence: u32,
    /// Cell returned by the last `&fn()` call (returnByReference tests).
    last_ret_cell: Option<Cell>,
    /// The last invoked function was declared `&name()` (returns by ref).
    last_call_by_ref: bool,
    /// Set just before invoking `[$closure,'__invoke']` so the callee
    /// frame reports diagnostics as `Closure::__invoke` (zend).
    pending_call_alias: Option<String>,
    /// Insertion order of global vars (for $GLOBALS ordering).
    globals_order: Vec<String>,
    /// Shared PhpArray backing $GLOBALS — same cells as globals.vars.
    globals_arr: Option<Rc<RefCell<PhpArray>>>,
    /// Function-scoped static storage: fn name → var → cell.
    pub(crate) statics: HashMap<String, HashMap<String, Cell>>,
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
    /// Active generator body's yield collector — `Expr::Yield` pushes
    /// (key, value) here while a generator function's body runs.
    gen_sink: Option<Rc<RefCell<Vec<crate::value::GenItem>>>>,
    /// send() queue feeding `yield`-expr results in the running body.
    gen_sends: std::collections::VecDeque<Value>,
    /// Auto-key counter for keyless `yield $v` — counts keyless yields
    /// only (explicit keys and `yield from` items don't advance it).
    gen_auto: i64,
    /// The generator whose body is currently running — output produced
    /// after a yield suspends is tagged with that yield's item index
    /// and buffered on the GenState until the consumer resumes past
    /// it (closure_call_leak_with_exception).
    gen_run_state: Option<Rc<RefCell<crate::value::GenState>>>,
    /// Declaring class of the method about to be invoked (set by
    /// invoke_method, consumed by invoke_fn to fill Frame::decl_class).
    pending_decl_class: Option<Rc<PhpClass>>,
    /// Called-scope (LSB) for the next invoke_fn frame — set by
    /// invoke_method/static_invoke, consumed like pending_decl_class.
    pending_called_class: Option<Rc<PhpClass>>,
    /// (object id, prop, is_get, owner) whose hook is about to run —
    /// consumed by invoke_fn to fill Frame::hook_prop.
    pending_hook_prop: Option<(u64, String, bool, String)>,
    /// Live object handles for PHP's var_dump `#N` id: the lowest freed
    /// slot is reused, matching Zend's object store recycling.
    obj_handles: Vec<ObjHandle>,
    /// Per-callsite unqualified fn resolution cache — Zend resolves
    /// `ns\f -> f` once per call site (constexpr/namespace_004).
    fcc_fn_cache: HashMap<(String, String, String), Option<String>>,
    /// Objects whose __destruct already ran (shutdown pass). The Rc
    /// is pinned so a later object's allocation can't reuse the
    /// address and collide with an entry (bug74053).
    destructed: HashMap<usize, Rc<RefCell<PhpObject>>>,
    /// `new` temporaries of the running expression statement — swept
    /// at statement end so unowned objects destruct promptly
    /// (bug29368_2/_3).
    expr_temps: Vec<Rc<RefCell<PhpObject>>>,
    /// Frame popped inside `bind_and_run_inner`, handed off to the
    /// `bind_and_run` wrapper which runs its deferred __destruct
    /// pass after the call-trace pop (bug52361).
    last_popped_frame: Option<Frame>,
    /// Container addresses currently being var_dumped — a re-entrant
    /// dump prints `*RECURSION*` (closure_034/035).
    pub dump_stack: std::collections::HashSet<usize>,
    /// Nonzero while a callable is invoked from inside a builtin's
    /// internals (ob handlers) — marks its trace site internal-function.
    internal_cb: u32,
    /// Nonzero while evaluating a compile-time constant expression
    /// (const/class-const defaults, prop/param defaults): `...`-FCC and
    /// `self`/`parent` resolution follow const-expr rules.
    in_const_expr: u32,
    /// Nonzero while a class-init const expr (prop default, static init,
    /// class const) is being evaluated — errors get a synthetic
    /// `[constant expression]` trace frame; top-level `const` decls
    /// don't (constexpr/error_*).
    class_const_ctx: u32,
    /// The class whose const/prop initializer is being evaluated —
    /// `self`/`parent` inside it bind to this class, not the caller.
    const_self: Option<Rc<PhpClass>>,
    /// spl_autoload_register() callbacks, in registration order.
    pub autoload_fns: Vec<Value>,
    /// File currently executing — include resolution uses its directory
    /// (PHP checks include_path, then the calling file's dir, then cwd).
    cur_file: String,
    /// Files that ran `declare(strict_types=1)` — scalar arg/prop/return
    /// coercion is off for code executing inside them.
    strict_files: std::collections::HashSet<String>,
    /// Cells backing declared-typed props, keyed by their Rc pointer —
    /// writes *through a reference* to a typed slot stay checked
    /// (typed_properties_045). The stored clone keeps the slot alive so
    /// the pointer key stays unique.
    pub typed_slots: std::collections::HashMap<usize, (Cell, Vec<String>, String, String)>,
    /// Additional typed-prop owners of a shared ref cell (the
    /// `typed_slots` entry holds the first) — `union_types/prop_ref_assign`.
    pub slot_owners: std::collections::HashMap<usize, Vec<SlotOwner>>,
    /// The running intersection of every bound owner's declared type —
    /// `typed_slots` keeps the holder's declared type for messages.
    pub slot_merged: std::collections::HashMap<usize, Vec<String>>,
    /// Where each typed cell came from: a prop slot stays a prop slot
    /// even while aliased — the rejecting owner is reported as
    /// "Cannot assign X to property" only for prop-born cells
    /// (typed_properties_034 vs _078).
    pub slot_anchor: std::collections::HashMap<usize, SlotAnchor>,
    /// Cells reached through a `=&` bind / by-ref fetch — Zend
    /// IS_REFERENCE zvals. Write-through errors say "a reference held
    /// by property"; plain prop slots say "property"
    /// (typed_properties_034 first vs second foo() call).
    pub ref_cells: std::collections::HashSet<usize>,
    /// Zend's per-op magic-property guards, keyed
    /// (object-ptr, kind, prop-name): while `__get($o,$p)` runs, an
    /// access to `$o->$p` bypasses magic and hits real storage
    /// (bug63462/bug66609 — no infinite recursion). kinds: 0 get,
    /// 1 set, 2 isset, 3 unset.
    pub magic_guards: std::collections::HashSet<(usize, u8, String)>,
    /// Prop cells bound into an ArrayIterator whose decl is readonly —
    /// acquiring a `&` on one is "Cannot acquire reference to readonly
    /// property C::$p" (typed_properties_115). Value = (class, prop).
    pub readonly_cells: std::collections::HashMap<usize, (String, String)>,
    /// Ptr of a typed prop slot eval_cell materialized to `null` just
    /// now — a rejected array auto-init must leave the prop
    /// uninitialized again (typed_properties_083).
    last_fresh_cell: Option<usize>,
    /// Interfaces registered by the builtin-class table — their method
    /// signatures carry *tentative* return types: implementations may
    /// declare any return type (typed_properties_065).
    builtin_ifaces: std::collections::HashSet<String>,
    /// Implicit-nullable deprecations already emitted (function
    /// declarations evaluate at both collect and `Stmt::Function`
    /// time — Zend compiles once, so each param warns once).
    dep_seen: std::collections::HashSet<String>,
    /// File the last `fail()` was raised in (uncaught-print attribution
    /// for engine errors — `self.file` is always the entry script).
    last_err_file: String,
    /// Rendered arg list of the current `assert()` call — the
    /// AssertionError message shows `assert(<args>)` as written
    /// (named_params/assert's `assert(assertion: false)`).
    pub(crate) assert_src: String,
    /// Closure captures staged for the next invoke_fn_run — a
    /// yield-bearing closure's `use` vars bind when its generator body
    /// finally starts (iterable_003).
    pending_gen_captures: Vec<(String, Cell, bool)>,
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

/// Where a typed-slot owner lives — pruned when the named prop no
/// longer points at the shared cell (static rebind 082, unset, dead
/// object 094).
#[derive(Clone)]
pub enum SlotAnchor {
    /// Object prop: weak object ref + the props-map slot key.
    Obj(std::rc::Weak<RefCell<PhpObject>>, String),
    /// Static prop: declaring class name + prop name.
    Statics(String, String),
    /// Unverifiable origin — kept unconditionally.
    None,
}

/// One typed-prop owner of a shared cell: declared type members,
/// declaring class name, prop name, and the anchor proving the owner
/// still points at the cell.
pub type SlotOwner = (Vec<String>, String, String, SlotAnchor);

/// One output-buffer level (ob_start) with its optional handler.
pub struct ObLevel {
    pub buf: Vec<u8>,
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
        constants.insert("PHP_RELEASE_VERSION".into(), Value::Int(11));
        constants.insert("PHP_EXTRA_VERSION".into(), Value::str(""));
        constants.insert("PHP_VERSION_ID".into(), Value::Int(80511));
        constants.insert("PHP_OS".into(), Value::str("Linux"));
        constants.insert("PHP_OS_FAMILY".into(), Value::str("Linux"));
        constants.insert("PHP_SAPI".into(), Value::str("cli"));
        for (name, which) in [("STDIN", 0u8), ("STDOUT", 1u8), ("STDERR", 2u8)] {
            constants.insert(
                name.into(),
                Value::Resource(Rc::new(RefCell::new(crate::value::PhpResource::Stdio {
                    id: which as u64 + 1,
                    which,
                }))),
            );
        }
        constants.insert("DIRECTORY_SEPARATOR".into(), Value::str("/"));
        // Reported as the engine's target PCRE2 level — feature checks
        // like symfony's `>= 10.39` gate on this, not the vendored lib.
        constants.insert("PCRE_VERSION".into(), Value::str("10.49 2026-09-28"));
        constants.insert("INI_USER".into(), Value::Int(1));
        constants.insert("INI_PERDIR".into(), Value::Int(2));
        constants.insert("INI_SYSTEM".into(), Value::Int(4));
        constants.insert("INI_ALL".into(), Value::Int(7));
        constants.insert("STR_PAD_RIGHT".into(), Value::Int(1));
        constants.insert("STR_PAD_LEFT".into(), Value::Int(0));
        constants.insert("STR_PAD_BOTH".into(), Value::Int(2));
        constants.insert("MB_CASE_UPPER".into(), Value::Int(0));
        constants.insert("MB_CASE_LOWER".into(), Value::Int(1));
        constants.insert("MB_CASE_TITLE".into(), Value::Int(2));
        constants.insert("MB_CASE_FOLD".into(), Value::Int(3));
        constants.insert("MB_CASE_UPPER_SIMPLE".into(), Value::Int(4));
        constants.insert("MB_CASE_LOWER_SIMPLE".into(), Value::Int(5));
        constants.insert("MB_CASE_TITLE_SIMPLE".into(), Value::Int(6));
        constants.insert("MB_CASE_FOLD_SIMPLE".into(), Value::Int(7));
        constants.insert("MB_OVERLOAD_MAIL".into(), Value::Int(1));
        constants.insert("MB_OVERLOAD_STRING".into(), Value::Int(2));
        constants.insert("MB_OVERLOAD_REGEX".into(), Value::Int(4));
        constants.insert("MB_ONIGURUMA_VERSION".into(), Value::str("6.9.10"));
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
        // parse_url() component selectors.
        constants.insert("PHP_URL_SCHEME".into(), Value::Int(0));
        constants.insert("PHP_URL_HOST".into(), Value::Int(1));
        constants.insert("PHP_URL_PORT".into(), Value::Int(2));
        constants.insert("PHP_URL_USER".into(), Value::Int(3));
        constants.insert("PHP_URL_PASS".into(), Value::Int(4));
        constants.insert("PHP_URL_PATH".into(), Value::Int(5));
        constants.insert("PHP_URL_QUERY".into(), Value::Int(6));
        constants.insert("PHP_URL_FRAGMENT".into(), Value::Int(7));
        constants.insert("PREG_PATTERN_ORDER".into(), Value::Int(1));
        constants.insert("PREG_SET_ORDER".into(), Value::Int(2));
        constants.insert("PREG_OFFSET_CAPTURE".into(), Value::Int(256));
        constants.insert("PREG_UNMATCHED_AS_NULL".into(), Value::Int(512));
        constants.insert("PREG_SPLIT_NO_EMPTY".into(), Value::Int(1));
        constants.insert("PREG_SPLIT_DELIM_CAPTURE".into(), Value::Int(2));
        constants.insert("PREG_SPLIT_OFFSET_CAPTURE".into(), Value::Int(4));
        constants.insert("PREG_GREP_INVERT".into(), Value::Int(1));
        constants.insert("PREG_NO_ERROR".into(), Value::Int(0));
        constants.insert("PREG_INTERNAL_ERROR".into(), Value::Int(1));
        constants.insert("PREG_BACKTRACK_LIMIT_ERROR".into(), Value::Int(2));
        constants.insert("PREG_RECURSION_LIMIT_ERROR".into(), Value::Int(3));
        constants.insert("PREG_BAD_UTF8_ERROR".into(), Value::Int(4));
        constants.insert("PREG_BAD_UTF8_OFFSET_ERROR".into(), Value::Int(5));
        constants.insert("PREG_JIT_STACKLIMIT_ERROR".into(), Value::Int(6));
        constants.insert("PREG_BAD_MODE_LIMIT_ERROR".into(), Value::Int(7));
        // ext/standard sort flags (used by sort-family builtins and
        // symfony console's command sorting).
        constants.insert("SORT_REGULAR".into(), Value::Int(0));
        constants.insert("SORT_NUMERIC".into(), Value::Int(1));
        constants.insert("SORT_STRING".into(), Value::Int(2));
        constants.insert("SORT_DESC".into(), Value::Int(3));
        constants.insert("SORT_ASC".into(), Value::Int(4));
        constants.insert("SORT_LOCALE_STRING".into(), Value::Int(5));
        constants.insert("SORT_NATURAL".into(), Value::Int(6));
        constants.insert("SORT_FLAG_CASE".into(), Value::Int(8));
        // ext/filter.
        constants.insert("FILTER_VALIDATE_INT".into(), Value::Int(257));
        constants.insert("FILTER_VALIDATE_BOOL".into(), Value::Int(258));
        constants.insert("FILTER_VALIDATE_FLOAT".into(), Value::Int(259));
        constants.insert("FILTER_VALIDATE_REGEXP".into(), Value::Int(272));
        constants.insert("FILTER_VALIDATE_URL".into(), Value::Int(273));
        constants.insert("FILTER_VALIDATE_EMAIL".into(), Value::Int(274));
        constants.insert("FILTER_VALIDATE_IP".into(), Value::Int(275));
        constants.insert("FILTER_VALIDATE_DOMAIN".into(), Value::Int(277));
        constants.insert("FILTER_DEFAULT".into(), Value::Int(516));
        constants.insert("FILTER_CALLBACK".into(), Value::Int(1024));
        constants.insert("FILTER_REQUIRE_ARRAY".into(), Value::Int(16777216));
        constants.insert("FILTER_REQUIRE_SCALAR".into(), Value::Int(33554432));
        constants.insert("FILTER_FORCE_ARRAY".into(), Value::Int(67108864));
        constants.insert("FILTER_NULL_ON_FAILURE".into(), Value::Int(134217728));
        constants.insert("FILTER_FLAG_IPV4".into(), Value::Int(1048576));
        constants.insert("FILTER_FLAG_IPV6".into(), Value::Int(2097152));
        // ext-json.
        constants.insert("JSON_ERROR_NONE".into(), Value::Int(0));
        constants.insert("JSON_ERROR_DEPTH".into(), Value::Int(1));
        constants.insert("JSON_ERROR_STATE_MISMATCH".into(), Value::Int(2));
        constants.insert("JSON_ERROR_CTRL_CHAR".into(), Value::Int(3));
        constants.insert("JSON_ERROR_SYNTAX".into(), Value::Int(4));
        constants.insert("JSON_ERROR_UTF8".into(), Value::Int(5));
        constants.insert("JSON_ERROR_RECURSION".into(), Value::Int(6));
        constants.insert("JSON_ERROR_INF_OR_NAN".into(), Value::Int(7));
        constants.insert("JSON_ERROR_UNSUPPORTED_TYPE".into(), Value::Int(8));
        constants.insert("JSON_HEX_TAG".into(), Value::Int(1));
        constants.insert("JSON_HEX_AMP".into(), Value::Int(2));
        constants.insert("JSON_HEX_APOS".into(), Value::Int(4));
        constants.insert("JSON_HEX_QUOT".into(), Value::Int(8));
        constants.insert("JSON_FORCE_OBJECT".into(), Value::Int(16));
        constants.insert("JSON_NUMERIC_CHECK".into(), Value::Int(32));
        constants.insert("JSON_UNESCAPED_SLASHES".into(), Value::Int(64));
        constants.insert("JSON_PRETTY_PRINT".into(), Value::Int(128));
        constants.insert("JSON_UNESCAPED_UNICODE".into(), Value::Int(256));
        constants.insert("JSON_PARTIAL_OUTPUT_ON_ERROR".into(), Value::Int(512));
        constants.insert("JSON_PRESERVE_ZERO_FRACTION".into(), Value::Int(1024));
        constants.insert("JSON_UNESCAPED_LINE_TERMINATORS".into(), Value::Int(2048));
        constants.insert("JSON_INVALID_UTF8_IGNORE".into(), Value::Int(1048576));
        constants.insert("JSON_INVALID_UTF8_SUBSTITUTE".into(), Value::Int(2097152));
        constants.insert("JSON_THROW_ON_ERROR".into(), Value::Int(4194304));
        constants.insert("JSON_OBJECT_AS_ARRAY".into(), Value::Int(1));
        constants.insert("JSON_BIGINT_AS_STRING".into(), Value::Int(2));
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
            pending_call_alias: None,
            globals_order: Vec::new(),
            globals_arr: None,
            stack: Vec::new(),
            functions: HashMap::new(),
            classes: HashMap::new(),
            traits: HashMap::new(),
            trait_statics: HashMap::new(),
            linking: Vec::new(),
            autoloading: std::collections::HashSet::new(),
            variance_obligations: Vec::new(),
            in_variance_pass: false,
            sig_fatal: None,
            declaring: Vec::new(),
            tentative: {
                let mut t = HashSet::new();
                t.insert(("datetimezone".into(), "listidentifiers".into()));
                t
            },
            interfaces: HashMap::new(),
            decl_order: Vec::new(),
            enum_cases: std::collections::HashMap::new(),
            consts_linked: std::collections::HashSet::new(),
            decl_aliases: Vec::new(),
            early_bound_classes: HashSet::new(),
            constants,
            out: Vec::new(),
            err_buf: String::new(),
            decl_file_ctx: None,
            out_headers: Vec::new(),
            resp_code: 200,
            last_json_error: 0,
            last_preg_error: 0,
            php_input: std::rc::Rc::new(Vec::new()),
            uploads: Vec::new(),
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
            gen_sink: None,
            pending_gen_captures: Vec::new(),
            gen_sends: std::collections::VecDeque::new(),
            gen_auto: 0,
            gen_run_state: None,
            pending_decl_class: None,
            pending_called_class: None,
            pending_hook_prop: None,
            in_const_expr: 0,
            class_const_ctx: 0,
            const_self: None,
            autoload_fns: Vec::new(),
            obj_handles: Vec::new(),
            fcc_fn_cache: HashMap::new(),
            destructed: HashMap::new(),
            expr_temps: Vec::new(),
            last_popped_frame: None,
            dump_stack: std::collections::HashSet::new(),
            internal_cb: 0,
            cur_file: file.to_string(),
            strict_files: std::collections::HashSet::new(),
            typed_slots: std::collections::HashMap::new(),
            slot_owners: std::collections::HashMap::new(),
            slot_merged: std::collections::HashMap::new(),
            slot_anchor: std::collections::HashMap::new(),
            ref_cells: std::collections::HashSet::new(),
            magic_guards: std::collections::HashSet::new(),
            readonly_cells: std::collections::HashMap::new(),
            last_fresh_cell: None,
            builtin_ifaces: std::collections::HashSet::new(),
            dep_seen: std::collections::HashSet::new(),
            last_err_file: String::new(),
            assert_src: String::new(),
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
        it.globals.file = file.to_string();
        it.register_builtin_classes();
        it
    }

    /// `phpun file.php a b c` — CLI args after the script name land in
    /// `$argv`/`$argc`/`$_SERVER['argv']` like reference php.
    pub fn set_script_args(&mut self, script: &str, args: &[String]) {
        let mut argv = PhpArray::new();
        argv.push(Value::str(script));
        for a in args {
            argv.push(Value::str(a));
        }
        let argc = argv.len() as i64;
        let argv_v = Value::Array(Rc::new(RefCell::new(argv)));
        if let Some(c) = self.globals.vars.get("_SERVER") {
            if let Value::Array(srv) = &*c.borrow() {
                srv.borrow_mut()
                    .set(ArrKey::Str("argv".into()), argv_v.clone());
                srv.borrow_mut()
                    .set(ArrKey::Str("argc".into()), Value::Int(argc));
            }
        }
        self.globals.vars.insert("argv".into(), cell(argv_v));
        self.globals
            .vars
            .insert("argc".into(), cell(Value::Int(argc)));
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
                adaptations: vec![],
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
                        attrs: vec![],
                        line: 0,
                    })
                    .collect(),
                consts: vec![],
                file: String::new(),
            }
        }
        fn method(name: &str, _params: &[&str]) -> Rc<MethodDecl> {
            Rc::new(MethodDecl {
                decl: FunctionDecl {
                    ret: None,
                    name: name.into(),
                    params: vec![],
                    body: vec![],
                    attrs: vec![],
                    by_ref: false,
                    line: 0,
                    end_line: 0,
                    file: String::new(),
                    ns: String::new(),
                    decl_in: None,
                },
                is_static: false,
                is_abstract: false,
                is_final: false,
                visibility: Visibility::Public,
                trait_alias_of: None,
            })
        }
        let mut reg = |d: ClassDecl, is_iface: bool| {
            let c = Rc::new(PhpClass {
                decl: Rc::new(d),
                statics: RefCell::new(HashMap::new()),
                statics_init: RefCell::new(true),
            });
            if is_iface {
                self.builtin_ifaces.insert(c.decl.name.to_lowercase());
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
            adaptations: vec![],
            methods: methods
                .iter()
                .map(|m| {
                    Rc::new(MethodDecl {
                        decl: FunctionDecl {
                            ret: None,
                            name: m.to_string(),
                            params: vec![],
                            body: vec![],
                            attrs: vec![],
                            by_ref: false,
                            line: 0,
                            end_line: 0,
                            file: String::new(),
                            ns: String::new(),
                            decl_in: None,
                        },
                        is_static: false,
                        is_abstract: true,
                        is_final: false,
                        visibility: Visibility::Public,
                        trait_alias_of: None,
                    })
                })
                .collect(),
            props: vec![],
            consts: vec![],
            file: String::new(),
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
        // Serializable's unserialize takes `string $data` — the iface()
        // helper emits param-less methods, so patch the decl (the
        // interface-sig check compares against it).
        let mut sd = iface("Serializable", &[], &["serialize", "unserialize"]);
        for m in sd.methods.iter_mut() {
            if m.decl.name == "unserialize" {
                let mut m2 = (**m).clone();
                m2.decl.params = vec![Param {
                    name: "data".into(),
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
                }];
                *m = Rc::new(m2);
            }
        }
        reg(sd, true);
        // SPL Observer pair — param-typed per the real SPL stubs so
        // interface-sig checks accept real-world impls (ns_054/056).
        let spl_param = |n: &str, t: &str| Param {
            name: n.into(),
            default: None,
            by_ref: false,
            variadic: false,
            ty: Some(vec![t.into()]),
            promoted: false,
            vis: None,
            readonly: false,
            is_final: false,
            set_vis: None,
            hooks: None,
        };
        let mut so = iface("SplObserver", &[], &["update"]);
        so.methods[0] = {
            let mut m = (*so.methods[0]).clone();
            m.decl.params = vec![spl_param("subject", "SplSubject")];
            Rc::new(m)
        };
        reg(so, true);
        let mut ss = iface("SplSubject", &[], &["attach", "detach", "notify"]);
        for (i, pn) in ["observer", "observer", ""].iter().enumerate() {
            let mut m = (*ss.methods[i]).clone();
            m.decl.params = if pn.is_empty() {
                vec![]
            } else {
                vec![spl_param(pn, "SplObserver")]
            };
            ss.methods[i] = Rc::new(m);
        }
        reg(ss, true);
        reg(iface("SeekableIterator", &["Iterator"], &["seek"]), true);
        // ArrayIterator — SPL iterator over an array; methods are
        // native-dispatched (array_iter_method) on the ArrayIter
        // internal. Named-arg params carry the Zend stub names.
        let stub_method = |name: &str, params: &[&str]| {
            Rc::new(MethodDecl {
                decl: FunctionDecl {
                    ret: None,
                    name: name.into(),
                    params: params
                        .iter()
                        .map(|n| Param {
                            name: n.to_string(),
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
                        })
                        .collect(),
                    body: vec![],
                    attrs: vec![],
                    by_ref: false,
                    line: 0,
                    end_line: 0,
                    file: String::new(),
                    ns: String::new(),
                    decl_in: None,
                },
                is_static: false,
                is_abstract: false,
                is_final: false,
                visibility: Visibility::Public,
                trait_alias_of: None,
            })
        };
        // stub where `req` params are required and `opt` have null defaults
        let stub_method_mix = |name: &str, req: &[&str], opt: &[&str]| {
            let m = stub_method(
                name,
                &req.iter().chain(opt.iter()).copied().collect::<Vec<_>>(),
            );
            let mut decl = m.decl.clone();
            for p in decl.params.iter_mut().skip(req.len()) {
                p.default = Some(Expr::Null);
            }
            Rc::new(MethodDecl {
                decl,
                is_static: m.is_static,
                is_abstract: m.is_abstract,
                is_final: m.is_final,
                visibility: m.visibility,
                trait_alias_of: None,
            })
        };

        reg(
            ClassDecl {
                name: "ArrayIterator".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![
                    "SeekableIterator".into(),
                    "ArrayAccess".into(),
                    "Countable".into(),
                ],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    stub_method("__construct", &["array", "flags"]),
                    stub_method("rewind", &[]),
                    stub_method("valid", &[]),
                    stub_method("current", &[]),
                    stub_method("key", &[]),
                    stub_method("next", &[]),
                    stub_method("count", &[]),
                    stub_method("offsetGet", &["key"]),
                    stub_method("offsetExists", &["key"]),
                    stub_method("offsetSet", &["key", "value"]),
                    stub_method("offsetUnset", &["key"]),
                    stub_method("getArrayCopy", &[]),
                    stub_method("seek", &["offset"]),
                    stub_method("getFlags", &[]),
                    stub_method("setFlags", &["flags"]),
                    stub_method("asort", &["flags"]),
                    stub_method("ksort", &["flags"]),
                    stub_method("natcasesort", &[]),
                    stub_method("natsort", &[]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        // Generator — engine iterator produced by `yield` functions;
        // methods native-dispatch on the Generator internal.
        reg(
            ClassDecl {
                name: "Generator".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: true,
                readonly: false,
                parent: None,
                implements: vec!["Iterator".into()],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    stub_method("rewind", &[]),
                    stub_method("valid", &[]),
                    stub_method("current", &[]),
                    stub_method("key", &[]),
                    stub_method("next", &[]),
                    stub_method("send", &["value"]),
                    stub_method("throw", &["exception"]),
                    stub_method("getReturn", &[]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        // SplFileInfo / DirectoryIterator — SPL filesystem surface;
        // methods native-dispatch on \0fi\0path / DirIter internals.
        reg(
            ClassDecl {
                name: "SplFileInfo".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    stub_method("__construct", &["filename"]),
                    stub_method("getFilename", &[]),
                    stub_method("getBasename", &[]),
                    stub_method("getPathname", &[]),
                    stub_method("getPath", &[]),
                    stub_method("getExtension", &[]),
                    stub_method("getRealPath", &[]),
                    stub_method("isFile", &[]),
                    stub_method("isDir", &[]),
                    stub_method("isLink", &[]),
                    stub_method("isReadable", &[]),
                    stub_method("isWritable", &[]),
                    stub_method("isExecutable", &[]),
                    stub_method("getSize", &[]),
                    stub_method("getMTime", &[]),
                    stub_method("getType", &[]),
                    stub_method("__toString", &[]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        reg(
            ClassDecl {
                name: "DirectoryIterator".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: Some("SplFileInfo".into()),
                implements: vec!["Iterator".into()],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    stub_method("rewind", &[]),
                    stub_method("valid", &[]),
                    stub_method("current", &[]),
                    stub_method("key", &[]),
                    stub_method("next", &[]),
                    stub_method("isDot", &[]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
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
        // PDO + PDOStatement + PDOException — sqlite storage spike (#15).
        reg(
            ClassDecl {
                name: "PDO".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    stub_method_mix(
                        "__construct",
                        &["dsn"],
                        &["username", "password", "options"],
                    ),
                    stub_method("query", &["query"]),
                    stub_method("exec", &["statement"]),
                    stub_method_mix("prepare", &["query"], &["options"]),
                    stub_method_mix("lastInsertId", &[], &["name"]),
                    stub_method("beginTransaction", &[]),
                    stub_method("commit", &[]),
                    stub_method("rollBack", &[]),
                    stub_method("inTransaction", &[]),
                    stub_method_mix("quote", &["string"], &["type"]),
                    stub_method("setAttribute", &["attribute", "value"]),
                    stub_method("getAttribute", &["attribute"]),
                    stub_method("errorCode", &[]),
                    stub_method("errorInfo", &[]),
                ],
                props: vec![],
                consts: vec![
                    crate::ast::ConstDecl {
                        name: "FETCH_ASSOC".into(),
                        value: Expr::Int(2),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "FETCH_NUM".into(),
                        value: Expr::Int(3),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "FETCH_BOTH".into(),
                        value: Expr::Int(4),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "FETCH_OBJ".into(),
                        value: Expr::Int(5),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "ATTR_ERRMODE".into(),
                        value: Expr::Int(3),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "ATTR_DEFAULT_FETCH_MODE".into(),
                        value: Expr::Int(19),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "ATTR_EMULATE_PREPARES".into(),
                        value: Expr::Int(20),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "ERRMODE_SILENT".into(),
                        value: Expr::Int(0),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "ERRMODE_WARNING".into(),
                        value: Expr::Int(1),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "ERRMODE_EXCEPTION".into(),
                        value: Expr::Int(2),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "PARAM_STR".into(),
                        value: Expr::Int(2),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "PARAM_INT".into(),
                        value: Expr::Int(1),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "PARAM_BOOL".into(),
                        value: Expr::Int(5),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "PARAM_NULL".into(),
                        value: Expr::Int(0),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "PARAM_LOB".into(),
                        value: Expr::Int(3),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                ],
                file: String::new(),
            },
            false,
        );
        reg(
            ClassDecl {
                name: "PDOStatement".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    stub_method_mix("execute", &[], &["params"]),
                    stub_method_mix("fetch", &[], &["mode", "cursorOrientation", "cursorOffset"]),
                    stub_method_mix("fetchObject", &[], &["class", "constructorArgs"]),
                    stub_method_mix("fetchAll", &[], &["mode", "args"]),
                    stub_method_mix("fetchColumn", &[], &["column"]),
                    stub_method("rowCount", &[]),
                    stub_method("columnCount", &[]),
                    stub_method_mix("bindValue", &["param", "value"], &["type"]),
                    stub_method_mix(
                        "bindParam",
                        &["param", "var"],
                        &["type", "maxLength", "driverOptions"],
                    ),
                    stub_method("closeCursor", &[]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        reg(
            throwable_class("PDOException", Some("RuntimeException"), &[]),
            true,
        );
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
                adaptations: vec![],
                methods: vec![],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        // ArrayObject — SPL stub carrying its flags as class constants;
        // the ns tests only need `ArrayObject::STD_PROP_LIST` to resolve
        // (namespaces/ns_035, ns_036, bug42819).
        reg(
            ClassDecl {
                name: "ArrayObject".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![
                    "IteratorAggregate".into(),
                    "ArrayAccess".into(),
                    "Countable".into(),
                ],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![],
                props: vec![],
                consts: vec![
                    crate::ast::ConstDecl {
                        name: "STD_PROP_LIST".into(),
                        value: Expr::Int(1),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "ARRAY_AS_PROPS".into(),
                        value: Expr::Int(2),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                ],
                file: String::new(),
            },
            false,
        );
        // SplDoublyLinkedList / SplStack — container stubs with the
        // iteration-state internal the SPL method dispatch reads;
        // SplStack inherits everything from the DLL (closure_061,
        // bug70685).
        reg(
            ClassDecl {
                name: "SplDoublyLinkedList".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    stub_method("__construct", &[]),
                    stub_method("count", &[]),
                    stub_method("push", &["value"]),
                    stub_method("pop", &[]),
                    stub_method("top", &[]),
                    stub_method("bottom", &[]),
                    stub_method("isEmpty", &[]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        reg(
            ClassDecl {
                name: "SplStack".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: Some("SplDoublyLinkedList".into()),
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![],
                props: vec![],
                consts: vec![],
                file: String::new(),
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
                adaptations: vec![],
                methods: vec![
                    Rc::new(MethodDecl {
                        decl: FunctionDecl {
                            ret: None,
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
                            attrs: vec![],
                            by_ref: false,
                            line: 0,
                            end_line: 0,
                            file: String::new(),
                            ns: String::new(),
                            decl_in: None,
                        },
                        is_static: false,
                        is_abstract: false,
                        is_final: false,
                        visibility: Visibility::Public,
                        trait_alias_of: None,
                    }),
                    Rc::new(MethodDecl {
                        decl: FunctionDecl {
                            ret: Some(vec!["DateTime".into(), "false".into()]),
                            name: "createFromFormat".into(),
                            params: vec![
                                Param {
                                    name: "format".into(),
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
                                },
                                Param {
                                    name: "datetime".into(),
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
                                },
                                Param {
                                    name: "timezone".into(),
                                    default: Some(Expr::Null),
                                    by_ref: false,
                                    variadic: false,
                                    ty: Some(vec!["DateTimeZone".into(), "null".into()]),
                                    promoted: false,
                                    vis: None,
                                    readonly: false,
                                    is_final: false,
                                    set_vis: None,
                                    hooks: None,
                                },
                            ],
                            body: vec![],
                            attrs: vec![],
                            by_ref: false,
                            line: 0,
                            end_line: 0,
                            file: String::new(),
                            ns: String::new(),
                            decl_in: None,
                        },
                        is_static: true,
                        is_abstract: false,
                        is_final: false,
                        visibility: Visibility::Public,
                        trait_alias_of: None,
                    }),
                    Rc::new(MethodDecl {
                        decl: FunctionDecl {
                            ret: Some(vec!["int".into()]),
                            name: "getTimestamp".into(),
                            params: vec![],
                            body: vec![],
                            attrs: vec![],
                            by_ref: false,
                            line: 0,
                            end_line: 0,
                            file: String::new(),
                            ns: String::new(),
                            decl_in: None,
                        },
                        is_static: false,
                        is_abstract: false,
                        is_final: false,
                        visibility: Visibility::Public,
                        trait_alias_of: None,
                    }),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        // DateTimeZone — stub with the const + tentative-typed
        // listIdentifiers the internal_parent variance tests override.
        reg(
            ClassDecl {
                name: "DateTimeZone".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![Rc::new(MethodDecl {
                    decl: FunctionDecl {
                        ret: Some(vec!["array".into()]),
                        name: "listIdentifiers".into(),
                        params: vec![
                            Param {
                                name: "timezoneGroup".into(),
                                default: Some(Expr::Const("DateTimeZone::ALL".into())),
                                by_ref: false,
                                variadic: false,
                                ty: Some(vec!["int".into()]),
                                promoted: false,
                                vis: None,
                                readonly: false,
                                is_final: false,
                                set_vis: None,
                                hooks: None,
                            },
                            Param {
                                name: "countryCode".into(),
                                default: Some(Expr::Null),
                                by_ref: false,
                                variadic: false,
                                ty: Some(vec!["string".into(), "null".into()]),
                                promoted: false,
                                vis: None,
                                readonly: false,
                                is_final: false,
                                set_vis: None,
                                hooks: None,
                            },
                        ],
                        body: vec![],
                        attrs: vec![],
                        by_ref: false,
                        line: 0,
                        end_line: 0,
                        file: String::new(),
                        ns: String::new(),
                        decl_in: None,
                    },
                    is_static: true,
                    is_abstract: false,
                    is_final: false,
                    visibility: Visibility::Public,
                    trait_alias_of: None,
                })],
                props: vec![],
                consts: vec![ConstDecl {
                    name: "ALL".into(),
                    value: Expr::Int(2047),
                    visibility: Visibility::Public,
                    is_final: false,
                    ty: None,
                    attrs: vec![],
                    decl_in: None,
                    enum_case: false,
                }],
                file: String::new(),
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
                    ret: None,
                    name: name.into(),
                    params,
                    body: vec![],
                    attrs: vec![],
                    by_ref: false,
                    line: 0,
                    end_line: 0,
                    file: String::new(),
                    ns: String::new(),
                    decl_in: None,
                },
                is_static: false,
                is_abstract: false,
                is_final: false,
                visibility: Visibility::Public,
                trait_alias_of: None,
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
        let any_param = |n: &str, variadic: bool| Param {
            name: n.into(),
            default: None,
            by_ref: false,
            variadic,
            ty: None,
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
                adaptations: vec![],
                methods: vec![
                    mk_method("__construct", vec![any_param("class", false)]),
                    mk_method("newInstanceWithoutConstructor", vec![]),
                    mk_method("newInstance", vec![any_param("args", true)]),
                    mk_method("newInstanceArgs", vec![any_param("args", false)]),
                    mk_method("getName", vec![]),
                    mk_method("getAttributes", vec![]),
                    mk_method("getConstant", vec![str_param("name")]),
                    mk_method("getConstants", vec![]),
                    mk_method("getReflectionConstant", vec![str_param("name")]),
                    mk_method("getReflectionConstants", vec![]),
                    mk_method("getTraitAliases", vec![]),
                    mk_method("getProperty", vec![str_param("name")]),
                    mk_method("hasProperty", vec![str_param("name")]),
                    mk_method("getDefaultProperties", vec![]),
                    mk_method("getInterfaceNames", vec![]),
                    mk_method("getInterfaces", vec![]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        reg(
            ClassDecl {
                name: "ReflectionFunction".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    mk_method("__construct", vec![any_param("function", false)]),
                    mk_method("invoke", vec![any_param("args", true)]),
                    mk_method("invokeArgs", vec![any_param("args", false)]),
                    mk_method("getName", vec![]),
                    mk_method("getAttributes", vec![]),
                    mk_method("getParameters", vec![]),
                    mk_method("isClosure", vec![]),
                    mk_method("isAnonymous", vec![]),
                    mk_method("getClosure", vec![]),
                    mk_method("getClosureScopeClass", vec![]),
                    mk_method("getClosureCalledClass", vec![]),
                    mk_method("getClosureThis", vec![]),
                    mk_method("getShortName", vec![]),
                    mk_method("getNamespaceName", vec![]),
                    mk_method("inNamespace", vec![]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        reg(
            ClassDecl {
                name: "ReflectionMethod".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    mk_method(
                        "__construct",
                        vec![any_param("class", false), any_param("name", false)],
                    ),
                    mk_method(
                        "invoke",
                        vec![any_param("object", false), any_param("args", true)],
                    ),
                    mk_method(
                        "invokeArgs",
                        vec![any_param("object", false), any_param("args", false)],
                    ),
                    mk_method("getName", vec![]),
                    mk_method("getShortName", vec![]),
                    mk_method("getNamespaceName", vec![]),
                    mk_method("inNamespace", vec![]),
                    mk_method("isFinal", vec![]),
                    mk_method("isAbstract", vec![]),
                    mk_method("isStatic", vec![]),
                    mk_method("isPublic", vec![]),
                    mk_method("isProtected", vec![]),
                    mk_method("isPrivate", vec![]),
                    mk_method("getParameters", vec![]),
                    mk_method("getClosure", vec![any_param("object", false)]),
                    mk_method("getClosureScopeClass", vec![]),
                    mk_method("getClosureCalledClass", vec![]),
                    mk_method("getClosureThis", vec![]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        // ReflectionParameter — produced by getParameters();
        // per-param data lives under \0rp\0* props.
        reg(
            ClassDecl {
                name: "ReflectionParameter".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    mk_method("isCallable", vec![]),
                    mk_method("isVariadic", vec![]),
                    mk_method("hasType", vec![]),
                    mk_method("getType", vec![]),
                    mk_method("getName", vec![]),
                    mk_method("getClass", vec![]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        // ReflectionClassConstant — produced by ReflectionClass::
        // getReflectionConstant(s) (constant_019-021).
        reg(
            ClassDecl {
                name: "ReflectionClassConstant".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    mk_method(
                        "__construct",
                        vec![any_param("class", false), any_param("name", false)],
                    ),
                    mk_method("getName", vec![]),
                    mk_method("getValue", vec![]),
                    mk_method("getDocComment", vec![]),
                    mk_method("getAttributes", vec![]),
                    mk_method("getDeclaringClass", vec![]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        // ReflectionAttribute — produced by getAttributes(); carries the
        // attribute name + unevaluated arg exprs + target kind.
        reg(
            ClassDecl {
                name: "ReflectionAttribute".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    mk_method("getName", vec![]),
                    mk_method("getArguments", vec![]),
                    mk_method("newInstance", vec![]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        // Attribute — the `#[Attribute]` marker class + TARGET_* flags.
        reg(
            ClassDecl {
                name: "Attribute".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![],
                props: vec![],
                consts: vec![
                    crate::ast::ConstDecl {
                        name: "TARGET_CLASS".into(),
                        value: Expr::Int(1),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "TARGET_FUNCTION".into(),
                        value: Expr::Int(2),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "TARGET_METHOD".into(),
                        value: Expr::Int(4),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "TARGET_PROPERTY".into(),
                        value: Expr::Int(8),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "TARGET_CLASS_CONSTANT".into(),
                        value: Expr::Int(16),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "TARGET_PARAMETER".into(),
                        value: Expr::Int(32),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "TARGET_ALL".into(),
                        value: Expr::Int(63),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                    crate::ast::ConstDecl {
                        name: "IS_REPEATABLE".into(),
                        value: Expr::Int(64),
                        visibility: crate::ast::Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    },
                ],
                file: String::new(),
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
                adaptations: vec![],
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
                    mk_method("getName", vec![]),
                    mk_method("getType", vec![]),
                    mk_method("getValue", vec![str_param("object")]),
                    mk_method("setValue", vec![str_param("object"), str_param("value")]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        // ReflectionObject — minimal stub (bug50146).
        reg(
            ClassDecl {
                name: "ReflectionObject".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    mk_method("__construct", vec![str_param("object")]),
                    mk_method("hasProperty", vec![str_param("name")]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
            false,
        );
        // ReflectionNamedType — produced by getType() on a property /
        // parameter / return (typed_properties_018).
        reg(
            ClassDecl {
                name: "ReflectionNamedType".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: false,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![
                    mk_method("getName", vec![]),
                    mk_method("allowsNull", vec![]),
                    mk_method("isBuiltin", vec![]),
                ],
                props: vec![],
                consts: vec![],
                file: String::new(),
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
        // Closure — name must resolve for `\Closure::bind()` /
        // `\Closure::fromCallable()` (composer's ClassLoader uses bind to
        // scope-isolate `include`). Methods dispatch natively in
        // static_invoke / the Value::Callable method arm.
        reg(
            ClassDecl {
                name: "Closure".into(),
                kind: ClassKind::Class,
                is_abstract: false,
                is_final: true,
                readonly: false,
                parent: None,
                implements: vec![],
                attrs: vec![],
                traits: vec![],
                adaptations: vec![],
                methods: vec![],
                props: vec![],
                consts: vec![],
                file: String::new(),
            },
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

    /// eval_const for a decl-attached expr (prop/const/param default):
    /// __FILE__/__DIR__ inside resolve to the declaring file.
    fn eval_decl_const(&mut self, e: &Expr, decl_file: &str) -> Result<Value, PhpError> {
        if decl_file.is_empty() {
            return self.eval_const(e);
        }
        let old = self.decl_file_ctx.replace(decl_file.to_string());
        let r = self.eval_const(e);
        self.decl_file_ctx = old;
        r
    }

    /// PHP binds a compilation unit's unconditional top-level function
    /// decls before executing it (bug23279's later-declared handler).
    fn hoist_funcs(&mut self, stmts: &[Stmt]) {
        for s in stmts {
            match s {
                Stmt::Function(d) => {
                    let _ = self.decl_type_checks(&d.name, d, None);
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
                    let _ = self.call_value(&h, CallArgs::positional(vec![cell(v)]));
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
            let _ = self.call_value(&f, CallArgs::positional(args));
        }
        // Zend calls __destruct on live objects after shutdown functions
        // and before output buffers flush — destructors still see their
        // buffers' contents (bug30578, bug24908). Two phases:
        //  1) CV teardown — the global symbol table frees in reverse
        //     declaration order; objects whose last ref is a global var
        //     die newest-created first (bug36759).
        //  2) object store pass — remaining live objects in creation
        //     order; objects a dtor spawns get visited too (bug74053).
        let mut cv_objs: Vec<(u64, Rc<RefCell<PhpObject>>)> = Vec::new();
        for c in self.globals.vars.values() {
            if let Value::Object(o) = &*c.borrow() {
                cv_objs.push((o.borrow().id, o.clone()));
            }
        }
        cv_objs.sort_by_key(|(id, _)| *id);
        for (_, o) in cv_objs.into_iter().rev() {
            // strong_count 2 = the var's cell + our clone.
            if Rc::strong_count(&o) != 2 {
                continue;
            }
            if self
                .find_method_in(&o.borrow().class, "__destruct")
                .is_some()
                && self.mark_destructed(&o)
            {
                let _ = self.method_invoke(o.clone(), "__destruct", CallArgs::empty());
            }
        }
        self.globals.vars.clear();
        // Objects a dtor spawns may land in already-visited recycled
        // handle slots — rescan until a full pass runs nothing new
        // (bug51822/bug74053).
        loop {
            let mut progressed = false;
            let mut i = 0;
            while i < self.obj_handles.len() {
                let w = match &self.obj_handles[i] {
                    ObjHandle::Obj(w) => w.clone(),
                    _ => {
                        i += 1;
                        continue;
                    }
                };
                i += 1;
                let Some(o) = w.upgrade() else { continue };
                let key = Rc::as_ptr(&o) as usize;
                if self.destructed.contains_key(&key) {
                    continue;
                }
                if self
                    .find_method_in(&o.borrow().class, "__destruct")
                    .is_some()
                {
                    self.mark_destructed(&o);
                    progressed = true;
                    let _ = self.method_invoke(o.clone(), "__destruct", CallArgs::empty());
                }
            }
            if !progressed {
                break;
            }
        }
        if !self.mem_exceeded {
            self.flush_ob_all();
        }
    }

    /// Free the expression statement's temporaries: an object with no
    /// remaining owner destructs now, first-created first — matching
    /// Zend freeing the VM temp slots at statement end. `base` scopes
    /// the sweep to temporaries created during *this* statement — a
    /// nested statement's unwind must not free an outer statement's
    /// live temps (bug29368_3).
    fn sweep_expr_temps(&mut self, base: usize) -> Result<(), PhpError> {
        let temps = self.expr_temps.split_off(base);
        for o in temps {
            if Rc::strong_count(&o) != 1 {
                continue;
            }
            let key = Rc::as_ptr(&o) as usize;
            if self.destructed.contains_key(&key) {
                continue;
            }
            if self
                .find_method_in(&o.borrow().class, "__destruct")
                .is_some()
            {
                self.mark_destructed(&o);
                self.method_invoke(o.clone(), "__destruct", CallArgs::empty())?;
            }
        }
        Ok(())
    }

    /// Objects whose last refs live inside a dropped value run
    /// __destruct — `unset($closure)` decrefs the closure's bound
    /// $this and captures (Zend refcount semantics — closure_005).
    fn destruct_dying_value(&mut self, v: &Value) -> Result<(), PhpError> {
        let mut held: HashMap<usize, (usize, Rc<RefCell<PhpObject>>)> = HashMap::new();
        let mut tally = |o: &Rc<RefCell<PhpObject>>| {
            held.entry(Rc::as_ptr(o) as usize)
                .or_insert_with(|| (0, o.clone()))
                .0 += 1;
        };
        match v {
            Value::Object(o) => tally(o),
            Value::Callable(c) => {
                if let Some(o) = &c.this_obj {
                    tally(o);
                }
                for (_, cap, _) in &c.captures {
                    if let Value::Object(o) = &*cap.borrow() {
                        tally(o);
                    }
                }
            }
            _ => {}
        }
        for (_, (n, o)) in held {
            // `o` contributes one ref from `held` itself; `v` holds n.
            if Rc::strong_count(&o) != n + 1 {
                continue;
            }
            if self
                .find_method_in(&o.borrow().class, "__destruct")
                .is_some()
                && self.mark_destructed(&o)
            {
                let _ = self.method_invoke(o.clone(), "__destruct", CallArgs::empty());
            }
        }
        Ok(())
    }

    /// Decref the running frame's CVs (vars/args/$this): an object
    /// whose strong refs are exactly the cells this frame is about
    /// to drop runs its __destruct now — Zend's behavior at function
    /// exit and exception unwind (bug52361).
    fn destruct_frame_objs(&mut self, f: &Frame) -> Result<(), PhpError> {
        let mut held: HashMap<usize, (usize, Rc<RefCell<PhpObject>>)> = HashMap::new();
        let mut tally = |c: &Cell| {
            if let Value::Object(o) = &*c.borrow() {
                held.entry(Rc::as_ptr(o) as usize)
                    .or_insert_with(|| (0, o.clone()))
                    .0 += 1;
            }
        };
        for c in f.vars.values() {
            tally(c);
        }
        for c in f.args.iter() {
            tally(c);
        }
        if let Some(o) = &f.this_obj {
            held.entry(Rc::as_ptr(o) as usize)
                .or_insert_with(|| (0, o.clone()))
                .0 += 1;
        }
        for (_, (n, o)) in held {
            // +1 for the `o` clone sitting in `held` itself.
            if Rc::strong_count(&o) != n + 1 {
                continue;
            }
            let key = Rc::as_ptr(&o) as usize;
            if !self.destructed.contains_key(&key)
                && self
                    .find_method_in(&o.borrow().class, "__destruct")
                    .is_some()
            {
                self.mark_destructed(&o);
                // A throw inside the dtor must not clobber the
                // in-flight exception being unwound (bug52361).
                let saved = self.pending_exception.take();
                let _ = self.method_invoke(o.clone(), "__destruct", CallArgs::empty());
                self.pending_exception = saved.or(self.pending_exception.take());
            }
        }
        Ok(())
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

    /// phpun serve: replace a request superglobal ($_GET/$_POST/...).
    pub fn set_superglobal(&mut self, name: &str, arr: PhpArray) {
        self.globals.vars.insert(
            name.to_string(),
            cell(Value::Array(Rc::new(RefCell::new(arr)))),
        );
    }

    /// phpun serve: set one $_SERVER entry (REQUEST_METHOD, HTTP_*, ...).
    pub fn set_server_var(&mut self, k: &str, v: &str) {
        let c = match self.globals.vars.get("_SERVER") {
            Some(c) => c.clone(),
            None => return,
        };
        let arr = match &*c.borrow() {
            Value::Array(a) => a.clone(),
            _ => return,
        };
        arr.borrow_mut().set(ArrKey::Str(k.into()), Value::str(v));
    }

    pub fn run_source(&mut self, src: &str) -> RunResult {
        match parser::parse_source(src, self.ini_on("short_open_tag")) {
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

    /// Like run_source but also returns the script's top-level `return`
    /// value — the boot phase of `phpun serve --worker` reads the app
    /// handler this way.
    pub fn run_source_ret(&mut self, src: &str) -> (RunResult, Option<Value>) {
        match parser::parse_source(src, self.ini_on("short_open_tag")) {
            Ok(stmts) => {
                self.hoist_funcs(&stmts);
                let flow = self.exec_block(&stmts);
                let rv = match &flow {
                    Flow::Return(v) => Some(v.clone()),
                    _ => None,
                };
                let res = self.finish(flow);
                self.run_shutdown();
                (res, rv)
            }
            Err(e) => {
                match e.kind {
                    ErrorKind::Parse => self.print_parse(&e),
                    _ => self.print_fatal(&e),
                }
                (
                    RunResult {
                        exit_code: 255,
                        fatal: Some(e),
                    },
                    None,
                )
            }
        }
    }

    /// Worker mode: clear per-request state while keeping the warm world
    /// (classes, functions, global vars, objects) alive.
    pub fn reset_request(&mut self) {
        self.out.clear();
        self.err_buf.clear();
        self.out_headers.clear();
        self.resp_code = 200;
        self.uploads.clear();
        self.ob_stack.clear();
        self.silence = 0;
        self.pending_exception = None;
        self.call_trace.clear();
        self.deadline = None;
    }

    /// Record an object as destructed: true iff newly marked.
    fn mark_destructed(&mut self, o: &Rc<RefCell<PhpObject>>) -> bool {
        self.destructed
            .insert(Rc::as_ptr(o) as usize, o.clone())
            .is_none()
    }

    /// Worker mode: objects created during boot are application state and
    /// must not be destructed at request end. Call once after the boot
    /// phase so per-request shutdown only sweeps request objects.
    pub fn seal_boot_objects(&mut self) {
        self.obj_handles.clear();
    }

    /// Worker-mode request end: registered shutdown functions and
    /// destructors for request-created objects.
    pub fn end_request(&mut self) {
        self.run_shutdown();
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

    /// Does the variable name resolve to an existing cell?
    fn var_lookup(&mut self, name: &str) -> Option<Cell> {
        self.cur()
            .vars
            .get(name)
            .cloned()
            .or_else(|| self.superglobal_cell(name))
    }

    fn var_get(&mut self, name: &str) -> Result<Value, PhpError> {
        match self.cur().vars.get(name) {
            Some(c) => Ok(c.borrow().clone()),
            None => match self.superglobal_cell(name) {
                Some(c) => Ok(c.borrow().clone()),
                None => {
                    // Inside any function frame, a missing $this is a
                    // hard "Using $this when not in object context"
                    // Error; top-level warns (closure_005).
                    if name == "this" && !self.stack.is_empty() {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Using $this when not in object context",
                            0,
                        ));
                    }
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

    /// Write through an existing var cell honoring typed-slot gates —
    /// a `&`-bound typed prop cell rejects bad values with TypeError
    /// (typed_properties_108 catch binding). `strict` applies
    /// catch-bind semantics: no coercion.
    fn var_set_gated(&mut self, name: &str, v: Value, strict: bool) -> Result<(), PhpError> {
        match self.var_cell_opt(name) {
            Some(c) => {
                let nv = self.typed_slot_store_mode(&c, v, strict)?;
                *c.borrow_mut() = nv;
                Ok(())
            }
            None => {
                self.cur()
                    .vars
                    .insert(name.to_string(), Rc::new(RefCell::new(v)));
                Ok(())
            }
        }
    }

    fn fn_statics_key(&self) -> String {
        self.stack
            .last()
            .map(|f| {
                // Method statics are per-(function, declaring class):
                // trait-merged methods get independent statics in each
                // using class, while inherited methods share their
                // declaring class's table (language013).
                match &f.decl_class {
                    Some(c) => format!("{}\u{0}{}", c.name(), f.fn_name),
                    None => f.fn_name.clone(),
                }
            })
            .unwrap_or_else(|| "\u{0}global".into())
    }

    /// Emit output through the output-buffer stack.
    pub fn emit(&mut self, s: &str) {
        self.emit_bytes(s.as_bytes());
    }

    /// Byte-faithful emit — program output is bytes (echo of binary
    /// strings, file reads, preg results must not be UTF-8 validated).
    pub fn emit_bytes(&mut self, b: &[u8]) {
        // memory_limit>0 turns into a deferred fatal once accumulated
        // writes pass it (bug45392); checked at the next statement.
        self.mem_used += b.len() as u64;
        self.mem_last = b.len() as u64;
        // Inside a generator run, output after a yield is deferred to
        // resume — `f(yield)` must not observe the call (nor its echo)
        // until the consumer advances past that yield.
        if let Some(run) = &self.gen_run_state {
            let done = self
                .gen_sink
                .as_ref()
                .map(|s| s.borrow().len())
                .unwrap_or(0);
            if done > 0 {
                run.borrow_mut().pending_out.push((done - 1, b.to_vec()));
                return;
            }
        }
        if let Some(buf) = self.ob_stack.last_mut() {
            buf.buf.extend_from_slice(b);
        } else {
            self.out.extend_from_slice(b);
        }
    }

    /// Emit generator-deferred output whose suspending yield the
    /// consumer has now advanced past (`pos > tag`). Pass
    /// `usize::MAX` to flush everything (getReturn runs to the end).
    fn gen_flush_out(&mut self, state: &Rc<RefCell<crate::value::GenState>>, pos: usize) {
        let ready = {
            let mut st = state.borrow_mut();
            let split = st
                .pending_out
                .iter()
                .position(|(t, _)| *t >= pos)
                .unwrap_or(st.pending_out.len());
            let mut rest = st.pending_out.split_off(split);
            std::mem::swap(&mut st.pending_out, &mut rest);
            rest
        };
        for (_, b) in ready {
            self.emit_bytes(&b);
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

    /// PHP CLI also logs a `PHP <Level>:` line to stderr (log_errors is
    /// on by default) — buffered separately so it lands after stdout in
    /// the merged PHPT stream.
    /// html_errors=1 switches to the `<b>` docref format (bug35176).
    fn diag(&mut self, level: &str, msg: &str) {
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
        self.log_diag(level, msg);
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
        self.err_buf.push_str(&format!(
            "PHP {}:  {} in {} on line {}\n",
            level,
            msg,
            self.diag_file(),
            self.cur_line
        ));
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
        let log_errors = self
            .ini
            .get("log_errors")
            .is_none_or(|v| matches!(v.to_lowercase().as_str(), "1" | "on" | "true" | "yes"));
        if log_errors {
            self.err_buf.push_str(&format!(
                "PHP Parse error:  {} in {} on line {}\n",
                e.message, self.file, e.line
            ));
        }
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
                let ef = if self.last_err_file.is_empty() {
                    self.file.to_string()
                } else {
                    self.last_err_file.clone()
                };
                let dmsg = e.display_msg.clone().unwrap_or_else(|| e.message.clone());
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
                let log_errors = self.ini.get("log_errors").is_none_or(|v| {
                    matches!(v.to_lowercase().as_str(), "1" | "on" | "true" | "yes")
                });
                if log_errors {
                    self.err_buf.push_str(&format!(
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
                if self.mem_exceeded {
                    // Memory-exhausted: buffers are dropped, so the
                    // fatal goes straight to output (bug45392).
                    self.out.extend_from_slice(s.as_bytes());
                } else {
                    self.emit(&s);
                }
                let log_errors = self.ini.get("log_errors").is_none_or(|v| {
                    matches!(v.to_lowercase().as_str(), "1" | "on" | "true" | "yes")
                });
                if log_errors {
                    self.err_buf.push_str(&format!(
                        "PHP Fatal error:  {} in {} on line {}\n{}",
                        e.message, ef, e.line, tr
                    ));
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
            } else {
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
                    self.out.extend_from_slice(format!(
                        "\nFatal error: Uncaught {}{}{} in {}:{}\nStack trace:\n{}\n  thrown in {} on line {}\n",
                        class, colon, msg, file, line, tr, file, thrown
                    ).as_bytes());
                    // The PHP CLI SAPI also logs the uncaught to stderr
                    // when log_errors is on (its default); merged-output
                    // PHPT runs see it as a `PHP Fatal error:` copy of
                    // the same block.
                    let log_errors = self.ini.get("log_errors").is_none_or(|v| {
                        matches!(v.to_lowercase().as_str(), "1" | "on" | "true" | "yes")
                    });
                    if log_errors {
                        self.err_buf.push_str(&format!(
                            "PHP Fatal error:  Uncaught {}{}{} in {}:{}\nStack trace:\n{}\n  thrown in {} on line {}\n",
                            class, colon, msg, file, line, tr, file, thrown
                        ));
                    }
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
        let obj = self.instantiate(&resolved, &[]).unwrap_or(Value::Null);
        if let Value::Object(o) = &obj {
            let mut o = o.borrow_mut();
            o.props.insert("message".into(), cell(Value::str(msg)));
            o.props.insert("code".into(), cell(Value::Int(0)));
            o.internal = Some(ObjectInternal::Exception {
                file: self.diag_file(),
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
    fn call_builtin(&mut self, name: &str, args: &CallArgs) -> Result<Option<Value>, PhpError> {
        self.call_trace.push(TraceFrame {
            function: name.to_string(),
            class: None,
            ty: String::new(),
            file: self.diag_file(),
            line: self.cur_line as u32,
            args: args.to_vec(),
            named_args: args
                .named
                .iter()
                .map(|(n, c, ..)| (n.clone(), c.clone()))
                .collect(),
            internal: true,
        });
        if name == "assert" {
            // AssertionError message = `assert(<args>)` as written.
            let mut parts: Vec<String> = Vec::new();
            for c in &args.cells {
                parts.push(assert_arg_repr(&c.borrow()));
            }
            for (n, c, ..) in &args.named {
                parts.push(format!("{}: {}", n, assert_arg_repr(&c.borrow())));
            }
            self.assert_src = parts.join(", ");
        }
        if !args.named.is_empty() && matches!(name, "call_user_func" | "forward_static_call") {
            // call_user_func forwards named args to the callee, not to
            // its own `callback` param (named_params/call_user_func).
            let cb = args
                .cells
                .first()
                .map(|c| c.borrow().clone())
                .unwrap_or(Value::Null);
            let ca = CallArgs {
                cells: args.cells[1.min(args.cells.len())..].to_vec(),
                named: args.named.clone(),
                nonref_cells: args
                    .nonref_cells
                    .iter()
                    .filter(|i| **i >= 1)
                    .map(|i| i - 1)
                    .collect(),
                trav_cells: args
                    .trav_cells
                    .iter()
                    .filter(|i| **i >= 1)
                    .map(|i| i - 1)
                    .collect(),
            };
            // Callbacks dispatched from inside an internal function
            // trace from `[internal function]` (closure_064).
            self.internal_cb += 1;
            let r = self.call_value(&cb, ca);
            self.internal_cb -= 1;
            self.call_trace.pop();
            return match r {
                Ok(v) => Ok(Some(v)),
                Err(e) => self.fail(e),
            };
        }
        // strict_types applies to internal-function calls too: scalar
        // args must already be the declared ZPP type (int->float still
        // widens), else a catchable TypeError — the short form, no
        // `called in ... and defined in` suffix (that's userland-only).
        // `callable` params are validated eagerly even in weak mode
        // (Zend's `f` ZPP flag) with the callback-specific messages.
        if args.named.is_empty() {
            if let Some(sig) = builtins::strict_sig(name) {
                let strict = self.caller_file_strict();
                for (i, (pname, pty)) in sig.iter().enumerate() {
                    if i >= args.cells.len() {
                        break;
                    }
                    let v = args.cells[i].borrow().clone();
                    let has_cb = pty
                        .trim_start_matches('?')
                        .split('|')
                        .any(|t| t == "callable");
                    if has_cb {
                        // A union member wins on its own type: '0'
                        // satisfies `string` in `string|array|callable`
                        // even though it is not callable
                        // (closure_047/048).
                        let ok = (pty.starts_with('?') && matches!(v, Value::Null))
                            || self.is_callable_value(&v)
                            || pty
                                .trim_start_matches('?')
                                .split('|')
                                .filter(|t| *t != "callable")
                                .any(|t| self.param_type_match(t, &v));
                        if !ok {
                            let null = if pty.starts_with('?') { " or null" } else { "" };
                            let msg = format!(
                                "{}(): Argument #{} (${}) must be a valid callback{}, {}",
                                name,
                                i + 1,
                                pname,
                                null,
                                self.zpp_callback_detail(&v),
                            );
                            let e = self.exception("TypeError", &msg);
                            let te = self.throw(e);
                            let r = self.fail(te);
                            self.call_trace.pop();
                            return r;
                        }
                    } else if strict && !self.zpp_strict_ok(pty, &v) {
                        let msg = format!(
                            "{}(): Argument #{} (${}) must be of type {}, {} given",
                            name,
                            i + 1,
                            pname,
                            pty,
                            self.zval_type_name(&v),
                        );
                        let e = self.exception("TypeError", &msg);
                        let te = self.throw(e);
                        let r = self.fail(te);
                        self.call_trace.pop();
                        return r;
                    }
                }
            }
        }
        match builtins::builtin_params(name) {
            // Internal fns with a known signature get Zend's named-arg
            // resolution AND positional arity checks.
            Some(params) => {
                self.internal_cb += 1;
                let r = match self.resolve_named_builtin(name, params, args) {
                    Ok(cells) => builtins::call(self, name, &cells),
                    Err(e) => Err(e),
                };
                self.internal_cb -= 1;
                // fail() captures call_trace — pop AFTER it so the
                // builtin's own frame shows in the backtrace
                // (`array_multisort(: 1)` in call_user_func_array_variadic).
                let r = match r {
                    Ok(v) => Ok(v),
                    Err(e) => self.fail(e),
                };
                self.call_trace.pop();
                r
            }
            // Internal fns without a signature accept no named args;
            // names that aren't builtins at all fall through so the
            // userland invoke path sees them.
            None if builtins::is_builtin(name) && !args.named.is_empty() => {
                let r = self.fail(PhpError::uncaught(
                    "Error",
                    format!("Unknown named parameter ${}", args.named[0].0),
                    0,
                ));
                self.call_trace.pop();
                r
            }
            None => {
                self.internal_cb += 1;
                let r = builtins::call(self, name, args);
                self.internal_cb -= 1;
                let r = match r {
                    Ok(r) => Ok(r),
                    Err(e) => self.fail(e),
                };
                self.call_trace.pop();
                r
            }
        }
    }

    /// Reorder named args to positional cells against an internal
    /// function's stub signature (Zend/tests/named_params/internal*).
    /// By-ref params receive the caller's cell; defaults fill interior
    /// gaps; unknown names are the "Unknown named parameter" Error
    /// (variadic-only stubs reject with a different message).
    fn resolve_named_builtin(
        &mut self,
        name: &str,
        params: &[(&'static str, builtins::BDef)],
        args: &CallArgs,
    ) -> Result<Vec<Cell>, PhpError> {
        use builtins::BDef;
        // Internal functions whose variadic is declared Z_PARAM_VARIADIC
        // ('*') reject every named arg (zend_compile "does not accept
        // unknown named parameters").
        const NAMED_REJECT: &[&str] = &[
            "array_merge",
            "array_merge_recursive",
            "array_diff",
            "array_diff_key",
            "array_diff_assoc",
            "array_diff_ukey",
            "array_diff_uassoc",
            "array_udiff",
            "array_udiff_assoc",
            "array_udiff_uassoc",
            "array_intersect",
            "array_intersect_key",
            "array_intersect_assoc",
            "array_intersect_ukey",
            "array_intersect_uassoc",
            "array_uintersect",
            "array_uintersect_assoc",
            "array_uintersect_uassoc",
        ];
        if NAMED_REJECT.contains(&name) && !args.named.is_empty() {
            return Err(PhpError::uncaught(
                "ArgumentCountError",
                format!("{}() does not accept unknown named parameters", name),
                0,
            ));
        }
        let variadic = params.iter().any(|(_, d)| matches!(d, BDef::Var));
        let n_fixed = params
            .iter()
            .take_while(|(_, d)| !matches!(d, BDef::Var))
            .count();
        let required = params[..n_fixed]
            .iter()
            .filter(|(_, d)| matches!(d, BDef::Req))
            .count();
        // Positional arity errors use Zend's internal-function wording:
        // "expects exactly" when all fixed params are required, else
        // "at least"/"at most" with the required/fixed bound.
        let arity_err = |got: usize, over: bool| {
            let (word, n) = if !variadic && over {
                if required == n_fixed {
                    ("exactly", n_fixed)
                } else {
                    ("at most", n_fixed)
                }
            } else if required == n_fixed && !variadic {
                ("exactly", required)
            } else {
                // Optional params or a variadic tail: "at least".
                ("at least", required)
            };
            PhpError::uncaught(
                "ArgumentCountError",
                format!(
                    "{}() expects {} {} argument{}, {} given",
                    name,
                    word,
                    n,
                    if n == 1 { "" } else { "s" },
                    got
                ),
                0,
            )
        };
        let mut slot: Vec<Option<Cell>> = vec![None; n_fixed];
        let mut extra_pos: Vec<Cell> = Vec::new();
        for (i, c) in args.cells.iter().enumerate() {
            if i < n_fixed {
                slot[i] = Some(c.clone());
            } else {
                extra_pos.push(c.clone());
            }
        }
        if !variadic && !extra_pos.is_empty() {
            return Err(arity_err(args.cells.len(), true));
        }
        // `assert(description: X)` with no positional/assertion arg hits
        // a Zend arg-parsing quirk that reports an overwrite (assert.phpt).
        if name == "assert" && args.cells.is_empty() {
            if let Some((n, ..)) = args.named.iter().find(|(n, ..)| n == "description") {
                if !args.named.iter().any(|(n, ..)| n == "assertion") {
                    return Err(PhpError::uncaught(
                        "Error",
                        format!("Named parameter ${} overwrites previous argument", n),
                        0,
                    ));
                }
            }
        }
        let mut any_fixed_named = false;
        for (n, c, ..) in &args.named {
            match params[..n_fixed]
                .iter()
                .position(|(pn, _)| *pn == n.as_str())
            {
                Some(j) if slot[j].is_some() => {
                    return Err(PhpError::uncaught(
                        "Error",
                        format!("Named parameter ${} overwrites previous argument", n),
                        0,
                    ))
                }
                Some(j) => {
                    slot[j] = Some(c.clone());
                    any_fixed_named = true;
                }
                None if variadic => extra_pos.push(c.clone()),
                None => {
                    return Err(PhpError::uncaught(
                        "Error",
                        format!("Unknown named parameter ${}", n),
                        0,
                    ))
                }
            }
        }
        // Materialize through the last bound slot: interior gaps take
        // the param default (an "unknown default" param errors instead),
        // unbound required params throw ArgumentCountError, unbound
        // optional tail params are omitted.
        let last_bound = slot
            .iter()
            .rposition(|s| s.is_some())
            .map(|i| i + 1)
            .unwrap_or(0);
        let mut out: Vec<Cell> = Vec::new();
        for (i, s) in slot.iter().enumerate() {
            match s {
                Some(c) => out.push(c.clone()),
                None if matches!(params[i].1, BDef::Req) => {
                    if any_fixed_named {
                        return Err(PhpError::uncaught(
                            "ArgumentCountError",
                            format!(
                                "{}(): Argument #{} (${}) not passed",
                                name,
                                i + 1,
                                params[i].0
                            ),
                            0,
                        ));
                    }
                    return Err(arity_err(args.cells.len(), false));
                }
                None if matches!(params[i].1, BDef::Unk) && i < last_bound => {
                    return Err(PhpError::uncaught(
                        "ArgumentCountError",
                        format!(
                            "{}(): Argument #{} (${}) must be passed explicitly, because the default value is not known",
                            name,
                            i + 1,
                            params[i].0
                        ),
                        0,
                    ));
                }
                None if i < last_bound => out.push(cell(params[i].1.val())),
                None => break,
            }
        }
        out.extend(extra_pos);
        Ok(out)
    }

    fn fail<T>(&mut self, e: PhpError) -> Result<T, PhpError> {
        self.last_err_file = self.diag_file();
        if let ErrorKind::Uncaught { class } = e.kind {
            // Errors raised mid-const-expr get a pseudo-frame for the
            // constant expression itself (property_initializer_scope_002:
            // `#0 %s(%d): [constant expression]()`).
            let e = if self.class_const_ctx > 0 {
                let fr = format!(
                    "{}({}): [constant expression]()",
                    self.diag_file(),
                    self.cur_line
                );
                let mut frames = e.trace.clone().unwrap_or_default();
                frames.insert(0, fr);
                PhpError {
                    trace: Some(frames),
                    ..e
                }
            } else {
                e
            };
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
            Stmt::Diag { level, msg, line } => {
                self.cur_line = *line;
                let r = match *level {
                    "Warning" => self.warn(msg),
                    "Notice" => self.notice(msg),
                    _ => self.deprecated(msg),
                };
                match r {
                    Ok(()) => Flow::Normal,
                    Err(e) => self.err_flow(e),
                }
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
                        Ok(v) => match self.conv_bytes(&v) {
                            Ok(s) => self.emit_bytes(&s),
                            Err(e) => return self.err_flow(e),
                        },
                        Err(e) => return self.err_flow(e),
                    }
                }
                Flow::Normal
            }
            Stmt::Expr(e) => match e {
                // A lone `$x;` compiles to a dead FREE op in Zend — no
                // undefined-variable warning (first_class_callable_dynamic).
                Expr::Var(n) if self.var_lookup(n).is_none() => Flow::Normal,
                _ => {
                    let base = self.expr_temps.len();
                    // A previous statement's `return $lval` can leave a
                    // stale last_ret_cell pinned to a real storage cell
                    // (inflating its strong_count → `&` in var_dump);
                    // only the current statement may consume it.
                    self.last_ret_cell = None;
                    let r = self.eval(e);
                    match r {
                        Ok(v) => {
                            // A discarded temporary object reaches
                            // refcount 0 here — Zend runs its
                            // __destruct immediately (methods_003
                            // `new bar;`). strong_count 2 = the
                            // statement value + its expr_temps slot.
                            if let Value::Object(o) = &v {
                                if Rc::strong_count(o) == 2
                                    && self
                                        .find_method_in(&o.borrow().class, "__destruct")
                                        .is_some()
                                    && self.mark_destructed(o)
                                {
                                    if let Err(e) = self.method_invoke(
                                        o.clone(),
                                        "__destruct",
                                        CallArgs::empty(),
                                    ) {
                                        self.expr_temps.truncate(base);
                                        return self.err_flow(e);
                                    }
                                }
                            }
                            // Statement end frees expression
                            // temporaries; a dtor exception propagates
                            // through the statement (bug29368_2).
                            match self.sweep_expr_temps(base) {
                                Ok(()) => Flow::Normal,
                                Err(e) => self.err_flow(e),
                            }
                        }
                        // On unwind the live temporaries die in order
                        // before the exception propagates
                        // (bug29368_3).
                        Err(e) => {
                            let _ = self.sweep_expr_temps(base);
                            self.err_flow(e)
                        }
                    }
                }
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
                if let Err(e) = self.decl_type_checks(&d.name, d, None) {
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
                    if let Err(e) =
                        self.decl_type_checks(&fname, &m.decl, Some((&d.name, d.parent.clone())))
                    {
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
                                // Runtime init: an unresolved const is a
                                // catchable Error, not silent NULL
                                // (bug79778).
                                Some(d) => match self.eval_const(d) {
                                    Ok(v) => v,
                                    Err(e) => return self.err_flow(e),
                                },
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
                            if let Some(c) = self.cur().vars.remove(n) {
                                // Removing the last handle runs
                                // __destruct immediately — for a
                                // Callable that also decrefs its bound
                                // $this and captures (closure_005).
                                let v = c.borrow().clone();
                                drop(c);
                                if let Err(e) = self.destruct_dying_value(&v) {
                                    return self.err_flow(e);
                                }
                            }
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
                                    // Binding the catch var is a normal
                                    // assign — a `&`-bound typed ref
                                    // gates it and the TypeError
                                    // propagates out of the try
                                    // (typed_properties_108).
                                    match self.var_set_gated(var, v.clone(), true) {
                                        Ok(_) => result = self.exec_block(&c.body),
                                        Err(e) => result = self.err_flow(e),
                                    }
                                } else {
                                    result = self.exec_block(&c.body);
                                }
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
                    match self.eval_const(e) {
                        Ok(v) => self.define_const(n, v),
                        Err(e) => return self.err_flow(e),
                    }
                }
                Flow::Normal
            }
            Stmt::Declare { name, value } => {
                if name.eq_ignore_ascii_case("strict_types")
                    && matches!(self.eval(value), Ok(Value::Int(1)))
                {
                    self.strict_files.insert(self.cur_file.clone());
                }
                Flow::Normal
            }
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
            // Ancestor NAME match: a parent still mid-registration (an
            // autoload cycle: `new C` → C's sig check autoloads D →
            // `class D extends C`) isn't in `classes` yet, but its name
            // is a known ancestor (abstract_method_9).
            if c.decl
                .parent
                .as_deref()
                .is_some_and(|p| p.trim_start_matches('\\').eq_ignore_ascii_case(&lname))
            {
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
            let nxt = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
            cur = nxt;
        }
        false
    }

    /// File diagnostics attribute to: the executing frame's declaring
    /// file, else the file currently being included/run (warnings inside
    /// autoloaded/library code report the library file, not the caller).
    fn diag_file(&self) -> String {
        self.stack
            .last()
            .map(|f| f.file.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.cur_file.clone())
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
                                if let Some(f) = self.readonly_ref_error(&c) {
                                    return f;
                                }
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
                    // .iter() skips tombstoned buckets — a value-foreach
                    // never sees shifted/unset elements.
                    rc.borrow()
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
                            if let Some(f) = self.readonly_ref_error(&c) {
                                return f;
                            }
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
                        let it_obj =
                            match self.method_invoke(cur.clone(), "getIterator", CallArgs::empty())
                            {
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
                    // `function &gen()` generators DO support
                    // `foreach .. as &$v` — their yields are cells
                    // (typed_properties_033/034). An ArrayIterator's
                    // entries are already cells too (113/115).
                    let gen_byref = match &o.borrow().internal {
                        Some(ObjectInternal::Generator(st)) => st.borrow().by_ref,
                        Some(ObjectInternal::ArrayIter { .. }) => true,
                        _ => false,
                    };
                    if matches!(val, ForeachTarget::ByRef(_)) && !gen_byref {
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
                    // `&$val` binds the prop cell — register it so
                    // writes stay type-gated (typed_properties_045).
                    if matches!(val, ForeachTarget::ByRef(_) | ForeachTarget::Lvalue(_)) {
                        if let Some((pd, dcls)) = self.decl_prop(&o, &dname) {
                            if let Some(tys) = &pd.ty {
                                let p = Rc::as_ptr(&c) as usize;
                                self.typed_slots.insert(
                                    p,
                                    (
                                        c.clone(),
                                        tys.clone(),
                                        dcls.name().to_string(),
                                        dname.clone(),
                                    ),
                                );
                                let sk = self
                                    .obj_prop_key(&o, &dname)
                                    .unwrap_or_else(|| dname.clone());
                                self.slot_anchor
                                    .insert(p, SlotAnchor::Obj(Rc::downgrade(&o), sk));
                                self.ref_cells.insert(p);
                            }
                        }
                    }
                    if let Some(ForeachKey::Var(kn)) = key {
                        self.var_set(kn, Value::str(n.clone()));
                    }
                    match val {
                        ForeachTarget::Var(n) => self.var_set(n, c.borrow().clone()),
                        ForeachTarget::ByRef(n) => {
                            self.ref_cells.insert(Rc::as_ptr(&c) as usize);
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
            Value::Callable(_) => {
                // A Closure is an object with no iterable props —
                // foreach yields nothing (closure_028).
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
        let f = self.exec_foreach_iter_loop(it.clone(), key, val, body);
        // The iterator's temp dies with the foreach — a `new` captured
        // only by the iteration frees here, not at statement end
        // (typed_properties_115: its prop cells must unalias before a
        // later var_dump counts holders).
        if Rc::strong_count(&it) == 2 && self.expr_temps.iter().any(|o| Rc::ptr_eq(o, &it)) {
            self.expr_temps.retain(|o| !Rc::ptr_eq(o, &it));
            let key = Rc::as_ptr(&it) as usize;
            if !self.destructed.contains_key(&key)
                && self
                    .find_method_in(&it.borrow().class, "__destruct")
                    .is_some()
            {
                self.mark_destructed(&it);
                if let Err(e) = self.method_invoke(it.clone(), "__destruct", CallArgs::empty()) {
                    return self.err_flow(e);
                }
            }
        }
        f
    }

    fn exec_foreach_iter_loop(
        &mut self,
        it: Rc<RefCell<PhpObject>>,
        key: &Option<ForeachKey>,
        val: &ForeachTarget,
        body: &[Stmt],
    ) -> Flow {
        if let Err(e) = self.method_invoke(it.clone(), "rewind", CallArgs::empty()) {
            return self.err_flow(e);
        }
        loop {
            let ok = self
                .method_invoke(it.clone(), "valid", CallArgs::empty())
                .map(|v| v.is_truthy())
                .unwrap_or(false);
            if !ok {
                break;
            }
            // PHP calls current() before key() on each iteration.
            let v = self
                .method_invoke(it.clone(), "current", CallArgs::empty())
                .unwrap_or(Value::Null);
            if let Some(ForeachKey::Var(kn)) = key {
                let k = self
                    .method_invoke(it.clone(), "key", CallArgs::empty())
                    .unwrap_or(Value::Null);
                self.var_set(kn, k);
            }
            match val {
                ForeachTarget::Var(n) => self.var_set(n, v),
                ForeachTarget::ByRef(n) => {
                    // A by-ref generator's current() is the yielded
                    // cell itself — bind to it directly. An
                    // ArrayIterator binds the backing entry cell —
                    // prop cells write through the typed gate
                    // (typed_properties_113/114).
                    let c = match &it.borrow().internal {
                        Some(ObjectInternal::Generator(st)) => {
                            let st = st.borrow();
                            st.items
                                .get(st.pos)
                                .map(|(_, c)| c.clone())
                                .unwrap_or_else(|| cell(v.clone()))
                        }
                        Some(ObjectInternal::ArrayIter { arr, pos, .. }) => arr
                            .borrow()
                            .entries
                            .get(*pos)
                            .map(|(_, c)| c.clone())
                            .unwrap_or_else(|| cell(v.clone())),
                        _ => cell(v),
                    };
                    if let Some(f) = self.readonly_ref_error(&c) {
                        return f;
                    }
                    self.ref_cells.insert(Rc::as_ptr(&c) as usize);
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
            if let Err(e) = self.method_invoke(it.clone(), "next", CallArgs::empty()) {
                return self.err_flow(e);
            }
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
                let mut s: Vec<u8> = Vec::new();
                for p in parts {
                    match p {
                        StringPart::Lit(t) => s.extend_from_slice(t),
                        StringPart::Var(name) => {
                            let v = self.var_get(name)?;
                            let cs = self.conv_bytes(&v)?;
                            s.extend_from_slice(&cs);
                        }
                        StringPart::Expr(src) => {
                            let (expr, _) = parser::parse_expr_src(src)
                                .map_err(|e| PhpError::parse(e.message, e.line))?;
                            let v = self.eval(&expr)?;
                            s.extend_from_slice(&self.conv_bytes(&v)?);
                        }
                        StringPart::DollarBraceExpr(src) => {
                            // `${expr}` — deprecated variable-variable
                            // interpolation; its deprecation + inner
                            // diagnostics already emitted at lex time
                            // (heredoc_nowdoc/flexible-heredoc-complex-*).
                            let (expr, _) = parser::parse_expr_src(src)
                                .map_err(|e| PhpError::parse(e.message, e.line))?;
                            let nv = self.eval(&expr)?;
                            let name = self.conv_str(&nv)?;
                            let v = self.var_get(&name)?;
                            s.extend_from_slice(&self.conv_bytes(&v)?);
                        }
                    }
                }
                Ok(Value::bytes(s))
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
                        self.ref_cells.insert(Rc::as_ptr(&c) as usize);
                        arr.is_ref = true;
                        match k {
                            Some(ke) => {
                                let kv = self.eval(ke)?;
                                self.check_offset_key(&kv)?;
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
                            self.check_offset_key(&kv)?;
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
            Expr::Fcc(inner) => self.fcc(inner),
            Expr::Unpack(_) | Expr::FccMark => {
                self.fail(PhpError::fatal("argument unpacking/FCC outside of call", 0))
            }
            Expr::Index { e, i } => self.index_read(e, i.as_deref()),
            Expr::PreInc(t) => self.incdec(t, 1, false),
            Expr::PreDec(t) => self.incdec(t, -1, false),
            Expr::PostInc(t) => self.incdec(t, 1, true),
            Expr::PostDec(t) => self.incdec(t, -1, true),
            Expr::Isset(args) => {
                self.silence += 1;
                let mut ok = true;
                for a in args {
                    match self.isset_val_mode(a, 0) {
                        Ok(Some(_)) => {}
                        Ok(None) => {
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
                let v = self.isset_val_mode(e, 1);
                self.silence -= 1;
                match v {
                    Ok(Some(v)) => Ok(Value::Bool(!v.is_truthy())),
                    Ok(None) => Ok(Value::Bool(true)),
                    Err(e) => Err(e),
                }
            }
            Expr::Print(e) => {
                let v = self.eval(e)?;
                let s = self.conv_str(&v)?;
                self.emit(&s);
                Ok(Value::Int(1))
            }
            Expr::Yield { key, val } => {
                // `function &gen()` yields the value's own cell so
                // `foreach ($gen as &$v)` writes back into it
                // (typed_properties_033/034).
                let by_ref = self.stack.last().map(|f| f.ret_by_ref).unwrap_or(false);
                let vc = match val {
                    Some(e) if by_ref => self.eval_cell(e)?,
                    Some(e) => cell(self.eval(e)?),
                    None => cell(Value::Null),
                };
                let k = match key {
                    Some(e) => self.eval(e)?,
                    None => {
                        if self.gen_sink.is_none() {
                            return self.fail(PhpError::fatal(
                                "The \"yield\" expression can only be used inside a function",
                                self.cur_line,
                            ));
                        }
                        let i = self.gen_auto;
                        self.gen_auto += 1;
                        Value::Int(i)
                    }
                };
                match &self.gen_sink {
                    Some(sink) => {
                        sink.borrow_mut().push((k, vc));
                        Ok(self.gen_sends.pop_front().unwrap_or(Value::Null))
                    }
                    None => self.fail(PhpError::fatal(
                        "The \"yield\" expression can only be used inside a function",
                        self.cur_line,
                    )),
                }
            }
            Expr::YieldFrom(e) => {
                let v = self.eval(e)?;
                let sink = self.gen_sink.clone();
                match sink {
                    Some(sink) => {
                        // `yield from` splices the inner keys verbatim —
                        // duplicates and all — and doesn't touch the
                        // keyless auto counter.
                        let items = self.yield_from_collect(&v)?;
                        sink.borrow_mut().extend(items);
                        Ok(Value::Null)
                    }
                    None => self.fail(PhpError::fatal(
                        "The \"yield from\" expression can only be used inside a function",
                        self.cur_line,
                    )),
                }
            }
            Expr::Exit(arg) => {
                let code = if let Some(a) = arg {
                    match self.eval(a)? {
                        Value::Int(i) => i as i32,
                        Value::Str(s) => {
                            self.emit_bytes(&s);
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
                    self.cur_file.clone()
                } else {
                    c.decl.file.clone()
                };
                // PHP 8.5 names a closure after its enclosing scope:
                // `{closure:Class::m():L}` inside a method,
                // `{closure:fn():L}` inside a function, `{closure:FILE:L}`
                // at top level, and `{closure:{closure:...}:L}` when
                // nested (iterable_003, closure_065).
                let enclosing = self
                    .stack
                    .last()
                    .map(|f| {
                        if f.fn_name.is_empty() || f.fn_name == "{main}" {
                            String::new()
                        } else if f.fn_name.starts_with("{closure:") {
                            f.fn_name.clone()
                        } else {
                            match f.trait_origin.clone().or_else(|| {
                                f.decl_class
                                    .as_ref()
                                    .or(f.scope_class.as_ref())
                                    .map(|c| c.name().to_string())
                            }) {
                                Some(o) => format!("{}::{}", o, f.fn_name),
                                None => f.fn_name.clone(),
                            }
                        }
                    })
                    .unwrap_or_default();
                let fname = if enclosing.is_empty() {
                    format!("{{closure:{}:{}}}", cfile, c.decl.line)
                } else if enclosing.starts_with('{') {
                    format!("{{closure:{}:{}}}", enclosing, c.decl.line)
                } else {
                    format!("{{closure:{}():{}}}", enclosing, c.decl.line)
                };
                // The closure's decl.file = the file currently executing
                // — __FILE__/__DIR__ inside it must resolve to where it
                // was defined, not where it is later invoked (autoloaders).
                let mut decl = c.decl.clone();
                if decl.file.is_empty() {
                    decl.file = cfile;
                }
                decl.name = fname.clone();
                self.decl_type_checks(&fname, &decl, None)?;
                let mut captures = Vec::new();
                if c.arrow {
                    // `fn` captures whole scope by value.
                    let f = self.stack.last().unwrap_or(&self.globals);
                    for (n, cellv) in f.vars.iter() {
                        captures.push((n.clone(), cell(cellv.borrow().clone()), false));
                    }
                } else {
                    for (n, by_ref) in &c.uses {
                        let cap = if *by_ref {
                            self.var_cell(n)
                        } else {
                            match self.var_cell_opt(n) {
                                Some(c) => cell(c.borrow().clone()),
                                // `use ($x)` on an undefined var warns
                                // and captures null; `use (&$x)` binds
                                // silently (closure_027).
                                None => {
                                    self.warn(&format!("Undefined variable ${}", n))?;
                                    cell(Value::Null)
                                }
                            }
                        };
                        captures.push((n.clone(), cap, *by_ref));
                    }
                }
                // `static function` never binds $this; `static::`
                // keeps the creating frame's late-bound class
                // (closure_049-052).
                let is_static = c.is_static;
                Ok(Value::Callable(self.new_callable(PhpCallable {
                    id: std::cell::Cell::new(0),
                    kind: CallableKind::Closure(Rc::new(decl)),
                    captures,
                    this_obj: if is_static {
                        None
                    } else {
                        self.stack.last().and_then(|f| f.this_obj.clone())
                    },
                    scope_class: self.stack.last().and_then(|f| f.scope_class.clone()),
                    called_class: self.stack.last().and_then(|f| f.called_class.clone()),
                    is_static,
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
                let argvals =
                    self.arg_cells(args, &params, &format!("{}::__construct()", name), false)?;
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
                // Non-string names are a catchable Error
                // (call_static_004).
                let cls = self.class_of(class)?;
                let nv = self.eval(name)?;
                let n = match nv {
                    Value::Str(s) => Self::nul_trunc(&crate::value::lossy(&s)),
                    _ => {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Method name must be a string",
                            0,
                        ))
                    }
                };
                let fwd = matches!(&**class, Expr::Const(_) | Expr::Str(_) | Expr::AnonClass(_));
                let argvals =
                    self.arg_cells(args, &[], &format!("{}::{{closure}}()", cls.name()), false)?;
                self.static_invoke_vis(cls, &n, argvals, None, fwd)
            }
            Expr::ClassConst { class, name } => self.class_const(class, name),
            Expr::Clone(e) => {
                let v = self.eval(e)?;
                match v {
                    Value::Object(o) => {
                        let ob = o.borrow();
                        let mut props = HashMap::new();
                        // References survive clone — the clone's prop
                        // shares the same zval and keeps the typed gate
                        // (typed_properties_081).
                        let mut shared: Vec<(String, Cell)> = Vec::new();
                        for (k, c) in ob.props.iter() {
                            if self.ref_cells.contains(&(Rc::as_ptr(c) as usize)) {
                                shared.push((k.clone(), c.clone()));
                            }
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
                            unset_props: ob.unset_props.clone(),
                        };
                        drop(ob);
                        let nv = Value::Object(self.alloc_obj(new_obj));
                        // Rebind shared cells into the clone and give it
                        // its own slot owner so type checks keep
                        // resolving against the clone's prop.
                        if let Value::Object(no) = &nv {
                            for (k, c) in shared {
                                let ptr = Rc::as_ptr(&c) as usize;
                                no.borrow_mut().props.insert(k.clone(), c);
                                if let Some(a) = self.slot_anchor.get_mut(&ptr) {
                                    if let SlotAnchor::Obj(w, sk) = a {
                                        if sk == &k
                                            && w.upgrade().is_some_and(|u| Rc::ptr_eq(&u, &o))
                                        {
                                            *a = SlotAnchor::Obj(Rc::downgrade(no), k.clone());
                                        }
                                    }
                                }
                                if let Some(owners) = self.slot_owners.get_mut(&ptr) {
                                    for (_, _, _, a) in owners.iter_mut() {
                                        // Repoint owners that anchored
                                        // the SOURCE object's prop to
                                        // the clone's slot.
                                        if let SlotAnchor::Obj(w, sk) = a {
                                            if sk == &k
                                                && w.upgrade().is_some_and(|u| Rc::ptr_eq(&u, &o))
                                            {
                                                *a = SlotAnchor::Obj(Rc::downgrade(no), k.clone());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Value::Object(no) = &nv {
                            let ncls = no.borrow().class.clone();
                            if self.find_method_in(&ncls, "__clone").is_some() {
                                self.method_invoke(no.clone(), "__clone", CallArgs::empty())?;
                            }
                        }
                        Ok(nv)
                    }
                    Value::Callable(c) => {
                        // `clone $closure` — fresh handle id; captured
                        // cells stay shared so by-ref uses still alias
                        // the outer var (closure_024).
                        let nc = self.new_callable((*c).clone());
                        Ok(Value::Callable(nc))
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
                    // A closure literal IS a Closure object.
                    Value::Callable(_) => Ok(Value::Bool(cname.eq_ignore_ascii_case("closure"))),
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
        // __FILE__/__DIR__ resolve against the DECLARING file of the
        // code that runs them — a closure defined in vendor/autoload.php
        // sees that file's dir even when invoked from elsewhere
        // (composer-style PSR-4 autoloaders depend on this).
        let decl_file = self
            .decl_file_ctx
            .clone()
            .or_else(|| {
                self.stack
                    .last()
                    .map(|f| f.file.clone())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| self.cur_file.clone());
        match m {
            MagicConst::Line => Value::Int(self.cur_line as i64),
            MagicConst::File => Value::str(decl_file.clone()),
            MagicConst::Dir => Value::str(
                std::path::Path::new(&decl_file)
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
                    // Inside a closure __METHOD__ is the closure's Zend
                    // name (`{closure:C::m():L}` — closure_033).
                    Some(f) if f.fn_name.starts_with("{closure:") => Value::str(f.fn_name.clone()),
                    Some(f) => {
                        // `T::m` when the method was merged from trait T
                        // (`__METHOD__` names the trait; `__CLASS__`
                        // names the consuming class).
                        let owner = f
                            .trait_origin
                            .clone()
                            .or_else(|| f.scope_class.as_ref().map(|c| c.name().to_string()));
                        match owner {
                            Some(o) => Value::str(format!("{}::{}", o, f.fn_name)),
                            None => Value::str(""),
                        }
                    }
                    None => Value::str(""),
                }
            }
            MagicConst::Class => Value::str(
                self.stack
                    .last()
                    .and_then(|f| f.scope_class.as_ref().map(|c| c.name().to_string()))
                    .or_else(|| self.const_self.as_ref().map(|c| c.name().to_string()))
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
            // `__TRAIT__` names the trait a method was merged from;
            // "" inside class-defined methods and at top level.
            MagicConst::Trait => Value::str(
                self.stack
                    .last()
                    .and_then(|f| f.trait_origin.clone())
                    .unwrap_or_default(),
            ),
        }
    }

    /// Whether the expr is "set" — for isset()/empty() without warnings.
    /// `isset()` semantics returning the read value — `Some(v)` when the
    /// operand exists and isn't null. `??`/`empty` consume the value
    /// directly so calls and prop-getters evaluate exactly once.
    /// isset/empty/?? property semantics (mode):
    ///  0 = isset()  — last segment's __isset answers directly, no
    ///                 fetch; absent prop without __isset is just not
    ///                 set — __get never fires (bug44899).
    ///  1 = empty()  — __isset gates; truthy result then reads via
    ///                 __get for the falsy check; absent prop without
    ///                 __isset is empty, still no __get.
    ///  2 = ?? / intermediate segment — __isset gates then __get
    ///                 fetches; without __isset the read runs __get
    ///                 directly (bug71359).
    fn isset_val_mode(&mut self, e: &Expr, mode: u8) -> Result<Option<Value>, PhpError> {
        match e {
            Expr::Var(n) => Ok(match self.var_cell_opt(n) {
                Some(c) => match &*c.borrow() {
                    Value::Null => None,
                    v => Some(v.clone()),
                },
                None => None,
            }),
            Expr::Index { e, i } => {
                // `isset($this->uninitTyped['k'])` and `$x ?? y` must not
                // throw on uninitialized typed properties. The base
                // chains through isset semantics — absent segments
                // short-circuit without __get (bug71359).
                self.silence += 1;
                let base = self.isset_val_mode(e, 2);
                self.silence -= 1;
                let base = match base {
                    Ok(Some(b)) => b,
                    Ok(None) => return Ok(None),
                    Err(err) if matches!(err.kind, ErrorKind::Throw) => {
                        if err
                            .message
                            .ends_with("must not be accessed before initialization")
                        {
                            return Ok(None);
                        }
                        return Err(err);
                    }
                    Err(_) => return Ok(None),
                };
                let key = match i {
                    Some(k) => self.eval(k)?,
                    None => return Ok(None),
                };
                self.check_offset_key(&key)?;
                match base {
                    Value::Array(a) => Ok(match a.borrow().get(&to_key(&key)) {
                        Some(v) => match v {
                            Value::Null => None,
                            _ => Some(v.clone()),
                        },
                        None => None,
                    }),
                    Value::Str(s) => {
                        let i = key.to_int();
                        Ok(if i >= 0 && (i as usize) < s.len() {
                            Some(Value::bytes(vec![s[i as usize]]))
                        } else {
                            None
                        })
                    }
                    Value::Object(o) => {
                        if self.obj_is_a(&o, "ArrayAccess") {
                            match self.method_invoke(
                                o.clone(),
                                "offsetExists",
                                CallArgs::positional(vec![cell(key.clone())]),
                            ) {
                                // isset() consults offsetExists alone —
                                // offsetGet is only chained by ??/empty
                                // (bug31683).
                                Ok(v) if v.is_truthy() && mode == 0 => Ok(Some(Value::Bool(true))),
                                Ok(v) if v.is_truthy() => {
                                    match self.method_invoke(
                                        o,
                                        "offsetGet",
                                        CallArgs::positional(vec![cell(key)]),
                                    ) {
                                        Ok(v) => Ok(if matches!(v, Value::Null) {
                                            None
                                        } else {
                                            Some(v)
                                        }),
                                        Err(e) => Err(e),
                                    }
                                }
                                Ok(_) => Ok(None),
                                Err(e) => Err(e),
                            }
                        } else {
                            Ok(None)
                        }
                    }
                    _ => Ok(None),
                }
            }
            Expr::Prop {
                obj,
                name,
                nullsafe,
            } => {
                // Missing/inaccessible props consult __isset first
                // (bug63462, bug44899); a re-entrant isset inside
                // __isset hits real storage only. The base evaluates
                // through isset semantics too: each segment of a chain
                // tests via __isset, then fetches via __get only when
                // set — never triggering __get on an absent segment
                // (bug71359).
                let pn = self.prop_name(name)?;
                let ov = match self.isset_val_mode(obj, 2)? {
                    Some(v) => v,
                    None => return Ok(None),
                };
                if let Value::Callable(_) = &ov {
                    // Props on a Closure warn like undeclared members
                    // of the real Closure class (closure_031).
                    self.check_prop_name(&pn)?;
                    self.warn(&format!("Undefined property: Closure::${}", pn))?;
                    return Ok(None);
                }
                if let Value::Object(o) = &ov {
                    let cls = o.borrow().class.clone();
                    // A declared prop checks its real slot unless it
                    // was unset() — only then does __isset fire
                    // (typed_properties_magic_set vs bug63462).
                    let was_unset = {
                        let ob = o.borrow();
                        ob.unset_props.contains(&pn)
                            || ob
                                .unset_props
                                .iter()
                                .any(|k| k.ends_with(&format!("\0{}", pn)))
                    };
                    let declared_live = self.decl_prop(o, &pn).is_some()
                        && !was_unset
                        && self.prop_visible(&cls, &pn);
                    let missing = match self.obj_prop_key(o, &pn) {
                        Some(_) => !self.prop_visible(&cls, &pn),
                        None => true,
                    } && !declared_live;
                    if missing {
                        if self.find_method_in(&cls, "__isset").is_some() {
                            let gkey = (Rc::as_ptr(o) as usize, 2u8, pn.clone());
                            if self.magic_guards.insert(gkey.clone()) {
                                let res = self.method_invoke(
                                    o.clone(),
                                    "__isset",
                                    CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                                );
                                self.magic_guards.remove(&gkey);
                                if !res?.is_truthy() {
                                    return Ok(None);
                                }
                                // isset() takes __isset's answer — no
                                // fetch (bug44899). empty()/?? then
                                // read through __get with the SAME
                                // bound name (bug75420).
                                if mode == 0 {
                                    return Ok(Some(Value::Bool(true)));
                                }
                                self.silence += 1;
                                let v = self.prop_read_value(ov.clone(), &pn, false);
                                self.silence -= 1;
                                return match v {
                                    Ok(v) => Ok(if matches!(v, Value::Null) {
                                        None
                                    } else {
                                        Some(v)
                                    }),
                                    Err(e)
                                        if matches!(e.kind, ErrorKind::Throw)
                                            && e.message.ends_with(
                                                "must not be accessed before initialization",
                                            ) =>
                                    {
                                        Ok(None)
                                    }
                                    Err(e) if matches!(e.kind, ErrorKind::Throw) => Err(e),
                                    Err(_) => Ok(None),
                                };
                            } else {
                                return Ok(None);
                            }
                        }
                        // No __isset: isset()/empty() see an absent
                        // prop — __get stays quiet; ?? still reads it
                        // (bug71359).
                        if mode != 2 {
                            return Ok(None);
                        }
                    }
                }
                self.check_prop_name(&pn)?;
                self.silence += 1;
                let v = self.prop_read_value(ov.clone(), &pn, *nullsafe);
                self.silence -= 1;
                match v {
                    Ok(v) => Ok(if matches!(v, Value::Null) {
                        None
                    } else {
                        Some(v)
                    }),
                    // A hooked get runs inside isset — its exceptions
                    // escape (write-only prop throws through the
                    // try/catch, not `false`).
                    Err(e) if matches!(e.kind, ErrorKind::Throw) => {
                        // Uninitialized typed prop reads still mean
                        // "not set" for isset — hook Errors escape.
                        if e.message
                            .ends_with("must not be accessed before initialization")
                        {
                            Ok(None)
                        } else {
                            Err(e)
                        }
                    }
                    Err(_) => Ok(None),
                }
            }
            Expr::StaticProp { class, name } => {
                // Scoped isset — `isset(A::$priv)` is false when the
                // static exists but is invisible to the current scope
                // (closure_041-046, disallows_*).
                let pn = match self.prop_name(name) {
                    Ok(pn) => pn,
                    Err(_) => return Ok(None),
                };
                let cls = match self.class_of(class) {
                    Ok(c) => c,
                    Err(_) => return Ok(None),
                };
                self.statics_init(&cls);
                let v = cls.statics.borrow().get(&pn).map(|c| c.borrow().clone());
                let ok = match self.find_static_prop_decl(&cls, &pn) {
                    Some((pd, dcls)) => match pd.visibility {
                        crate::ast::Visibility::Public => true,
                        _ => {
                            let scope = self
                                .stack
                                .last()
                                .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()))
                                .map(|s| s.name().to_string());
                            match (pd.visibility, scope) {
                                (crate::ast::Visibility::Private, Some(s)) => s == dcls.name(),
                                (crate::ast::Visibility::Protected, Some(s)) => {
                                    self.is_a_str(&s, dcls.name()) || self.is_a_str(dcls.name(), &s)
                                }
                                _ => false,
                            }
                        }
                    },
                    None => true,
                };
                match (v, ok) {
                    (Some(v), true) if !matches!(v, Value::Null) => Ok(Some(v)),
                    _ => Ok(None),
                }
            }
            _ => {
                let v = self.eval(e)?;
                Ok(if matches!(v, Value::Null) {
                    None
                } else {
                    Some(v)
                })
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

    /// Byte-faithful string coercion — strings pass through untouched;
    /// other scalars go through conv_str (their output is ASCII anyway).
    fn conv_bytes(&mut self, v: &Value) -> Result<Vec<u8>, PhpError> {
        if let Value::Str(s) = v {
            return Ok(s.to_vec());
        }
        Ok(self.conv_str(v)?.into_bytes())
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
                // __toString may be inherited — walk the chain, not just
                // the leaf decl (AbstractString defines it for
                // UnicodeString).
                if self.find_method_in(&class, "__tostring").is_some() {
                    let r = self.method_invoke(o.clone(), "__tostring", CallArgs::empty())?;
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
                _ => {
                    let c = self.eval_cell(value)?;
                    // A `=&` source that is itself a typed-prop slot
                    // carries that prop's declared type into the
                    // conflict check (typed_properties_068/076).
                    let decl = match value {
                        Expr::Prop { obj, name, .. } => {
                            let ov = self.eval(obj)?;
                            match (&ov, self.prop_name(name)) {
                                (Value::Object(o), Ok(pn)) => {
                                    self.decl_prop(o, &pn).map(|(pd, dc)| {
                                        let sk =
                                            self.obj_prop_key(o, &pn).unwrap_or_else(|| pn.clone());
                                        (
                                            pd.ty.clone(),
                                            dc.name().to_string(),
                                            pn,
                                            SlotAnchor::Obj(Rc::downgrade(o), sk),
                                        )
                                    })
                                }
                                _ => None,
                            }
                        }
                        Expr::StaticProp { class, name } => {
                            match (self.member_class_of(class), self.prop_name(name)) {
                                (Ok((cls, _)), Ok(pn)) => {
                                    self.find_static_prop_decl(&cls, &pn).map(|(pd, dc)| {
                                        (
                                            pd.ty.clone(),
                                            dc.name().to_string(),
                                            pn.clone(),
                                            SlotAnchor::Statics(dc.name().to_string(), pn),
                                        )
                                    })
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    };
                    if let Some((Some(tys), dcn, dpn, anc)) = decl {
                        let p = Rc::as_ptr(&c) as usize;
                        self.typed_slots.insert(p, (c.clone(), tys, dcn, dpn));
                        self.slot_anchor.insert(p, anc);
                    }
                    c
                }
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
        #[allow(clippy::large_enum_variant)]
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
                // Prop-chain index: the container resolves early; the dim
                // expr's own value is always the key — `$o->a[${f()}]` is a
                // variable-variable, not a register quirk
                // (engine_assignExecutionOrder_001 reads $name that way).
                match self.eval_cell(e) {
                    Ok(c) => {
                        let key = match i.as_deref() {
                            Some(ie) => self.eval(ie).ok(),
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
                    // `??=` reads with isset() semantics (no undefined
                    // warnings); every other compound op warns
                    // (typed_properties_103).
                    let quiet = op == "??=";
                    if quiet {
                        self.silence += 1;
                    }
                    let c = self.eval(target);
                    if quiet {
                        self.silence -= 1;
                    }
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
                newv = self.store_prop(ov, &pn, newv.clone())?;
            }
            Late::PropStr { ov, pn } => {
                newv = self.store_prop(ov, &pn, newv.clone())?;
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
                    match self.method_invoke(
                        o,
                        "offsetSet",
                        CallArgs::positional(vec![cell(kv), cell(newv.clone())]),
                    ) {
                        Ok(_) => {}
                        Err(e) => return Err(e),
                    }
                    return Ok(newv);
                }
                if matches!(&*base.borrow(), Value::Null) {
                    self.auto_init_gate(&base)?;
                    let mut b = base.borrow_mut();
                    if matches!(*b, Value::Null) {
                        *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
                    }
                }
                {
                    let mut b = base.borrow_mut();
                    if let Value::Array(rc) = &mut *b {
                        // Shared zend_array: CoW-separate before the
                        // write — an overloaded prop's fetched temp
                        // must not write through into the getter's
                        // backing store (bug32660).
                        if Rc::strong_count(rc) > 1 && !rc.borrow().is_ref {
                            let fresh = rc.borrow().clone();
                            *b = Value::Array(Rc::new(RefCell::new(fresh)));
                        }
                        let rc = match &*b {
                            Value::Array(rc) => rc.clone(),
                            _ => unreachable!(),
                        };
                        let mut arr = rc.borrow_mut();
                        if append {
                            arr.push(newv.clone());
                        } else {
                            let key = key.map(|k| to_key(&k)).unwrap_or(to_key(&newv));
                            arr.set(key, newv.clone());
                        }
                    }
                }
            }
            Late::Static { class, pn } => {
                let c = self.static_prop_named(&class, &pn)?;
                // Static prop writes coerce to the declared type like
                // instance props (typed_properties_023).
                if let Ok((cls, _)) = self.member_class_of(&class) {
                    if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &pn) {
                        newv = self.prop_typed_write_check(&pd, &dcls, newv)?;
                    }
                }
                let nv = self.typed_slot_store(&c, newv.clone())?;
                *c.borrow_mut() = nv;
            }
            Late::Keyed { e, keys } => {
                newv = self.assign_index_path(&e, &keys, newv)?;
            }
            Late::None => match target_cell {
                Some(c) => {
                    let nv = self.typed_slot_store(&c, newv.clone())?;
                    *c.borrow_mut() = nv;
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
        let (c, was_ref) = self.eval_call_cell_inner(e)?;
        if was_ref {
            self.ref_cells.insert(Rc::as_ptr(&c) as usize);
        }
        Ok((c, was_ref))
    }

    fn eval_call_cell_inner(&mut self, e: &Expr) -> Result<(Cell, bool), PhpError> {
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
        // `=&` creates Zend's IS_REFERENCE — writes through it say
        // "a reference held by property", not "property" (034/078).
        self.ref_cells.insert(Rc::as_ptr(&src) as usize);
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
                        // `=&` into an overloaded prop (missing slot +
                        // __get) still fetches through __get — then the
                        // indirect-modification notice and the
                        // cannot-assign-by-reference Error (bug32660).
                        {
                            let was_unset = {
                                let ob = o.borrow();
                                ob.unset_props.contains(&pn)
                                    || ob
                                        .unset_props
                                        .iter()
                                        .any(|k| k.ends_with(&format!("\0{}", pn)))
                            };
                            let cls = o.borrow().class.clone();
                            let declared_live = self.decl_prop(o, &pn).is_some()
                                && !was_unset
                                && self.prop_visible(&cls, &pn);
                            let inaccessible = match self.obj_prop_key(o, &pn) {
                                Some(_) => !self.prop_visible(&cls, &pn),
                                None => true,
                            };
                            if inaccessible
                                && !declared_live
                                && self.find_method_in(&cls, "__get").is_some()
                            {
                                let gkey = (Rc::as_ptr(o) as usize, 0u8, pn.clone());
                                if self.magic_guards.insert(gkey.clone()) {
                                    let res = self.method_invoke(
                                        o.clone(),
                                        "__get",
                                        CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                                    );
                                    self.magic_guards.remove(&gkey);
                                    res?;
                                }
                                self.notice(&format!(
                                    "Indirect modification of overloaded property {}::${} has no effect",
                                    cls.name(),
                                    pn
                                ))?;
                                let v = self.exception(
                                    "Error",
                                    "Cannot assign by reference to overloaded object",
                                );
                                let e = self.throw(v);
                                return self.fail(e);
                            }
                        }
                        // `=&` installs the source cell as the prop's
                        // slot itself — later writes through either name
                        // hit the same storage; a missing dynamic prop
                        // materializes a real slot (oss-fuzz-382922236).
                        // The array itself stays unmarked — zend's ref
                        // is a property of the zval, not the array; a
                        // later value-copy still CoW-separates
                        // (bug39775).
                        // Binding a ref into a TYPED prop validates the
                        // source (076/068 conflict); the shared cell
                        // then stays gated through typed_slots (071).
                        let mut merged: Option<Vec<String>> = None;
                        let mut owner_ty: Option<Vec<String>> = None;
                        if let Some((pd, dcls)) = self.decl_prop(o, &pn) {
                            if let Some(pt) = &pd.ty {
                                owner_ty = Some(pt.clone());
                                merged = Some(self.bind_typed_check(&pd, &dcls, &src)?);
                            }
                        }
                        let key = self.obj_prop_key(o, &pn).unwrap_or_else(|| pn.clone());
                        let mut ob = o.borrow_mut();
                        if !ob.prop_order.contains(&key) {
                            ob.prop_order.push(key.clone());
                        }
                        ob.props.insert(key.clone(), src.clone());
                        drop(ob);
                        if let (Some(m), Some((_, dcls))) = (merged, self.decl_prop(o, &pn)) {
                            let sptr = Rc::as_ptr(&src) as usize;
                            // The FIRST owner is the ref's holder for
                            // "held by property X of type T" messages —
                            // later merges only narrow slot_merged.
                            self.typed_slots.entry(sptr).or_insert_with(|| {
                                (src.clone(), m.clone(), dcls.name().to_string(), pn.clone())
                            });
                            self.slot_anchor
                                .entry(sptr)
                                .or_insert_with(|| SlotAnchor::Obj(Rc::downgrade(o), key.clone()));
                            self.slot_merged.insert(sptr, m);
                            // Owners keep their *declared* type — a
                            // shared write must satisfy each and yield
                            // one consistent result.
                            let ot = owner_ty.unwrap_or_default();
                            let dcn = dcls.name().to_string();
                            let owners = self.slot_owners.entry(sptr).or_default();
                            if !owners
                                .iter()
                                .any(|(_, cn, pn2, _)| cn == &dcn && pn2 == &pn)
                            {
                                owners.push((
                                    ot,
                                    dcn,
                                    pn.clone(),
                                    SlotAnchor::Obj(Rc::downgrade(o), key.clone()),
                                ));
                            }
                        }
                        return Ok(());
                    }
                }
                let c = self.eval_cell(target)?;
                *c.borrow_mut() = src.borrow().clone();
                Ok(())
            }
            Expr::StaticProp { class, name } => {
                let pn = self.prop_name(name)?;
                let (cls, _t) = self.member_class_of(class)?;
                self.statics_init(&cls);
                let mut merged: Option<Vec<String>> = None;
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &pn) {
                    if pd.ty.is_some() {
                        // Binding a ref to a typed static validates the
                        // source (catchable TypeError — 068/069); the
                        // cell then stays gated via typed_slots.
                        merged = Some(self.bind_typed_check(&pd, &dcls, &src)?);
                    }
                }
                cls.statics.borrow_mut().insert(pn.clone(), src.clone());
                if let (Some(m), Some((pd, dcls))) = (merged, self.find_static_prop_decl(&cls, &pn))
                {
                    let sptr = Rc::as_ptr(&src) as usize;
                    self.typed_slots.entry(sptr).or_insert_with(|| {
                        (src.clone(), m.clone(), dcls.name().to_string(), pn.clone())
                    });
                    self.slot_anchor.entry(sptr).or_insert_with(|| {
                        SlotAnchor::Statics(dcls.name().to_string(), pn.clone())
                    });
                    self.slot_merged.insert(sptr, m);
                    let ot = pd.ty.clone().unwrap_or_default();
                    let dcn = dcls.name().to_string();
                    let owners = self.slot_owners.entry(sptr).or_default();
                    if !owners
                        .iter()
                        .any(|(_, cn, pn2, _)| cn == &dcn && pn2 == &pn)
                    {
                        owners.push((
                            ot,
                            dcn,
                            pn.clone(),
                            SlotAnchor::Statics(dcls.name().to_string(), pn.clone()),
                        ));
                    }
                }
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
                let (cls, _) = self.member_class_of(class)?;
                let pname = self.prop_name(name)?;
                // The stored cell may be a `=&`-bound cell shared with a
                // DIFFERENT typed prop — typed_slots gates that write
                // (typed_properties_107).
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &pname) {
                    let v2 = self.prop_typed_write_check(&pd, &dcls, v)?;
                    let nv = self.typed_slot_store(&c, v2)?;
                    *c.borrow_mut() = nv;
                } else {
                    let nv = self.typed_slot_store(&c, v)?;
                    *c.borrow_mut() = nv;
                }
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
                self.store_prop(ov, &pn, v).map(|_| ())
            }
            _ => self.fail(PhpError::fatal("Cannot assign to this expression", 0)),
        }
    }

    /// Write `$ov->$pn = v` — private-slot, `__set` or dynamic-prop rules.
    /// Write `$ov->$pn = v`; returns the STORED value (typed props
    /// coerce — the assign expr yields the coerced result, 077).
    /// `$cell[] = v` / `$cell[k] = v` on a null typed slot: Zend
    /// auto-initializes an array only when `array` fits the merged
    /// member type; otherwise `Cannot auto-initialize an array inside
    /// ...` TypeError and a just-materialized slot reverts to
    /// uninitialized (typed_properties_083).
    fn auto_init_gate(&mut self, c: &Cell) -> Result<(), PhpError> {
        let ptr = Rc::as_ptr(c) as usize;
        self.prune_typed_slot(ptr);
        let gated = self
            .slot_merged
            .get(&ptr)
            .cloned()
            .or_else(|| self.typed_slots.get(&ptr).map(|(_, t, _, _)| t.clone()));
        let Some(tys) = gated else {
            return Ok(());
        };
        if tys.iter().any(|m| self.ty_member_is_a("array", m)) {
            *c.borrow_mut() = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
            return Ok(());
        }
        let (cn, pn) = self
            .typed_slots
            .get(&ptr)
            .map(|(_, _, n, p)| (n.clone(), p.clone()))
            .unwrap_or_default();
        let where_ = if self.ref_cells.contains(&ptr) {
            "a reference held by property"
        } else {
            "property"
        };
        let msg = format!(
            "Cannot auto-initialize an array inside {} {}::${} of type {}",
            where_,
            cn,
            pn,
            ty_disp(&tys)
        );
        // A slot materialized for this write goes back to
        // uninitialized — reads must still raise the uninit Error.
        if self.last_fresh_cell == Some(ptr) {
            if let Some(anc) = self.slot_anchor.get(&ptr).cloned() {
                match anc {
                    SlotAnchor::Obj(w, key) => {
                        if let Some(o) = w.upgrade() {
                            o.borrow_mut().props.remove(&key);
                        }
                    }
                    SlotAnchor::Statics(ccn, ppn) => {
                        if let Some(cc) = self.classes.get(&ccn.to_lowercase()) {
                            cc.statics.borrow_mut().remove(&ppn);
                        }
                    }
                    SlotAnchor::None => {}
                }
            }
            self.typed_slots.remove(&ptr);
            self.slot_anchor.remove(&ptr);
            self.slot_owners.remove(&ptr);
            self.slot_merged.remove(&ptr);
            self.last_fresh_cell = None;
        }
        let mut e = PhpError::uncaught("TypeError", msg, 0);
        e.thrown_line = Some(self.cur_line);
        self.fail(e)
    }

    /// Dynamic property names starting with `\0` hit zend's
    /// private-name-mangle check — a catchable Error, not magic
    /// (bug52484).
    fn check_prop_name(&mut self, pn: &str) -> Result<(), PhpError> {
        if pn.starts_with('\0') {
            return self.fail(PhpError::uncaught(
                "Error",
                "Cannot access property starting with \"\\0\"",
                0,
            ));
        }
        Ok(())
    }

    fn store_prop(&mut self, ov: Value, pn: &str, mut v: Value) -> Result<Value, PhpError> {
        match ov {
            Value::Object(o) => {
                if !self.in_own_hook(&o, pn) {
                    if let Some((pd, hs)) = self.hooked_prop(&o, pn) {
                        {
                            self.hook_write(&o, &pd, &hs, v.clone())?;
                            return Ok(v);
                        }
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
                let cls = o.borrow().class.clone();
                let k = self.obj_prop_key(&o, pn).or_else(|| {
                    self.decl_prop(&o, pn).map(|(pd, dcls)| {
                        if pd.visibility == crate::ast::Visibility::Private {
                            format!("\0{}\0{}", dcls.name(), pd.name)
                        } else {
                            pd.name.clone()
                        }
                    })
                });
                // A slot the caller can't see is __set territory —
                // zend never writes it directly from an outside scope
                // (overloaded_prop_assign_op_refs).
                let k = match k {
                    Some(k) if self.prop_visible(&cls, pn) => Some(k),
                    _ => None,
                };
                if let Some(k) = k {
                    let mut ob = o.borrow_mut();
                    // Write into the existing slot — a `&`-bound
                    // reference must see the update (typed_properties_010).
                    if let Some(existing) = ob.props.get(&k) {
                        let existing = existing.clone();
                        drop(ob);
                        // The slot may be shared with a DIFFERENT typed
                        // prop via `=&` — that prop's type still gates
                        // the write (typed_properties_062).
                        let nv = self.typed_slot_store(&existing, v)?;
                        *existing.borrow_mut() = nv.clone();
                        v = nv;
                    } else {
                        // A declared prop that was unset() is
                        // inaccessible — writes go through __set
                        // (typed_properties_magic_set). The in-set
                        // guard writes re-entrant assignments to real
                        // storage instead of recursing (bug63462).
                        let gkey = (Rc::as_ptr(&o) as usize, 1u8, pn.to_string());
                        if ob.unset_props.contains(&k)
                            && self.find_method_in(&cls, "__set").is_some()
                            && self.magic_guards.insert(gkey.clone())
                        {
                            drop(ob);
                            let res = self.method_invoke(
                                o.clone(),
                                "__set",
                                CallArgs::positional(vec![
                                    cell(Value::str(pn.to_string())),
                                    cell(v.clone()),
                                ]),
                            );
                            self.magic_guards.remove(&gkey);
                            res?;
                            return Ok(v);
                        }
                        if !ob.prop_order.contains(&k) {
                            ob.prop_order.push(k.clone());
                        }
                        ob.props.insert(k, cell(v.clone()));
                    }
                    Ok(v)
                } else if self.find_method_in(&cls, "__set").is_some()
                    && self
                        .magic_guards
                        .insert((Rc::as_ptr(&o) as usize, 1u8, pn.to_string()))
                {
                    let res = self.method_invoke(
                        o.clone(),
                        "__set",
                        CallArgs::positional(vec![
                            cell(Value::str(pn.to_string())),
                            cell(v.clone()),
                        ]),
                    );
                    self.magic_guards
                        .remove(&(Rc::as_ptr(&o) as usize, 1u8, pn.to_string()));
                    res?;
                    Ok(v)
                } else {
                    // Writing a DECLARED prop the scope can't see is
                    // `Cannot access private/protected property` — the
                    // declared name can't be shadowed by a dynamic prop
                    // (bug38461's re-entrant __set write; direct writes
                    // without __set too).
                    if let Some(e) = self.hidden_decl_error(&o, pn) {
                        return self.fail(e);
                    }
                    // `\0` names error on the real-storage path before
                    // any deprecation (bug52484_2).
                    self.check_prop_name(pn)?;
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
                            a.name
                                .rsplit('\\')
                                .next()
                                .unwrap_or(&a.name)
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
                    ob.props.insert(pn, cell(v.clone()));
                    Ok(v)
                }
            }
            Value::Callable(_) => {
                // Closures have no prop storage — writes are a
                // catchable Error, not a dynamic-prop create
                // (closure_022, closure_write_prop).
                let e = PhpError::uncaught(
                    "Error",
                    format!("Cannot create dynamic property Closure::${}", pn),
                    0,
                );
                self.fail(e)
            }
            _ => {
                self.warn(&format!(
                    "Attempt to assign property \"{}\" on {}",
                    pn,
                    ov.gettype()
                ))?;
                Ok(Value::Null)
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
        self.last_fresh_cell = None;
        let mut c = self.eval_cell(e)?;
        let last = keys.len() - 1;
        for (n, k) in keys.iter().enumerate() {
            // Illegal offset types must not fall into the string-offset
            // fallback — the key Error propagates
            // (closure_array_offset_error).
            if let Some(kv) = k {
                self.check_offset_key(kv)?;
            }
            // Auto-init gate: writing through a typed slot that is
            // null (or a just-materialized uninit slot) must produce
            // `Cannot auto-initialize an array inside property ...`
            // unless its type accepts an array (typed_properties_083).
            if matches!(&*c.borrow(), Value::Null) {
                self.auto_init_gate(&c)?;
            }
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
                        CallArgs::positional(vec![
                            cell(k.clone().unwrap_or(Value::Null)),
                            cell(v.clone()),
                        ]),
                    ) {
                        Ok(_) => return Ok(v),
                        Err(e) => return Err(e),
                    }
                }
                let iv = self
                    .method_invoke(
                        o,
                        "offsetGet",
                        CallArgs::positional(vec![cell(k.clone().unwrap_or(Value::Null))]),
                    )
                    .unwrap_or(Value::Null);
                c = cell(iv);
                continue;
            }
            match self.index_into_key(c.clone(), k.clone()) {
                Ok(nc) => {
                    if n == last {
                        // `$ref[k] = v` where the element cell is bound to
                        // a typed prop stays type-gated (064).
                        let nv = self.typed_slot_store(&nc, v.clone())?;
                        *nc.borrow_mut() = nv;
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
                            Value::Str(s) => s.to_vec(),
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
                                *s = bytes.clone().into();
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

    /// `$a[$k]` keys: object/closure keys are a catchable Error
    /// naming the class (closure_array_key_error/offset_error).
    fn check_offset_key(&mut self, v: &Value) -> Result<(), PhpError> {
        let cn = match v {
            Value::Object(o) => Some(o.borrow().class.name().to_string()),
            Value::Callable(_) => Some("Closure".to_string()),
            _ => None,
        };
        if let Some(cn) = cn {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot access offset of type {} on array", cn),
                0,
            ));
        }
        Ok(())
    }

    /// `set_index` with an already-evaluated key.
    fn set_index_val(&mut self, e: &Expr, key: Option<Value>, v: Value) -> Result<(), PhpError> {
        if let Some(k) = &key {
            self.check_offset_key(k)?;
        }
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
                        let mut bytes = s.to_vec();
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
                                let mut bytes = s.to_vec();
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
                                *s = bytes.clone().into();
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
                            match self.method_invoke(
                                o,
                                "offsetSet",
                                CallArgs::positional(vec![cell(kv), cell(v)]),
                            ) {
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

    /// Writable array handle for by-ref builtin args: PHP COW-separates
    /// a shared array at the callee boundary, so a builtin mutating
    /// `&$array` replaces the caller's slot with a private copy while
    /// other variables keep the old contents. is_ref arrays (true `=&`
    /// bindings) write through to every alias instead.
    pub fn arr_mut(&self, c: &Cell) -> Option<Rc<RefCell<PhpArray>>> {
        let mut b = c.borrow_mut();
        if let Value::Array(rc) = &mut *b {
            if Rc::strong_count(rc) > 1 && !rc.borrow().is_ref {
                let fresh = rc.borrow().clone();
                *b = Value::Array(Rc::new(RefCell::new(fresh)));
            }
            if let Value::Array(rc) = &*b {
                return Some(rc.clone());
            }
        }
        None
    }

    /// Index into `c`'s array value, taking a cell for `key`/`[]`.
    fn index_into_key(&mut self, c: Cell, key: Option<Value>) -> Result<Cell, PhpError> {
        if let Some(k) = &key {
            self.check_offset_key(k)?;
        }
        let mut b = c.borrow_mut();
        if matches!(*b, Value::Null) {
            *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
        }
        if let Value::Array(rc) = &mut *b {
            // Deliberately-shared arrays ($GLOBALS, &-bound storage)
            // are exempted from CoW via is_ref; an ordinary shared
            // zend_array still separates on write (bug32660).
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
        self.check_offset_key(&key)?;
        match base {
            Value::Array(rc) => {
                let k = to_key(&key);
                let arr = rc.borrow();
                match arr.get(&k) {
                    Some(v) => Ok(v),
                    None => {
                        let shown = match &key {
                            Value::Str(s) => format!("\"{}\"", crate::value::lossy(s)),
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
                        let b: &[u8] = k;
                        let mut i = usize::from(b.first() == Some(&b'-'));
                        let start = i;
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                        if i == b.len() && i > start {
                            crate::value::lossy(&k[..]).parse::<i64>().unwrap_or(0)
                        } else if i > start && matches!(numeric(k), Numeric::Leading(_, _)) {
                            self.warn(&format!(
                                "Illegal string offset \"{}\"",
                                crate::value::lossy(&k[..])
                            ))?;
                            crate::value::lossy(&k[..i]).parse::<i64>().unwrap_or(0)
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
                let bytes: &[u8] = &s[..];
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
                    Ok(Value::bytes(bytes[idx as usize..idx as usize + 1].to_vec()))
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
                    return match self.method_invoke(
                        o,
                        "offsetGet",
                        CallArgs::positional(vec![cell(key)]),
                    ) {
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
                return match self.method_invoke(
                    o,
                    "offsetUnset",
                    CallArgs::positional(vec![cell(kv)]),
                ) {
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
                        // `unset($copy[$k])` must cow-separate a shared
                        // array like a write does — PHP copies `$a = $b`
                        // lazily; mutating the shared table would corrupt
                        // the source (InputDefinition::parseArgument
                        // unsets on its own copy of getArguments()).
                        if Rc::strong_count(rc) > 1 && !rc.borrow().is_ref {
                            let fresh = rc.borrow().clone();
                            *b = Value::Array(Rc::new(RefCell::new(fresh)));
                        }
                        if let Value::Array(rc) = &*b {
                            if let Some(k) = key {
                                rc.borrow_mut().unset(&to_key(&k));
                            }
                        }
                    }
                }
                Ok(())
            }
            Expr::Index { e: inner, i: ii } => {
                if let Ok(Value::Object(o)) = self.eval(inner) {
                    if self.obj_is_a(&o, "ArrayAccess") {
                        let kv = key.unwrap_or(Value::Null);
                        match self.method_invoke(
                            o,
                            "offsetUnset",
                            CallArgs::positional(vec![cell(kv)]),
                        ) {
                            Ok(_) => return Ok(()),
                            Err(e) => return Err(e),
                        }
                    }
                }
                if let Ok(c) = self.index_cell(inner, ii.as_deref()) {
                    let mut b = c.borrow_mut();
                    if let Value::Array(rc) = &mut *b {
                        if Rc::strong_count(rc) > 1 && !rc.borrow().is_ref {
                            let fresh = rc.borrow().clone();
                            *b = Value::Array(Rc::new(RefCell::new(fresh)));
                        }
                        if let Value::Array(rc) = &*b {
                            if let Some(k) = key {
                                rc.borrow_mut().unset(&to_key(&k));
                            }
                        }
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Catch-bind checking: strict member fit only — no __toString or
    /// scalar coercion (int still widens to float)
    /// (typed_properties_108).
    fn slot_write_strict(&mut self, tys: &[String], v: &Value) -> Option<Value> {
        if self.ty_exact(tys, v) {
            if tys.iter().any(|t| t.eq_ignore_ascii_case("float")) {
                if let Value::Int(i) = v {
                    return Some(Value::Float(*i as f64));
                }
            }
            return Some(v.clone());
        }
        None
    }

    /// The write result one typed-slot owner would produce — exact
    /// match widens `int` into a `float` member, weak files coerce;
    /// `None` when the type can't be satisfied (union_types/prop_ref_assign).
    fn slot_write_one(&mut self, tys: &[String], v: &Value) -> Result<Option<Value>, PhpError> {
        if self.ty_exact(tys, v) {
            if tys.iter().any(|t| t.eq_ignore_ascii_case("float")) {
                if let Value::Int(i) = v {
                    return Ok(Some(Value::Float(*i as f64)));
                }
            }
            return Ok(Some(v.clone()));
        }
        if !self.exec_file_strict() {
            // Object with __toString coerces into a `string` slot (107).
            if let Value::Object(o) = v {
                if tys.iter().any(|t| t.eq_ignore_ascii_case("string"))
                    && (self
                        .find_method_in(&o.borrow().class, "__tostring")
                        .is_some()
                        || self
                            .find_method_in(&o.borrow().class, "__toString")
                            .is_some())
                {
                    let sv =
                        self.method_invoke(o.clone(), "__toString", CallArgs::positional(vec![]))?;
                    let svs = self.conv_str(&sv)?;
                    return Ok(Some(Value::str(svs)));
                }
            }
            if let Some(cv) = weak_ty_coerce(tys, v) {
                self.deprecate_lossy_int(tys, v, &cv);
                return Ok(Some(cv));
            }
        }
        Ok(None)
    }

    /// An owner is stale when the prop it names no longer holds this
    /// cell — static rebinds (082), unsets, or a dead object (094).
    fn slot_anchor_alive(&self, ptr: usize, anc: &SlotAnchor) -> bool {
        match anc {
            SlotAnchor::Obj(w, key) => w
                .upgrade()
                .map(|o| {
                    o.borrow()
                        .props
                        .get(key)
                        .map(|c| Rc::as_ptr(c) as usize == ptr)
                        .unwrap_or(false)
                })
                .unwrap_or(false),
            SlotAnchor::Statics(cn, pn) => self
                .classes
                .get(&cn.to_lowercase())
                .map(|c| {
                    c.statics
                        .borrow()
                        .get(pn)
                        .map(|c2| Rc::as_ptr(c2) as usize == ptr)
                        .unwrap_or(false)
                })
                .unwrap_or(false),
            SlotAnchor::None => true,
        }
    }

    /// Drop owners whose prop no longer points at `ptr`, then rebuild
    /// the merged constraint from the live owners' declared types.
    fn prune_typed_slot(&mut self, ptr: usize) {
        let prim_dead = self
            .slot_anchor
            .get(&ptr)
            .map(|a| !self.slot_anchor_alive(ptr, a))
            .unwrap_or(false);
        if prim_dead {
            self.typed_slots.remove(&ptr);
            self.slot_anchor.remove(&ptr);
        }
        if let Some(os) = self.slot_owners.get(&ptr) {
            let os = os.clone();
            let dead: Vec<usize> = os
                .iter()
                .enumerate()
                .filter(|(_, (_, _, _, a))| !self.slot_anchor_alive(ptr, a))
                .map(|(i, _)| i)
                .collect();
            if !dead.is_empty() {
                let os = self.slot_owners.get_mut(&ptr).unwrap();
                for i in dead.into_iter().rev() {
                    os.remove(i);
                }
                if os.is_empty() {
                    self.slot_owners.remove(&ptr);
                }
            }
        }
        if let Some(os) = self.slot_owners.get(&ptr) {
            let os = os.clone();
            let mut acc: Vec<String> = Vec::new();
            for (i, (t, _, _, _)) in os.iter().enumerate() {
                acc = if i == 0 {
                    t.clone()
                } else {
                    self.ty_bind_merge(&acc, t).unwrap_or_default()
                };
            }
            self.slot_merged.insert(ptr, acc);
        } else if let Some((_, t, _, _)) = self.typed_slots.get(&ptr) {
            self.slot_merged.insert(ptr, t.clone());
        } else {
            self.slot_merged.remove(&ptr);
        }
    }

    /// By-ref foreach bind onto a readonly prop cell shared through an
    /// ArrayIterator — Error "Cannot acquire reference to readonly
    /// property C::$p" (typed_properties_115).
    fn readonly_ref_error(&mut self, c: &Cell) -> Option<Flow> {
        let (cn, pn) = self.readonly_cells.get(&(Rc::as_ptr(c) as usize))?;
        let v = self.exception(
            "Error",
            &format!("Cannot acquire reference to readonly property {cn}::${pn}"),
        );
        let e = self.throw(v);
        Some(self.err_flow(e))
    }

    fn typed_slot_store(&mut self, c: &Cell, v: Value) -> Result<Value, PhpError> {
        self.typed_slot_store_mode(c, v, false)
    }

    /// `strict` = catch-binding semantics: no __toString/scalar
    /// coercion — the value must fit the declared type as-is
    /// (typed_properties_108).
    fn typed_slot_store_mode(
        &mut self,
        c: &Cell,
        v: Value,
        strict: bool,
    ) -> Result<Value, PhpError> {
        let ptr = Rc::as_ptr(c) as usize;
        self.prune_typed_slot(ptr);
        let owners: Vec<(Vec<String>, String, String)> =
            if let Some(os) = self.slot_owners.get(&ptr) {
                os.iter()
                    .map(|(t, n, p, _)| (t.clone(), n.clone(), p.clone()))
                    .collect()
            } else {
                match self
                    .typed_slots
                    .get(&ptr)
                    .map(|(_, t, n, p)| (t.clone(), n.clone(), p.clone()))
                {
                    Some(o) => vec![o],
                    None => return Ok(v),
                }
            };
        let mut results: Vec<Option<Value>> = Vec::with_capacity(owners.len());
        for (tys, _, _) in &owners {
            results.push(if strict {
                self.slot_write_strict(tys, &v)
            } else {
                self.slot_write_one(tys, &v)?
            });
        }
        // A value an owner rejects outright reports that owner; only
        // all-accepted-but-divergent coercions are "inconsistent"
        // (typed_reference).
        let consistent = results.iter().all(|r| r.is_some())
            && results
                .iter()
                .map(|r| r.clone().unwrap())
                .all(|rv| Self::value_identical(&rv, results[0].as_ref().unwrap()));
        if consistent {
            return Ok(results.into_iter().next().unwrap().unwrap());
        }
        if let Some(bad) = results.iter().position(|r| r.is_none()) {
            let (tys, cn, pn) = &owners[bad];
            let where_ = if self.ref_cells.contains(&ptr) {
                "reference held by property"
            } else {
                "property"
            };
            let mut e = PhpError::uncaught(
                "TypeError",
                format!(
                    "Cannot assign {} to {} {}::${} of type {}",
                    self.zval_type_name(&v),
                    where_,
                    cn,
                    pn,
                    ty_disp(tys)
                ),
                0,
            );
            e.thrown_line = Some(self.cur_line);
            return self.fail(e);
        }
        let mut e = if owners.len() == 1 {
            let (tys, cn, pn) = &owners[0];
            let where_ = if self.ref_cells.contains(&ptr) {
                "reference held by property"
            } else {
                "property"
            };
            PhpError::uncaught(
                "TypeError",
                format!(
                    "Cannot assign {} to {} {}::${} of type {}",
                    self.zval_type_name(&v),
                    where_,
                    cn,
                    pn,
                    ty_disp(tys)
                ),
                0,
            )
        } else {
            let held: Vec<String> = owners
                .iter()
                .map(|(tys, cn, pn)| format!("property {}::${} of type {}", cn, pn, ty_disp(tys)))
                .collect();
            PhpError::uncaught(
                "TypeError",
                format!(
                    "Cannot assign {} to reference held by {}, as this would result in an inconsistent type conversion",
                    self.zval_type_name(&v),
                    held.join(" and ")
                ),
                0,
            )
        };
        e.thrown_line = Some(self.cur_line);
        self.fail(e)
    }

    /// Synthesized signature of a magic-method trampoline FCC:
    /// `mixed ...$arguments` (trampoline_closure_named_arguments).
    fn trampoline_decl() -> Rc<crate::ast::FunctionDecl> {
        Rc::new(crate::ast::FunctionDecl {
            name: "{trampoline}".into(),
            params: vec![crate::ast::Param {
                name: "arguments".into(),
                default: None,
                by_ref: false,
                variadic: true,
                ty: Some(vec!["mixed".into()]),
                promoted: false,
                vis: None,
                readonly: false,
                is_final: false,
                set_vis: None,
                hooks: None,
            }],
            ret: None,
            body: vec![],
            attrs: vec![],
            by_ref: false,
            line: 0,
            end_line: 0,
            file: String::new(),
            ns: String::new(),
            decl_in: None,
        })
    }

    /// Decl for the function a callable value points at — builtins
    /// synthesize one from builtin_sig. Shared by the function and
    /// method (Closure::__invoke) reflector paths.
    fn callable_decl(&mut self, v: &Value) -> Option<Rc<crate::ast::FunctionDecl>> {
        match v {
            Value::Str(s) => {
                let n = String::from_utf8_lossy(s).to_lowercase();
                self.functions
                    .get(&n)
                    .cloned()
                    .or_else(|| Self::builtin_decl(&n))
            }
            Value::Callable(c) => match &c.kind {
                crate::value::CallableKind::Closure(d) => Some(d.clone()),
                crate::value::CallableKind::Named(n) => self
                    .functions
                    .get(&n.to_lowercase())
                    .cloned()
                    .or_else(|| Self::builtin_decl(&n.to_lowercase())),
                crate::value::CallableKind::Method { name, obj, class } => {
                    let c = class
                        .clone()
                        .or_else(|| obj.as_ref().map(|o| o.borrow().class.clone()));
                    match c {
                        Some(c) => self
                            .find_method_in(&c, name)
                            .map(|(m, _)| Rc::new(m.decl.clone()))
                            // A magic-method trampoline (`C::undef(...)`
                            // on __callStatic / `$o->undef(...)` on
                            // __call) reflects as `mixed ...$arguments`
                            // (trampoline_closure_named_arguments).
                            .or_else(|| {
                                let magic = if obj.is_some() {
                                    "__call"
                                } else {
                                    "__callstatic"
                                };
                                self.find_method_in(&c, magic)
                                    .is_some()
                                    .then(Self::trampoline_decl)
                            }),
                        None => None,
                    }
                }
            },
            _ => None,
        }
    }

    /// Synthetic decl for an internal function, from builtin_sig +
    /// builtin_param_ty — lets reflectors report param names,
    /// required flags and declared types for builtins (bug69802_2).
    fn builtin_decl(lname: &str) -> Option<Rc<crate::ast::FunctionDecl>> {
        let sig = crate::builtins::builtin_sig(lname)?;
        Some(Rc::new(crate::ast::FunctionDecl {
            name: lname.into(),
            params: sig
                .into_iter()
                .map(|(name, req)| crate::ast::Param {
                    default: if req {
                        None
                    } else {
                        Some(crate::ast::Expr::Null)
                    },
                    ty: crate::builtins::builtin_param_ty(lname, &name),
                    name,
                    by_ref: false,
                    variadic: false,
                    promoted: false,
                    vis: None,
                    readonly: false,
                    is_final: false,
                    set_vis: None,
                    hooks: None,
                })
                .collect(),
            ret: None,
            body: vec![],
            attrs: vec![],
            by_ref: false,
            line: 0,
            end_line: 0,
            file: String::new(),
            ns: String::new(),
            decl_in: None,
        }))
    }

    /// Same-type same-value — owners must agree on the *exact* result
    /// (int(42) vs float(42.0) is inconsistent).
    fn value_identical(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(x), Value::Bool(y)) => x == y,
            (Value::Int(x), Value::Int(y)) => x == y,
            (Value::Float(x), Value::Float(y)) => x == y,
            (Value::Str(x), Value::Str(y)) => x == y,
            (Value::Object(x), Value::Object(y)) => Rc::ptr_eq(x, y),
            (Value::Array(x), Value::Array(y)) => Rc::ptr_eq(x, y),
            _ => false,
        }
    }

    fn incdec(&mut self, target: &Expr, delta: i64, post: bool) -> Result<Value, PhpError> {
        // PHP warns on undefined vars/props/keys during ++/-- (bug25547).
        let old = match target {
            Expr::Var(name) => self.var_get(name).unwrap_or(Value::Null),
            Expr::Index { e, i } => {
                let base = self.eval(e)?;
                let key = match i {
                    Some(ie) => self.eval(ie)?,
                    None => return self.fail(PhpError::fatal("[] used in read context", 0)),
                };
                // `++`/`--` on an ArrayAccess offset writes the by-ref
                // offsetGet cell directly — no offsetSet call
                // (typed_properties_065).
                if let Value::Object(o) = &base {
                    if self.obj_is_a(o, "ArrayAccess") {
                        return self.incdec_aa(o.clone(), key, delta, post);
                    }
                }
                self.index_read_base(base, key).unwrap_or(Value::Null)
            }
            // ++/-- reads through __get first — its exceptions
            // propagate (the __set is never reached, bug38624).
            Expr::Prop { .. } => self.prop_read_loose(target)?,
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
        // Typed `int` prop can't overflow to float — a dedicated Error
        // instead of the generic assign TypeError (typed_properties_019).
        // The target's storage cell carries the owning prop's type —
        // "property" when the target IS that prop, "a reference held
        // by property" when reached through an alias/bound ref.
        // ++/-- on an *overloaded* prop (__get/__set magic) never
        // touches the backing cell — it reads a value and calls __set
        // (typed_properties_061); skip the ref-cell overflow check.
        let prop_overloaded = match target {
            Expr::Prop { obj, name, .. } => {
                let ov = self.eval(obj)?;
                let pn = self.prop_name(name)?;
                match &ov {
                    Value::Object(o) => self.decl_prop(o, &pn).is_none(),
                    _ => false,
                }
            }
            _ => false,
        };
        if !prop_overloaded && matches!(old, Value::Int(i) if i.checked_add(delta).is_none()) {
            let ent = self.eval_cell(target).ok().map(|c| {
                // Unaliased count is 3 (storage + typed_slots + this
                // temp); a `=&` bind adds a var slot -> "a reference
                // held by" (union_types/incdec_prop).
                let shared = Rc::strong_count(&c) > 3;
                let e = self
                    .typed_slots
                    .get(&(Rc::as_ptr(&c) as usize))
                    .map(|(cc, t, n, p)| (cc.clone(), t.clone(), n.clone(), p.clone()));
                (shared, e)
            });
            if let Some((shared, Some((_, tys, cn, cpn)))) = ent {
                if tys.iter().any(|m| m.eq_ignore_ascii_case("int"))
                    && !tys.iter().any(|m| m.eq_ignore_ascii_case("float"))
                {
                    let dir = if delta > 0 { "increment" } else { "decrement" };
                    let bound = if delta > 0 { "maximal" } else { "minimal" };
                    // "a reference held by" once the slot is aliased —
                    // a `&$prop` bind makes the storage cell shared
                    // (union_types/incdec_prop).
                    let own = !shared
                        && match target {
                            Expr::Prop { name, .. } | Expr::StaticProp { name, .. } => {
                                self.prop_name(name).map(|pn| pn == cpn).unwrap_or(false)
                            }
                            _ => false,
                        };
                    let msg = if own {
                        format!(
                            "Cannot {} property {}::${} of type {} past its {} value",
                            dir,
                            cn,
                            cpn,
                            ty_disp(&tys),
                            bound
                        )
                    } else {
                        format!(
                            "Cannot {} a reference held by property {}::${} of type {} past its {} value",
                            dir,
                            cn,
                            cpn,
                            ty_disp(&tys),
                            bound
                        )
                    };
                    let mut e = PhpError::uncaught("TypeError", msg, 0);
                    e.thrown_line = Some(self.cur_line);
                    return self.fail(e);
                }
            }
        }
        let new = self.incdec_value(&old, delta)?;
        self.store(target, new.clone())?;
        Ok(if post { old } else { new })
    }

    /// `$o[k]++` on an ArrayAccess: `&offsetGet` hands back the real
    /// backing cell and ++/-- writes through it (no offsetSet);
    /// a value-returning offsetGet falls back to offsetSet
    /// (typed_properties_065).
    fn incdec_aa(
        &mut self,
        o: Rc<RefCell<PhpObject>>,
        key: Value,
        delta: i64,
        post: bool,
    ) -> Result<Value, PhpError> {
        self.last_ret_cell = None;
        let rv = self.method_invoke(
            o.clone(),
            "offsetGet",
            CallArgs::positional(vec![cell(key.clone())]),
        )?;
        let rc = self.last_ret_cell.take();
        if let Some(c) = &rc {
            self.ref_cells.insert(Rc::as_ptr(c) as usize);
        }
        let old = rc.as_ref().map(|c| c.borrow().clone()).unwrap_or(rv);
        // int-typed backing cell can't overflow to float — dedicated
        // "past its minimal/maximal value" Error (065).
        if let (Value::Int(iv), Some(c)) = (old.clone(), rc.clone()) {
            if iv.checked_add(delta).is_none() {
                let ptr = Rc::as_ptr(&c) as usize;
                self.prune_typed_slot(ptr);
                let owners: Vec<(Vec<String>, String, String)> = self
                    .slot_owners
                    .get(&ptr)
                    .map(|os| {
                        os.iter()
                            .map(|(t, n, p, _)| (t.clone(), n.clone(), p.clone()))
                            .collect()
                    })
                    .or_else(|| {
                        self.typed_slots
                            .get(&ptr)
                            .map(|(_, t, n, p)| vec![(t.clone(), n.clone(), p.clone())])
                    })
                    .unwrap_or_default();
                for (tys, cn, cpn) in &owners {
                    if tys.iter().any(|m| m.eq_ignore_ascii_case("int"))
                        && !tys.iter().any(|m| m.eq_ignore_ascii_case("float"))
                    {
                        let dir = if delta > 0 { "increment" } else { "decrement" };
                        let bound = if delta > 0 { "maximal" } else { "minimal" };
                        let mut e = PhpError::uncaught(
                            "TypeError",
                            format!(
                                "Cannot {} a reference held by property {}::${} of type {} past its {} value",
                                dir,
                                cn,
                                cpn,
                                ty_disp(tys),
                                bound
                            ),
                            0,
                        );
                        e.thrown_line = Some(self.cur_line);
                        return self.fail(e);
                    }
                }
            }
        }
        let new = self.incdec_value(&old, delta)?;
        match rc {
            Some(c) => {
                let nv = self.typed_slot_store(&c, new.clone())?;
                *c.borrow_mut() = nv;
            }
            None => {
                self.method_invoke(
                    o,
                    "offsetSet",
                    CallArgs::positional(vec![cell(key), cell(new.clone())]),
                )?;
            }
        }
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
                        Value::bytes(perl_inc(s))
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
                    other => match numeric(&other.to_php_bytes()) {
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
                    Value::Str(s) => Ok(Value::bytes(s.iter().map(|b| !b).collect::<Vec<u8>>())),
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
                // isset() semantics: undefined vars, missing offsets and
                // uninitialized typed props fall through to the right.
                // isset_val returns the read value so calls and getters
                // evaluate exactly once.
                match self.isset_val_mode(l, 2)? {
                    Some(v) => Ok(v),
                    None => self.eval(r),
                }
            }
            "." => {
                let (lv, rv) = self.binary_operands(l, r)?;
                let mut ls = self.conv_bytes(&lv)?;
                let rs = self.conv_bytes(&rv)?;
                ls.extend_from_slice(&rs);
                Ok(Value::bytes(ls))
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
                    return Ok(Value::bytes(bitwise_str(op, a, b)));
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

        // `array + array` is PHP's union operator: lhs keys win and rhs
        // supplies only missing keys (not arithmetic — arrays never
        // reach the numeric path).
        if op == "+" {
            if let (Value::Array(a), Value::Array(b)) = (&l, &r) {
                let mut out = a.borrow().clone();
                for (k, c) in b.borrow().entries.iter() {
                    if out.get_cell(k).is_none() {
                        out.set(k.clone(), c.borrow().clone());
                    }
                }
                return Ok(Value::Array(Rc::new(RefCell::new(out))));
            }
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
                        unset_props: std::collections::HashSet::new(),
                    }))
                }
            },
        })
    }

    // ----- calls -----

    /// Param decls a callable Value will bind against — needed so
    /// arg_cells aliases by-ref params (first_class_callable_refs).
    fn callable_params(&mut self, v: &Value) -> Vec<Param> {
        match v {
            Value::Callable(c) => match &c.kind {
                CallableKind::Closure(d) => d.params.clone(),
                CallableKind::Named(n) => self
                    .functions
                    .get(&n.trim_start_matches('\\').to_lowercase())
                    .map(|d| d.params.clone())
                    .unwrap_or_default(),
                CallableKind::Method { obj, class, name } => {
                    let cls = match obj {
                        Some(o) => Some(o.borrow().class.clone()),
                        None => class.clone(),
                    };
                    cls.and_then(|c| self.find_method_in(&c, name))
                        .map(|(m, _)| m.decl.params.clone())
                        .unwrap_or_default()
                }
            },
            // `$obj()` invokes __invoke — the params are that
            // method's (by-ref flags included, closure_014).
            Value::Object(o) => self
                .find_method_in(&o.borrow().class.clone(), "__invoke")
                .map(|(m, _)| m.decl.params.clone())
                .unwrap_or_default(),
            _ => vec![],
        }
    }

    fn call(&mut self, name: &Expr, args: &[Expr]) -> Result<Value, PhpError> {
        // Resolve callee name/value.
        let fname = match name {
            Expr::Str(s) => s.to_string(),
            Expr::Var(_) | Expr::VarVar(_) => {
                let v = self.eval(name)?;
                match v {
                    Value::Callable(_) | Value::Object(_) => {
                        // $closure() / $obj->__invoke()
                        let params = self.callable_params(&v);
                        let ctx = format!("{}()", self.callable_ctx_name(&v));
                        let vals = self.arg_cells(args, &params, &ctx, false)?;
                        return self.call_value(&v, vals);
                    }
                    Value::Array(_) => {
                        // `[obj,'m']` / `[$closure,'__invoke']` array
                        // callables (bug78689).
                        let c = self.fcc_val(&v)?;
                        let params = self.callable_params(&c);
                        let ctx = format!("{}()", self.callable_ctx_name(&c));
                        let vals = self.arg_cells(args, &params, &ctx, false)?;
                        return self.call_value(&c, vals);
                    }
                    _ => self.conv_str(&v).unwrap_or_default(),
                }
            }

            Expr::StaticProp { class, name } => {
                // `C::$var()` — dynamic static method call.
                let cls = self.class_of(class)?;
                let mn = Self::nul_trunc(&self.prop_name(name)?);
                let params = self
                    .find_method_in(&cls, &mn)
                    .map(|m| m.0.decl.params.clone())
                    .unwrap_or_default();
                let vals = self.arg_cells(args, &params, &format!("{}()", mn), false)?;
                let fwd = matches!(&**class, Expr::Const(_) | Expr::Str(_) | Expr::AnonClass(_));
                return self.static_invoke_vis(cls, &mn, vals, None, fwd);
            }
            Expr::Prop { .. } | Expr::MethodCall { .. } | Expr::Index { .. } => {
                let v = self.eval(name)?;
                let params = self.callable_params(&v);
                let ctx = format!("{}()", self.callable_ctx_name(&v));
                let vals = self.arg_cells(args, &params, &ctx, false)?;
                return self.call_value(&v, vals);
            }
            _ => {
                let v = self.eval(name)?;
                match v {
                    // `(expr)()` — IIFE on a closure/invokable value.
                    Value::Callable(_) | Value::Object(_) => {
                        let params = self.callable_params(&v);
                        let ctx = format!("{}()", self.callable_ctx_name(&v));
                        let vals = self.arg_cells(args, &params, &ctx, false)?;
                        return self.call_value(&v, vals);
                    }
                    Value::Array(_) => {
                        let c = self.fcc_val(&v)?;
                        let params = self.callable_params(&c);
                        let ctx = format!("{}()", self.callable_ctx_name(&c));
                        let vals = self.arg_cells(args, &params, &ctx, false)?;
                        return self.call_value(&c, vals);
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
        internal: bool,
    ) -> Result<CallArgs, PhpError> {
        let mut out = CallArgs::empty();
        // Position of the *next positional* arg for by-ref lookup — named
        // args don't advance it (they bind by name at call time).
        let mut pos = 0usize;
        let mut seen_named = false;
        for a in args {
            let (name, expr): (Option<String>, &Expr) = match a {
                Expr::Binary {
                    op: "named", l, r, ..
                } => {
                    let n = match l.as_ref() {
                        Expr::Str(s) => s.clone(),
                        _ => match self.eval(l)? {
                            Value::Str(s) => crate::value::lossy(&s).into_owned(),
                            v => v.to_php_string(),
                        },
                    };
                    (Some(n), r.as_ref())
                }
                _ => (None, a),
            };
            if let Expr::Unpack(e) = expr {
                // `...$arr`: int-keyed entries become positionals (in
                // iteration order), string-keyed become named args
                // (named_params/unpack*). Entries from a Traversable are
                // fresh cells — a by-ref param gets the unpack warning
                // and a by-value bind (named_params/unpack's test2).
                if seen_named {
                    // The parser rejects `...` after named at compile
                    // time; unreachable for normal calls.
                    return self.fail(PhpError::fatal(
                        "Cannot use argument unpacking after named arguments",
                        0,
                    ));
                }
                let mut v = self.eval(e)?;
                if let (Expr::Var(_), Value::Array(a)) = (e.as_ref(), &v) {
                    // `...$ary` may hand out element cells for by-ref
                    // binding — Zend cow-separates $ary first so other
                    // variables sharing the array keep the old cells
                    // (named_params/unpack's $ary2 stays 0).
                    if Rc::strong_count(a) > 1 {
                        let mut na = a.borrow().clone();
                        for (_, c) in na.entries.iter_mut() {
                            let v = c.borrow().clone();
                            *c = cell(v);
                        }
                        let nv = Value::Array(Rc::new(RefCell::new(na)));
                        if let Ok(c) = self.eval_cell(e) {
                            *c.borrow_mut() = nv.clone();
                        }
                        v = nv;
                    }
                }
                let trav = matches!(&v, Value::Object(_));
                let mut unpack_named = false;
                for (k, c) in self.unpack_items(&v)? {
                    match k {
                        Some(n) => {
                            seen_named = true;
                            unpack_named = true;
                            out.named.push((n.to_string(), c, true, trav));
                        }
                        None if unpack_named => {
                            return self.fail(PhpError::uncaught(
                                "Error",
                                "Cannot use positional argument after named argument during unpacking",
                                0,
                            ));
                        }
                        None => {
                            if trav {
                                out.trav_cells.push(out.cells.len());
                            }
                            out.cells.push(c);
                            pos += 1;
                        }
                    }
                }
                continue;
            }
            let by_ref = match &name {
                // Unknown named args land in the variadic — a by-ref
                // `&...$refs` variadic binds them as cells
                // (named_params/variadic's test2 increments $x/$y).
                Some(n) => decl
                    .iter()
                    .find(|p| !p.variadic && p.name == *n)
                    .map(|p| p.by_ref)
                    .unwrap_or_else(|| decl.iter().any(|p| p.variadic && p.by_ref)),
                None => decl
                    .get(pos)
                    .map(|p| p.by_ref)
                    .unwrap_or_else(|| decl.iter().any(|p| p.variadic && p.by_ref)),
            };
            if by_ref {
                match expr {
                    Expr::Var(_) | Expr::Index { .. } | Expr::Prop { .. } | Expr::VarVar(_) => {
                        match self.eval_cell(expr) {
                            Ok(c) => {
                                if let Some(n) = name {
                                    out.named.push((n, c, true, false));
                                    seen_named = true;
                                } else {
                                    out.cells.push(c);
                                    pos += 1;
                                }
                            }
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
                        self.eval(expr)?;
                        let c = self.eval_cell(target)?;
                        if let Some(n) = name {
                            out.named.push((n, c, true, false));
                            seen_named = true;
                        } else {
                            out.cells.push(c);
                            pos += 1;
                        }
                    }
                    Expr::Call { .. } | Expr::MethodCall { .. } | Expr::StaticCall { .. } => {
                        // `f(g())`: binds only when g() returns by reference,
                        // otherwise a notice and pass by value (passByReference_004/007).
                        let (c, was_ref) = self.eval_call_cell(expr)?;
                        if !was_ref {
                            self.notice("Only variables should be passed by reference")?;
                        }
                        if let Some(n) = name {
                            out.named.push((n, c, was_ref, false));
                            seen_named = true;
                        } else {
                            out.cells.push(c);
                            pos += 1;
                        }
                    }
                    _ => {
                        // The reported number is the PARAM slot, not the
                        // call position (cannot_pass_by_ref: `test(e: 42)`
                        // reports #2 for `function test($a, &$e)`).
                        if internal {
                            // Internal functions silently materialize
                            // temporaries for by-ref params —
                            // `current(array())` is legal (bug55754).
                            let c = cell(self.eval(expr)?);
                            if let Some(n) = name {
                                out.named.push((n, c, true, false));
                                seen_named = true;
                            } else {
                                out.cells.push(c);
                                pos += 1;
                            }
                            continue;
                        }
                        let argno = match &name {
                            Some(n) => decl
                                .iter()
                                .position(|p| !p.variadic && p.name == *n)
                                .map(|i| i + 1)
                                .unwrap_or(pos + 1),
                            None => pos + 1,
                        };
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "{}: Argument #{} (${}) could not be passed by reference",
                                ctx,
                                argno,
                                name.as_deref()
                                    .or_else(|| decl.get(pos).map(|p| p.name.as_str()))
                                    .unwrap_or("")
                            ),
                            0,
                        ));
                    }
                }
            } else {
                let v = self.eval(expr)?;
                if let Some(n) = name {
                    out.named.push((n, cell(v), false, false));
                    seen_named = true;
                } else {
                    out.cells.push(cell(v));
                    pos += 1;
                }
            }
        }
        Ok(out)
    }

    /// Spreadable items of `...$v`: arrays yield entries, Traversables
    /// iterate via the rewind/valid/current/key/next protocol
    /// (IteratorAggregate chains resolve first). `None` key = positional.
    #[allow(clippy::type_complexity)]
    fn unpack_items(&mut self, v: &Value) -> Result<Vec<(Option<Rc<str>>, Cell)>, PhpError> {
        match v {
            Value::Array(a) => {
                // Element cells are handed to the call as potential
                // references — Zend separates the array first so a
                // shared copy (e.g. `$ary2 = $ary`) keeps its own
                // values (named_params/unpack).
                for (_, c) in a.borrow_mut().entries.iter_mut() {
                    let fresh = cell(c.borrow().clone());
                    *c = fresh;
                }
                let mut out = Vec::new();
                for (k, c) in a.borrow().iter() {
                    let n = match k {
                        ArrKey::Str(s) => Some(s.clone()),
                        _ => None,
                    };
                    out.push((n, c.clone()));
                }
                Ok(out)
            }
            Value::Object(o) => {
                // IteratorAggregate → getIterator() chain to a real Iterator.
                let mut cur = o.clone();
                let it = loop {
                    if self.obj_is_a(&cur, "IteratorAggregate") {
                        match self.method_invoke(cur.clone(), "getIterator", CallArgs::empty())? {
                            Value::Object(io) => cur = io,
                            _ => {
                                return self.fail(PhpError::uncaught(
                                    "Exception",
                                    "Objects returned by getIterator() must be traversable or implement interface Iterator",
                                    0,
                                ))
                            }
                        }
                    } else if self.obj_is_a(&cur, "Iterator") {
                        break cur;
                    } else {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Only arrays and Traversables can be unpacked, {} given",
                                cur.borrow().class.name()
                            ),
                            0,
                        ));
                    }
                };
                let _ = self.method_invoke(it.clone(), "rewind", CallArgs::empty());
                let mut out = Vec::new();
                loop {
                    let ok = self
                        .method_invoke(it.clone(), "valid", CallArgs::empty())
                        .map(|v| v.is_truthy())
                        .unwrap_or(false);
                    if !ok {
                        break;
                    }
                    let val = self
                        .method_invoke(it.clone(), "current", CallArgs::empty())
                        .unwrap_or(Value::Null);
                    let key = self
                        .method_invoke(it.clone(), "key", CallArgs::empty())
                        .unwrap_or(Value::Null);
                    let n = match &key {
                        Value::Str(s) => Some(crate::value::lossy(&s).into_owned().into()),
                        _ => None,
                    };
                    out.push((n, cell(val)));
                    let _ = self.method_invoke(it.clone(), "next", CallArgs::empty());
                }
                Ok(out)
            }
            _ => self.fail(PhpError::uncaught(
                "Error",
                format!(
                    "Only arrays and Traversables can be unpacked, {} given",
                    self.zval_type_name(v)
                ),
                0,
            )),
        }
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
        // When the ns\name fallback misses too, the undefined-function
        // error names the ns-qualified candidate (bugs/77376).
        let mut miss_name = fname.trim_start_matches('\u{1}').to_string();
        if decl.is_none() && unqualified {
            let ns = self.caller_ns();
            if !ns.is_empty() {
                let cand = format!("{}\\{}", ns.to_lowercase(), lname);
                decl = self.functions.get(&cand).cloned();
                ns_resolved = decl.is_some();
                if !ns_resolved {
                    miss_name = format!("{}\\{}", ns, fname.trim_start_matches('\u{1}'));
                }
            }
        }
        // Synthetic params carrying builtin by-ref flags so call results in
        // by-ref slots emit "Only variables should be passed by reference"
        // (passByReference_012, array_shift(array_shift($a))).
        let builtin_params: Vec<Param> = if decl.is_none() {
            let sig = crate::builtins::builtin_sig(&lname).unwrap_or_default();
            builtin_byref(&lname)
                .map(|flags| {
                    flags
                        .iter()
                        .enumerate()
                        .map(|(i, by_ref)| Param {
                            name: sig.get(i).map(|(n, _)| n.clone()).unwrap_or_default(),
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
            decl.is_none(),
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
                    format!("Call to undefined function {}()", miss_name),
                    0,
                ))
            }
        };
        self.invoke_fn(&decl, argvals, None, None)
    }

    /// Call any callable-ish Value: Callable, string name, [obj,'m'], obj
    /// with __invoke.
    pub fn call_value(&mut self, v: &Value, args: CallArgs) -> Result<Value, PhpError> {
        match v {
            Value::Callable(c) => {
                match &c.kind {
                    CallableKind::Closure(decl) => {
                        // A `yield`-bearing closure body makes the call
                        // a Generator factory — `function() { yield }`
                        // returns a Generator like any other function
                        // (iterable_003).
                        if Self::decl_contains_yield(&decl.body) {
                            return Ok(Value::Object(self.make_generator(
                                decl.clone(),
                                args,
                                c.this_obj.clone(),
                                c.scope_class.clone(),
                                None,
                                c.called_class.clone(),
                                c.captures.clone(),
                            )));
                        }
                        let mut frame_args = Vec::new();
                        // fn_name is the closure's Zend name
                        // (`{closure:enclosing():L}`) — __FUNCTION__/
                        // __METHOD__ read it, and a nested closure's
                        // `enclosing` resolves through it (closure_065).
                        let mut frame = Frame::new(decl.name.clone());
                        frame.closure_rc = Some(c.clone());
                        frame.call_alias = self.pending_call_alias.take();
                        frame.fn_line = decl.line;
                        frame.file = decl.file.clone();
                        frame.ret_by_ref = decl.by_ref;
                        for (n, cap, by_ref) in &c.captures {
                            // By-value captures re-import the stored
                            // value on every call — the caller's writes
                            // inside the closure don't persist
                            // (closure_009/011).
                            let c2 = if *by_ref {
                                cap.clone()
                            } else {
                                cell(cap.borrow().clone())
                            };
                            frame.vars.insert(n.clone(), c2);
                        }
                        frame.this_obj = c.this_obj.clone();
                        frame.scope_class = c.scope_class.clone();
                        frame.called_class = c.called_class.clone();
                        // $this binds like a normal method frame —
                        // closures defined in an object context auto-capture it.
                        if let Some(o) = &c.this_obj {
                            frame
                                .vars
                                .insert("this".to_string(), cell(Value::Object(o.clone())));
                        }
                        frame.file = if decl.file.is_empty() {
                            self.cur_file.clone()
                        } else {
                            decl.file.clone()
                        };
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
                            Some(cls) => self.static_invoke(cls.clone(), name, args, None, true),
                            None => self.fail(PhpError::fatal("bad callable", 0)),
                        },
                    },
                }
            }
            Value::Str(s) => {
                // Fully-qualified dynamic names carry a leading `\`
                // (namespaces/ns_032).
                let name = crate::value::lossy(s).trim_start_matches('\\').to_string();
                // "Class::method" string callables
                if let Some((cls, m)) = name.split_once("::") {
                    if let Some(c) = self.resolve_class(cls) {
                        let cls = self.classes.get(&c.to_lowercase()).cloned();
                        if let Some(cls) = cls {
                            return self.static_invoke_vis(cls, m, args, None, true);
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
                            // `[$closure, '__invoke']` is callable
                            // (closure_invoke_ref_warning).
                            Value::Callable(c) if mname.eq_ignore_ascii_case("__invoke") => {
                                // `[$closure,'__invoke']` reports args
                                // under `Closure::__invoke` (zend).
                                self.pending_call_alias = Some("Closure::__invoke".into());
                                let r = self.call_value(&Value::Callable(c.clone()), args);
                                self.pending_call_alias = None;
                                r
                            }
                            Value::Object(o) => self.method_invoke_vis(o.clone(), &mname, args),
                            Value::Str(cn) => {
                                let cls = self
                                    .resolve_class(&crate::value::lossy(&cn))
                                    .and_then(|c| self.classes.get(&c.to_lowercase()).cloned());
                                match cls {
                                    Some(cls) => {
                                        self.static_invoke_vis(cls, &mname, args, None, true)
                                    }
                                    None => self.fail(PhpError::uncaught(
                                        "Error",
                                        format!("Class \"{}\" not found", crate::value::lossy(&cn)),
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
                let icls = o.borrow().class.clone();
                if self.find_method_in(&icls, "__invoke").is_some() {
                    // `$b()` calls __invoke with NO visibility check —
                    // only an explicit `->` invoke is gated
                    // (bug61025).
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
    /// Display name used in call-time diagnostics (`f(): Argument #N`),
    /// matching zend's callable naming (closure_019).
    pub fn callable_ctx_name(&mut self, v: &Value) -> String {
        match v {
            Value::Callable(c) => match &c.kind {
                CallableKind::Closure(d) => d.name.clone(),
                CallableKind::Named(n) => n.trim_start_matches('\\').to_string(),
                CallableKind::Method { obj, class, name } => {
                    let cn = obj
                        .as_ref()
                        .map(|o| o.borrow().class.name().to_string())
                        .or_else(|| class.as_ref().map(|c| c.name().to_string()))
                        .unwrap_or_else(|| "Closure".into());
                    format!("{}::{}", cn, name)
                }
            },
            Value::Object(o) => format!("{}::__invoke", o.borrow().class.name()),
            Value::Str(s) => crate::value::lossy(s).trim_start_matches('\\').to_string(),
            _ => self.conv_str(v).unwrap_or_default(),
        }
    }

    /// `expr(...)` — first-class callable creation (PHP 8.1). Errors at
    /// creation are thrown `Error`s (catchable); abstract methods fail
    /// only when the closure is invoked (constexpr/error_abstract).
    fn fcc(&mut self, e: &Expr) -> Result<Value, PhpError> {
        match e {
            Expr::Call { name, .. } => match name.as_ref() {
                Expr::Str(s) => self.fcc_named(s),
                other => {
                    let v = self.eval(other)?;
                    self.fcc_val(&v)
                }
            },
            Expr::MethodCall { obj, name, .. } => {
                let ov = self.eval(obj)?;
                let mn = Self::nul_trunc(&self.prop_name(name)?);
                self.fcc_method(&ov, &mn)
            }
            Expr::StaticCall { class, name, .. } => {
                let cls = self.fcc_class_of(class)?;
                self.fcc_static(cls, name)
            }
            Expr::StaticCallDyn { class, name, .. } => {
                let cls = self.fcc_class_of(class)?;
                let nv = self.eval(name)?;
                let mn = match nv {
                    Value::Str(s) => Self::nul_trunc(&crate::value::lossy(&s)),
                    _ => {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Method name must be a string",
                            0,
                        ))
                    }
                };
                self.fcc_static(cls, &mn)
            }
            other => {
                let v = self.eval(other)?;
                self.fcc_val(&v)
            }
        }
    }

    /// `C::m(...)` class resolution — traits resolve too (calling a
    /// static trait method directly is deprecated, not undefined;
    /// constexpr/error_static_call_trait_method).
    fn fcc_class_of(&mut self, e: &Expr) -> Result<Rc<PhpClass>, PhpError> {
        if let Expr::Const(n) | Expr::Str(n) = e {
            let rn = self.resolve_class_name(n);
            if let Some(t) = self.traits.get(&rn.to_lowercase()).cloned() {
                // Traits aren't PhpClass-registered; wrap the decl so
                // find_method_in/late-static binding see the trait's
                // own methods (constexpr/error_static_call_trait_method).
                return Ok(Rc::new(PhpClass {
                    decl: t,
                    statics: std::cell::RefCell::new(std::collections::HashMap::new()),
                    statics_init: std::cell::RefCell::new(false),
                }));
            }
        }
        self.class_of(e)
    }

    /// `name(...)` — fn-name FCC with the same resolution as call_named
    /// (ns fallback for unqualified literals, \u{1} marker).
    fn fcc_named(&mut self, fname: &str) -> Result<Value, PhpError> {
        let (unqualified, lname) = match fname.strip_prefix('\u{1}') {
            Some(n) => (true, n.to_lowercase()),
            None => (false, fname.trim_start_matches('\\').to_lowercase()),
        };
        // Zend resolves unqualified FCC names ns\f -> f once per call
        // site and caches it — a later-conditionally-defined ns\f does
        // NOT rebind existing sites (constexpr/namespace_004).
        let caller = self
            .stack
            .last()
            .map(|f| f.fn_name.clone())
            .unwrap_or_else(|| "{main}".to_string());
        let cache_key = if unqualified {
            Some((caller, self.caller_ns().to_lowercase(), lname.clone()))
        } else {
            None
        };
        if let Some(k) = &cache_key {
            if let Some(hit) = self.fcc_fn_cache.get(k).cloned() {
                return self.fcc_named_emit(hit, fname);
            }
        }
        // Zend resolves unqualified FCC names ns\f -> f at creation;
        // a namespaced user function outranks the global builtin
        // (constexpr/namespace_003).
        let mut resolved: Option<String> = None;
        let mut miss = fname
            .trim_start_matches('\u{1}')
            .trim_start_matches('\\')
            .to_string();
        if unqualified {
            let ns = self.caller_ns();
            if !ns.is_empty() {
                let cand = format!("{}\\{}", ns.to_lowercase(), lname);
                if self.functions.contains_key(&cand) {
                    resolved = Some(cand);
                } else {
                    miss = format!("{}\\{}", ns, fname.trim_start_matches('\u{1}'));
                }
            }
        }
        if resolved.is_none()
            && (self.functions.contains_key(&lname) || builtins::is_builtin(&lname))
        {
            resolved = Some(lname.clone());
        }
        if let Some(k) = cache_key {
            self.fcc_fn_cache.insert(k, resolved.clone());
        }
        self.fcc_named_emit(resolved, &miss)
    }

    fn fcc_named_emit(&mut self, resolved: Option<String>, miss: &str) -> Result<Value, PhpError> {
        match resolved {
            // Function names resolve case-insensitively but display in
            // declared case (ReflectionFunction::getNamespaceName,
            // closure_068).
            Some(r) => Ok(Value::Callable(self.new_callable(PhpCallable {
                id: std::cell::Cell::new(0),
                kind: CallableKind::Named(
                    self.functions.get(&r).map(|d| d.name.clone()).unwrap_or(r),
                ),
                captures: Vec::new(),
                this_obj: None,
                scope_class: None,
                called_class: None,
                is_static: false,
            }))),
            None => self.fail(PhpError::uncaught(
                "Error",
                format!("Call to undefined function {}()", miss),
                0,
            )),
        }
    }

    /// Shared `Closure::bind`/`bindTo`/`call` rebinding model
    /// (closure_036-044/061/063, zend_closures, bug70685):
    /// - binding an instance to a static closure warns → NULL
    /// - unbinding $this warns — "of method" for method-created
    ///   closures, "of closure using $this" otherwise
    /// - explicit scope arg: null → unscoped ("dummy"), object → its
    ///   class, string → resolved class; omitted or 'static' keeps
    ///   the previous scope — an unscoped closure stays unscoped
    ///   ("dummy scope", closure_046)
    /// - internal-class scopes are rejected for everything but
    ///   method-created closures (their scope already is internal)
    /// - fake closures (Named/Method kinds) can't change scope, but
    ///   CAN rebind $this freely (closure_063: silent success)
    ///
    /// Returns Ok(None) after emitting a warning → caller returns NULL.
    fn rebind_closure(
        &mut self,
        c: &PhpCallable,
        new_this: Option<Rc<RefCell<PhpObject>>>,
        scope_arg: Option<Value>,
    ) -> Result<Option<PhpCallable>, PhpError> {
        if new_this.is_some() && c.is_static {
            self.warn(
                "Cannot bind an instance to a static closure, this will be an error in PHP 9",
            )?;
            return Ok(None);
        }
        if new_this.is_none() {
            match &c.kind {
                // Method-created closures carry their target in
                // `kind.obj` — dropping it is the "of method" unbind
                // (closure_061).
                CallableKind::Method { obj: Some(_), .. } => {
                    self.warn(
                        "Cannot unbind $this of method, this will be an error in PHP 9",
                    )?;
                    return Ok(None);
                }
                // "uses $this" is the compile-time body flag, not
                // merely a bound instance — a static-scope closure
                // that references $this but never captured one
                // unbinds quietly (closure_062).
                CallableKind::Closure(d)
                    if c.this_obj.is_some()
                        && Self::body_uses_this(&d.body)
                        && !c.is_static =>
                {
                    self.warn(
                        "Cannot unbind $this of closure using $this, this will be an error in PHP 9",
                    )?;
                    return Ok(None);
                }
                _ => {}
            }
        }
        let scope: Option<Rc<PhpClass>> = match &scope_arg {
            Some(Value::Null) => None,
            Some(Value::Object(o)) => Some(o.borrow().class.clone()),
            Some(Value::Str(s)) => {
                let sn = crate::value::lossy(s).to_string();
                if sn.eq_ignore_ascii_case("static") {
                    // 'static' keeps the previous scope verbatim —
                    // same as omitting the argument (zend's default
                    // IS "static", closure_046).
                    c.scope_class.clone()
                } else {
                    match self
                        .resolve_class(&sn)
                        .and_then(|c| self.classes.get(&c.to_lowercase()).cloned())
                    {
                        Some(c) => Some(c),
                        None => {
                            // Unresolvable scope string — warning +
                            // NULL, not a throw (bug78658).
                            self.warn(&format!("Class \"{}\" not found", sn))?;
                            return Ok(None);
                        }
                    }
                }
            }
            // Omitted — keep the previous scope; an unscoped closure
            // stays on the "dummy scope" (isset() on foreign privates
            // is still false, closure_046).
            None => c.scope_class.clone(),
            // Other arg types were rejected by the caller's TypeError.
            Some(_) => None,
        };
        // Internal classes (decl.file empty) can't be closure scopes —
        // `call()` always resolves scope to the new instance's class,
        // so $x->call($std) hits this (closure_call). Method-kind
        // callables are exempt: their declaring scope is the internal
        // class already (closure_call_internal). This check precedes
        // the per-kind scope warnings — a fake-function closure bound
        // to stdClass reports the internal class (closure_061).
        if !matches!(c.kind, CallableKind::Method { .. }) {
            if let Some(sc) = &scope {
                if sc.decl.file.is_empty() && !sc.name().eq_ignore_ascii_case("closure") {
                    self.warn(&format!(
                        "Cannot bind closure to scope of internal class {}, this will be an error in PHP 9",
                        sc.name()
                    ))?;
                    return Ok(None);
                }
            }
        }
        match &c.kind {
            // A closure created from a function has no scope — only an
            // actual scope change warns; binding $this is silent
            // (bug70630 vs closure_063).
            CallableKind::Named(_)
                if Self::scope_changed(&scope, &c.scope_class) =>
            {
                self.warn(
                    "Cannot rebind scope of closure created from function, this will be an error in PHP 9",
                )?;
                return Ok(None);
            }
            CallableKind::Method { name, .. } => {
                // The new instance must be instanceof the method's
                // DECLARING class — `scope_class` already holds it
                // (SplStack::count → SplDoublyLinkedList, bug70685).
                // Checked before the scope warning: call(new B) rebinds
                // scope AND target yet reports the target
                // (closure_from_callable_rebinding).
                if let Some(t) = &new_this {
                    let tc = t.borrow().class.clone();
                    let dc = c.scope_class.clone().unwrap_or_else(|| tc.clone());
                    if !self.is_a(&tc, dc.name()) {
                        self.warn(&format!(
                            "Cannot bind method {}::{}() to object of class {}, this will be an error in PHP 9",
                            dc.name(),
                            name,
                            tc.name()
                        ))?;
                        return Ok(None);
                    }
                }
                // A method-created closure keeps the declaring scope —
                // resolving to a different class is a rebind
                // (bug70685).
                if Self::scope_changed(&scope, &c.scope_class) {
                    self.warn(
                        "Cannot rebind scope of closure created from method, this will be an error in PHP 9",
                    )?;
                    return Ok(None);
                }
            }
            _ => {}
        }
        let mut nc = (*c).clone();
        nc.this_obj = new_this;
        nc.scope_class = scope.clone();
        nc.called_class = scope.clone();
        // Method-kind callables rebind the invocation target too.
        if let CallableKind::Method { obj, name, .. } = &mut nc.kind {
            if nc.this_obj.is_some() {
                *obj = nc.this_obj.clone();
            } else {
                let _ = name;
            }
        }
        Ok(Some(nc))
    }

    /// Scope comparison for rebind warnings: None-vs-Some counts as
    /// a change (dummy scope is a different scope, closure_061).
    fn scope_changed(a: &Option<Rc<PhpClass>>, b: &Option<Rc<PhpClass>>) -> bool {
        match (a, b) {
            (Some(x), Some(y)) => !x.name().eq_ignore_ascii_case(y.name()),
            (Some(_), None) | (None, Some(_)) => true,
            (None, None) => false,
        }
    }

    /// `Closure::fromCallable($v)` — like fcc_val but: failures throw
    /// TypeError (caller wraps), and the scope keywords
    /// `self`/`parent`/`static` are deprecated yet still resolve
    /// non-static methods against the current `$this`
    /// (closure_from_callable_basic).
    fn callable_to_closure(&mut self, v: &Value) -> Result<Value, PhpError> {
        type Spec = Option<(String, String, Option<Rc<RefCell<PhpObject>>>)>;
        let spec: Spec = match v {
            Value::Str(s) => crate::value::lossy(s)
                .split_once("::")
                .map(|(cn, mn)| (cn.to_string(), mn.to_string(), None)),
            Value::Array(a) => {
                let a = a.borrow();
                let t = a.get(&ArrKey::Int(0));
                let m = a.get(&ArrKey::Int(1));
                match (t, m) {
                    (Some(Value::Str(cn)), Some(mv)) => Some((
                        crate::value::lossy(&cn).to_string(),
                        mv.to_php_string(),
                        None,
                    )),
                    (Some(Value::Object(o)), Some(mv)) => {
                        Some((String::new(), mv.to_php_string(), Some(o.clone())))
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let Some((cn, mn, bound_obj)) = spec else {
            return self.fcc_val(v);
        };
        let kw = cn.to_ascii_lowercase();
        let is_scope_kw = matches!(kw.as_str(), "self" | "parent" | "static");
        if bound_obj.is_none() && is_scope_kw {
            self.deprecated(&format!("Use of \"{}\" in callables is deprecated", kw))?;
        }
        let cls: Option<Rc<PhpClass>> = if let Some(o) = &bound_obj {
            Some(o.borrow().class.clone())
        } else if is_scope_kw {
            let f = self.stack.last();
            let scope = f.and_then(|f| f.decl_class.clone().or(f.scope_class.clone()));
            match kw.as_str() {
                "self" => scope,
                "parent" => scope.and_then(|s| {
                    s.decl
                        .parent
                        .as_ref()
                        .and_then(|p| self.classes.get(&p.to_lowercase()).cloned())
                }),
                "static" => f.and_then(|f| f.called_class.clone()).or(scope),
                _ => None,
            }
        } else {
            self.resolve_class(&cn)
                .and_then(|c| self.classes.get(&c.to_lowercase()).cloned())
        };
        let Some(cls) = cls else {
            return self.fcc_val(v);
        };
        let Some((m, dc)) = self.find_method_in(&cls, &mn) else {
            if bound_obj.is_none() {
                return self.fcc_static(cls, &mn);
            }
            return self.fcc_method(&Value::Object(bound_obj.unwrap()), &mn);
        };
        if m.is_abstract {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot call abstract method {}::{}()", dc.name(), mn),
                0,
            ));
        }
        self.fcc_vis_check(&m, &dc)?;
        if m.is_static || bound_obj.is_some() {
            if bound_obj.is_none() {
                return self.fcc_static(cls, &mn);
            }
            return Ok(Value::Callable(self.new_callable(PhpCallable {
                id: std::cell::Cell::new(0),
                kind: CallableKind::Method {
                    obj: bound_obj.clone(),
                    class: None,
                    name: mn.to_string(),
                },
                captures: Vec::new(),
                this_obj: bound_obj.clone(),
                scope_class: Some(dc.clone()),
                called_class: Some(dc),
                is_static: false,
            })));
        }
        // Scope-keyword callable to a non-static method binds the
        // current `$this` when it's an instance of the class.
        let this = self
            .stack
            .last()
            .and_then(|f| f.this_obj.clone())
            .filter(|o| {
                let cname = o.borrow().class.name().to_string();
                self.is_a_str(&cname, cls.name())
            });
        let Some(this) = this else {
            return self.fcc_static(cls, &mn);
        };
        Ok(Value::Callable(self.new_callable(PhpCallable {
            id: std::cell::Cell::new(0),
            kind: CallableKind::Method {
                obj: Some(this.clone()),
                class: None,
                name: mn.to_string(),
            },
            captures: Vec::new(),
            this_obj: Some(this),
            scope_class: Some(dc.clone()),
            called_class: Some(dc),
            is_static: false,
        })))
    }

    /// Any value → callable coercion for FCC (`$fn(...)`, `($c)(...)`,
    /// `[$o,'m'](...)`). Non-callables throw `Error` (Zend "not callable").
    fn fcc_val(&mut self, v: &Value) -> Result<Value, PhpError> {
        match v {
            Value::Callable(_) => Ok(v.clone()),
            Value::Str(s) => {
                let name = crate::value::lossy(s);
                let name = name.trim_start_matches('\\');
                if let Some((cls, m)) = name.split_once("::") {
                    if let Some(rcn) = self.resolve_class(cls) {
                        if let Some(c) = self.classes.get(&rcn.to_lowercase()).cloned() {
                            return self.fcc_static(c, m);
                        }
                    }
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Class \"{}\" not found", cls),
                        0,
                    ));
                }
                if self.functions.contains_key(&name.to_lowercase())
                    || builtins::is_builtin(&name.to_lowercase())
                {
                    Ok(Value::Callable(self.new_callable(PhpCallable {
                        id: std::cell::Cell::new(0),
                        kind: CallableKind::Named(name.to_lowercase()),
                        captures: Vec::new(),
                        this_obj: None,
                        scope_class: None,
                        called_class: None,
                        is_static: false,
                    })))
                } else {
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Call to undefined function {}()", name),
                        0,
                    ))
                }
            }
            Value::Object(o) => {
                let icls = o.borrow().class.clone();
                if self.find_method_in(&icls, "__invoke").is_some() {
                    Ok(Value::Callable(self.new_callable(PhpCallable {
                        id: std::cell::Cell::new(0),
                        kind: CallableKind::Method {
                            obj: Some(o.clone()),
                            class: None,
                            name: "__invoke".to_string(),
                        },
                        captures: Vec::new(),
                        this_obj: Some(o.clone()),
                        scope_class: Some(o.borrow().class.clone()),
                        called_class: Some(o.borrow().class.clone()),
                        is_static: false,
                    })))
                } else {
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Object of type {} is not callable", o.borrow().class.name()),
                        0,
                    ))
                }
            }
            Value::Array(a) => {
                let a = a.borrow();
                let t = a.get(&ArrKey::Int(0));
                let m = a.get(&ArrKey::Int(1));
                match (t, m) {
                    (Some(Value::Object(o)), Some(mv)) => {
                        let mn = mv.to_php_string();
                        self.fcc_method(&Value::Object(o.clone()), &mn)
                    }
                    // `[$closure, '__invoke']` — a closure is callable
                    // (bug78689).
                    (Some(Value::Callable(c)), Some(mv)) => {
                        let mn = mv.to_php_string();
                        if mn.eq_ignore_ascii_case("__invoke") {
                            Ok(Value::Callable(c.clone()))
                        } else {
                            self.fail(PhpError::uncaught(
                                "Error",
                                "Value of type array is not callable",
                                0,
                            ))
                        }
                    }
                    (Some(Value::Str(cn)), Some(mv)) => {
                        let mn = mv.to_php_string();
                        match self
                            .resolve_class(&crate::value::lossy(&cn))
                            .and_then(|c| self.classes.get(&c.to_lowercase()).cloned())
                        {
                            Some(c) => self.fcc_static(c, &mn),
                            None => self.fail(PhpError::uncaught(
                                "Error",
                                format!("Class \"{}\" not found", crate::value::lossy(&cn)),
                                0,
                            )),
                        }
                    }
                    _ => self.fail(PhpError::uncaught(
                        "Error",
                        format!(
                            "Value of type {} is not callable",
                            v.type_name().to_lowercase()
                        ),
                        0,
                    )),
                }
            }
            _ => self.fail(PhpError::uncaught(
                "Error",
                format!(
                    "Value of type {} is not callable",
                    v.type_name().to_lowercase()
                ),
                0,
            )),
        }
    }

    /// `$obj->method(...)` — bound method closure. Visibility is checked
    /// at creation from the calling scope (zend_closures).
    fn fcc_method(&mut self, ov: &Value, mn: &str) -> Result<Value, PhpError> {
        let o = match ov {
            Value::Object(o) => o.clone(),
            Value::Callable(_) if mn.eq_ignore_ascii_case("__invoke") => {
                // `$closure->__invoke(...)` — the closure itself.
                return Ok(ov.clone());
            }
            other => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Call to a member function {}() on {}",
                        mn,
                        other.type_name().to_lowercase()
                    ),
                    0,
                ))
            }
        };
        let cls = o.borrow().class.clone();
        match self.find_method_in(&cls, mn) {
            Some((m, dc)) => {
                if m.is_abstract {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot call abstract method {}::{}()", dc.name(), mn),
                        0,
                    ));
                }
                self.fcc_vis_check(&m, &dc)?;
                Ok(Value::Callable(self.new_callable(PhpCallable {
                    id: std::cell::Cell::new(0),
                    kind: CallableKind::Method {
                        obj: Some(o.clone()),
                        class: None,
                        name: mn.to_string(),
                    },
                    captures: Vec::new(),
                    this_obj: Some(o),
                    scope_class: Some(dc.clone()),
                    called_class: Some(dc),
                    is_static: false,
                })))
            }
            None => {
                if self.find_method_in(&cls, "__call").is_some() {
                    // `Foo::doesNotExist` routes through __call at call
                    // time (first_class_callable_005).
                    Ok(Value::Callable(self.new_callable(PhpCallable {
                        id: std::cell::Cell::new(0),
                        kind: CallableKind::Method {
                            obj: Some(o.clone()),
                            class: None,
                            name: mn.to_string(),
                        },
                        captures: Vec::new(),
                        this_obj: Some(o),
                        scope_class: Some(cls.clone()),
                        called_class: Some(cls),
                        is_static: false,
                    })))
                } else {
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Call to undefined method {}::{}()", cls.name(), mn),
                        0,
                    ))
                }
            }
        }
    }

    /// `C::method(...)` — static method closure; non-static methods fail
    /// "cannot be called statically" at creation (Error, catchable).
    fn fcc_static(&mut self, cls: Rc<PhpClass>, mn: &str) -> Result<Value, PhpError> {
        match self.find_method_in(&cls, mn) {
            Some((m, dc)) => {
                if m.is_abstract {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot call abstract method {}::{}()", dc.name(), mn),
                        0,
                    ));
                }
                if !m.is_static {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!(
                            "Non-static method {}::{}() cannot be called statically",
                            cls.name(),
                            mn
                        ),
                        0,
                    ));
                }
                // `Foo::m(...)` where Foo is a trait: allowed but
                // deprecated outside a using class (8.4+;
                // constexpr/error_static_call_trait_method).
                if self.traits.contains_key(&dc.name().to_lowercase()) {
                    self.deprecated(&format!(
                        "Calling static trait method {}::{} is deprecated, it should only be called on a class using the trait",
                        dc.name(),
                        m.decl.name
                    ))?;
                }
                self.fcc_vis_check(&m, &dc)?;
                Ok(Value::Callable(self.new_callable(PhpCallable {
                    id: std::cell::Cell::new(0),
                    // `class` is the called class for late static
                    // binding (`Bar::method(...)` -> static::class
                    // is Bar; first_class_callable_010).
                    kind: CallableKind::Method {
                        obj: None,
                        class: Some(cls.clone()),
                        name: mn.to_string(),
                    },
                    captures: Vec::new(),
                    this_obj: None,
                    scope_class: Some(dc),
                    called_class: Some(cls),
                    is_static: true,
                })))
            }
            None => {
                if self.find_method_in(&cls, "__callstatic").is_some() {
                    if self.in_const_expr > 0 {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Creating a callable for the magic __callStatic() method is not supported in constant expressions",
                            0,
                        ));
                    }
                    Ok(Value::Callable(self.new_callable(PhpCallable {
                        id: std::cell::Cell::new(0),
                        kind: CallableKind::Method {
                            obj: None,
                            class: Some(cls.clone()),
                            name: mn.to_string(),
                        },
                        captures: Vec::new(),
                        this_obj: None,
                        scope_class: Some(cls.clone()),
                        called_class: Some(cls),
                        is_static: true,
                    })))
                } else {
                    self.fail(PhpError::uncaught(
                        "Error",
                        format!("Call to undefined method {}::{}()", cls.name(), mn),
                        0,
                    ))
                }
            }
        }
    }

    /// FCC visibility gate (zend_closures): checked at creation from the
    /// calling scope; `const_self` covers class-const initializers.
    fn fcc_vis_check(&mut self, m: &MethodDecl, dc: &Rc<PhpClass>) -> Result<(), PhpError> {
        let scope = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned())
            .or_else(|| self.const_self.clone());
        let ok = match m.visibility {
            crate::ast::Visibility::Public => true,
            crate::ast::Visibility::Private => scope
                .as_ref()
                .map(|s| s.name() == dc.name())
                .unwrap_or(false),
            crate::ast::Visibility::Protected => scope
                .as_ref()
                .map(|s| {
                    let proto = self.method_prototype(dc, &m.decl.name.to_lowercase());
                    self.is_a_str(s.name(), dc.name())
                        || self.is_a_str(dc.name(), s.name())
                        || self.is_a_str(s.name(), &proto)
                })
                .unwrap_or(false),
        };
        if ok {
            return Ok(());
        }
        let vis = match m.visibility {
            crate::ast::Visibility::Public => "public",
            crate::ast::Visibility::Protected => "protected",
            crate::ast::Visibility::Private => "private",
        };
        let from = match &scope {
            Some(s) => format!("scope {}", s.name()),
            None => "global scope".to_string(),
        };
        self.fail(PhpError::uncaught(
            "Error",
            format!(
                "Call to {} method {}::{}() from {}",
                vis,
                dc.name(),
                m.decl.name,
                from
            ),
            0,
        ))
    }

    /// Compile-time constant-expression eval: FCC shape rules apply,
    /// `self`/`parent` bind to `const_self`, magic __callStatic is
    /// rejected (constexpr/*).
    fn eval_const(&mut self, e: &Expr) -> Result<Value, PhpError> {
        self.const_fcc_check(e)?;
        self.in_const_expr += 1;
        let r = self.eval(e);
        self.in_const_expr -= 1;
        r
    }

    /// Scalar-literal callees (`(0)(...)`, `(1.5)(...)`) fail "Illegal
    /// function name"; everything else non-literal is `msg`.
    fn const_scalar_callee(&self, e: &Expr, msg: &str) -> Result<(), PhpError> {
        match e {
            Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::Null => {
                Err(PhpError::fatal("Illegal function name", self.cur_line))
            }
            _ => Err(PhpError::fatal(msg, self.cur_line)),
        }
    }

    /// FCC-in-constant-expression shape rules (zend_compile): the callee
    /// must be a literal function name or `LiteralClass::literalMethod`.
    /// Recursed so FCCs nested in const exprs get the same check.
    fn const_fcc_check(&self, e: &Expr) -> Result<(), PhpError> {
        match e {
            Expr::Fcc(inner) => match inner.as_ref() {
                Expr::Call { name, .. } => match name.as_ref() {
                    Expr::Str(_) => Ok(()),
                    Expr::Paren(p) => self.const_scalar_callee(
                        p.as_ref(),
                        "Cannot use dynamic function name in constant expression",
                    ),
                    other => self.const_scalar_callee(
                        other,
                        "Cannot use dynamic function name in constant expression",
                    ),
                },
                Expr::StaticCall { class, .. } | Expr::StaticCallDyn { class, .. } => {
                    match class.as_ref() {
                        Expr::Const(c) | Expr::Str(c) => {
                            if c.eq_ignore_ascii_case("static") {
                                Err(PhpError::fatal(
                                    "\"static\" is not allowed in compile-time constants",
                                    self.cur_line,
                                ))
                            } else {
                                Ok(())
                            }
                        }
                        _ => Err(PhpError::fatal(
                            "Constant expression contains invalid operations",
                            self.cur_line,
                        )),
                    }
                }
                _ => Err(PhpError::fatal(
                    "Constant expression contains invalid operations",
                    self.cur_line,
                )),
            },
            Expr::Paren(inner) | Expr::Assign { value: inner, .. } => self.const_fcc_check(inner),
            Expr::Binary { l, r, .. } => {
                self.const_fcc_check(l)?;
                self.const_fcc_check(r)
            }
            Expr::Unary { e, .. } => self.const_fcc_check(e),
            Expr::Ternary { c, t, f, .. } => {
                self.const_fcc_check(c)?;
                if let Some(t) = t.as_ref() {
                    self.const_fcc_check(t)?;
                }
                self.const_fcc_check(f)
            }
            Expr::Index { e, i, .. } => {
                self.const_fcc_check(e)?;
                if let Some(i) = i.as_ref() {
                    self.const_fcc_check(i)?;
                }
                Ok(())
            }
            Expr::ArrayLit(items) => {
                for (k, v) in items {
                    if let Some(k) = k {
                        self.const_fcc_check(k)?;
                    }
                    self.const_fcc_check(v)?;
                }
                Ok(())
            }
            Expr::Call { name, args, .. } => {
                self.const_fcc_check(name)?;
                for a in args {
                    self.const_fcc_check(a)?;
                }
                Ok(())
            }
            Expr::MethodCall { obj, args, .. } => {
                self.const_fcc_check(obj)?;
                for a in args {
                    self.const_fcc_check(a)?;
                }
                Ok(())
            }
            Expr::StaticCall { class, args, .. } | Expr::StaticCallDyn { class, args, .. } => {
                self.const_fcc_check(class)?;
                for a in args {
                    self.const_fcc_check(a)?;
                }
                Ok(())
            }
            Expr::New { class, args, .. } => {
                self.const_fcc_check(class)?;
                for a in args {
                    self.const_fcc_check(a)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// spl_autoload: invoke each registered loader until `name`
    /// resolves (resolve_class/class_of retry on miss).
    /// An exception thrown by an autoloader propagates to the code that
    /// triggered the load (PHP stops the chain on throw).
    pub fn run_autoload(&mut self, name: &str) -> Result<(), PhpError> {
        let key = name.trim_start_matches('\\').to_lowercase();
        if !self.autoloading.insert(key.clone()) {
            return Ok(());
        }
        let fns = self.autoload_fns.clone();
        let mut res = Ok(());
        for f in fns {
            if let Err(e) = self.call_value(&f, CallArgs::positional(vec![cell(Value::str(name))]))
            {
                res = Err(e);
                break;
            }
            if self.classes.contains_key(&name.to_lowercase()) {
                break;
            }
        }
        self.autoloading.remove(&key);
        // A throwable escaping an autoloader while variance obligations
        // are pending leaves the in-progress class half-linked — Zend
        // falls back to a fatal naming the class being inherited
        // (variance/loading_exception*).
        if let Err(e) = &res {
            if e.kind == crate::error::ErrorKind::Throw && !self.variance_obligations.is_empty() {
                if let Some(Value::Object(o)) = &self.pending_exception {
                    let (cls, msg, file, line, tr) = {
                        let ob = o.borrow();
                        let msg = ob
                            .props
                            .get("message")
                            .map(|v| v.borrow().to_php_string())
                            .unwrap_or_default();
                        let (file, line, tr) = match &ob.internal {
                            Some(ObjectInternal::Exception {
                                file,
                                line,
                                trace,
                                frames,
                                ..
                            }) => (
                                file.clone(),
                                *line as usize,
                                if !trace.is_empty() {
                                    trace.clone()
                                } else {
                                    crate::value::format_trace(frames)
                                },
                            ),
                            _ => (self.diag_file(), e.line, "#0 {main}".to_string()),
                        };
                        (ob.class.name().to_string(), msg, file, line, tr)
                    };
                    self.pending_exception = None;
                    let outer = self
                        .declaring
                        .last()
                        .map(|d| d.name.clone())
                        .unwrap_or_default();
                    return Err(PhpError::fatal(
                        format!(
                            "During inheritance of {outer} with variance dependencies: Uncaught {cls}: {msg} in {file}:{line}\nStack trace:\n{}",
                            tr.trim_end()
                        ),
                        self.cur_line,
                    ));
                }
            }
        }
        res
    }
    /// Params binding + body run for a pushed frame context (closures).
    fn bind_and_run(
        &mut self,
        decl: &FunctionDecl,
        args: CallArgs,
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
            // Call-site file = the frame below the callee (the caller's
            // executing file); top-level calls report the file currently
            // being run.
            let sf = self
                .stack
                .iter()
                .rev()
                .nth(1)
                .map(|f| f.file.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| self.cur_file.clone());
            (sf, saved_line as u32)
        };
        // Trace args are the send list normalized through the last
        // bound slot (unbound params render null; named args appear in
        // declaration order — `test3(NULL, 'B')` in named_params/defaults).
        // Named args collected by a variadic stay keyed
        // (`test(1, 2, x: 3, y: 4)` in named_params/backtrace).
        let mut targs_named: Vec<(String, Cell)> = Vec::new();
        let targs: Vec<Cell> = if args.named.is_empty() {
            args.cells.clone()
        } else {
            let mut last: i64 = -1;
            for (i, p) in decl.params.iter().enumerate() {
                if p.variadic {
                    break;
                }
                if args.cells.get(i).is_some() || args.named.iter().any(|(n, ..)| *n == p.name) {
                    last = i as i64;
                }
            }
            let mut t: Vec<Cell> = Vec::new();
            for (i, p) in decl.params.iter().enumerate() {
                if p.variadic || i as i64 > last {
                    break;
                }
                let c = args
                    .cells
                    .get(i)
                    .cloned()
                    .or_else(|| {
                        args.named
                            .iter()
                            .find(|(n, ..)| *n == p.name)
                            .map(|(_, c, ..)| c.clone())
                    })
                    .unwrap_or_else(|| cell(Value::Null));
                t.push(c);
            }
            if decl.params.iter().any(|p| p.variadic) {
                // Variadic: every sent positional shows up; named args
                // that matched no declared param stay keyed.
                for (i, c) in args.cells.iter().enumerate() {
                    if i >= t.len() {
                        t.push(c.clone());
                    }
                }
                for (n, c, ..) in &args.named {
                    if !decl.params.iter().any(|p| !p.variadic && p.name == *n) {
                        targs_named.push((n.clone(), c.clone()));
                    }
                }
            }
            t
        };
        let fr = self
            .stack
            .last()
            .map(|f| TraceFrame {
                function: if f.fn_name.starts_with("{closure:") {
                    format!("{{closure:{}:{}}}", f.file, f.fn_line)
                } else {
                    f.fn_name.clone()
                },
                // A closure bound to $this without a real scope runs
                // on the "dummy scope" — traces show `Closure->`
                // (closure_038).
                class: f
                    .scope_class
                    .as_ref()
                    .map(|c| c.name().to_string())
                    .or_else(|| {
                        if f.fn_name.starts_with("{closure:") && f.this_obj.is_some() {
                            Some("Closure".to_string())
                        } else {
                            None
                        }
                    }),
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
                args: targs.clone(),
                named_args: targs_named.clone(),
                internal: false,
            })
            .unwrap_or_else(|| TraceFrame {
                function: decl.name.clone(),
                class: None,
                ty: String::new(),
                file: site_file,
                line: site_line,
                args: targs,
                named_args: targs_named,
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
        // Zend decrefs the frame's CVs at unwind — a local object
        // whose last strong refs are that frame's cells runs its
        // __destruct now (bug52361).
        if let Some(f) = self.last_popped_frame.take() {
            let _ = self.destruct_frame_objs(&f);
        }
        r
    }

    /// PHP's compile-time checks on typed params (tests/lang/type_hints_*):
    /// `= null` on a non-nullable type is the implicit-nullable deprecation;
    /// a scalar literal default on a class type is a fatal.
    /// `#[ReturnTypeWillChange]` is method-only — any other target is
    /// a compile fatal (variance/return_type_will_change_*).
    fn check_rtwc_attr(
        &self,
        attrs: &[crate::ast::AttrDecl],
        target: &str,
    ) -> Result<(), PhpError> {
        for a in attrs {
            let short = a.name.rsplit('\\').next().unwrap_or(&a.name);
            if short.eq_ignore_ascii_case("ReturnTypeWillChange") {
                return Err(PhpError::compile_fatal(
                    format!(
                        "Attribute \"ReturnTypeWillChange\" cannot target {} (allowed targets: method)",
                        target
                    ),
                    a.line,
                ));
            }
        }
        Ok(())
    }

    fn decl_type_checks(
        &mut self,
        fname: &str,
        decl: &FunctionDecl,
        cls_ctx: Option<(&str, Option<String>)>,
    ) -> Result<(), PhpError> {
        if cls_ctx.is_none() {
            self.check_rtwc_attr(&decl.attrs, "function")?;
        }
        let builtins = [
            "int", "float", "string", "bool", "array", "object", "callable", "iterable", "mixed",
            "void", "never", "false", "true", "self", "parent", "static", "null",
        ];
        let saved = self.cur_line;
        for p in &decl.params {
            let Some(ty) = &p.ty else { continue };
            self.cur_line = decl.line;
            // `mixed` already spans null (and `?mixed` is a parse error),
            // so `mixed $x = null` is never the implicit-nullable case.
            let nullable = ty
                .iter()
                .any(|m| m.eq_ignore_ascii_case("null") || m.eq_ignore_ascii_case("mixed"));
            let null_default = match &p.default {
                Some(Expr::Null) => true,
                Some(Expr::Const(c)) => c.eq_ignore_ascii_case("null"),
                _ => false,
            };
            match &p.default {
                _ if null_default => {
                    if !nullable
                        && self
                            .dep_seen
                            .insert(format!("{}\0{}\0{}", decl.file, decl.line, p.name))
                    {
                        self.deprecated(&format!(
                            "{}(): Implicitly marking parameter ${} as nullable is deprecated, the explicit nullable type must be used instead",
                            fname, p.name
                        ))?;
                    }
                }
                Some(Expr::Int(_))
                | Some(Expr::Float(_))
                | Some(Expr::Str(_))
                | Some(Expr::Bool(_))
                | Some(Expr::Interp(_)) => {
                    // An Interp made of only literal parts is still a
                    // string literal default (`"x"` lexes as Interp);
                    // one with real interpolations isn't a literal.
                    let literal_interp = !matches!(
                        &p.default,
                        Some(Expr::Interp(parts)) if !parts
                            .iter()
                            .all(|p| matches!(p, crate::lexer::StringPart::Lit(_)))
                    );
                    let kind = match p.default {
                        Some(Expr::Int(_)) => "int",
                        Some(Expr::Float(_)) => "float",
                        Some(Expr::Str(_)) | Some(Expr::Interp(_)) => "string",
                        _ => "bool",
                    };
                    if !literal_interp {
                        if let Some(ty) = &p.ty {
                            if let Err(e) = self.check_ty_redundant(ty, &cls_ctx) {
                                self.cur_line = saved;
                                return Err(e);
                            }
                        }
                        continue;
                    }
                    let mut disp = Self::zpp_ty_disp(ty);
                    disp.retain(|m| !m.eq_ignore_ascii_case("null"));
                    let tn = disp.join("|");
                    // A literal default must satisfy a member EXACTLY —
                    // the only widening is int -> float
                    // (scalar_float_with_invalid_default).
                    let lit_ok = disp
                        .iter()
                        .all(|m| builtins.contains(&m.to_lowercase().as_str()))
                        && (disp.iter().any(|m| m.eq_ignore_ascii_case("mixed"))
                            || match kind {
                                "int" => disp.iter().any(|m| {
                                    m.eq_ignore_ascii_case("int") || m.eq_ignore_ascii_case("float")
                                }),
                                "float" => disp.iter().any(|m| m.eq_ignore_ascii_case("float")),
                                "string" => disp.iter().any(|m| m.eq_ignore_ascii_case("string")),
                                _ => disp.iter().any(|m| m.eq_ignore_ascii_case("bool")),
                            });
                    if !lit_ok {
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
            if let Some(ty) = &p.ty {
                if let Err(e) = self.check_ty_redundant(ty, &cls_ctx) {
                    self.cur_line = saved;
                    return Err(e);
                }
            }
        }
        if let Some(ty) = &decl.ret {
            if let Err(e) = self.check_ty_redundant(ty, &cls_ctx) {
                self.cur_line = saved;
                return Err(e);
            }
            // `return;` (or any `return` under `never`) is a compile
            // error in typed functions — generators are exempt
            // (typed_return_without_value, never).
            if !Self::decl_contains_yield(&decl.body) {
                let never = ty.iter().any(|m| m.eq_ignore_ascii_case("never"));
                let void = ty.iter().all(|m| m.eq_ignore_ascii_case("void"));
                if never {
                    if let Some(l) = Self::first_return_line(&decl.body, decl.line, false) {
                        self.cur_line = saved;
                        return Err(PhpError::compile_fatal(
                            "A never-returning function must not return",
                            l,
                        ));
                    }
                } else if !void {
                    if let Some(l) = Self::first_return_line(&decl.body, decl.line, true) {
                        self.cur_line = saved;
                        let hint = if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) {
                            " (did you mean \"return null;\" instead of \"return;\"?)"
                        } else {
                            ""
                        };
                        return Err(PhpError::compile_fatal(
                            format!("A function with return type must return a value{hint}"),
                            l,
                        ));
                    }
                }
            }
        }
        self.cur_line = saved;
        Ok(())
    }

    /// First `return`'s line within a body — `bare_only` restricts to
    /// value-less `return;`. Nested function/closure/class bodies are
    /// their own scope and skipped (typed_return_without_value).
    fn first_return_line(stmts: &[Stmt], mut cur: usize, bare_only: bool) -> Option<usize> {
        for s in stmts {
            match s {
                Stmt::Line(l) => cur = *l,
                Stmt::Return(e) if e.is_none() || !bare_only => return Some(cur),
                Stmt::Return(_) => {}
                Stmt::Block(b) => {
                    if let Some(l) = Self::first_return_line(b, cur, bare_only) {
                        return Some(l);
                    }
                }
                Stmt::If { then, else_, .. } => {
                    if let Some(l) = Self::first_return_line(then, cur, bare_only)
                        .or_else(|| Self::first_return_line(else_, cur, bare_only))
                    {
                        return Some(l);
                    }
                }
                Stmt::While { body, .. }
                | Stmt::DoWhile { body, .. }
                | Stmt::For { body, .. }
                | Stmt::Foreach { body, .. } => {
                    if let Some(l) = Self::first_return_line(body, cur, bare_only) {
                        return Some(l);
                    }
                }
                Stmt::Switch { cases, .. } => {
                    for (_, b) in cases {
                        if let Some(l) = Self::first_return_line(b, cur, bare_only) {
                            return Some(l);
                        }
                    }
                }
                Stmt::Try {
                    body,
                    catches,
                    finally,
                } => {
                    if let Some(l) = Self::first_return_line(body, cur, bare_only)
                        .or_else(|| {
                            catches
                                .iter()
                                .find_map(|c| Self::first_return_line(&c.body, cur, bare_only))
                        })
                        .or_else(|| {
                            finally
                                .as_ref()
                                .and_then(|b| Self::first_return_line(b, cur, bare_only))
                        })
                    {
                        return Some(l);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Union-type redundancy rules at decl time (PHP 8.x compile
    /// checks): iterable expands to array|Traversable; self/parent/
    /// static resolve against the declaring class; reports the member
    /// as written (or the expanded member it collides with).
    fn check_ty_redundant(
        &mut self,
        ty: &[String],
        cls: &Option<(&str, Option<String>)>,
    ) -> Result<(), PhpError> {
        // Confusable-type warnings (`integer`/`double`/`boolean`/
        // `resource` as class names) are a compile-time diagnostic
        // emitted by the parser — it owns the written-vs-resolved
        // distinction and the use-import table (confusable_type_warning).
        // Intersection conjuncts may only be class-like names — any
        // builtin scalar/compound member is a compile error
        // (invalid_types/*).
        const NON_CLASS: &[&str] = &[
            "int", "float", "string", "bool", "array", "object", "callable", "iterable", "mixed",
            "void", "never", "false", "true", "null", "numeric", "resource",
        ];
        for m in ty {
            if !m.contains('&') {
                continue;
            }
            for part in m.split('&') {
                let p = part.trim_start_matches('\\');
                if NON_CLASS.contains(&p.to_lowercase().as_str()) {
                    return Err(PhpError::fatal(
                        format!("Type {p} cannot be part of an intersection type"),
                        self.cur_line,
                    ));
                }
            }
        }
        // Conjunct-level dedupe inside an intersection member:
        // `A&A` (or `A&B` where B aliases A via `use`) is redundant.
        for m in ty {
            if !m.contains('&') {
                continue;
            }
            let mut conj: Vec<String> = Vec::new();
            for c in m.split('&') {
                if conj.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                    return Err(PhpError::fatal(
                        format!("Duplicate type {} is redundant", c),
                        self.cur_line,
                    ));
                }
                conj.push(c.to_string());
            }
        }
        if ty.len() < 2 {
            return Ok(());
        }
        let builtins = [
            "int",
            "float",
            "string",
            "bool",
            "array",
            "object",
            "callable",
            "iterable",
            "mixed",
            "void",
            "never",
            "false",
            "true",
            "null",
            "traversable",
        ];
        // `T|object` — a class member (incl. an intersection of
        // classes) alongside `object` is redundant
        // (dnf_types/redundant_types/object_and_dnf_type).
        if ty.iter().any(|m| m.eq_ignore_ascii_case("object"))
            && ty
                .iter()
                .any(|m| !builtins.contains(&m.to_lowercase().as_str()))
        {
            return Err(PhpError::fatal(
                format!(
                    "Type {} contains both object and a class type, which is redundant",
                    ty_norm_disp(ty)
                ),
                self.cur_line,
            ));
        }
        // `A&B|A` / `(A&B&C)|(A&B)` — an intersection member is
        // redundant when another member already covers it by name
        // (less_restrive_type_constraint_already_present*).
        for (i, m) in ty.iter().enumerate() {
            if !m.contains('&') {
                continue;
            }
            let conj: Vec<&str> = m.split('&').collect();
            for (j, s) in ty.iter().enumerate() {
                if i == j {
                    continue;
                }
                // Identical members are the seen-loop's "redundant
                // with" case, not the restrictive one.
                if m.eq_ignore_ascii_case(s) {
                    continue;
                }
                let covers = if s.contains('&') {
                    s.split('&')
                        .all(|sc| conj.iter().any(|c| c.eq_ignore_ascii_case(sc)))
                } else {
                    conj.iter().any(|c| c.eq_ignore_ascii_case(s))
                };
                if covers {
                    return Err(PhpError::fatal(
                        format!(
                            "Type {} is redundant as it is more restrictive than type {}",
                            m, s
                        ),
                        self.cur_line,
                    ));
                }
            }
        }
        let mut seen: Vec<(String, String)> = Vec::new();
        let mut seen_true = false;
        let mut seen_false = false;
        // Zend dedupes builtin scalars before class names: for
        // `iterable|iterable` the reported dup is `array`, not
        // `Traversable` (iterable_alias_redundancy_iterable).
        const DEDUP_BUILTINS: &[&str] = &[
            "int", "float", "string", "bool", "array", "object", "callable", "iterable", "mixed",
            "void", "never", "false", "true", "null",
        ];
        let ordered: Vec<&String> = ty
            .iter()
            .filter(|m| DEDUP_BUILTINS.contains(&m.to_lowercase().as_str()))
            .chain(
                ty.iter()
                    .filter(|m| !DEDUP_BUILTINS.contains(&m.to_lowercase().as_str())),
            )
            .collect();
        for m in ordered {
            let l = m.to_lowercase();
            let exps: Vec<String> = if l == "iterable" {
                vec!["array".into(), "Traversable".into()]
            } else {
                vec![m.clone()]
            };
            for e in exps {
                // self/parent resolve for comparison only — `static`
                // stays itself (`static|self` is not redundant;
                // static_to_self_to_unions).
                let cmp = {
                    let el = e.to_lowercase();
                    match (el.as_str(), cls.as_ref()) {
                        ("self", Some((c, _))) => c.to_string(),
                        ("parent", Some((_, p))) => p.clone().unwrap_or_else(|| e.clone()),
                        _ => e.clone(),
                    }
                };
                let dup = seen.iter().any(|(s, _)| {
                    s.eq_ignore_ascii_case(&cmp)
                        || (cmp.eq_ignore_ascii_case("false") || cmp.eq_ignore_ascii_case("true"))
                            && s.eq_ignore_ascii_case("bool")
                        || cmp.eq_ignore_ascii_case("closure") && s.eq_ignore_ascii_case("callable")
                });
                if dup {
                    // Identical intersection members report differently
                    // from plain dups (duplicate_class_alias_type).
                    if cmp.contains('&') {
                        let other = seen
                            .iter()
                            .find(|(s, _)| s.eq_ignore_ascii_case(&cmp))
                            .map(|(_, w)| w.clone())
                            .unwrap_or_else(|| e.clone());
                        return Err(PhpError::fatal(
                            format!("Type {} is redundant with type {}", e, other),
                            self.cur_line,
                        ));
                    }
                    if cmp.eq_ignore_ascii_case("null") {
                        return Err(PhpError::fatal(
                            "null cannot be marked as nullable".to_string(),
                            self.cur_line,
                        ));
                    }
                    let el = e.to_lowercase();
                    let builtin = [
                        "int", "float", "string", "bool", "array", "object", "callable",
                        "iterable", "mixed", "void", "never", "false", "true", "null",
                    ]
                    .contains(&el.as_str());
                    let shown = if el == "static" {
                        e.clone()
                    } else if ["self", "parent"].contains(&el.as_str()) {
                        cmp.clone()
                    } else if builtin {
                        el.clone()
                    } else {
                        e.clone()
                    };
                    return Err(PhpError::fatal(
                        format!("Duplicate type {} is redundant", shown),
                        self.cur_line,
                    ));
                }
                if cmp.eq_ignore_ascii_case("true") {
                    seen_true = true;
                }
                if cmp.eq_ignore_ascii_case("false") {
                    seen_false = true;
                }
                seen.push((cmp, e));
            }
        }
        if seen_true && seen_false {
            return Err(PhpError::fatal(
                "Type contains both true and false, bool must be used instead".to_string(),
                self.cur_line,
            ));
        }
        Ok(())
    }

    /// `static` members resolve to the called class for checks and
    /// messages; unbound (unscoped closure) stays literal `static`
    /// (static_type_return).
    fn resolve_static(&self, ty: &[String]) -> Vec<String> {
        if !ty.iter().any(|m| m.eq_ignore_ascii_case("static")) {
            return ty.to_vec();
        }
        let cn = self
            .stack
            .last()
            .and_then(|f| {
                f.called_class
                    .as_ref()
                    .or(f.scope_class.as_ref())
                    .or(f.decl_class.as_ref())
            })
            .map(|c| c.name().to_string());
        ty.iter()
            .map(|m| {
                if m.eq_ignore_ascii_case("static") {
                    cn.clone().unwrap_or_else(|| m.clone())
                } else {
                    m.clone()
                }
            })
            .collect()
    }

    /// Scalar literal default check context ends; whether `v` satisfies a
    /// type member — scalar builtins pass (weak-mode coercion territory).
    fn param_type_match(&mut self, m: &str, v: &Value) -> bool {
        // Intersection member `A&B`: every part must match
        // (intersection_types/variance).
        if m.contains('&') && !m.starts_with('(') {
            let parts: Vec<String> = m.split('&').map(|p| p.to_string()).collect();
            return parts.iter().all(|p| self.param_type_match(p, v));
        }
        let l = m.to_lowercase();
        match l.as_str() {
            "null" => matches!(v, Value::Null),
            "mixed" | "void" | "never" | "self" | "parent" => true,
            // `static` = instance of the called class (late static);
            // unresolved (unbound closure) it can match nothing —
            // displayed literally (static_type_return).
            "static" => match v {
                Value::Object(o) => {
                    let cn = self
                        .stack
                        .last()
                        .and_then(|f| {
                            f.called_class
                                .as_ref()
                                .or(f.scope_class.as_ref())
                                .or(f.decl_class.as_ref())
                        })
                        .map(|c| c.name().to_string())
                        .unwrap_or_else(|| "\u{1}static".to_string());
                    self.obj_is_a(o, &cn)
                }
                _ => false,
            },
            "false" => matches!(v, Value::Bool(false)),
            "true" => matches!(v, Value::Bool(true)),
            // Weak-mode scalar params accept what coercion can convert:
            // non-numeric strings are a TypeError, not silent (trait_type_errors).
            "int" => match v {
                Value::Int(_) | Value::Bool(_) => true,
                // Out-of-range/NaN floats can't coerce -> TypeError
                // (scalar_return_basic_64bit).
                Value::Float(f) => {
                    f.is_finite() && *f < 9.223372036854776e18 && *f >= -9.223372036854776e18
                }
                // Only well-formed numeric strings pass — `"1a"` and
                // `"0x1A"` are a TypeError in PHP 8 weak mode.
                Value::Str(b) => matches!(numeric(b), Numeric::Int(_) | Numeric::Float(_)),
                _ => false,
            },
            "float" => match v {
                Value::Int(_) | Value::Float(_) | Value::Bool(_) => true,
                Value::Str(b) => matches!(numeric(b), Numeric::Int(_) | Numeric::Float(_)),
                _ => false,
            },
            "string" => match v {
                Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Str(_) => true,
                // Objects coerce via __toString only — a stdClass is a
                // TypeError, not "" (scalar_return_basic_64bit).
                Value::Object(o) => {
                    let tcls = o.borrow().class.clone();
                    self.find_method_in(&tcls, "__tostring").is_some()
                }
                _ => false,
            },
            "bool" => matches!(
                v,
                Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Str(_)
            ),
            "array" => matches!(v, Value::Array(_)),
            "iterable" => {
                matches!(v, Value::Array(_))
                    || matches!(v, Value::Object(o) if self.obj_is_a(o, "Traversable"))
            }
            "callable" => self.is_callable_value(v),
            "object" => matches!(v, Value::Object(_)),
            // Named class/interface — instanceof check. `Closure` is
            // our Callable value's class (constexpr/default_args).
            _ => match v {
                Value::Callable(_) => m.eq_ignore_ascii_case("closure"),
                Value::Object(o) => self.obj_is_a(o, m),
                _ => false,
            },
        }
    }

    /// Weak-mode scalar coercion for typed params/returns: returns the
    /// coerced value, or None when no scalar member applies (objects
    /// pass through unchanged).
    fn coerce_scalar(&mut self, ty: &[String], v: &Value) -> Option<Value> {
        // A null value is never coerced to a scalar — `?T` params keep
        // null (scalar_null). The caller's `ok` check gates the member.
        if matches!(v, Value::Null) {
            return if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) {
                Some(Value::Null)
            } else {
                None
            };
        }
        // Zend weak-union coercion preference (type_checking_weak): a
        // numeric string picks the member matching its own kind first
        // (`"42.0"` prefers `float`), then members are tried in scalar
        // order int -> float -> string -> bool family. Non-scalar
        // members never coerce.
        let has = |n: &str| ty.iter().any(|m| m.eq_ignore_ascii_case(n));
        let kind_flt = matches!(v, Value::Str(b) if matches!(numeric(b), Numeric::Float(_)));
        let mut order: Vec<String> = Vec::with_capacity(4);
        if kind_flt && has("float") {
            order.push("float".into());
        }
        for n in ["int", "float", "string"] {
            if has(n) && !order.iter().any(|o| o == n) {
                order.push(n.into());
            }
        }
        if let Some(b) = ty
            .iter()
            .find(|m| matches!(m.to_lowercase().as_str(), "bool" | "false" | "true"))
        {
            order.push(b.clone());
        }
        for m in &order {
            let l = m.to_lowercase();
            match l.as_str() {
                "int" => match v {
                    Value::Int(_) => return Some(v.clone()),
                    Value::Float(f)
                        if f.is_finite()
                            && *f < 9.223372036854776e18
                            && *f >= -9.223372036854776e18 =>
                    {
                        return Some(Value::Int(*f as i64));
                    }
                    Value::Bool(b) => return Some(Value::Int(*b as i64)),
                    Value::Str(b) => match numeric(b) {
                        Numeric::Int(i) => return Some(Value::Int(i)),
                        Numeric::Float(f) => return Some(Value::Int(f as i64)),
                        _ => {}
                    },
                    _ => {}
                },
                "float" => match v {
                    Value::Float(_) => return Some(v.clone()),
                    Value::Int(i) => return Some(Value::Float(*i as f64)),
                    Value::Bool(b) => return Some(Value::Float(*b as i64 as f64)),
                    Value::Str(b) => match numeric(b) {
                        Numeric::Int(i) => return Some(Value::Float(i as f64)),
                        Numeric::Float(f) => return Some(Value::Float(f)),
                        _ => {}
                    },
                    _ => {}
                },
                "string" => {
                    if let Value::Float(f) = v {
                        if f.is_nan() {
                            let _ = self.emit_diag(
                                "Warning",
                                2,
                                "unexpected NAN value was coerced to string",
                            );
                        }
                    }
                    if let Ok(b) = self.conv_bytes(v) {
                        return Some(Value::Str(b.into()));
                    }
                }
                "bool" | "false" | "true" => {
                    if let Value::Float(f) = v {
                        if f.is_nan() {
                            let _ = self.emit_diag(
                                "Warning",
                                2,
                                "unexpected NAN value was coerced to bool",
                            );
                        }
                    }
                    let t = v.is_truthy();
                    // Standalone `false`/`true` members only accept
                    // values that coerce to exactly that bool.
                    if l == "bool" || t == (l == "true") {
                        return Some(Value::Bool(t));
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Strict value-in-members test for the weak path: weak union
    /// coercion only applies when the value doesn't exactly match a
    /// member — `bool|array` + `[]` stays `[]`, `float|int` + 1 stays
    /// int(1) (union_types/type_checking_weak, legal_default_values).
    /// Unlike `ty_exact` (strict boundary), an int is NOT exact for
    /// `float` — it still widens through coercion.
    fn ty_weak_exact(&mut self, ty: &[String], v: &Value) -> bool {
        ty.iter().any(|m| {
            let l = m.to_lowercase();
            match l.as_str() {
                "int" => matches!(v, Value::Int(_)),
                "float" => matches!(v, Value::Float(_)),
                "string" => matches!(v, Value::Str(_)),
                "bool" => matches!(v, Value::Bool(_)),
                "false" => matches!(v, Value::Bool(false)),
                "true" => matches!(v, Value::Bool(true)),
                "null" => matches!(v, Value::Null),
                "array" => matches!(v, Value::Array(_)),
                "iterable" => {
                    matches!(v, Value::Array(_))
                        || matches!(v, Value::Object(o) if self.obj_is_a(o, "Traversable"))
                }
                "object" => matches!(v, Value::Object(_)),
                "callable" => self.is_callable_value(v),
                "mixed" | "void" | "never" | "self" | "static" | "parent" => true,
                _ if m.contains('&') => {
                    let ok = m
                        .trim_start_matches('(')
                        .trim_end_matches(')')
                        .split('&')
                        .all(|p| self.ty_weak_exact(&[p.to_string()], v));
                    ok
                }
                _ => match v {
                    Value::Callable(_) => m.eq_ignore_ascii_case("closure"),
                    Value::Object(o) => self.obj_is_a(o, m),
                    _ => false,
                },
            }
        })
    }

    /// Is the currently-executing code inside a `strict_types=1` file?
    /// (prop writes, const writes, incdec — Zend uses the writer's file.)
    fn exec_file_strict(&self) -> bool {
        self.stack
            .last()
            .map(|f| self.strict_files.contains(&f.file))
            .unwrap_or_else(|| self.strict_files.contains(&self.cur_file))
    }

    /// Strictness for argument checks is determined by the file holding
    /// the call site — the frame just below the callee's own.
    fn caller_file_strict(&self) -> bool {
        if self.stack.len() < 2 {
            // Top-level call site: `self.globals` lives off `self.stack`,
            // so the caller is the top-level file currently executing
            // (`cur_file` swaps for includes mid-eval).
            return self.strict_files.contains(&self.cur_file);
        }
        self.strict_files
            .contains(&self.stack[self.stack.len() - 2].file)
    }

    /// `callable` accepts an actual callable: a Closure/FCC value, a
    /// function-name string, a `"Class::method"` string, a `[cls|obj, m]`
    /// pair, or an object with `__invoke` (callable_001).
    /// `is_callable($v, $syntax_only, $name)` name written back
    /// (closure_016): syntax_only gives the `Class::m` / closure's
    /// Zend-name form; the default form is the engine's
    /// `Class::__invoke` / `Closure::__invoke`.
    pub fn callable_name_of(&mut self, v: &Value, _syntax_only: bool) -> Option<String> {
        match v {
            Value::Callable(c) => Some(match &c.kind {
                // A Closure's name is always its Zend name, syntax
                // flag or not (closure_016).
                CallableKind::Closure(d) => d.name.clone(),
                CallableKind::Named(n) => n.trim_start_matches('\\').to_string(),
                CallableKind::Method { obj, class, name } => {
                    let cn = obj
                        .as_ref()
                        .map(|o| o.borrow().class.name().to_string())
                        .or_else(|| class.as_ref().map(|c| c.name().to_string()))
                        .unwrap_or_else(|| "Closure".into());
                    format!("{}::{}", cn, name)
                }
            }),
            Value::Object(o) => {
                let c = o.borrow().class.clone();
                self.find_method_in(&c, "__invoke")?;
                Some(format!("{}::__invoke", c.name()))
            }
            Value::Str(s) => Some(crate::value::lossy(s).trim_start_matches('\\').to_string()),
            Value::Array(a) => {
                let arr = a.borrow();
                let (f, m) = (
                    arr.get(&crate::value::ArrKey::Int(0))?,
                    arr.get(&crate::value::ArrKey::Int(1))?,
                );
                let mn = m.to_php_string();
                match f {
                    // `[$closure, '__invoke']` canonicalizes to
                    // `Closure::__invoke` in $name (closure_016).
                    Value::Callable(_c) if mn.eq_ignore_ascii_case("__invoke") => {
                        Some("Closure::__invoke".into())
                    }
                    Value::Object(o) => Some(format!("{}::{}", o.borrow().class.name(), mn)),
                    Value::Str(cn) => Some(format!("{}::{}", crate::value::lossy(&cn), mn)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub fn is_callable_value(&mut self, v: &Value) -> bool {
        match v {
            Value::Callable(_) => true,
            Value::Str(s) => {
                let s = String::from_utf8_lossy(s).to_string();
                if self.functions.contains_key(&s.to_lowercase())
                    || builtins::is_builtin(&s.to_lowercase())
                    || builtins::builtin_params(&s.to_lowercase()).is_some()
                {
                    return true;
                }
                let Some((cn, mn)) = s.split_once("::") else {
                    return false;
                };
                let Some(c) = self.classes.get(&cn.to_lowercase()).cloned() else {
                    return false;
                };
                // "Class::method" strings only call statics
                // (callable_001).
                self.find_method_in(&c, mn)
                    .map(|(mm, _)| mm.is_static)
                    .unwrap_or(false)
            }
            Value::Object(o) => {
                let cn = o.borrow().class.decl.name.clone();
                let Some(c) = self.classes.get(&cn.to_lowercase()).cloned() else {
                    return false;
                };
                self.find_method_in(&c, "__invoke").is_some()
            }
            Value::Array(a) => {
                let arr = a.borrow();
                let first = arr.get(&crate::value::ArrKey::Int(0));
                let second = arr.get(&crate::value::ArrKey::Int(1));
                let (Some(first), Some(second)) = (first, second) else {
                    return false;
                };
                let Value::Str(mn) = &second else {
                    return false;
                };
                let mn = String::from_utf8_lossy(mn).to_string();
                let (cn, need_static) = match &first {
                    Value::Str(cn) => (String::from_utf8_lossy(cn).to_string(), true),
                    Value::Callable(_) => {
                        return mn.eq_ignore_ascii_case("__invoke");
                    }
                    Value::Object(o) => (o.borrow().class.decl.name.clone(), false),
                    _ => return false,
                };
                let Some(c) = self.classes.get(&cn.to_lowercase()).cloned() else {
                    return false;
                };
                // [class-string, method] only calls statics; [obj, m]
                // calls any (callable_001).
                self.find_method_in(&c, &mn)
                    .map(|(mm, _)| mm.is_static || !need_static)
                    .unwrap_or(false)
            }
            _ => false,
        }
    }

    /// PHP's "given" type word in TypeError messages.
    fn zval_type_name(&self, v: &Value) -> String {
        match v {
            Value::Null => "null".into(),
            Value::Bool(b) => if *b { "true" } else { "false" }.into(),
            Value::Int(_) => "int".into(),
            Value::Float(_) => "float".into(),
            Value::Str(_) => "string".into(),
            Value::Array(_) => "array".into(),
            Value::Object(o) => o.borrow().class.name().to_string(),
            Value::Callable(_) => "Closure".into(),
            Value::Resource(_) => "resource".into(),
        }
    }

    /// Strict-mode ZPP arg check for internal functions: `?` = nullable,
    /// `|` = union; int widens to float, everything else must match
    /// exactly (no scalar coercion, no __toString).
    fn zpp_strict_ok(&mut self, pty: &str, v: &Value) -> bool {
        let (pty, nullable) = match pty.strip_prefix('?') {
            Some(t) => (t, true),
            None => (pty, false),
        };
        if nullable && matches!(v, Value::Null) {
            return true;
        }
        pty.split('|').any(|t| match t {
            "string" => matches!(v, Value::Str(_)),
            "int" => matches!(v, Value::Int(_)),
            "float" => matches!(v, Value::Float(_) | Value::Int(_)),
            "bool" => matches!(v, Value::Bool(_)),
            "array" => matches!(v, Value::Array(_)),
            "object" => matches!(v, Value::Object(_) | Value::Callable(_)),
            "callable" => self.is_callable_value(v),
            "iterable" => {
                matches!(v, Value::Array(_))
                    || matches!(v, Value::Object(o) if {
                        let n = o.borrow().class.name().to_string();
                        self.is_a_str(&n, "traversable")
                    })
            }
            "resource" => matches!(v, Value::Resource(_)),
            _ => true, // mixed and unknown tags accept everything
        })
    }

    /// Zend's callback-validation error detail for internal functions
    /// (the part after `must be a valid callback`/`or null,`).
    fn zpp_callback_detail(&mut self, v: &Value) -> String {
        match v {
            Value::Array(a) => {
                let a = a.borrow();
                let mut it = a.entries.iter();
                match it.next() {
                    None => "first array member is not a valid class name or object".into(),
                    Some((_, c0)) => {
                        if !matches!(&*c0.borrow(), Value::Str(_) | Value::Object(_)) {
                            return "first array member is not a valid class name or object".into();
                        }
                        let c0 = c0.borrow().clone();
                        let second = it.next().map(|(_, c)| c.borrow().clone());
                        let Some(Value::Str(m)) = second else {
                            return "second array member is not a valid method".into();
                        };
                        let m = String::from_utf8_lossy(&m).to_string();
                        match c0 {
                            Value::Str(cn) => format!(
                                "class {} does not have a method \"{}\"",
                                String::from_utf8_lossy(&cn),
                                m
                            ),
                            Value::Object(o) => format!(
                                "class {} does not have a method \"{}\"",
                                o.borrow().class.name(),
                                m
                            ),
                            _ => "first array member is not a valid class name or object".into(),
                        }
                    }
                }
            }
            Value::Str(s) => format!(
                "function \"{}\" not found or invalid function name",
                String::from_utf8_lossy(s)
            ),
            _ => "no array or string given".into(),
        }
    }

    /// ZPP-style type display: `iterable` expands to `Traversable|array`
    /// in param/return TypeErrors and default-value fatals (iterable_*).
    fn zpp_ty_disp(ty: &[String]) -> Vec<String> {
        ty.iter()
            .flat_map(|m| {
                if m.eq_ignore_ascii_case("iterable") {
                    vec!["Traversable".to_string(), "array".to_string()]
                } else if let Some(pos) = m.find("@anonymous$") {
                    vec![format!("{}@anonymous", &m[..pos])]
                } else if m.contains('&') && ty.len() > 1 {
                    // Intersection members parenthesize inside a union
                    // ((X&Y)|(W&Z) — dnf_2_intersection).
                    vec![format!("({m})")]
                } else {
                    vec![m.clone()]
                }
            })
            .collect()
    }

    /// Display name for a decl in diagnostics — closures are named
    /// `{closure:FILE:LINE}` like Zend (named_params/call_user_func).
    fn decl_fname(&self, decl: &FunctionDecl) -> String {
        let base = if decl.name.is_empty() {
            format!("{{closure:{}:{}}}", decl.file, decl.line)
        } else {
            decl.name.clone()
        };
        self.stack
            .last()
            .and_then(|f| f.decl_class.as_ref().map(|c| c.name().to_string()))
            .map(|c| format!("{}::{}", c, base))
            .unwrap_or(base)
    }

    fn bind_and_run_inner(
        &mut self,
        decl: &FunctionDecl,
        args: CallArgs,
        _unused: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        let required = decl
            .params
            .iter()
            .filter(|p| p.default.is_none() && !p.variadic)
            .count();
        // With named args, missing-required is reported per-param during
        // binding ("Argument #N ($x) not passed"); the count check below
        // is the positional-only form.
        if args.named.is_empty() && args.len() < required {
            self.stack.pop();
            return self.fail(PhpError::uncaught(
                "ArgumentCountError",
                format!(
                    "Too few arguments to function {}(), {} passed in {} on line {} and {} {} expected",
                    self.decl_fname(decl),
                    args.len(),
                    self.diag_file(),
                    self.cur_line,
                    if required == decl.params.len() { "exactly" } else { "at least" },
                    required
                ),
                0,
            ));
        }
        // Named arguments resolve against decl.params by name
        // (Zend/tests/named_params): unknown names land in a trailing
        // variadic's array as string keys, else "Unknown named
        // parameter"; a name colliding with a positional or a prior
        // named arg is the "overwrites previous argument" Error.
        let n_pos = args.cells.len();
        let mut by_name: Vec<Option<(Cell, bool, bool)>> = vec![None; decl.params.len()];
        let mut variadic_named: Vec<(String, Cell)> = Vec::new();
        let has_variadic = decl.params.iter().any(|p| p.variadic);
        for (n, c, refable, trav) in &args.named {
            match decl.params.iter().position(|p| !p.variadic && p.name == *n) {
                Some(j) if j < n_pos || by_name[j].is_some() => {
                    self.stack.pop();
                    // Caller-side arg-verify error: the callee frame
                    // never existed (gh19653_2).
                    if self
                        .call_trace
                        .last()
                        .map(|f| f.function == decl.name)
                        .unwrap_or(false)
                    {
                        self.call_trace.pop();
                    }
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Named parameter ${} overwrites previous argument", n),
                        0,
                    ));
                }
                Some(j) => by_name[j] = Some((c.clone(), *refable, *trav)),
                None if has_variadic => variadic_named.push((n.clone(), c.clone())),
                None => {
                    self.stack.pop();
                    if self
                        .call_trace
                        .last()
                        .map(|f| f.function == decl.name)
                        .unwrap_or(false)
                    {
                        self.call_trace.pop();
                    }
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Unknown named parameter ${}", n),
                        0,
                    ));
                }
            }
        }
        // Enforce declared param types (tests/lang/type_hints_*.phpt).
        for (i, p) in decl.params.iter().enumerate() {
            let (Some(ty), Some(a)) = (
                &p.ty,
                args.cells.get(i).or(by_name[i].as_ref().map(|t| &t.0)),
            ) else {
                continue;
            };
            if p.by_ref {
                // By-ref params bind cells, not values — the contained
                // value isn't checked at the boundary (typed_properties_010),
                // but weak scalar args still coerce into the caller's
                // cell (scalar_weak_reference).
                if !self.caller_file_strict() {
                    let bv = a.borrow().clone();
                    if !self.ty_weak_exact(ty, &bv) {
                        if let Some(cv) = self.coerce_scalar(ty, &bv) {
                            *a.borrow_mut() = cv;
                        }
                    }
                }
                continue;
            }
            let v = a.borrow().clone();
            let implicit_null = !ty.iter().any(|m| m.eq_ignore_ascii_case("null"))
                && match &p.default {
                    Some(Expr::Null) => true,
                    Some(Expr::Const(c)) => c.eq_ignore_ascii_case("null"),
                    _ => false,
                };
            let ok = (implicit_null && matches!(v, Value::Null))
                || if self.caller_file_strict() {
                    self.ty_exact(ty, &v)
                } else {
                    ty.iter().any(|m| self.param_type_match(m, &v))
                };
            let caller_strict = self.caller_file_strict();
            if ok && !caller_strict && !self.ty_weak_exact(ty, &v) {
                if let Some(cv) = self.coerce_scalar(ty, &v) {
                    // Arg-coercion deprecations attribute to the
                    // callee's declaration line (scalar_basic).
                    let pl = self.cur_line;
                    self.cur_line = decl.line;
                    self.deprecate_lossy_int(ty, &v, &cv);
                    self.cur_line = pl;
                    *a.borrow_mut() = cv;
                }
            }
            // strict mode still allows the int->float widening stored
            // back for visibility in the callee.
            if ok
                && caller_strict
                && ty.iter().any(|m| m.eq_ignore_ascii_case("float"))
                && !self.ty_weak_exact(ty, &v)
            {
                if let Value::Int(i) = v {
                    *a.borrow_mut() = Value::Float(i as f64);
                }
            }
            if !ok {
                let mut fname = self.decl_fname(decl);
                // Anonymous-class methods report args under just the
                // class name (union_types/anonymous_class).
                if let Some(pos) = fname.find("@anonymous::") {
                    fname = fname[..pos + "@anonymous".len()].into();
                }
                // Implicit-nullable needs the phantom `null` member so
                // an intersection renders `(X&Y)|null`
                // (implicit_nullable_intersection_type_error).
                let tyv: Vec<String> = if implicit_null {
                    let mut t = ty.to_vec();
                    t.push("null".into());
                    t
                } else {
                    ty.to_vec()
                };
                let mut disp: Vec<String> = Self::zpp_ty_disp(&self.resolve_static(&tyv));
                disp.retain(|m| !m.eq_ignore_ascii_case("null"));
                if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) || implicit_null {
                    if disp.len() == 1 && !disp[0].contains('&') {
                        disp[0] = format!("?{}", disp[0]);
                    } else {
                        disp.push("null".into());
                    }
                }
                let given = self.zval_type_name(&v);
                // getMessage() is the short form; the uncaught display
                // appends ` and defined in FILE:M` (catchable_error_002).
                // The ", called in FILE on line" suffix only applies to
                // function-call style invocations — invoking through the
                // internal `Closure::__invoke` ( `$f->__invoke()` or
                // `[$f,'__invoke']`) drops it (closure_059).
                let call_alias = self.stack.last().and_then(|f| f.call_alias.clone());
                let msg = if call_alias.is_some() {
                    format!(
                        "{}(): Argument #{} (${}) must be of type {}, {} given",
                        fname,
                        i + 1,
                        p.name,
                        disp.join("|"),
                        given,
                    )
                } else {
                    format!(
                        "{}(): Argument #{} (${}) must be of type {}, {} given, called in {} on line {}",
                        fname,
                        i + 1,
                        p.name,
                        disp.join("|"),
                        given,
                        self.diag_file(),
                        self.cur_line
                    )
                };
                let display = format!("{} and defined", msg);
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
                let frame = format!(
                    "{}({}): {}({})",
                    self.diag_file(),
                    self.cur_line,
                    tname,
                    argdesc
                );
                let call_line = self.cur_line;
                // Frames below the call site (include/require and
                // outer calls) join the synthetic #0 — the callee's
                // own trace frame is the top of call_trace.
                let mut frs = vec![frame];
                for fr in self.call_trace.iter().rev().skip(1) {
                    if crate::value::trace_frame_hidden(fr) {
                        continue;
                    }
                    frs.push(crate::value::trace_frame_str(fr));
                }
                self.stack.pop();
                let mut e = PhpError::uncaught("TypeError", msg, call_line);
                e.trace = Some(frs);
                e.thrown_line = Some(decl.line);
                e.display_msg = Some(display);
                let r = self.fail(e);
                if let Some(Value::Object(o)) = &self.pending_exception {
                    if let Some(crate::value::ObjectInternal::Exception { file, .. }) =
                        &mut o.borrow_mut().internal
                    {
                        *file = decl.file.clone();
                    }
                }
                self.last_err_file = decl.file.clone();
                return r;
            }
        }
        {
            // Compute param bindings first (defaults may eval exprs that
            // need &mut self).
            let mut binds: Vec<(String, Cell)> = Vec::new();
            for (i, p) in decl.params.iter().enumerate() {
                if p.variadic {
                    let mut arr = PhpArray::new();
                    // `&...$refs` aliases the arg cells themselves
                    // (named_params/variadic's test2 increments $x/$y) —
                    // an array containing references is itself a
                    // reference set: by-ref foreach iterates it live
                    // without separating.
                    for v in &args.cells[i.min(args.cells.len())..] {
                        if p.by_ref {
                            arr.is_ref = true;
                            arr.push_cell(v.clone());
                        } else {
                            arr.push(v.borrow().clone());
                        }
                    }
                    for (n, c) in &variadic_named {
                        if p.by_ref {
                            arr.is_ref = true;
                            arr.set_cell(ArrKey::Str(n.clone().into()), c.clone());
                        } else {
                            arr.set(ArrKey::Str(n.clone().into()), c.borrow().clone());
                        }
                    }
                    binds.push((
                        p.name.clone(),
                        cell(Value::Array(Rc::new(RefCell::new(arr)))),
                    ));
                } else if let Some((v, refable, trav)) = args
                    .cells
                    .get(i)
                    .map(|c| {
                        (
                            c,
                            // A nonref (call_user_func) slot still
                            // forwards when the element itself is a
                            // reference — zend keeps ref-ness through
                            // cufa arrays (bug50394).
                            !args.nonref_cells.contains(&i)
                                || self.ref_cells.contains(&(Rc::as_ptr(c) as usize)),
                            args.trav_cells.contains(&i),
                        )
                    })
                    .or(by_name[i].as_ref().map(|t| (&t.0, t.1, t.2)))
                {
                    if p.by_ref {
                        if trav {
                            let fname = self.decl_fname(decl);
                            self.warn(&format!(
                                "Cannot pass by-reference argument {} of {}() by unpacking a Traversable, passing by-value instead",
                                i + 1,
                                fname
                            ))?;
                            binds.push((p.name.clone(), cell(v.borrow().clone())));
                            continue;
                        }
                        if !refable {
                            let fname = self
                                .stack
                                .last()
                                .and_then(|f| f.call_alias.clone())
                                .unwrap_or_else(|| self.decl_fname(decl));
                            self.warn(&format!(
                                "{}(): Argument #{} (${}) must be passed by reference, value given",
                                fname,
                                i + 1,
                                p.name
                            ))?;
                        }
                        // The callee's var becomes a Zend IS_REFERENCE
                        // over the caller's cell — write-through errors
                        // say "reference held by property"
                        // (typed_properties_055/108).
                        self.ref_cells.insert(Rc::as_ptr(v) as usize);
                        binds.push((p.name.clone(), v.clone()));
                    } else {
                        binds.push((p.name.clone(), cell(v.borrow().clone())));
                    }
                } else if let Some(d) = &p.default {
                    // Default exprs are evaluated at call time; an error
                    // (e.g. an undefined constant) propagates as the
                    // call's failure (namespaces/ns_077) and attributes
                    // to the declaration line (named_params/defaults).
                    let prev_line = self.cur_line;
                    self.cur_line = decl.line;
                    let prev = self
                        .stack
                        .last()
                        .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned());
                    let old = match prev {
                        Some(c) => self.const_self.replace(c),
                        None => self.const_self.take(),
                    };
                    let r = self.eval_decl_const(d, &decl.file);
                    self.const_self = old;
                    self.cur_line = prev_line;
                    let mut dv = match r {
                        Ok(v) => v,
                        Err(e) => {
                            self.stack.pop();
                            return self.fail(e);
                        }
                    };
                    // `float $f = 0` — the int default widens to float
                    // at bind time, even under strict_types
                    // (scalar_float_with_integer_default_strict). In a
                    // union this applies whenever no `int` member can
                    // take it exactly (`float|string` = 3 -> float(3);
                    // `int|float` = 1 stays int — legal_default_values).
                    if let Some(ty) = &p.ty {
                        let float_widens = ty.iter().any(|m| m.eq_ignore_ascii_case("float"))
                            && !ty.iter().any(|m| m.eq_ignore_ascii_case("int"));
                        if float_widens {
                            if let Value::Int(i) = &dv {
                                dv = Value::Float(*i as f64);
                            }
                        }
                        // A non-literal default (const, expr) is checked
                        // like a passed arg — `int $a = NULL_CONST`
                        // TypeErrors when the default binds
                        // (scalar_constant_defaults).
                        let implicit_null = !ty.iter().any(|m| m.eq_ignore_ascii_case("null"))
                            && match &p.default {
                                Some(Expr::Null) => true,
                                Some(Expr::Const(c)) => c.eq_ignore_ascii_case("null"),
                                _ => false,
                            };
                        let ok = (implicit_null && matches!(dv, Value::Null))
                            || ty.iter().any(|m| self.param_type_match(m, &dv));
                        if !ok {
                            self.stack.pop();
                            let tyv: Vec<String> = if implicit_null {
                                let mut t = ty.to_vec();
                                t.push("null".into());
                                t
                            } else {
                                ty.to_vec()
                            };
                            let mut disp: Vec<String> =
                                Self::zpp_ty_disp(&self.resolve_static(&tyv));
                            disp.retain(|m| !m.eq_ignore_ascii_case("null"));
                            if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) || implicit_null {
                                if disp.len() == 1 && !disp[0].contains('&') {
                                    disp[0] = format!("?{}", disp[0]);
                                } else {
                                    disp.push("null".into());
                                }
                            }
                            let fname = self.decl_fname(decl);
                            let msg = format!(
                                "{}(): Argument #{} (${}) must be of type {}, {} given, called in {} on line {}",
                                fname,
                                i + 1,
                                p.name,
                                disp.join("|"),
                                self.zval_type_name(&dv),
                                self.diag_file(),
                                self.cur_line
                            );
                            let display = format!("{} and defined", msg);
                            let mut e = PhpError::uncaught("TypeError", msg, self.cur_line);
                            e.display_msg = Some(display);
                            e.thrown_line = Some(decl.line);
                            self.stack.pop();
                            return self.fail(e);
                        }
                        if !self.caller_file_strict() && !self.ty_weak_exact(ty, &dv) {
                            if let Some(cv) = self.coerce_scalar(ty, &dv) {
                                dv = cv;
                            }
                        }
                    }
                    binds.push((p.name.clone(), cell(dv)));
                } else {
                    // Unbound required param — only reachable via named
                    // args (the positional count check runs earlier).
                    let fname = self.decl_fname(decl);
                    self.stack.pop();
                    return self.fail(PhpError::uncaught(
                        "ArgumentCountError",
                        format!("{}(): Argument #{} (${}) not passed", fname, i + 1, p.name),
                        0,
                    ));
                }
            }
            let frame = self.stack.last_mut().unwrap();
            // func_get_arg(i)/func_num_args(): the bound non-variadic
            // params (named or positional) plus positional extras —
            // variadic extras don't count (named_params/variadic).
            let n_fixed = decl.params.iter().take_while(|p| !p.variadic).count();
            // func_num_args()/func_get_args(): Zend binds named args into
            // the CV table positionally, so a named call fills the table
            // up to the highest bound param — `test(c:'C', a:'A')`
            // reports 3 args, not 2 (named_params/func_get_args).
            let max_bound = (0..n_fixed)
                .filter(|i| args.cells.get(*i).is_some() || by_name[*i].is_some())
                .max();
            let mut fa: Vec<Cell> = Vec::new();
            if let Some(max_i) = max_bound {
                for bind in binds.iter().take(max_i + 1) {
                    fa.push(bind.1.clone());
                }
            }
            for a in &args.cells[n_fixed.min(args.cells.len())..] {
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
                let _ = self.store_prop(Value::Object(obj), &pname, v)?;
            }
        }
        let flow = self.exec_block(&decl.body);
        let ret_fname = self.decl_fname(decl);
        // `static` resolves against THIS frame's called class — after
        // the pop, `stack.last()` is the caller (static_type_return).
        let resolved_ret = decl.ret.as_ref().map(|ty| self.resolve_static(ty));
        let popped = self.stack.pop();
        // Zend decrefs the frame's CVs at unwind — the popped frame
        // is handed to bind_and_run, which runs its __destruct pass
        // after the call-trace pop so the dtor's trace attributes to
        // the caller's site (bug52361).
        self.last_popped_frame = popped;
        match flow {
            Flow::Return(v) => {
                // In a generator body `return v` is the iterator's
                // getReturn() payload — the declared return type binds
                // the produced Generator object, not this value
                // (generator_return_return_type).
                if decl.ret.is_some() && Self::decl_contains_yield(&decl.body) {
                    return Ok(v);
                }
                if let Some(ty) = &resolved_ret {
                    let ret_strict = self.strict_files.contains(&decl.file);
                    let ok = ty
                        .iter()
                        .any(|m| self.param_type_match(m, &v) || m.eq_ignore_ascii_case("void"))
                        && (!ret_strict || self.ty_exact(ty, &v));
                    if ok {
                        if !ret_strict && !self.ty_weak_exact(ty, &v) {
                            match self.coerce_scalar(ty, &v) {
                                Some(cv) => {
                                    let pl = self.cur_line;
                                    self.cur_line = decl.line;
                                    self.deprecate_lossy_int(ty, &v, &cv);
                                    self.cur_line = pl;
                                    Ok(cv)
                                }
                                None => Ok(v),
                            }
                        } else {
                            Ok(v)
                        }
                    } else {
                        let mut disp: Vec<String> = Self::zpp_ty_disp(&self.resolve_static(ty));
                        disp.retain(|m| !m.eq_ignore_ascii_case("null"));
                        if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) {
                            if disp.len() == 1 && !disp[0].contains('&') {
                                disp[0] = format!("?{}", disp[0]);
                            } else {
                                disp.push("null".into());
                            }
                        }
                        let given = self.zval_type_name(&v);
                        let msg = format!(
                            "{}(): Return value must be of type {}, {} returned",
                            ret_fname,
                            disp.join("|"),
                            given
                        );
                        self.fail(PhpError::uncaught("TypeError", msg, self.cur_line))
                    }
                } else {
                    // __toString carries an implicit `string` contract
                    // — scalars coerce weakly; other types are
                    // TypeErrors (bug26166).
                    if decl.name.eq_ignore_ascii_case("__tostring") {
                        match &v {
                            Value::Str(_) => Ok(v),
                            Value::Int(_) | Value::Float(_) | Value::Bool(_) => {
                                Ok(Value::str(v.to_php_string()))
                            }
                            other => {
                                let given = self.zval_type_name(other);
                                self.fail(PhpError::uncaught(
                                    "TypeError",
                                    format!(
                                        "{}(): Return value must be of type string, {} returned",
                                        ret_fname, given
                                    ),
                                    self.cur_line,
                                ))
                            }
                        }
                    } else {
                        Ok(v)
                    }
                }
            }
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
            Flow::Normal => {
                // Falling off the end of a typed function still checks
                // the return type: `none returned` TypeError for real
                // types, `must not implicitly return` for `never`
                // (typed_return*_without_value). Generators are exempt —
                // their declared type describes the produced object.
                if decl.ret.is_some() && Self::decl_contains_yield(&decl.body) {
                    return Ok(Value::Null);
                }
                if let Some(ty) = &resolved_ret {
                    let never = ty.iter().any(|m| m.eq_ignore_ascii_case("never"));
                    let void = ty.iter().all(|m| m.eq_ignore_ascii_case("void"));
                    if never {
                        let msg = format!(
                            "{}: never-returning function must not implicitly return",
                            ret_fname
                        );
                        let mut e = PhpError::uncaught("TypeError", msg, self.cur_line);
                        e.thrown_line = Some(decl.line);
                        return self.fail(e);
                    }
                    if !void {
                        let mut disp_v = Self::zpp_ty_disp(&self.resolve_static(ty));
                        disp_v.retain(|m| !m.eq_ignore_ascii_case("null"));
                        if ty.iter().any(|m| m.eq_ignore_ascii_case("null")) {
                            if disp_v.len() == 1 && !disp_v[0].contains('&') {
                                disp_v[0] = format!("?{}", disp_v[0]);
                            } else {
                                disp_v.push("null".into());
                            }
                        }
                        let disp = disp_v.join("|");
                        let msg = format!(
                            "{}(): Return value must be of type {}, none returned",
                            ret_fname, disp
                        );
                        let mut e = PhpError::uncaught("TypeError", msg, self.cur_line);
                        e.thrown_line = Some(decl.end_line);
                        return self.fail(e);
                    }
                }
                // Falling off an untyped __toString is the same
                // `none returned` TypeError (bug26166).
                if decl.name.eq_ignore_ascii_case("__tostring") {
                    let mut e = PhpError::uncaught(
                        "TypeError",
                        format!(
                            "{}(): Return value must be of type string, none returned",
                            ret_fname
                        ),
                        self.cur_line,
                    );
                    e.thrown_line = Some(decl.end_line);
                    return self.fail(e);
                }
                Ok(Value::Null)
            }
        }
    }

    fn invoke_fn(
        &mut self,
        decl: &Rc<FunctionDecl>,
        args: CallArgs,
        this_obj: Option<Rc<RefCell<PhpObject>>>,
        scope_class: Option<Rc<PhpClass>>,
    ) -> Result<Value, PhpError> {
        let required = decl
            .params
            .iter()
            .filter(|p| p.default.is_none() && !p.variadic)
            .count();
        if args.named.is_empty() && args.len() < required {
            return self.fail(PhpError::uncaught(
                "ArgumentCountError",
                format!(
                    "Too few arguments to function {}(), {} passed in {} on line {} and {} {} expected",
                    self.decl_fname(decl),
                    args.len(),
                    self.diag_file(),
                    self.cur_line,
                    if required == decl.params.len() { "exactly" } else { "at least" },
                    required
                ),
                0,
            ));
        }
        // A `yield`-bearing body makes the call a Generator factory:
        // the caller gets a Generator object immediately and the body
        // only runs when iteration first demands it.
        if Self::decl_contains_yield(&decl.body) {
            let dc = self.pending_decl_class.take();
            let cc = self.pending_called_class.take();
            return Ok(Value::Object(self.make_generator(
                decl.clone(),
                args,
                this_obj,
                scope_class,
                dc,
                cc,
                Vec::new(),
            )));
        }
        let dc = self.pending_decl_class.take();
        let cc = self.pending_called_class.take();
        self.invoke_fn_run(decl, args, this_obj, scope_class, dc, cc)
    }

    /// Frame push + body run — the part of invoke_fn the Generator
    /// start path also uses (the yield check must not re-trip here).
    fn invoke_fn_run(
        &mut self,
        decl: &Rc<FunctionDecl>,
        args: CallArgs,
        this_obj: Option<Rc<RefCell<PhpObject>>>,
        scope_class: Option<Rc<PhpClass>>,
        decl_class: Option<Rc<PhpClass>>,
        called_class: Option<Rc<PhpClass>>,
    ) -> Result<Value, PhpError> {
        let mut frame = Frame::new(decl.name.clone());
        frame.fn_line = decl.line;
        frame.file = decl.file.clone();
        frame.ns = decl.ns.clone();
        frame.ret_by_ref = decl.by_ref;
        if let Some(obj) = &this_obj {
            frame
                .vars
                .insert("this".to_string(), cell(Value::Object(obj.clone())));
        }
        frame.decl_class = decl_class;
        frame.called_class = called_class;
        frame.hook_prop = self.pending_hook_prop.take();
        frame.this_obj = this_obj;
        frame.scope_class = scope_class;
        frame.trait_origin = decl.decl_in.clone();
        frame.file = if decl.file.is_empty() {
            self.cur_file.clone()
        } else {
            decl.file.clone()
        };
        let pending_caps = std::mem::take(&mut self.pending_gen_captures);
        self.stack.push(frame);
        if let Some(top) = self.stack.last_mut() {
            for (n, c, by_ref) in pending_caps {
                let c2 = if by_ref { c } else { cell(c.borrow().clone()) };
                top.vars.insert(n, c2);
            }
        }
        self.bind_and_run(decl, args, Vec::new())
    }

    // ----- generators -----

    /// Whether a function body yields — scanning skips nested closures
    /// and function decls (each is its own generator context).
    fn decl_contains_yield(stmts: &[Stmt]) -> bool {
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
    fn body_uses_this(stmts: &[crate::ast::Stmt]) -> bool {
        stmts.iter().any(|st| {
            !matches!(st, crate::ast::Stmt::Function(_))
                && format!("{:?}", st).contains(r#"Var("this")"#)
        })
    }

    /// Build the deferred Generator object for a yielding call.
    #[allow(clippy::too_many_arguments)]
    fn make_generator(
        &mut self,
        decl: Rc<FunctionDecl>,
        args: CallArgs,
        this_obj: Option<Rc<RefCell<PhpObject>>>,
        scope_class: Option<Rc<PhpClass>>,
        decl_class: Option<Rc<PhpClass>>,
        called_class: Option<Rc<PhpClass>>,
        captures: Vec<(String, Cell, bool)>,
    ) -> Rc<RefCell<PhpObject>> {
        let by_ref = decl.by_ref;
        let state = Rc::new(RefCell::new(GenState {
            setup: GenSetup::Invoke {
                decl,
                args,
                this_obj,
                scope_class,
                decl_class,
                called_class,
                captures,
            },
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
                    ..
                } => (
                    decl.clone(),
                    this_obj.clone(),
                    scope_class.clone(),
                    decl_class.clone(),
                    called_class.clone(),
                    captures.clone(),
                ),
            };
            (setup, st.sends.clone())
        };
        let (decl, this_obj, scope_class, decl_class, called_class, captures) = setup;
        // Re-evaluating stored arg cells is unnecessary — bind_and_run
        // consumes the cells captured at call time.
        let args = {
            let mut st = state.borrow_mut();
            match &mut st.setup {
                GenSetup::Invoke { args, .. } => std::mem::replace(args, CallArgs::empty()),
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
        let r = self.invoke_fn_run(&decl, args, this_obj, scope_class, decl_class, called_class);
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
    fn spl_method(
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
                let is_iter = matches!(
                    obj.borrow().class.name().to_lowercase().as_str(),
                    "directoryiterator" | "filesystemiterator"
                );
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
                    ob.internal = Some(ObjectInternal::DirIter { entries, pos: 0 });
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
                Some(ObjectInternal::DirIter { entries, pos }) => *pos < entries.len(),
                _ => false,
            }))),
            "current" => {
                // PHP yields SplFileInfo instances for each entry.
                let path = match &obj.borrow().internal {
                    Some(ObjectInternal::DirIter { entries, pos }) if *pos < entries.len() => {
                        Some(entries[*pos].clone())
                    }
                    _ => None,
                };
                match path {
                    Some(p) => {
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
            _ => {
                let path_v = obj
                    .borrow()
                    .props
                    .get("\0fi\0path")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let path = self.conv_str(&path_v)?.to_string();
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
    fn generator_method(
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
                        out.push((k, cell(val)));
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

    // ----- classes -----

    fn register_class(&mut self, decl: Rc<ClassDecl>) -> Result<(), PhpError> {
        // The name is "in progress" from the moment registration is
        // entered: type probes treat it as resolvable so a check can
        // defer on it instead of autoloading recursively
        // (infinite_recursion: `class C extends Z implements C`).
        self.declaring.push(decl.clone());
        let res = self.register_class_inner(decl);
        self.declaring.pop();
        res
    }

    fn register_class_inner(&mut self, decl: Rc<ClassDecl>) -> Result<(), PhpError> {
        // Reserved scalar names can't name a class/interface/trait/enum
        // (scalar_reserved*): `class int {}` is a compile fatal.
        let short = decl.name.rsplit('\\').next().unwrap_or(&decl.name);
        const RESERVED_DECL: &[&str] = &[
            "int", "float", "string", "bool", "void", "iterable", "object", "mixed", "never",
            "null", "false", "true",
        ];
        if RESERVED_DECL.contains(&short.to_lowercase().as_str()) {
            let kind = match decl.kind {
                ClassKind::Interface => "an interface",
                ClassKind::Trait => "a trait",
                ClassKind::Enum => "an enum",
                ClassKind::Class => "a class",
            };
            return Err(PhpError::compile_fatal(
                format!(
                    "Cannot use \"{}\" as {} name as it is reserved",
                    short, kind
                ),
                self.cur_line,
            ));
        }
        // PHP links a declared class eagerly: the parent class, every
        // implemented interface, and every used trait must resolve at
        // declaration time, autoloading them when unregistered
        // (composer PSR-4 trees depend on this — MarkBased links
        // RegexBasedAbstract and the DataGenerator interface here).
        if let Some(p) = &decl.parent {
            let pl = p.to_lowercase();
            // Kind-mismatched parents (a trait/interface under
            // `extends`) count as "found" so the dedicated fatals
            // below report them (error_009). Names still on the
            // linking stack are mid-registration CEs — resolvable
            // (traits/abstract_method_9).
            let mut found = if decl.kind == ClassKind::Interface {
                self.interfaces.contains_key(&pl)
            } else {
                self.classes.contains_key(&pl)
                    || self.traits.contains_key(&pl)
                    || self.interfaces.contains_key(&pl)
                    || self
                        .linking
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&pl))
            };
            if !found {
                self.run_autoload(p.trim_start_matches('\\'))?;
                found = if decl.kind == ClassKind::Interface {
                    self.interfaces.contains_key(&pl)
                } else {
                    self.classes.contains_key(&pl)
                        || self.traits.contains_key(&pl)
                        || self.interfaces.contains_key(&pl)
                        || self
                            .linking
                            .iter()
                            .any(|c| c.name.eq_ignore_ascii_case(&pl))
                };
            }
            // Still unlinked after autoload — catchable Error
            // (variance/unlinked_parent_1).
            if !found {
                let v = self.exception("Error", &format!("Class \"{}\" not found", p));
                return Err(self.throw(v));
            }
        }
        for i in &decl.implements {
            if !self.interfaces.contains_key(&i.to_lowercase())
                && !self.classes.contains_key(&i.to_lowercase())
            {
                self.run_autoload(i.trim_start_matches('\\'))?;
            }
        }
        for t in &decl.traits {
            if !self.traits.contains_key(&t.to_lowercase()) {
                self.run_autoload(t.trim_start_matches('\\'))?;
            }
        }
        let mut d = (*decl).clone();
        // Declaring file — prop/const default exprs bind __FILE__/__DIR__
        // to it (composer's generated `__DIR__ . '/../..' . ...` paths).
        if d.file.is_empty() {
            d.file = self.cur_file.clone();
        }

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
                            attrs: vec![],
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
                if let Some(t0) = d.traits.first() {
                    return Err(PhpError::fatal(
                        format!(
                            "Cannot use traits inside of interfaces. {} is used in {}",
                            t0, d.name
                        ),
                        self.cur_line,
                    ));
                }
                Self::resolve_scope_tys(&mut d);
                // `interface B extends A` — B's methods must stay
                // compatible with A's (invalid_covariance_*).
                self.linking.push(Rc::new(d.clone()));
                let checks_res = self.check_interface_sigs(&d);
                self.linking.pop();
                checks_res?;
                self.magic_method_checks(&d)?;
                // An interface declaring __toString implicitly extends
                // Stringable (interface_with_tostring).
                let mut d = d;
                if d.methods
                    .iter()
                    .any(|m| m.decl.name.eq_ignore_ascii_case("__tostring"))
                    && !d
                        .implements
                        .iter()
                        .any(|i| i.eq_ignore_ascii_case("stringable"))
                {
                    d.implements.push("Stringable".to_string());
                }
                self.interfaces.insert(lname.clone(), Rc::new(d));
                self.decl_order.push(lname);
            }
            ClassKind::Trait => {
                // `trait _ {}` — deprecated since 8.4.
                if d.name.rsplit('\\').next() == Some("_") {
                    self.deprecated("Using \"_\" as a trait name is deprecated since 8.4")?;
                }
                // Traits may `use` traits — flatten recursively
                // (flattening003). Origin chains keep the DEFINING
                // trait's name via decl_in.
                if !d.traits.is_empty() {
                    for t in &d.traits {
                        if !self.traits.contains_key(&t.to_lowercase()) {
                            let msg = if self.lookup_class(t).is_some()
                                || self.interfaces.contains_key(&t.to_lowercase())
                            {
                                format!("{} cannot use {} - it is not a trait", d.name, t)
                            } else {
                                format!("Trait \"{}\" not found", t)
                            };
                            // Catchable Error, not a fatal (gh17959).
                            let v = self.exception("Error", &msg);
                            return Err(self.throw(v));
                        }
                    }
                    self.linking.push(Rc::new(d.clone()));
                    let merge_res = self.merge_trait_adaptations(&mut d);
                    self.linking.pop();
                    merge_res?;
                }
                self.magic_method_checks(&d)?;
                self.traits.insert(lname.clone(), Rc::new(d));
                self.decl_order.push(lname);
            }
            _ => {
                // `use static|self|parent` — reserved, uncatchable
                // (class_uses_static).
                for t in &d.traits {
                    if ["static", "self", "parent"]
                        .iter()
                        .any(|r| t.eq_ignore_ascii_case(r))
                    {
                        return Err(PhpError::fatal(
                            format!("Cannot use \"{}\" as trait name, as it is reserved", t),
                            self.cur_line,
                        ));
                    }
                }
                // Apply traits: merge methods/props into the decl.
                if !d.traits.is_empty() {
                    // Missing trait → catchable Error (gh17959).
                    for t in &d.traits {
                        if !self.traits.contains_key(&t.to_lowercase()) {
                            let msg = if self.lookup_class(t).is_some()
                                || self.interfaces.contains_key(&t.to_lowercase())
                            {
                                format!("{} cannot use {} - it is not a trait", d.name, t)
                            } else {
                                format!("Trait \"{}\" not found", t)
                            };
                            let v = self.exception("Error", &msg);
                            return Err(self.throw(v));
                        }
                    }
                    self.linking.push(Rc::new(d.clone()));
                    let merge_res = self.merge_trait_adaptations(&mut d);
                    self.linking.pop();
                    merge_res?;
                }
                // `self`/`static`/`parent` in member types bind to the
                // declaring class at registration — a `self` member
                // otherwise wildcard-matches everything
                // (union_types/anonymous_class). This runs AFTER trait
                // merge so trait methods' `self` resolves to the
                // consuming class (traits/abstract_method_8). `static`
                // loses late-static nuance, which type checks don't
                // distinguish anyway.
                Self::resolve_scope_tys(&mut d);
                self.check_rtwc_attr(&d.attrs, "class")?;
                for p in &d.props {
                    self.check_rtwc_attr(&p.attrs, "property")?;
                }
                // `extends <trait>` / `extends <interface>` → fatal
                // (error_009/error_010).
                if let Some(pn) = &d.parent {
                    let pl = pn.to_lowercase();
                    if self.traits.contains_key(&pl) {
                        return Err(PhpError::fatal(
                            format!("Class {} cannot extend trait {}", d.name, pn),
                            self.cur_line,
                        ));
                    }
                    if self.interfaces.contains_key(&pl) {
                        return Err(PhpError::fatal(
                            format!("Class {} cannot extend interface {}", d.name, pn),
                            self.cur_line,
                        ));
                    }
                }
                // `implements <non-interface>` → fatal (error_008).
                for i in &d.implements {
                    if !self.interfaces.contains_key(&i.to_lowercase()) {
                        let missing = !(self.lookup_class(i).is_some()
                            || self.traits.contains_key(&i.to_lowercase()));
                        if missing {
                            // Catchable Error like a missing trait
                            // (variance/unlinked_parent_2).
                            let v =
                                self.exception("Error", &format!("Interface \"{}\" not found", i));
                            return Err(self.throw(v));
                        }
                        return Err(PhpError::fatal(
                            format!("{} cannot implement {} - it is not an interface", d.name, i),
                            self.cur_line,
                        ));
                    }
                }
                // Implementing Serializable is deprecated (8.1+) — the
                // __serialize/__unserialize pair is the replacement.
                if !d.name.eq_ignore_ascii_case("serializable")
                    && self.implements_iface(&d, "serializable")
                {
                    self.deprecated(&format!(
                        "{} implements the Serializable interface, which is deprecated. Implement __serialize() and __unserialize() instead (or in addition, if support for old PHP versions is necessary)",
                        d.name
                    ))?;
                }
                // Magic-method declaration diagnostics are compile-time
                // in zend — they precede every link-time inheritance
                // fatal (magic_methods_008).
                // A private+final method (declared outright or produced
                // by `m as final` / `m as private` adaptations) warns
                // once per class (gh12854).
                if d.methods.iter().any(|m| {
                    m.is_final
                        && m.visibility == crate::ast::Visibility::Private
                        && m.trait_alias_of.is_none()
                }) {
                    self.warn("Private methods cannot be final as they are never overridden by other classes")?;
                }
                self.magic_method_checks(&d)?;
                self.linking.push(Rc::new(d.clone()));
                let checks_res = self
                    .check_interface_sigs(&d)
                    .and_then(|_| self.check_abstract_hooks(&d))
                    .and_then(|_| self.check_abstract_methods(&d))
                    .and_then(|_| self.check_final_override(&d))
                    .and_then(|_| self.check_override_sigs(&d))
                    .and_then(|_| self.check_const_types(&d));
                self.linking.pop();
                checks_res?;
                // Declaring __toString (incl. via a trait) implicitly
                // implements Stringable — added post-checks like zend,
                // so no sig-compat check runs against it
                // (stringable_automatic_implementation).
                if d.methods
                    .iter()
                    .any(|m| m.decl.name.eq_ignore_ascii_case("__tostring"))
                    && !d
                        .implements
                        .iter()
                        .any(|i| i.eq_ignore_ascii_case("stringable"))
                {
                    d.implements.push("Stringable".to_string());
                }
                self.classes.insert(
                    lname.clone(),
                    Rc::new(PhpClass {
                        decl: Rc::new(d),
                        statics: RefCell::new(HashMap::new()),
                        statics_init: RefCell::new(false),
                    }),
                );
                self.decl_order.push(lname);
            }
        }
        // Delayed variance obligations re-verify once the types they
        // waited on link — a registration re-checks only its own
        // ancestors' obligations, and never nests a pass inside a
        // running pass (class_order_autoload*).
        if !self.in_variance_pass && !self.variance_obligations.is_empty() {
            self.in_variance_pass = true;
            let res = self.process_variance_obligations(&decl);
            self.in_variance_pass = false;
            res?;
        }
        Ok(())
    }

    /// Zend's magic-method signature validation at class registration
    /// (zend_compile_magic_method): non-public visibility is a warning
    /// (ctor/dtor/clone exempt — private __clone is the
    /// clone-prevention idiom); wrong static-ness, arity or by-ref
    /// params are fatals. Checked after trait merge so merged methods
    /// validate too; interfaces and traits get the same rules.
    fn magic_method_checks(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        for m in &d.methods {
            let n = m.decl.name.to_lowercase();
            let (arity, want_static, vis_exempt): (Option<usize>, bool, bool) = match n.as_str() {
                "__call" | "__callstatic" => (Some(2), n == "__callstatic", false),
                "__get" | "__isset" | "__unset" | "__unserialize" => (Some(1), false, false),
                "__set" => (Some(2), false, false),
                "__set_state" => (Some(1), true, false),
                "__sleep" | "__wakeup" | "__tostring" | "__serialize" | "__debuginfo" => {
                    (Some(0), false, false)
                }
                "__invoke" => (None, false, false),
                "__construct" => (None, false, true),
                "__destruct" => (Some(0), false, true),
                "__clone" => (Some(0), false, true),
                _ => continue,
            };
            let cn = d.name.rsplit('\\').next().unwrap_or(&d.name);
            // Zend echoes the declared spelling in all diagnostics.
            let mn = &m.decl.name;
            // Zend order: arity first, then static-ness, by-ref and
            // type checks; the public-visibility warning only fires
            // when the signature is otherwise valid
            // (magic_methods_007/010).
            let nargs = m.decl.params.len();
            if let Some(need) = arity {
                if nargs != need {
                    return self.fail(PhpError::fatal(
                        if need == 0 {
                            format!("Method {}::{}() cannot take arguments", cn, mn)
                        } else {
                            format!(
                                "Method {}::{}() must take exactly {} argument{}",
                                cn,
                                mn,
                                need,
                                if need == 1 { "" } else { "s" }
                            )
                        },
                        m.decl.line,
                    ));
                }
            }
            if m.is_static != want_static {
                return self.fail(PhpError::fatal(
                    format!(
                        "Method {}::{}() {}",
                        cn,
                        mn,
                        if want_static {
                            "must be static"
                        } else {
                            "cannot be static"
                        }
                    ),
                    m.decl.line,
                ));
            }
            // `__invoke` is exempt — `function &__invoke(&$a)` is a
            // legal signature (closure_014).
            if n != "__invoke" && m.decl.params.iter().any(|p| p.by_ref) {
                return self.fail(PhpError::fatal(
                    format!("Method {}::{}() cannot take arguments by reference", cn, mn),
                    m.decl.line,
                ));
            }
            // Declared param types must still accept the values zend
            // passes (`?string` and `iterable` are fine where `string`
            // /`array` are required).
            let req_params: &[&str] = match n.as_str() {
                "__call" | "__callstatic" => &["string", "array"],
                "__get" | "__set" | "__isset" | "__unset" => &["string"],
                "__unserialize" | "__set_state" => &["array"],
                _ => &[],
            };
            for (i, req) in req_params.iter().enumerate() {
                if let Some(p) = m.decl.params.get(i) {
                    if let Some(ty) = &p.ty {
                        if !ty.iter().any(|t| {
                            t == req || t == "mixed" || (*req == "array" && t == "iterable")
                        }) {
                            return self.fail(PhpError::fatal(
                                format!(
                                    "{}::{}(): Parameter #{} (${}) must be of type {} when declared",
                                    cn,
                                    mn,
                                    i + 1,
                                    p.name,
                                    req
                                ),
                                m.decl.line,
                            ));
                        }
                    }
                }
            }
            // Declared return type must be a subtype of zend's
            // requirement (nullable allowed only when required).
            let req_ret: Option<&str> = match n.as_str() {
                "__isset" => Some("bool"),
                "__tostring" => Some("string"),
                "__sleep" | "__serialize" => Some("array"),
                "__debuginfo" => Some("?array"),
                "__set" | "__unset" | "__unserialize" | "__wakeup" | "__clone" => Some("void"),
                "__set_state" => Some("object"),
                _ => None,
            };
            if let Some(req) = req_ret {
                if let Some(ty) = &m.decl.ret {
                    let req_nullable = req.starts_with('?');
                    let req_base = req.trim_start_matches('?');
                    let declared_nullable = ty.iter().any(|t| t == "null");
                    let fits = (!declared_nullable || req_nullable)
                        && ty.iter().filter(|t| *t != "null").all(|t| {
                            t == req_base
                                || (req_base == "bool" && (t == "true" || t == "false"))
                                || (req_base == "object"
                                    && !matches!(
                                        t.as_str(),
                                        "int"
                                            | "float"
                                            | "string"
                                            | "bool"
                                            | "array"
                                            | "void"
                                            | "iterable"
                                            | "callable"
                                            | "mixed"
                                            | "never"
                                            | "false"
                                            | "true"
                                    ))
                        });
                    if !fits {
                        return self.fail(PhpError::fatal(
                            format!(
                                "{}::{}(): Return type must be {} when declared",
                                cn, mn, req
                            ),
                            m.decl.line,
                        ));
                    }
                }
            }
            if !vis_exempt && m.visibility != crate::ast::Visibility::Public {
                self.warn(&format!(
                    "The magic method {}::{}() must have public visibility",
                    cn, mn
                ))?;
            }
        }
        Ok(())
    }

    /// The class currently linking owes a signature re-check whenever
    /// one of its probes autoloads a type — record it before the
    /// autoloader runs so nested registrations can re-verify it.
    fn note_variance_obligation(&mut self) {
        if let Some(d) = self.linking.last() {
            let l = d.name.to_lowercase();
            if !self.variance_obligations.contains(&l) {
                self.variance_obligations.push(l);
            }
        }
    }

    /// Re-run the deferred signature checks for obligated ancestors
    /// of the just-linked class — a subclass can't link against a
    /// parent whose own variance is still unverified, so linking it
    /// forces the parent's obligations first (class_order_autoload*).
    /// Obligations on unrelated classes stay pending (error8).
    fn process_variance_obligations(&mut self, decl: &Rc<ClassDecl>) -> Result<(), PhpError> {
        let mut anc: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut work: Vec<String> = decl
            .parent
            .iter()
            .chain(decl.implements.iter())
            .map(|n| n.trim_start_matches('\\').to_lowercase())
            .collect();
        while let Some(n) = work.pop() {
            if !anc.insert(n.clone()) {
                continue;
            }
            let d = self
                .classes
                .get(&n)
                .map(|c| c.decl.clone())
                .or_else(|| self.interfaces.get(&n).cloned())
                .or_else(|| self.traits.get(&n).cloned())
                .or_else(|| {
                    self.linking
                        .iter()
                        .find(|c| c.name.to_lowercase() == n)
                        .cloned()
                });
            if let Some(d) = d {
                work.extend(
                    d.parent
                        .iter()
                        .chain(d.implements.iter())
                        .map(|x| x.trim_start_matches('\\').to_lowercase()),
                );
            }
        }
        let obls = std::mem::take(&mut self.variance_obligations);
        for lname in obls {
            if !anc.contains(&lname) {
                // Not an ancestor of what just linked — stays pending.
                self.variance_obligations.push(lname);
                continue;
            }
            let decl = self
                .classes
                .get(&lname)
                .map(|c| c.decl.clone())
                .or_else(|| self.interfaces.get(&lname).cloned())
                .or_else(|| {
                    self.linking
                        .iter()
                        .find(|c| c.name.to_lowercase() == lname)
                        .cloned()
                });
            let Some(d) = decl else { continue };
            self.check_interface_sigs(&d)?;
            self.check_override_sigs(&d)?;
        }
        Ok(())
    }

    /// `self`/`static`/`parent` members in method/prop types bind to
    /// the declaring class name at registration (anonymous_class).
    fn resolve_scope_tys(d: &mut ClassDecl) {
        let (dn, dp) = (d.name.clone(), d.parent.clone());
        let resolve = |ms: &mut Vec<String>| {
            for m in ms.iter_mut() {
                let l = m.to_lowercase();
                // `static` stays literal — late-static binds to the
                // called class, resolved at check time.
                if l == "self" {
                    *m = dn.clone();
                } else if l == "parent" {
                    if let Some(p) = &dp {
                        *m = p.clone();
                    }
                }
            }
        };
        for m in d.methods.iter_mut() {
            let mut mm = (**m).clone();
            for p in mm.decl.params.iter_mut() {
                if let Some(ty) = &mut p.ty {
                    resolve(ty);
                }
            }
            if let Some(ty) = &mut mm.decl.ret {
                resolve(ty);
            }
            *m = Rc::new(mm);
        }
        for p in d.props.iter_mut() {
            if let Some(ty) = &mut p.ty {
                resolve(ty);
            }
        }
    }

    /// Merge used traits' methods into `d`, applying `insteadof`
    /// exclusions, `as` aliases/visibility changes, and collision
    /// detection. Trait origin is preserved on each merged method as
    /// `decl.decl_in` (drives `__METHOD__`/`__TRAIT__`).
    fn merge_trait_adaptations(&mut self, d: &mut ClassDecl) -> Result<(), PhpError> {
        let used: Vec<(String, Rc<ClassDecl>)> = d
            .traits
            .iter()
            .filter_map(|t| {
                self.traits
                    .get(&t.to_lowercase())
                    .map(|td| (t.clone(), td.clone()))
            })
            .collect();
        let cur_line = self.cur_line;
        let dname = d.name.clone();
        let is_used = |n: &str| d.traits.iter().any(|u| u.eq_ignore_ascii_case(n));
        // `insteadof`/`as` on a trait that doesn't exist: distinct
        // message from exists-but-not-used (precedence_unknown_class).
        // Free-standing fn (not a closure) so later `&mut self` calls
        // don't conflict with a captured borrow of `self.traits`.
        fn not_found(
            traits: &HashMap<String, Rc<ClassDecl>>,
            n: &str,
            dname: &str,
            line: usize,
        ) -> PhpError {
            if traits.contains_key(&n.to_lowercase()) {
                PhpError::fatal(
                    format!("Required Trait {} wasn't added to {}", n, dname),
                    line,
                )
            } else {
                PhpError::fatal(format!("Could not find trait {}", n), line)
            }
        }
        // `static`/`self`/`parent` are reserved — never valid trait
        // names (static_in_trait_*).
        let reserved = |n: &str| {
            ["static", "self", "parent"]
                .iter()
                .any(|r| n.eq_ignore_ascii_case(r))
        };
        for t in &d.traits {
            if reserved(t) {
                return Err(PhpError::fatal(
                    format!("Cannot use \"{}\" as trait name, as it is reserved", t),
                    cur_line,
                ));
            }
        }
        // Validate adaptation trait references.
        for ad in &d.adaptations {
            match ad {
                crate::ast::TraitAdaptation::Insteadof {
                    trait_name,
                    method,
                    excludes,
                } => {
                    for n in std::iter::once(trait_name).chain(excludes.iter()) {
                        if reserved(n) {
                            return Err(PhpError::fatal(
                                format!("Cannot use \"{}\" as trait name, as it is reserved", n),
                                cur_line,
                            ));
                        }
                        // A class name in `as`/`insteadof` is its own
                        // error (bug64235).
                        if !is_used(n) && self.classes.contains_key(&n.to_lowercase()) {
                            return Err(PhpError::fatal(
                                format!(
                                    "Class {} is not a trait, Only traits may be used in 'as' and 'insteadof' statements",
                                    n
                                ),
                                cur_line,
                            ));
                        }
                    }
                    if !is_used(trait_name) {
                        return Err(not_found(&self.traits, trait_name, &dname, cur_line));
                    }
                    // `T::m insteadof ...` — the method must exist in T
                    // (bug60165d).
                    if let Some(td) = self.traits.get(&trait_name.to_lowercase()) {
                        if !td
                            .methods
                            .iter()
                            .any(|m| m.decl.name.eq_ignore_ascii_case(method))
                        {
                            return Err(PhpError::fatal(
                                format!(
                                    "A precedence rule was defined for {}::{} but this method does not exist",
                                    trait_name, method
                                ),
                                cur_line,
                            ));
                        }
                    }
                    for e in excludes {
                        if !is_used(e) {
                            return Err(not_found(&self.traits, e, &dname, cur_line));
                        }
                    }
                    if excludes.iter().any(|e| e.eq_ignore_ascii_case(trait_name)) {
                        return Err(PhpError::fatal(
                            format!(
                                "Inconsistent insteadof definition. The method {} is to be used from {}, but {} is also on the exclude list",
                                method, trait_name, trait_name
                            ),
                            cur_line,
                        ));
                    }
                }
                crate::ast::TraitAdaptation::Alias {
                    trait_name: Some(tn),
                    ..
                } => {
                    if !is_used(tn) {
                        if self.classes.contains_key(&tn.to_lowercase()) {
                            return Err(PhpError::fatal(
                                format!(
                                    "Class {} is not a trait, Only traits may be used in 'as' and 'insteadof' statements",
                                    tn
                                ),
                                cur_line,
                            ));
                        }
                        return Err(not_found(&self.traits, tn, &dname, cur_line));
                    }
                }
                crate::ast::TraitAdaptation::Alias {
                    trait_name: None,
                    method,
                    alias,
                    ..
                } => {
                    // `method as alias` with no qualifier — method must
                    // exist somewhere among the used traits.
                    if alias.is_some()
                        && !used.iter().any(|(_, td)| {
                            td.methods
                                .iter()
                                .any(|m| m.decl.name.eq_ignore_ascii_case(method))
                        })
                    {
                        return Err(PhpError::fatal(
                            format!(
                                "An alias ({}) was defined for method {}(), but this method does not exist",
                                alias.as_deref().unwrap_or_default(),
                                method
                            ),
                            cur_line,
                        ));
                    }
                }
            }
        }
        // insteadof exclusions: (method_lname, suppressed trait_lname)
        let mut exclusions: Vec<(String, String)> = Vec::new();
        for ad in &d.adaptations {
            if let crate::ast::TraitAdaptation::Insteadof {
                method, excludes, ..
            } = ad
            {
                for e in excludes {
                    // Each trait's method may be excluded only once
                    // (error_010).
                    if exclusions
                        .iter()
                        .any(|(mn, et)| *mn == method.to_lowercase() && et.eq_ignore_ascii_case(e))
                    {
                        return Err(PhpError::fatal(
                            format!(
                                "Failed to evaluate a trait precedence ({}). Method of trait {} was defined to be excluded multiple times",
                                method, e
                            ),
                            cur_line,
                        ));
                    }
                    exclusions.push((method.to_lowercase(), e.to_lowercase()));
                }
            }
        }
        // Merge methods in `use` order. Class-own methods always win
        // silently; a trait's ABSTRACT requirement is still checked
        // against the class's implementation (abstract_method_*).
        // `taken` = name -> (display trait, origin trait, own|abstract)
        let mut taken: HashMap<String, (String, String)> = HashMap::new();
        for m in &d.methods {
            taken.insert(m.decl.name.to_lowercase(), (dname.clone(), String::new()));
        }
        for (t, td) in &used {
            for m in &td.methods {
                let lname = m.decl.name.to_lowercase();
                if exclusions
                    .iter()
                    .any(|(mn, et)| *mn == lname && et.eq_ignore_ascii_case(t))
                {
                    continue;
                }
                let origin = m.decl.decl_in.clone().unwrap_or_else(|| t.clone());
                // A trait abstract requirement already implemented by
                // an ancestor stays inherited, not merged (bug55424) —
                // but the inherited impl must still be signature-
                // compatible with the abstract (gh14009_002).
                if m.is_abstract {
                    if let Some((aon, am)) = self.ancestor_concrete(d, &lname) {
                        if let Some(e) = self.trait_sig_error(&am, m, &aon, &origin, false) {
                            return Err(e);
                        }
                        continue;
                    }
                }
                if let Some((pt, porig)) = taken.get(&lname).cloned() {
                    // Diamond reuse of the same origin trait is fine
                    // (bug63911).
                    if porig.eq_ignore_ascii_case(&origin) {
                        continue;
                    }
                    let class_own = porig.is_empty();
                    let existing = d
                        .methods
                        .iter()
                        .find(|x| x.decl.name.eq_ignore_ascii_case(&m.decl.name))
                        .cloned();
                    // Abstract requirements interplay with what holds
                    // the name already.
                    if m.is_abstract || existing.as_ref().is_some_and(|x| x.is_abstract) {
                        if class_own {
                            // Class impl must satisfy the abstract
                            // (abstract_method_1/3/4/5).
                            if let Some(ex) = existing {
                                if let Some(e) =
                                    self.trait_sig_error(&ex, m, &dname, &origin, false)
                                {
                                    return Err(e);
                                }
                            }
                            continue;
                        }
                        if m.is_abstract && existing.as_ref().is_some_and(|x| x.is_abstract) {
                            // Two abstract requirements: signatures must
                            // agree in both directions (bug60217).
                            let ex = existing.unwrap();
                            if let Some(e) = self.trait_sig_error(&ex, m, &pt, &origin, true) {
                                return Err(e);
                            }
                            continue;
                        }
                        // concrete-vs-abstract: concrete impl checked
                        // against the abstract requirement; on success
                        // the concrete replaces (or keeps) the slot.
                        let (impl_m, abs_m, abs_owner) = if m.is_abstract {
                            (existing.clone().unwrap(), m.clone(), &origin)
                        } else {
                            (m.clone(), existing.clone().unwrap(), &porig)
                        };
                        if let Some(e) =
                            self.trait_sig_error(&impl_m, &abs_m, &dname, abs_owner, false)
                        {
                            return Err(e);
                        }
                        if m.is_abstract {
                            continue;
                        }
                        // Concrete replaces the abstract slot.
                        if let Some(slot) = d
                            .methods
                            .iter_mut()
                            .find(|x| x.decl.name.eq_ignore_ascii_case(&m.decl.name))
                        {
                            let mut m2 = (**m).clone();
                            m2.decl.decl_in = Some(origin.clone());
                            *slot = Rc::new(m2);
                            taken.insert(lname, (t.clone(), origin));
                        }
                        continue;
                    }
                    if class_own {
                        continue;
                    }
                    return Err(PhpError::fatal(
                        format!(
                            "Trait method {}::{} has not been applied as {}::{}, because of collision with {}::{}",
                            t, m.decl.name, dname, m.decl.name, pt, m.decl.name
                        ),
                        cur_line,
                    ));
                }
                let mut m2 = (**m).clone();
                m2.decl.decl_in = Some(origin.clone());
                taken.insert(lname, (t.clone(), origin));
                d.methods.push(Rc::new(m2));
            }
        }
        // Aliases: clone the source method under a new name and/or apply
        // a visibility override to the merged original.
        for ad in &d.adaptations {
            let crate::ast::TraitAdaptation::Alias {
                trait_name,
                method,
                alias,
                vis,
                is_final,
            } = ad
            else {
                continue;
            };
            if let Some(tn) = trait_name {
                if reserved(tn) {
                    return Err(PhpError::fatal(
                        format!("Cannot use \"{}\" as trait name, as it is reserved", tn),
                        cur_line,
                    ));
                }
                if !is_used(tn) {
                    if self.classes.contains_key(&tn.to_lowercase()) {
                        return Err(PhpError::fatal(
                            format!(
                                "Class {} is not a trait, Only traits may be used in 'as' and 'insteadof' statements",
                                tn
                            ),
                            cur_line,
                        ));
                    }
                    return Err(not_found(&self.traits, tn, &dname, cur_line));
                }
            }
            let src: Option<Rc<MethodDecl>> = match trait_name {
                Some(tn) => self.traits.get(&tn.to_lowercase()).and_then(|td| {
                    td.methods
                        .iter()
                        .find(|m| m.decl.name.eq_ignore_ascii_case(method))
                        .cloned()
                }),
                None => {
                    // Unqualified `m as x` is ambiguous when >1 used
                    // trait provides `m` (bug62069).
                    let mut holders = used.iter().filter(|(_, td)| {
                        td.methods
                            .iter()
                            .any(|m| m.decl.name.eq_ignore_ascii_case(method))
                    });
                    match (holders.next(), holders.next()) {
                        (Some((n1, _)), Some((n2, _))) => {
                            return Err(PhpError::fatal(
                                format!(
                                    "An alias was defined for method {}(), which exists in both {} and {}. Use {}::{} or {}::{} to resolve the ambiguity",
                                    method, n1, n2, n1, method, n2, method
                                ),
                                cur_line,
                            ));
                        }
                        (Some((_, td1)), None) => td1
                            .methods
                            .iter()
                            .find(|m| m.decl.name.eq_ignore_ascii_case(method))
                            .cloned(),
                        _ => None,
                    }
                }
            };
            let Some(m) = src else {
                // `T::m as x` cites the qualified name; `m as x` cites
                // the alias (bug60165b); a bare `m as vis` modifier is
                // the "modifiers changed" wording (bug54441).
                if trait_name.is_some() {
                    return Err(PhpError::fatal(
                        format!(
                            "An alias was defined for {}::{} but this method does not exist",
                            trait_name.as_deref().unwrap_or_default(),
                            method
                        ),
                        cur_line,
                    ));
                }
                if let Some(a) = alias {
                    return Err(PhpError::fatal(
                        format!(
                            "An alias ({}) was defined for method {}(), but this method does not exist",
                            a, method
                        ),
                        cur_line,
                    ));
                }
                return Err(PhpError::fatal(
                    format!(
                        "The modifiers of the trait method {}() are changed, but this method does not exist. Error in",
                        method
                    ),
                    cur_line,
                ));
            };
            // `as abstract|final|static` are not valid alias modifiers
            // (language018/019) — PHP rejects them as alias names too.
            for bad in ["abstract", "static"] {
                if alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(bad))
                {
                    return Err(PhpError::fatal(
                        format!("Cannot use \"{}\" as method modifier in trait alias", bad),
                        cur_line,
                    ));
                }
            }
            let origin = m
                .decl
                .decl_in
                .clone()
                .or_else(|| trait_name.clone())
                .unwrap_or_default();
            if let Some(a) = alias {
                let alname = a.to_lowercase();
                // An aliased name collides like a real method
                // (language010/014): a merged method or an earlier
                // alias holding the name is fatal.
                let src_trait = trait_name.clone().unwrap_or_else(|| {
                    used.iter()
                        .find(|(_, td)| {
                            td.methods
                                .iter()
                                .any(|mm| mm.decl.name.eq_ignore_ascii_case(method))
                        })
                        .map(|(t, _)| t.clone())
                        .unwrap_or_default()
                });
                if let Some((pt, porg)) = taken.get(&alname) {
                    if !porg.is_empty() {
                        // Trait order decides the loser: a slot held by
                        // a trait merged LATER loses to the earlier
                        // trait's alias (language010 vs language014).
                        let ord =
                            |n: &str| used.iter().position(|(u, _)| u.eq_ignore_ascii_case(n));
                        let (lt, lm, wt) = match (ord(&src_trait), ord(pt)) {
                            (Some(si), Some(pi)) if pi > si => {
                                (pt.clone(), a.clone(), src_trait.clone())
                            }
                            _ => (src_trait.clone(), method.clone(), pt.clone()),
                        };
                        return Err(PhpError::fatal(
                            format!(
                                "Trait method {}::{} has not been applied as {}::{}, because of collision with {}::{}",
                                lt, lm, dname, a, wt, a
                            ),
                            cur_line,
                        ));
                    }
                    // The class's own method wins silently (bug61998).
                    continue;
                }
                let mut m2 = (*m).clone();
                m2.decl.name = a.clone();
                if let Some(v) = vis {
                    m2.visibility = *v;
                }
                if *is_final {
                    m2.is_final = true;
                }
                m2.decl.decl_in = Some(origin.clone());
                m2.trait_alias_of = Some(m.decl.name.clone());
                taken.insert(alname, (src_trait, origin));
                d.methods.push(Rc::new(m2));
            } else if vis.is_some() || *is_final {
                // `m as private` / `m as final` — modifier change on the
                // merged original itself.
                for slot in d.methods.iter_mut() {
                    if slot.decl.name.eq_ignore_ascii_case(method) {
                        let mut m2 = (**slot).clone();
                        if let Some(v) = vis {
                            m2.visibility = *v;
                        }
                        if *is_final {
                            m2.is_final = true;
                        }
                        *slot = Rc::new(m2);
                        break;
                    }
                }
            }
        }
        // Trait props merge with the identical-definition rule
        // (property001/002, bug74922): differing decls fatal; hooked
        // props can't be resolved at all.
        for (t, td) in &used {
            for p in &td.props {
                if let Some(ex) = d.props.iter().find(|x| x.name == p.name) {
                    if ex.hooks.is_some() || p.hooks.is_some() {
                        let ex_src = ex.decl_in.clone().unwrap_or_else(|| dname.clone());
                        return Err(PhpError::fatal(
                            format!(
                                "{} and {} define the same hooked property (${}) in the composition of {}. Conflict resolution between hooked properties is currently not supported. Class was composed",
                                ex_src, t, p.name, dname
                            ),
                            cur_line,
                        ));
                    }
                    let compat = ex.visibility == p.visibility
                        && ex.is_static == p.is_static
                        && ex.readonly == p.readonly
                        && ex.ty == p.ty
                        && self.const_exprs_eq(
                            &ex.default,
                            &ex.decl_in.clone().unwrap_or_else(|| dname.clone()),
                            &p.default,
                            t,
                        );
                    if !compat {
                        let ex_src = ex.decl_in.clone().unwrap_or_else(|| dname.clone());
                        return Err(PhpError::fatal(
                            format!(
                                "{} and {} define the same property (${}) in the composition of {}. However, the definition differs and is considered incompatible. Class was composed",
                                ex_src, t, p.name, dname
                            ),
                            cur_line,
                        ));
                    }
                    continue;
                }
                let mut np = p.clone();
                np.decl_in = Some(t.clone());
                d.props.push(np);
            }
        }
        // Trait constants merge like props: same name+identical
        // definition is fine, differing definitions are fatal
        // (constant_*). `T::CONST` direct access is rejected at the
        // lookup site instead.
        for (t, td) in &used {
            for cd in &td.consts {
                if let Some(ex) = d.consts.iter().find(|x| x.name == cd.name) {
                    let compat = ex.visibility == cd.visibility
                        && ex.is_final == cd.is_final
                        && ty_list_eq(&ex.ty, &cd.ty)
                        && self.const_exprs_eq(
                            &Some(ex.value.clone()),
                            &ex.decl_in.clone().unwrap_or_else(|| dname.clone()),
                            &Some(cd.value.clone()),
                            t,
                        );
                    if !compat {
                        let ex_src = ex.decl_in.clone().unwrap_or_else(|| dname.clone());
                        return Err(PhpError::fatal(
                            format!(
                                "{} and {} define the same constant ({}) in the composition of {}. However, the definition differs and is considered incompatible. Class was composed",
                                ex_src, t, cd.name, dname
                            ),
                            cur_line,
                        ));
                    }
                    continue;
                }
                let mut nc = cd.clone();
                nc.decl_in = Some(t.clone());
                d.consts.push(nc);
            }
        }
        Ok(())
    }

    /// Two default-value exprs are compatible when their evaluated
    /// values match loosely (bug74922, constant_016); falls back to a
    /// textual compare when either side won't eval (both-None is fine).
    /// Each side evals in its declaring trait's namespace so an
    /// unqualified `FOO` in `Bug74922\T1` means `Bug74922\FOO`.
    fn const_exprs_eq(
        &mut self,
        a: &Option<Expr>,
        a_owner: &str,
        b: &Option<Expr>,
        b_owner: &str,
    ) -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(x), Some(y)) => {
                match (self.eval_in_ns(x, a_owner), self.eval_in_ns(y, b_owner)) {
                    (Ok(va), Ok(vb)) => identical(&va, &vb),
                    _ => format!("{:?}", x) == format!("{:?}", y),
                }
            }
            _ => false,
        }
    }

    /// Const-eval an expr as if inside `owner`'s namespace (trait prop/
    /// const defaults resolve unqualified names against their declaring
    /// namespace — bug74922b).
    fn eval_in_ns(&mut self, e: &Expr, owner: &str) -> Result<Value, PhpError> {
        let ns = owner
            .rsplit_once('\\')
            .map(|(p, _)| p.to_string())
            .unwrap_or_default();
        let old = std::mem::replace(&mut self.globals.ns, ns);
        let f = self.cur_file.clone();
        let r = self.eval_decl_const(e, &f);
        self.globals.ns = old;
        r
    }

    /// Param-list render for "Declaration of X::m(...) must be
    /// compatible" diagnostics (Zend prints the declared signature).
    /// Render one type member for signature messages: `self` resolves
    /// against the composing class (abstract_method_10), other members
    /// stay verbatim.
    fn sig_ty(ty: &[String], ctx: &str) -> String {
        // `X|null` renders as `?X` in Zend signatures (internal_parent).
        if ty.len() == 2 {
            if let Some(other) = ty.iter().find(|t| !t.eq_ignore_ascii_case("null")) {
                if ty.iter().any(|t| t.eq_ignore_ascii_case("null")) && !other.contains('&') {
                    return format!(
                        "?{}",
                        if other.eq_ignore_ascii_case("self") {
                            ctx.to_string()
                        } else {
                            other.clone()
                        }
                    );
                }
            }
        }
        ty.iter()
            .map(|t| {
                if t.eq_ignore_ascii_case("self") {
                    ctx.to_string()
                } else if t.eq_ignore_ascii_case("iterable") {
                    // Compatibility messages render the normalized
                    // form (invalid5).
                    "Traversable|array".to_string()
                } else if t.contains('&') && ty.len() > 1 {
                    format!("({t})")
                } else {
                    t.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    fn sig_str(f: &crate::ast::FunctionDecl, ctx: &str) -> String {
        f.params
            .iter()
            .map(|p| {
                let ty =
                    p.ty.as_ref()
                        .map(|t| Self::sig_ty(t, ctx) + " ")
                        .unwrap_or_default();
                let br = if p.by_ref { "&" } else { "" };
                let var = if p.variadic {
                    format!("...${}", p.name)
                } else {
                    format!("${}", p.name)
                };
                let def = match &p.default {
                    Some(Expr::Int(i)) => format!(" = {}", i),
                    Some(Expr::Float(f)) => format!(" = {}", f),
                    Some(Expr::Str(b)) => format!(" = '{}'", b),
                    Some(Expr::Null) => " = null".to_string(),
                    Some(Expr::Bool(b)) => format!(" = {}", b),
                    Some(Expr::Const(c)) => format!(" = {}", c),
                    Some(Expr::ClassConst { class, name }) => match class.as_ref() {
                        Expr::Var(n) | Expr::Const(n) => format!(" = {}::{}", n, name),
                        _ => format!(" = {}", name),
                    },
                    Some(Expr::ArrayLit(_)) => " = []".to_string(),
                    _ => String::new(),
                };
                format!("{}{}{}{}", ty, br, var, def)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `sig_str` variant that appends `: ret` OUTSIDE the paren — used
    /// inside `({})` placeholders, so the signature is `(params): ret`.
    fn sig_str_full(f: &crate::ast::FunctionDecl, ctx: &str) -> String {
        let ps = Self::sig_str(f, ctx);
        match &f.ret {
            Some(r) => format!("({}): {}", ps, Self::sig_ty(r, ctx)),
            None => format!("({})", ps),
        }
    }

    /// The (declaring-class name, method) pair for `lname` provided by
    /// a concrete method in `d`'s ancestor chain — nearest wins.
    fn ancestor_concrete(&self, d: &ClassDecl, lname: &str) -> Option<(String, Rc<MethodDecl>)> {
        let mut pn = d.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()).cloned() else {
                break;
            };
            if let Some(m) = pc
                .decl
                .methods
                .iter()
                .find(|m| m.decl.name.to_lowercase() == lname && !m.is_abstract)
            {
                return Some((pc.decl.name.clone(), m.clone()));
            }
            pn = pc.decl.parent.clone();
        }
        None
    }

    /// Signature-compat check of an implementation against a trait's
    /// abstract requirement (abstract_method_*). `both_abs` marks the
    /// two-traits-both-abstract case where Zend cites trait names for
    /// both sides.
    fn trait_sig_error(
        &mut self,
        impl_m: &Rc<MethodDecl>,
        abs_m: &Rc<MethodDecl>,
        impl_disp: &str,
        abs_disp: &str,
        both_abs: bool,
    ) -> Option<PhpError> {
        let m = &impl_m.decl.name;
        if impl_m.is_static != abs_m.is_static {
            return Some(PhpError::fatal(
                format!(
                    "Cannot make {} method {}::{}() {} in class {}",
                    if abs_m.is_static {
                        "static"
                    } else {
                        "non static"
                    },
                    abs_disp,
                    m,
                    if impl_m.is_static {
                        "static"
                    } else {
                        "non static"
                    },
                    // impl_disp is the using class for concrete impls;
                    // for two abstract traits Zend still prints the
                    // class being composed... using impl_disp for both.
                    impl_disp
                ),
                self.cur_line,
            ));
        }
        let req = |ms: &MethodDecl| {
            ms.decl
                .params
                .iter()
                .filter(|p| p.default.is_none() && !p.variadic)
                .count()
        };
        let (ir, ar) = (req(impl_m), req(abs_m));
        let count_ok = ir <= ar
            && (impl_m.decl.params.iter().any(|p| p.variadic)
                || (!abs_m.decl.params.iter().any(|p| p.variadic)
                    && impl_m.decl.params.len() >= abs_m.decl.params.len()));
        let mut ok = count_ok;
        // An unresolvable class member makes the check impossible
        // rather than incompatible — Zend reports which class it
        // couldn't load (variance/trait_error, abstract_constructor).
        let mut miss: Option<String> = None;
        if ok {
            for (i, ap) in abs_m.decl.params.iter().enumerate() {
                if ap.variadic {
                    break;
                }
                let Some(ip) = impl_m.decl.params.get(i) else {
                    ok = false;
                    break;
                };
                if ip.variadic {
                    break;
                }
                if ip.by_ref != ap.by_ref {
                    ok = false;
                    break;
                }
                let it = ip.ty.clone().unwrap_or_else(|| vec!["mixed".into()]);
                let at = ap.ty.clone().unwrap_or_else(|| vec!["mixed".into()]);
                if !self.ty_sup(&it, &at) {
                    miss = self.first_unres(&it).or_else(|| self.first_unres(&at));
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            // Return-type covariance: an untyped impl fails a typed
            // abstract; a typed impl must be a subtype of the abstract's
            // (`never` bottoms out any requirement) (bug81192).
            ok = match (&impl_m.decl.ret, &abs_m.decl.ret) {
                (_, None) => true,
                (None, Some(_)) => false,
                (Some(ir), Some(ar)) => {
                    let resolve = |ms: &[String]| -> Vec<String> {
                        ms.iter()
                            .map(|m| {
                                if m.eq_ignore_ascii_case("self") {
                                    impl_disp.to_string()
                                } else {
                                    m.clone()
                                }
                            })
                            .collect()
                    };
                    let (ir2, ar2) = (resolve(ir), resolve(ar));
                    // `static` in the abstract keeps its late-static
                    // meaning: the impl's own class satisfies it only
                    // when the class is final (self == static then);
                    // a real subclass member always narrows it
                    // (override_static_with_self/*).
                    let impl_final = self.linking.last().map(|c| c.is_final).unwrap_or(false);
                    let covers = ir2.iter().all(|t| {
                        ar2.iter().any(|s| {
                            if s.eq_ignore_ascii_case("static") {
                                t.eq_ignore_ascii_case("static")
                                    || (t.eq_ignore_ascii_case(impl_disp) && impl_final)
                                    || self.ty_member_is_a(t, impl_disp)
                                        && !t.eq_ignore_ascii_case(impl_disp)
                            } else if t.eq_ignore_ascii_case("static") {
                                // impl-side `static` ⊆ s when the impl
                                // class is-a s (any late-static callee
                                // is still an s) (static_variance_success).
                                self.ty_member_is_a(impl_disp, s)
                            } else {
                                self.ty_member_is_a(t, s)
                            }
                        })
                    });
                    let pass = ir2.iter().any(|m| m.eq_ignore_ascii_case("never")) || covers;
                    if !pass {
                        miss = self.first_unres(&ir2).or_else(|| self.first_unres(&ar2));
                    }
                    pass
                }
            };
        }
        // A fatal raised inside an autoload the probes triggered
        // aborts the whole check — the original error wins over any
        // synthesized compatibility message (error3 cascade).
        if let Some(e) = self.sig_fatal.take() {
            return Some(e);
        }
        if ok && both_abs {
            // Requirements must agree in BOTH directions (bug60217c).
            return self.trait_sig_error(abs_m, impl_m, abs_disp, impl_disp, false);
        }
        if ok {
            return None;
        }
        // An unresolvable class member makes the check impossible
        // rather than incompatible — Zend reports which class it
        // couldn't load (variance/trait_error, abstract_constructor).
        if let Some(cn) = miss {
            if self.autoloading.contains(&cn.to_lowercase()) {
                // The compared type's own autoload is still in flight —
                // Zend defers the verdict; the obligation re-runs when
                // the type links (class_order_autoload1).
                return None;
            }
            let mut e = PhpError::fatal(
                format!(
                    "Could not check compatibility between {}::{}{} and {}::{}{}, because class {} is not available",
                    impl_disp,
                    m,
                    Self::sig_str_full(&impl_m.decl, impl_disp),
                    abs_disp,
                    m,
                    Self::sig_str_full(&abs_m.decl, impl_disp),
                    cn
                ),
                impl_m.decl.line,
            );
            e.line = impl_m.decl.line;
            if impl_m.decl.file != self.diag_file() {
                self.last_err_file = impl_m.decl.file.clone();
            }
            return Some(e);
        }
        // Zend cites the implementing method's declaration — for merged
        // trait methods that's the trait's own file/line (bug81192).
        let mut e = PhpError::fatal(
            format!(
                "Declaration of {}::{}{} must be compatible with {}::{}{}",
                impl_disp,
                m,
                Self::sig_str_full(&impl_m.decl, impl_disp),
                abs_disp,
                m,
                Self::sig_str_full(&abs_m.decl, impl_disp)
            ),
            impl_m.decl.line,
        );
        e.line = impl_m.decl.line;
        if impl_m.decl.file != self.diag_file() {
            self.last_err_file = impl_m.decl.file.clone();
        }
        Some(e)
    }

    /// Interface method signatures must be compatible with the class's
    /// implementation (bug60153): same rules as trait abstracts.
    fn check_interface_sigs(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        // (iface, display-for-errors): own `implements` cites the
        // interface; an ancestor's requirement cites the ancestor
        // (bug62358).
        let mut ifaces: Vec<(Rc<ClassDecl>, String)> = Vec::new();
        for iname in &d.implements {
            if let Some(f) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                ifaces.push((f, String::new()));
            }
        }
        let mut chain: Vec<Rc<ClassDecl>> = Vec::new();
        let mut pn = d.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()) else {
                break;
            };
            chain.push(pc.decl.clone());
            pn = pc.decl.parent.clone();
        }
        for c in &chain {
            for iname in &c.implements {
                if let Some(f) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                    ifaces.push((f, c.name.clone()));
                }
            }
        }
        let mut seen = 0;
        while seen < ifaces.len() {
            let (f, disp) = ifaces[seen].clone();
            seen += 1;
            let cite = if disp.is_empty() {
                f.name.clone()
            } else {
                disp
            };
            for im in &f.methods {
                if let Some(impl_m) = d
                    .methods
                    .iter()
                    .find(|m| m.decl.name.eq_ignore_ascii_case(&im.decl.name))
                    .cloned()
                {
                    // Interface methods must stay public in the
                    // implementation (bug69467).
                    if !matches!(impl_m.visibility, crate::ast::Visibility::Public) {
                        return Err(PhpError::fatal(
                            format!(
                                "Access level to {}::{}() must be public (as in class {})",
                                d.name, im.decl.name, cite
                            ),
                            impl_m.decl.line,
                        ));
                    }
                    if !self.builtin_ifaces.contains(&f.name.to_lowercase()) {
                        if let Some(e) = self.trait_sig_error(&impl_m, im, &d.name, &cite, false) {
                            return Err(e);
                        }
                    }
                }
            }
            for p2 in &f.implements {
                if let Some(pp) = self.interfaces.get(&p2.to_lowercase()).cloned() {
                    ifaces.push((pp, String::new()));
                }
            }
        }
        Ok(())
    }

    /// Non-abstract classes must implement every abstract method: own/
    /// trait-merged abstracts (labelled `C::m`), plus abstracts from
    /// ancestor classes and interfaces (labelled `Src::m`).
    fn check_abstract_methods(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        if d.kind != crate::ast::ClassKind::Class {
            return Ok(());
        }
        // A private abstract requirement can't be delegated to a
        // subclass — the composing class itself must implement it,
        // even when abstract (abstract_method_6).
        let priv_missing: Vec<String> = d
            .methods
            .iter()
            .filter(|m| {
                m.is_abstract
                    && m.visibility == crate::ast::Visibility::Private
                    && m.decl.decl_in.is_some()
            })
            .map(|m| format!("{}::{}", d.name, m.decl.name))
            .collect();
        if !priv_missing.is_empty() {
            let n = priv_missing.len();
            return Err(PhpError::fatal(
                format!(
                    "Class {} must implement {} abstract method{} ({})",
                    d.name,
                    n,
                    if n == 1 { "" } else { "s" },
                    priv_missing.join(", ")
                ),
                self.cur_line,
            ));
        }
        if d.is_abstract {
            return Ok(());
        }
        // Concrete impls visible to this class: own methods (incl.
        // trait-merged) plus ancestor classes' methods.
        let mut chain: Vec<Rc<ClassDecl>> = Vec::new();
        let mut pn = d.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()) else {
                break;
            };
            chain.push(pc.decl.clone());
            pn = pc.decl.parent.clone();
        }
        // `end` = ancestors chain[0..end] that may satisfy the abstract
        // (the declaring link itself plus everything below it).
        let concrete = |lname: &str, end: usize| -> bool {
            if d.methods
                .iter()
                .any(|m| m.decl.name.to_lowercase() == lname && !m.is_abstract)
            {
                return true;
            }
            chain.iter().take(end).any(|c| {
                c.methods
                    .iter()
                    .any(|m| m.decl.name.to_lowercase() == lname && !m.is_abstract)
            })
        };
        let mut missing: Vec<String> = Vec::new();
        // Own + trait-merged abstracts first (label: this class).
        for m in &d.methods {
            if m.is_abstract {
                let lname = m.decl.name.to_lowercase();
                if concrete(&lname, chain.len()) {
                    continue;
                }
                let label = format!("{}::{}", d.name, m.decl.name);
                if !missing.contains(&label) {
                    missing.push(label);
                }
            }
        }
        // Ancestor abstracts: an impl must also be signature-compat
        // (bug62358) — find it in this class or a descendant link.
        for (i, c) in chain.iter().enumerate() {
            for m in &c.methods {
                if !m.is_abstract {
                    continue;
                }
                let impl_at =
                    |m: &Rc<MethodDecl>| -> Option<(Rc<MethodDecl>, String)> {
                        if let Some(x) = d.methods.iter().find(|x| {
                            x.decl.name.eq_ignore_ascii_case(&m.decl.name) && !x.is_abstract
                        }) {
                            return Some((x.clone(), d.name.clone()));
                        }
                        chain.iter().take(i + 1).find_map(|c2| {
                            c2.methods
                                .iter()
                                .find(|x| {
                                    x.decl.name.eq_ignore_ascii_case(&m.decl.name) && !x.is_abstract
                                })
                                .map(|x| (x.clone(), c2.name.clone()))
                        })
                    };
                if let Some((im, iname)) = impl_at(m) {
                    if let Some(e) = self.trait_sig_error(&im, m, &iname, &c.name, false) {
                        let mut e = e;
                        e.line = im.decl.line;
                        return Err(e);
                    }
                    continue;
                }
                let label = format!("{}::{}", c.name, m.decl.name);
                if !missing.contains(&label) {
                    missing.push(label);
                }
            }
        }
        // Interfaces implemented anywhere in the chain (including
        // this class's own `implements`).
        let mut ifaces: Vec<Rc<ClassDecl>> = Vec::new();
        for iname in d
            .implements
            .iter()
            .chain(chain.iter().flat_map(|c| c.implements.iter()))
        {
            if let Some(f) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                ifaces.push(f);
            }
        }
        let mut seen = 0;
        while seen < ifaces.len() {
            let f = ifaces[seen].clone();
            seen += 1;
            for m in &f.methods {
                // Interface methods are implicitly abstract.
                let lname = m.decl.name.to_lowercase();
                if concrete(&lname, chain.len()) {
                    continue;
                }
                let label = format!("{}::{}", f.name, m.decl.name);
                if !missing.contains(&label) {
                    missing.push(label);
                }
            }
            for p2 in &f.implements {
                if let Some(pp) = self.interfaces.get(&p2.to_lowercase()).cloned() {
                    ifaces.push(pp);
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

    /// Resolve `self`/`static`/`parent` members against a declaring
    /// class given only its name + parent name (variance checks run
    /// while the child class is still mid-registration).
    fn ty_scope_resolve(&self, ty: &[String], name: &str, parent: &Option<String>) -> Vec<String> {
        ty.iter()
            .map(|m| match m.to_lowercase().as_str() {
                "self" | "static" => name.to_string(),
                "parent" => parent.clone().unwrap_or_else(|| "\\0parent".to_string()),
                _ => m.clone(),
            })
            .collect()
    }

    /// `iterable` ≡ `Traversable|array` for type-set comparisons.
    fn ty_expand_iterable(ty: &[String]) -> Vec<String> {
        let mut out = Vec::with_capacity(ty.len() + 1);
        for m in ty {
            if m.eq_ignore_ascii_case("iterable") {
                out.push("Traversable".into());
                out.push("array".into());
            } else {
                out.push(m.clone());
            }
        }
        out
    }

    /// Member coverage: `covers(big, small)` — every value matching
    /// `small` also matches `big`. Drives semantic type equality for
    /// prop variance (union_types/variance/valid).
    fn ty_covers(&mut self, big: &str, small: &str) -> bool {
        let bl = big.to_lowercase();
        let sl = small.to_lowercase();
        if bl == sl {
            return true;
        }
        let b_inner = big.trim_start_matches('(').trim_end_matches(')');
        let s_inner = small.trim_start_matches('(').trim_end_matches(')');
        if b_inner.contains('&') {
            // `small ⊆ B1&B2` iff every conjunct of big is covered by
            // some conjunct of small (`B&A` ⊆ `A&B` — commutative).
            let sparts: Vec<&str> = s_inner.split('&').collect();
            return b_inner
                .split('&')
                .all(|b| sparts.iter().any(|p| self.ty_covers(b, p)));
        }
        if s_inner.contains('&') {
            // `A&B` ⊆ anything covering one of its parts.
            return s_inner.split('&').any(|p| self.ty_covers(big, p));
        }
        const SCALARS: &[&str] = &[
            "int", "float", "string", "bool", "array", "callable", "object", "mixed", "void",
            "never", "null", "false", "true", "iterable", "resource", "numeric",
        ];
        match bl.as_str() {
            "mixed" => true,
            "bool" => sl == "false" || sl == "true",
            "float" => sl == "int",
            "object" => !SCALARS.contains(&sl.as_str()),
            "callable" => sl == "closure" || sl == "callable",
            _ => {
                if SCALARS.contains(&sl.as_str()) || SCALARS.contains(&bl.as_str()) {
                    false
                } else {
                    self.is_a_str(&sl, &bl)
                }
            }
        }
    }

    /// `final` props/hooks may not be overridden by a subclass.
    fn check_final_override(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        let mut an = d.parent.clone();
        while let Some(pname) = an {
            let Some(pc) = self.classes.get(&pname.to_lowercase()).cloned() else {
                break;
            };
            for cm in &d.methods {
                let Some(am) = pc
                    .decl
                    .methods
                    .iter()
                    .find(|x| x.decl.name.eq_ignore_ascii_case(&cm.decl.name))
                else {
                    continue;
                };
                if am.is_final && !cm.is_abstract {
                    return Err(PhpError::fatal(
                        format!(
                            "Cannot override final method {}::{}()",
                            pc.decl.name, cm.decl.name
                        ),
                        self.cur_line,
                    ));
                }
            }
            for cc in &d.consts {
                let Some(ac) = pc.decl.consts.iter().find(|x| x.name == cc.name) else {
                    continue;
                };
                if ac.is_final {
                    return Err(PhpError::fatal(
                        format!(
                            "{}::{} cannot override final constant {}::{}",
                            d.name, cc.name, pc.decl.name, cc.name
                        ),
                        self.cur_line,
                    ));
                }
            }
            for cp in &d.props {
                let Some(ap) =
                    pc.decl.props.iter().find(|x| {
                        x.name == cp.name && x.visibility != crate::ast::Visibility::Private
                    })
                else {
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
                // Prop types are invariant but compared SEMANTICALLY:
                // `X|Y` ≡ `X` when Y extends X (dropping a member
                // subsumed by another is identity), and `iterable`
                // expands to `Traversable|array` (union variance
                // valid.phpt). Untyped props stay exact `==`.
                let mut ty_equiv =
                    |ct: &Option<Vec<String>>, at: &Option<Vec<String>>| match (ct, at) {
                        (None, None) => true,
                        (Some(c), Some(a)) => {
                            let ce = self.ty_scope_resolve(c, &d.name, &d.parent);
                            let ae = self.ty_scope_resolve(a, &pc.decl.name, &pc.decl.parent);
                            let ce = Self::ty_expand_iterable(&ce);
                            let ae = Self::ty_expand_iterable(&ae);
                            ce.iter().all(|c| ae.iter().any(|a| self.ty_covers(a, c)))
                                && ae.iter().all(|a| ce.iter().any(|c| self.ty_covers(c, a)))
                        }
                        _ => false,
                    };
                if backed && !ty_equiv(&cp.ty, &ap.ty) {
                    // Child re-types an untyped parent prop → "must be
                    // omitted"; typed-vs-typed mismatch → "must be T".
                    if ap.ty.is_none() && cp.ty.is_some() {
                        return Err(PhpError::fatal(
                            format!(
                                "Type of {}::${} must be omitted to match the parent definition in class {}",
                                d.name, cp.name, pc.decl.name
                            ),
                            self.cur_line,
                        ));
                    }
                    let aty = ap
                        .ty
                        .as_ref()
                        .map(|m| {
                            m.iter()
                                .map(|t| {
                                    if t.contains('&') && m.len() > 1 {
                                        format!("({})", t)
                                    } else {
                                        t.clone()
                                    }
                                })
                                .collect::<Vec<_>>()
                                .join("|")
                        })
                        .unwrap_or_else(|| "mixed".into());
                    return Err(PhpError::fatal(
                        format!(
                            "Type of {}::${} must be {} (as in class {})",
                            d.name, cp.name, aty, pc.decl.name
                        ),
                        self.cur_line,
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

    /// First type member naming a class that can't be resolved even
    /// after an autoload attempt — builtins, `self`/`parent`/`static`,
    /// registered classes/interfaces, and names on the linking stack
    /// all count as resolvable (variance/mixed_return_type: members
    /// covered without resolution never reach here).
    fn first_unres(&mut self, tys: &[String]) -> Option<String> {
        const BUILTIN_T: &[&str] = &[
            "int", "float", "string", "bool", "array", "object", "callable", "iterable", "mixed",
            "void", "never", "false", "true", "null", "resource", "self", "parent", "static",
        ];
        tys.iter()
            .flat_map(|m| m.split('&').map(str::to_string).collect::<Vec<_>>())
            .find(|m| {
                let ml = m.trim_start_matches('\\').to_lowercase();
                if BUILTIN_T.contains(&ml.as_str())
                    || self.classes.contains_key(&ml)
                    || self.interfaces.contains_key(&ml)
                    || self
                        .declaring
                        .iter()
                        .any(|d| d.name.eq_ignore_ascii_case(&ml))
                    || self
                        .linking
                        .iter()
                        .any(|c| c.name.eq_ignore_ascii_case(&ml))
                {
                    return false;
                }
                self.note_variance_obligation();
                if let Err(e) = self.run_autoload(m.trim_start_matches('\\')) {
                    self.sig_fatal.get_or_insert(e);
                    self.pending_exception = None;
                }
                !self.classes.contains_key(&ml) && !self.interfaces.contains_key(&ml)
            })
            .map(|m| m.trim_start_matches('\\').to_string())
    }

    /// `sup` is a supertype of `sub` when every `sub` member is admitted
    /// by some `sup` member — equal names, `mixed`, or a class/interface
    /// the member is-a (set_value_parameter_type_variance_006).
    fn ty_sup(&mut self, sup: &[String], sub: &[String]) -> bool {
        sub.iter()
            .all(|t| sup.iter().any(|s| self.ty_member_is_a(t, s)))
    }

    /// Whether a single type conjunct resolves to a registered (or
    /// mid-linking / autoloadable) class-like name or builtin. Used
    /// to gate `&`-member coverage of `object`/`iterable`/`callable`.
    fn ty_conj_resolvable(&mut self, c: &str) -> bool {
        let cl = c.to_lowercase();
        const BUILTIN: &[&str] = &[
            "int",
            "float",
            "string",
            "bool",
            "array",
            "object",
            "callable",
            "iterable",
            "mixed",
            "void",
            "never",
            "false",
            "true",
            "null",
            "numeric",
            "resource",
            "self",
            "static",
            "parent",
            "closure",
            "traversable",
            "iterator",
            "generator",
        ];
        if BUILTIN.contains(&cl.as_str()) {
            return true;
        }
        if self.classes.contains_key(&cl)
            || self.interfaces.contains_key(&cl)
            || self.traits.contains_key(&cl)
            || self
                .declaring
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(&cl))
            || self.linking.iter().any(|d| d.name.to_lowercase() == cl)
        {
            return true;
        }
        // Autoload errors must not surface here — resolvability is a
        // yes/no probe (invalid4 "could not check" is raised by the
        // caller). Preserve any pre-existing pending exception; a fatal
        // still propagates through sig_fatal.
        self.note_variance_obligation();
        let prior = self.pending_exception.take();
        if let Err(e) = self.run_autoload(c) {
            self.sig_fatal.get_or_insert(e);
        }
        self.pending_exception = prior;
        self.classes.contains_key(&cl)
            || self.interfaces.contains_key(&cl)
            || self.traits.contains_key(&cl)
    }

    /// Type-member acceptance: `t` is admitted by `s` when they match by
    /// name, `s` is `mixed`, or `t`'s class/interface ancestry includes
    /// `s` (interfaces live in `self.interfaces`, not `self.classes`).
    fn ty_member_is_a(&mut self, t: &str, s: &str) -> bool {
        self.ty_member_is_a_impl(t, s, false)
    }

    /// Strict membership for ref-bind merges — `int ⊄ float` (scalar
    /// coercions don't apply to declared-type sets, prop_ref_assign).
    fn ty_member_is_a_strict(&mut self, t: &str, s: &str) -> bool {
        self.ty_member_is_a_impl(t, s, true)
    }

    fn ty_member_is_a_impl(&mut self, t: &str, s: &str, strict: bool) -> bool {
        let tl0 = t.to_lowercase();
        let sl0 = s.to_lowercase();
        // `never` is the bottom type (subtype of everything). `void`
        // and `never` match only themselves — `void` is NOT a subtype
        // of `mixed` (mixed_return_inheritance_error1), and nothing
        // but `never` is a subtype of `never`.
        if tl0 == "never" {
            return true;
        }
        if tl0 == "void" || sl0 == "void" || sl0 == "never" {
            return tl0 == sl0;
        }
        if s.eq_ignore_ascii_case(t) || s.eq_ignore_ascii_case("mixed") {
            return true;
        }
        if s.contains('&') {
            // `t ⊆ S1&S2&…` iff every conjunct of s is covered by some
            // conjunct of t (`A&B&C` is a subtype of `A&B`).
            let tparts: Vec<&str> = t.split('&').collect();
            return s.split('&').all(|sc| {
                tparts
                    .iter()
                    .any(|tc| self.ty_member_is_a_impl(tc, sc, strict))
            });
        }
        if t.contains('&') {
            // `C1&C2 ⊆ s` when some conjunct already is-a `s` — but a
            // conjunct covering `object`/`iterable`/`callable` must be
            // a resolvable class-like name; an unloadable conjunct
            // can't prove the member is object-like (invalid4).
            return t.split('&').any(|sm| {
                let atomish =
                    ["object", "iterable", "callable"].contains(&s.to_lowercase().as_str());
                if atomish && !self.ty_conj_resolvable(sm) {
                    return false;
                }
                self.ty_member_is_a_impl(sm, s, strict)
            });
        }
        if t.eq_ignore_ascii_case("null") || t.eq_ignore_ascii_case("mixed") {
            return false;
        }
        let tl = t.to_lowercase();
        let sl = s.to_lowercase();
        if sl == "iterable"
            && (["array", "traversable", "iterator", "generator"].contains(&tl.as_str())
                || self.is_a_str(&tl, "traversable"))
        {
            return true;
        }
        if sl == "callable" && tl == "closure" {
            return true;
        }
        if sl == "object" && tl == "closure" {
            return true;
        }
        if !strict && sl == "float" && tl == "int" {
            return true;
        }
        if sl == "object" {
            const SCALARS: &[&str] = &[
                "int", "float", "string", "bool", "array", "callable", "iterable", "void", "never",
                "null", "false", "true", "resource", "numeric",
            ];
            if !SCALARS.contains(&tl.as_str()) {
                // A non-scalar member covers `object` only when it
                // actually resolves to a class-like — Zend autoloads
                // it to verify (enum_forward_compat).
                return self.ty_conj_resolvable(t);
            }
        }
        if sl == "bool" && (tl == "true" || tl == "false") {
            return true;
        }
        // A class declaring __toString implicitly implements
        // Stringable for variance (variance/stringable).
        if sl == "stringable"
            && (self
                .lookup_class(t)
                .map(|c| self.find_method_in(&c, "__tostring").is_some())
                .unwrap_or(false)
                || self.linking.iter().any(|c| {
                    c.name.eq_ignore_ascii_case(t)
                        && c.methods
                            .iter()
                            .any(|mm| mm.decl.name.eq_ignore_ascii_case("__tostring"))
                }))
        {
            return true;
        }
        if let Some(iface) = self.interfaces.get(&tl).cloned() {
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

    /// Typed class constants (PHP 8.3): forbidden members, the
    /// declared-value check (strict — no coercion), and the
    /// inheritance variance rule (child ⊆ parent when the parent side
    /// declares a type; private consts exempt).
    fn check_const_types(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        for cd in &d.consts {
            let Some(ty) = &cd.ty else { continue };
            for m in ty {
                let l = m.to_lowercase();
                if ["callable", "void", "never"].contains(&l.as_str()) {
                    return Err(PhpError::fatal(
                        format!(
                            "Class constant {}::{} cannot have type {}",
                            d.name, cd.name, m
                        ),
                        self.cur_line,
                    ));
                }
            }
            // Compile-time values: fatal now. Runtime values (define'd
            // consts, `new`) defer to the access-time TypeError below.
            if is_compile_const(&cd.value) {
                if let Ok(v) = self.eval_decl_const(&cd.value, &d.file) {
                    if !self.const_ty_accepts(ty, &v, &d.name) {
                        let tn = self.zval_type_name(&v);
                        return Err(PhpError::fatal(
                            format!(
                                "Cannot use {} as value for class constant {}::{} of type {}",
                                tn,
                                d.name,
                                cd.name,
                                ty_disp(ty)
                            ),
                            self.cur_line,
                        ));
                    }
                }
            }
        }
        for pd in &d.props {
            if let Some(ty) = &pd.ty {
                for m in ty {
                    let l = m.to_lowercase();
                    if ["callable", "void", "never"].contains(&l.as_str()) {
                        return Err(PhpError::fatal(
                            format!(
                                "Property {}::${} cannot have type {}",
                                d.name,
                                pd.name,
                                ty_disp(ty)
                            ),
                            pd.line,
                        ));
                    }
                }
                let ctx = Some((d.name.as_str(), d.parent.clone()));
                self.check_ty_redundant(ty, &ctx)?;
                if let Some(def) = &pd.default {
                    if is_compile_const(def) {
                        if let Ok(v) = self.eval_decl_const(def, &d.file) {
                            // `= null` on a non-nullable prop needs `?T`
                            // (typed_properties_015).
                            if matches!(v, Value::Null)
                                && !ty.iter().any(|m| {
                                    m.eq_ignore_ascii_case("null")
                                        || m.eq_ignore_ascii_case("mixed")
                                })
                            {
                                // Implicit nullable is only hinted for
                                // single non-`&` types — unions and
                                // intersections report the plain
                                // "Cannot use null" (bug81268).
                                if ty.len() > 1 || ty.iter().any(|m| m.contains('&')) {
                                    return Err(PhpError::fatal(
                                        format!(
                                            "Cannot use null as default value for property {}::${} of type {}",
                                            d.name,
                                            pd.name,
                                            ty_disp(ty)
                                        ),
                                        pd.line,
                                    ));
                                }
                                let hint = if ty.len() == 1 {
                                    format!("?{}", ty_disp(ty))
                                } else {
                                    format!("{}|null", ty_disp(ty))
                                };
                                return Err(PhpError::fatal(
                                    format!(
                                        "Default value for property of type {} may not be null. Use the nullable type {} to allow null default value",
                                        ty_disp(ty),
                                        hint
                                    ),
                                    pd.line,
                                ));
                            }
                            // int→float is the only allowed widening.
                            if !(self.const_ty_accepts(ty, &v, &d.name)
                                || (matches!(v, Value::Int(_))
                                    && ty.iter().any(|m| m.eq_ignore_ascii_case("float"))))
                            {
                                return Err(PhpError::fatal(
                                    format!(
                                        "Cannot use {} as default value for property {}::${} of type {}",
                                        self.zval_type_name(&v),
                                        d.name,
                                        pd.name,
                                        ty_disp(ty)
                                    ),
                                    pd.line,
                                ));
                            }
                        }
                    }
                }
            }
        }
        // Inheritance variance — parent classes and implemented
        // interfaces' same-name consts constrain this class's.
        let mut supers: Vec<(String, crate::ast::ConstDecl)> = Vec::new();
        let mut pn = d.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()).cloned() else {
                break;
            };
            for cd in &pc.decl.consts {
                if cd.visibility != crate::ast::Visibility::Private {
                    supers.push((pc.decl.name.clone(), cd.clone()));
                }
            }
            pn = pc.decl.parent.clone();
        }
        for i in &d.implements {
            if let Some(id) = self.interfaces.get(&i.to_lowercase()).cloned() {
                for cd in &id.consts {
                    supers.push((id.name.clone(), cd.clone()));
                }
            }
        }
        for cd in &d.consts {
            if cd.visibility == crate::ast::Visibility::Private {
                continue;
            }
            for (sn, scd) in &supers {
                if scd.name != cd.name {
                    continue;
                }
                let Some(pty) = &scd.ty else { continue };
                let ok = match &cd.ty {
                    Some(cty) => self.ty_sup(pty, cty),
                    None => false,
                };
                if !ok {
                    return Err(PhpError::fatal(
                        format!(
                            "Type of {}::{} must be compatible with {}::{} of type {}",
                            d.name,
                            cd.name,
                            sn,
                            cd.name,
                            ty_disp(pty)
                        ),
                        self.cur_line,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Strict const-value acceptance: any union member (intersections
    /// are flattened to union members by take_type — DNF types in const
    /// positions behave the same for the tests at hand). `self`/`static`
    ////`parent` resolve against the declaring class.
    fn const_ty_accepts(&mut self, ty: &[String], v: &Value, dname: &str) -> bool {
        ty.iter().any(|m| {
            if m.contains('&') {
                return m
                    .split('&')
                    .all(|sm| self.const_ty_accepts(&[sm.to_string()], v, dname));
            }
            let l = m.to_lowercase();
            match l.as_str() {
                "null" => matches!(v, Value::Null),
                "bool" | "true" | "false" => {
                    matches!(v, Value::Bool(b) if l == "bool" || (*b) == (l == "true"))
                }
                "int" => matches!(v, Value::Int(_)),
                "float" => matches!(v, Value::Float(_) | Value::Int(_)),
                "string" => matches!(v, Value::Str(_)),
                "array" => matches!(v, Value::Array(_)),
                "object" => matches!(v, Value::Object(_)),
                "iterable" => {
                    matches!(v, Value::Array(_))
                        || matches!(v, Value::Object(o) if self.obj_is_a(o, "Traversable"))
                }
                "mixed" => true,
                "self" | "static" => match v {
                    Value::Object(o) => self.obj_is_a(o, dname),
                    _ => false,
                },
                "parent" => match v {
                    Value::Object(o) => {
                        let p = self
                            .classes
                            .get(&dname.to_lowercase())
                            .and_then(|c| c.decl.parent.clone());
                        match p {
                            Some(pn) => self.obj_is_a(o, &pn),
                            None => false,
                        }
                    }
                    _ => false,
                },
                _ => match v {
                    Value::Object(o) => self.obj_is_a(o, m),
                    Value::Callable(_) => m.eq_ignore_ascii_case("closure"),
                    _ => false,
                },
            }
        })
    }

    /// Zend link semantics: at a class's first use, every const
    /// initializer is evaluated — eval errors (undefined constant)
    /// propagate as Errors, then typed checks raise TypeErrors.
    fn link_const_inits(&mut self, cls: &Rc<PhpClass>) -> Result<(), PhpError> {
        let lname = cls.decl.name.to_lowercase();
        if !self.consts_linked.insert(lname) {
            return Ok(());
        }
        let mut chain = vec![cls.clone()];
        let mut pn = cls.decl.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()).cloned() else {
                break;
            };
            if !self.consts_linked.insert(pc.decl.name.to_lowercase()) {
                break;
            }
            pn = pc.decl.parent.clone();
            chain.push(pc);
        }
        for c in chain {
            for cd in &c.decl.consts {
                if cd.enum_case {
                    continue;
                }
                let file = c.decl.file.clone();
                self.class_const_ctx += 1;
                let r = self.eval_decl_const(&cd.value, &file);
                self.class_const_ctx -= 1;
                let v = r?;
                self.const_apply_ty(cd, &c.decl.name, v)?;
            }
        }
        Ok(())
    }

    /// Unit/backed enum case singleton: `E::Foo` is an object of class
    /// E with `name` (+ `value` for backed enums) props; one instance
    /// per case so `===` holds.
    fn enum_case_value(
        &mut self,
        cls: &str,
        case: &str,
        cd: &crate::ast::ConstDecl,
    ) -> Result<Value, PhpError> {
        let key = format!("{}\0{}", cls.to_lowercase(), case);
        if let Some(v) = self.enum_cases.get(&key) {
            return Ok(v.clone());
        }
        let mut props = std::collections::HashMap::new();
        props.insert("name".to_string(), Value::str(case));
        let is_unit = matches!(cd.value, Expr::Null);
        if !is_unit {
            let file = self
                .classes
                .get(&cls.to_lowercase())
                .map(|c| c.decl.file.clone())
                .unwrap_or_default();
            let v = self.eval_decl_const(&cd.value, &file)?;
            props.insert("value".to_string(), v);
        }
        let o = self.instantiate(cls, &[])?;
        if let Value::Object(h) = &o {
            for (k, v) in props {
                h.borrow_mut().props.insert(k, cell(v));
            }
        }
        self.enum_cases.insert(key, o.clone());
        Ok(o)
    }

    /// Access-time typed-const enforcement: int→float widening, else
    /// a catchable TypeError ("Cannot assign ...").
    fn const_apply_ty(
        &mut self,
        cd: &crate::ast::ConstDecl,
        owner: &str,
        v: Value,
    ) -> Result<Value, PhpError> {
        let Some(ty) = &cd.ty else { return Ok(v) };
        let v = if ty.iter().any(|m| m.eq_ignore_ascii_case("float")) {
            match v {
                Value::Int(i) => Value::Float(i as f64),
                _ => v,
            }
        } else {
            v
        };
        if !self.const_ty_accepts(ty, &v, owner) {
            return self.fail(PhpError::uncaught(
                "TypeError",
                format!(
                    "Cannot assign {} to class constant {}::{} of type {}",
                    self.zval_type_name(&v),
                    owner,
                    cd.name,
                    ty_disp(ty)
                ),
                0,
            ));
        }
        Ok(v)
    }

    /// Every visible override must be signature-compatible with the
    /// nearest ancestor method of the same name — not just abstracts;
    /// this also covers trait-merged methods vs concrete ancestors
    /// (bug81192). `__construct` is exempt from LSP rules in PHP.
    fn check_override_sigs(&mut self, d: &ClassDecl) -> Result<(), PhpError> {
        let mut chain: Vec<(String, Rc<ClassDecl>)> = Vec::new();
        let mut pn = d.parent.clone();
        while let Some(p) = pn {
            let Some(pc) = self.classes.get(&p.to_lowercase()) else {
                break;
            };
            chain.push((pc.decl.name.clone(), pc.decl.clone()));
            pn = pc.decl.parent.clone();
        }
        if chain.is_empty() {
            return Ok(());
        }
        let rank = |v: &crate::ast::Visibility| match v {
            crate::ast::Visibility::Public => 2,
            crate::ast::Visibility::Protected => 1,
            crate::ast::Visibility::Private => 0,
        };
        for m in &d.methods {
            let lname = m.decl.name.to_lowercase();
            let Some((aname, am)) = chain.iter().find_map(|(pn, pc)| {
                pc.methods
                    .iter()
                    .find(|x| x.decl.name.to_lowercase() == lname)
                    .filter(|x| !matches!(x.visibility, crate::ast::Visibility::Private))
                    .map(|x| (pn.clone(), x.clone()))
            }) else {
                continue;
            };
            // An abstract declaration's contract propagates through
            // intermediate concrete impls — the fatal cites the
            // abstract declarer, not the nearest impl (bug61970_2).
            let (aname, am) = chain
                .iter()
                .find_map(|(pn, pc)| {
                    pc.methods
                        .iter()
                        .find(|x| {
                            x.decl.name.to_lowercase() == lname
                                && x.is_abstract
                                && !matches!(x.visibility, crate::ast::Visibility::Private)
                        })
                        .map(|x| (pn.clone(), x.clone()))
                })
                .unwrap_or((aname, am));
            // `__construct` and private impls escape LSP visibility —
            // except when an ancestor declares the contract abstractly,
            // which the impl must satisfy (bug61970, magic_methods_008).
            if (m.decl.name.eq_ignore_ascii_case("__construct")
                || matches!(m.visibility, crate::ast::Visibility::Private))
                && !am.is_abstract
            {
                continue;
            }
            if rank(&m.visibility) < rank(&am.visibility) {
                let want = match am.visibility {
                    crate::ast::Visibility::Public => {
                        format!("public (as in class {})", aname)
                    }
                    crate::ast::Visibility::Protected => {
                        format!("protected (as in class {}) or weaker", aname)
                    }
                    crate::ast::Visibility::Private => unreachable!(),
                };
                return Err(PhpError::fatal(
                    format!(
                        "Access level to {}::{}() must be {}",
                        d.name, m.decl.name, want
                    ),
                    m.decl.line,
                ));
            }
            if let Some(e) = self.trait_sig_error(m, &am, &d.name, &aname, false) {
                // An internal method's tentative return type warns
                // instead of erroring unless the override carries
                // #[ReturnTypeWillChange] (internal_parent/*).
                if !e.message.starts_with("Could not check")
                    && self
                        .tentative
                        .contains(&(aname.to_lowercase(), lname.clone()))
                    && !m.decl.attrs.iter().any(|a| {
                        a.name
                            .rsplit('\\')
                            .next()
                            .unwrap_or(&a.name)
                            .eq_ignore_ascii_case("ReturnTypeWillChange")
                    })
                {
                    self.deprecated(&format!(
                        "Return type of {}::{}{} should either be compatible with {}::{}{}, or the #[\\ReturnTypeWillChange] attribute should be used to temporarily suppress the notice",
                        d.name,
                        m.decl.name,
                        Self::sig_str_full(&m.decl, &d.name),
                        aname,
                        am.decl.name,
                        Self::sig_str_full(&am.decl, &d.name)
                    ))?;
                    continue;
                }
                return Err(e);
            }
        }
        Ok(())
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
            "static" => {
                if self.in_const_expr > 0 {
                    if let Some(c) = &self.const_self {
                        return c.name().to_string();
                    }
                }
                self.stack
                    .last()
                    .and_then(|f| {
                        f.called_class
                            .as_ref()
                            .map(|c| c.name().to_string())
                            .or_else(|| f.scope_class.as_ref().map(|c| c.name().to_string()))
                    })
                    .unwrap_or_else(|| lname.to_string())
            }
            "self" => {
                if self.in_const_expr > 0 {
                    if let Some(c) = &self.const_self {
                        return c.name().to_string();
                    }
                }
                self.stack
                    .last()
                    .and_then(|f| f.scope_class.as_ref().map(|c| c.name().to_string()))
                    .unwrap_or_else(|| lname.to_string())
            }
            "parent" => {
                if self.in_const_expr > 0 {
                    if let Some(cself) = &self.const_self {
                        return cself
                            .decl
                            .parent
                            .clone()
                            .unwrap_or_else(|| lname.to_string());
                    }
                }
                self.stack
                    .last()
                    .and_then(|f| f.scope_class.clone())
                    .and_then(|c| c.decl.parent.clone())
                    .unwrap_or_else(|| lname.to_string())
            }
            _ => lname.to_string(),
        }
    }

    /// name → registered class name; a miss runs the autoloaders
    /// once (FCC string args, class_exists).
    fn resolve_class(&mut self, name: &str) -> Option<String> {
        let n = name.trim_start_matches('\\');
        if !self.classes.contains_key(&n.to_lowercase())
            && !self.interfaces.contains_key(&n.to_lowercase())
        {
            // Option-typed: a throwing autoloader can't surface here —
            // PHP propagates it, but callers of resolve_class (e.g.
            // class_exists) mostly can't throw either; keep the swallow.
            let _ = self.run_autoload(n);
        }
        if self.classes.contains_key(&n.to_lowercase())
            || self.interfaces.contains_key(&n.to_lowercase())
        {
            Some(n.to_string())
        } else {
            None
        }
    }

    /// Allocate a PHP object/closure handle id: reuse the lowest dead
    /// slot, like Zend's object store recycling freed handles (closures
    /// share the store — `object(Closure)#N` interleaves with objects).
    fn next_obj_id(&mut self, rc: &Rc<RefCell<PhpObject>>) -> u64 {
        let w = ObjHandle::Obj(Rc::downgrade(rc));
        self.push_handle(w)
    }

    fn next_callable_id(&mut self, c: &Rc<PhpCallable>) -> u64 {
        self.push_handle(ObjHandle::Callable(Rc::downgrade(c)))
    }

    fn push_handle(&mut self, w: ObjHandle) -> u64 {
        // Zend reuses the most recently freed handle first (its free
        // list is a LIFO stack), so scan dead slots back-to-front
        // (namespace_004: call2's $c reuses call1's $d handle, then
        // $d reuses call1's $c — not the other way around).
        let n = self.obj_handles.len();
        for i in (0..n).rev() {
            if !self.obj_handles[i].alive() {
                self.obj_handles[i] = w;
                return (i + 1) as u64;
            }
        }
        self.obj_handles.push(w);
        self.obj_handles.len() as u64
    }

    /// Wrap a PhpCallable assigning its object-store id.
    fn new_callable(&mut self, inner: PhpCallable) -> Rc<PhpCallable> {
        let c = Rc::new(inner);
        let id = self.next_callable_id(&c);
        c.id.set(id);
        c
    }

    /// Wrap a PhpObject in Rc and assign its handle id.
    pub fn alloc_obj(&mut self, o: PhpObject) -> Rc<RefCell<PhpObject>> {
        let rc = Rc::new(RefCell::new(o));
        let id = self.next_obj_id(&rc);
        rc.borrow_mut().id = id;
        rc
    }

    /// `new X(args)` — instantiate + call __construct.
    fn new_instance(&mut self, name: &str, args: CallArgs) -> Result<Value, PhpError> {
        let lname = name.to_lowercase();
        if !self.classes.contains_key(&lname) {
            self.run_autoload(name.trim_start_matches('\\'))?;
        }
        let cls = match self.classes.get(&lname) {
            Some(c) => c.clone(),
            None => {
                let t = name.trim_start_matches('\\');
                if self.traits.contains_key(&t.to_lowercase()) {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot instantiate trait {}", t),
                        0,
                    ));
                }
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Class \"{}\" not found", name),
                    0,
                ));
            }
        };
        if cls.name().eq_ignore_ascii_case("closure") {
            return self.fail(PhpError::uncaught(
                "Error",
                "Instantiation of class Closure is not allowed",
                0,
            ));
        }
        if cls.decl.kind == ClassKind::Trait {
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Cannot instantiate trait {}", cls.name()),
                0,
            ));
        }
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
        self.link_const_inits(&cls)?;
        let has_ctor = self.find_method_in(&cls, "__construct").is_some();
        if !has_ctor {
            if let Some((n, ..)) = args.named.first() {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Unknown named parameter ${}", n),
                    0,
                ));
            }
        }
        let obj = self.instantiate(&lname, &[])?;
        // __construct (native for builtins via method_invoke's
        // interception); the ctor may be inherited (property_hooks/foreach).
        if has_ctor {
            if let Value::Object(o) = &obj {
                if let Err(e) = self.method_invoke_vis(o.clone(), "__construct", args) {
                    // A ctor that throws leaves a half-built object;
                    // zend never runs its __destruct (bug29368_1/_3).
                    self.mark_destructed(o);
                    return Err(e);
                }
            }
        }
        if let Value::Object(o) = &obj {
            self.expr_temps.push(o.clone());
        }
        Ok(obj)
    }

    /// Build the object shell: init props along the whole parent chain.
    pub fn instantiate(&mut self, lname: &str, _args: &[Value]) -> Result<Value, PhpError> {
        let cls = self.classes.get(&lname.to_lowercase()).cloned();
        let cls = match cls {
            Some(c) => c,
            None => {
                return Ok(Value::Object(Rc::new(RefCell::new(PhpObject {
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
                            adaptations: vec![],
                            methods: vec![],
                            props: vec![],
                            consts: vec![],
                            file: String::new(),
                        }),
                        statics: RefCell::new(HashMap::new()),
                        statics_init: RefCell::new(true),
                    }),
                    props: HashMap::new(),
                    prop_order: vec![],
                    id: 0,
                    internal: None,
                    unset_props: std::collections::HashSet::new(),
                }))))
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
                let mut default = match &p.default {
                    Some(d) => {
                        let old = self.const_self.replace(c.clone());
                        self.class_const_ctx += 1;
                        let r = self.eval_decl_const(d, &c.decl.file);
                        self.class_const_ctx -= 1;
                        self.const_self = old;
                        r?
                    }
                    None => Value::Null,
                };
                // Runtime defaults (define()'d consts etc.) go through
                // the same write check as assignments — strict files
                // TypeError here (typed_properties_058).
                if p.ty.is_some() {
                    default = self.prop_typed_write_check(p, c, default)?;
                }
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
                file: self.diag_file(),
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
        Ok(Value::Object(self.alloc_obj(PhpObject {
            class: cls,
            props,
            prop_order,
            id: 0,
            internal,
            unset_props: std::collections::HashSet::new(),
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
        let s = match n {
            PropName::Name(s) => return Ok(s.clone()),
            PropName::Var(v) => {
                let val = self.var_get(v)?;
                self.conv_str(&val)?
            }
            PropName::Expr(e) => {
                let v = self.eval(e)?;
                self.conv_str(&v)?
            }
        };
        // Property names keep NUL bytes — the private-name-mangle
        // check fires downstream (bug52484). METHOD names truncate
        // at their call sites (bug46238).
        Ok(s)
    }

    /// Zend method names are C strings — a NUL byte truncates the
    /// name (`"\0"` invokes `""`; bug46238).
    fn nul_trunc(s: &str) -> String {
        s.split('\0').next().unwrap_or_default().to_string()
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
                    Some(d) => {
                        let old = self.const_self.replace(c.clone());
                        self.class_const_ctx += 1;
                        let v = self.eval_const(d).unwrap_or(Value::Null);
                        self.class_const_ctx -= 1;
                        self.const_self = old;
                        v
                    }
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
    /// Declared *static* prop lookup across the class chain
    /// (`Foo::$i = v` write checks).
    fn find_static_prop_decl(
        &self,
        cls: &Rc<PhpClass>,
        pn: &str,
    ) -> Option<(PropDecl, Rc<PhpClass>)> {
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            let priv_ok = std::rc::Rc::ptr_eq(&c, cls);
            if let Some(pd) = c.decl.props.iter().find(|p| {
                p.name == pn
                    && p.is_static
                    && (priv_ok || p.visibility != crate::ast::Visibility::Private)
            }) {
                return Some((pd.clone(), c.clone()));
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        None
    }

    /// Instance-prop decl lookup on a class (Reflection, no object) —
    /// any visibility, walking the parent chain.
    fn find_prop_decl(&self, cls: &Rc<PhpClass>, pn: &str) -> Option<(PropDecl, Rc<PhpClass>)> {
        let mut cur = Some(cls.clone());
        while let Some(c) = cur {
            if let Some(pd) = c.decl.props.iter().find(|p| p.name == pn && !p.is_static) {
                return Some((pd.clone(), c.clone()));
            }
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        None
    }

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
            ret: None,
            name: format!("${}::{}", pname, kind),
            params,
            body: hook.body.clone().unwrap_or_default(),
            attrs: vec![],
            by_ref: hook.by_ref,
            line: self.cur_line,
            end_line: self.cur_line,
            file: self.cur_file.clone(),
            ns: String::new(),
            decl_in: None,
        });
        let owner = decl_owner(dcls, pname);
        let args = arg.into_iter().collect::<Vec<Cell>>();
        self.pending_decl_class = Some(dcls.clone());
        self.pending_hook_prop = Some((o.borrow().id, pname.to_string(), hook.is_get, owner));
        let called = o.borrow().class.clone();
        self.pending_called_class = Some(called);
        let r = self.invoke_fn(
            &decl,
            CallArgs::positional(args),
            Some(o.clone()),
            Some(dcls.clone()),
        );
        self.pending_decl_class = None;
        self.pending_called_class = None;
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
        match v {
            Value::Float(f) if f.fract() != 0.0 => {
                let _ = self.emit_diag(
                    "Deprecated",
                    8192,
                    &format!(
                        "Implicit conversion from float {} to int loses precision",
                        format_float_repr(*f)
                    ),
                );
            }
            // Float-strings name the value `float-string "1.5"`
            // (scalar_return_basic_64bit).
            Value::Str(b) => {
                if let Numeric::Float(f) = numeric(b) {
                    if f.fract() != 0.0 {
                        let _ = self.emit_diag(
                            "Deprecated",
                            8192,
                            &format!(
                                "Implicit conversion from float-string \"{}\" to int loses precision",
                                String::from_utf8_lossy(b)
                            ),
                        );
                    }
                }
            }
            _ => {}
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
        // self/parent/static in a prop type resolve against the
        // DECLARING class (typed_properties_043); keep the literal
        // members for error display.
        let resolved = self.resolved_ty(tys, dcls.as_ref());
        // Object with __toString coerces into a `string` prop weakly
        // (typed_properties_051).
        if let Value::Object(o) = &v {
            if tys.iter().any(|t| t.eq_ignore_ascii_case("string")) {
                let tcls = o.borrow().class.clone();
                let tostr = self.find_method_in(&tcls, "__tostring").is_some();
                if tostr {
                    let sv =
                        self.method_invoke(o.clone(), "__toString", CallArgs::positional(vec![]))?;
                    return Ok(sv);
                }
            }
        }
        let tys = &resolved;
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

    /// Resolve `self`/`parent`/`static` members of a declared type to
    /// concrete class names for the declaring class.
    fn resolved_ty(&self, tys: &[String], dcls: &PhpClass) -> Vec<String> {
        tys.iter()
            .map(|m| {
                let l = m.to_lowercase();
                match l.as_str() {
                    "self" | "static" => dcls.name().to_string(),
                    "parent" => dcls
                        .decl
                        .parent
                        .clone()
                        .unwrap_or_else(|| "\\0parent".to_string()),
                    _ => m.clone(),
                }
            })
            .collect()
    }

    /// Intersect two single type members for `=&` slot-compat: same
    /// name → itself; class types narrow by hierarchy; `object` accepts
    /// any class; `iterable` accepts array/Traversable. Disjoint atoms
    /// (`int` ∩ `float`) return None (typed_properties_076).
    fn ty_member_intersect(&mut self, a: &str, b: &str) -> Option<String> {
        let al = a.to_lowercase();
        let bl = b.to_lowercase();
        if al == bl {
            return Some(a.to_string());
        }
        if al == "mixed" {
            return Some(b.to_string());
        }
        if bl == "mixed" {
            return Some(a.to_string());
        }
        if al.contains('&') || bl.contains('&') {
            // `(X&Y) ∩ (X&Z)` = `X&Y&Z` — conjunct sets merge
            // (typed_reference). A scalar builtin can't coexist with
            // class conjuncts; `object`/`mixed` absorb; two unrelated
            // concrete classes can't both hold.
            let scalarish = |c: &str| {
                matches!(
                    c.to_lowercase().as_str(),
                    "int"
                        | "float"
                        | "string"
                        | "bool"
                        | "array"
                        | "null"
                        | "false"
                        | "true"
                        | "void"
                        | "never"
                        | "resource"
                        | "numeric"
                )
            };
            let mut conj: Vec<String> = Vec::new();
            for c in a.split('&').chain(b.split('&')) {
                let cl = c.to_lowercase();
                if cl == "object" || cl == "mixed" {
                    continue;
                }
                if conj.iter().any(|x| x.eq_ignore_ascii_case(c)) {
                    continue;
                }
                conj.push(c.to_string());
            }
            if conj.is_empty() {
                return Some("object".to_string());
            }
            if conj.iter().any(|c| scalarish(c)) && conj.iter().any(|c| !scalarish(c)) {
                return None;
            }
            for i in 0..conj.len() {
                for j in (i + 1)..conj.len() {
                    let (x, y) = (conj[i].to_lowercase(), conj[j].to_lowercase());
                    if self.interfaces.contains_key(&x) || self.interfaces.contains_key(&y) {
                        continue;
                    }
                    if self.classes.contains_key(&x)
                        && self.classes.contains_key(&y)
                        && !self.ty_member_is_a(&x, &y)
                        && !self.ty_member_is_a(&y, &x)
                    {
                        return None;
                    }
                }
            }
            return Some(conj.join("&"));
        }
        const ATOMS: &[&str] = &[
            "int", "float", "string", "bool", "array", "null", "false", "true", "void", "never",
            "resource", "callable", "iterable", "object",
        ];
        let a_atom = ATOMS.contains(&al.as_str());
        let b_atom = ATOMS.contains(&bl.as_str());
        if !a_atom && !b_atom {
            // Both class-like: compatible when one is a subtype of the
            // other — the narrower type wins (A&B refs, 076).
            return if self.ty_member_is_a(&al, &bl) {
                Some(a.to_string())
            } else if self.ty_member_is_a(&bl, &al) {
                Some(b.to_string())
            } else {
                None
            };
        }
        if a_atom && b_atom {
            return match (al.as_str(), bl.as_str()) {
                ("iterable", "array") | ("array", "iterable") => Some("array".to_string()),
                ("iterable", "object") | ("object", "iterable") => Some("Traversable".to_string()),
                _ => None,
            };
        }
        // Exactly one side is a class-like name.
        let (cls, atom) = if a_atom { (b, a) } else { (a, b) };
        let cl = cls.to_lowercase();
        match atom.to_lowercase().as_str() {
            "object" => Some(cls.to_string()),
            "iterable" => {
                if self.ty_member_is_a(&cl, "Traversable") || self.ty_member_is_a(&cl, "iterable") {
                    Some(cls.to_string())
                } else {
                    None
                }
            }
            "callable" => {
                if self.ty_member_is_a(&cl, "Closure") {
                    Some(cls.to_string())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Non-empty member-wise type intersection of two declared types —
    /// returns the intersected member list, or None when disjoint
    /// (typed_properties_068/076 `=&` conflict check).
    /// Ref-bind merge: the memberwise intersection of the two type
    /// sets (`int|string ∩ float|string = string`).
    fn ty_bind_merge(&mut self, a: &[String], b: &[String]) -> Option<Vec<String>> {
        let mut inter: Vec<String> = Vec::new();
        for am in a {
            for bm in b {
                if let Some(m) = self.ty_member_intersect(am, bm) {
                    if !inter.iter().any(|x| x.eq_ignore_ascii_case(&m)) {
                        inter.push(m);
                    }
                }
            }
        }
        if inter.is_empty() {
            None
        } else {
            Some(inter)
        }
    }

    /// Binding `src` into a typed prop (`$p =& $src`): when src is
    /// another typed prop's slot, the two declared types must
    /// intersect ("Reference ... not compatible", 068/076); the
    /// current value is weak-checked like a normal write ("Cannot
    /// assign X to property"). Returns the merged member list.
    fn bind_typed_check(
        &mut self,
        pd: &PropDecl,
        dcls: &Rc<PhpClass>,
        src: &Cell,
    ) -> Result<Vec<String>, PhpError> {
        let tys = pd.ty.clone().unwrap_or_default();
        let mut merged = tys.clone();
        // The value must satisfy the NEW owner's type first — a plain
        // `Cannot assign X to property` error (typed_properties_034,
        // union_types/prop_ref_assign); the held-by incompatible
        // message only reports a type-SET conflict.
        let v = src.borrow().clone();
        let nv = self.prop_typed_write_check(pd, dcls, v)?;
        let sptr = Rc::as_ptr(src) as usize;
        // Drop owners whose prop stopped holding the cell (rebind/unset)
        // before reading the prior constraint.
        self.prune_typed_slot(sptr);
        // The cell's current constraint: the last merge result, else
        // the primary owner's declared type.
        let prior = self
            .slot_merged
            .get(&sptr)
            .cloned()
            .or_else(|| self.typed_slots.get(&sptr).map(|(_, t, _, _)| t.clone()));
        if let Some(cur) = prior {
            let ta = self.resolved_ty(&cur, dcls.as_ref());
            let tb = self.resolved_ty(&tys, dcls.as_ref());
            let inter = self.ty_bind_merge(&ta, &tb);
            // Bind fails when the sets are disjoint OR the current
            // value isn't already in the intersection (prop_ref_assign
            // B2-style held-by error naming the primary holder).
            let fits = match &inter {
                Some(m) => {
                    let vty = self.zval_type_name(&src.borrow());
                    m.iter().any(|mm| self.ty_member_is_a_strict(&vty, mm))
                }
                None => false,
            };
            if !fits {
                let (otys, ocn, opn) = self
                    .typed_slots
                    .get(&sptr)
                    .map(|(_, t, n, p)| (t.clone(), n.clone(), p.clone()))
                    .unwrap_or((cur.clone(), String::new(), String::new()));
                let sv = src.borrow().clone();
                let mut e = PhpError::uncaught(
                    "TypeError",
                    format!(
                        "Reference with value of type {} held by property {}::${} of type {} is not compatible with property {}::${} of type {}",
                        self.zval_type_name(&sv),
                        ocn,
                        opn,
                        ty_disp(&otys),
                        dcls.name(),
                        pd.name,
                        ty_disp(&tys)
                    ),
                    0,
                );
                e.thrown_line = Some(self.cur_line);
                return self.fail(e);
            }
            merged = inter.unwrap();
        }
        self.slot_merged.insert(sptr, merged.clone());
        *src.borrow_mut() = nv;
        Ok(merged)
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
        // self/parent/static in a prop type resolve against the
        // DECLARING class (typed_properties_043); keep the literal
        // members for error display.
        let resolved = self.resolved_ty(tys, dcls.as_ref());
        // Object with __toString coerces into a `string` prop weakly
        // (typed_properties_051).
        if let Value::Object(o) = &v {
            if tys.iter().any(|t| t.eq_ignore_ascii_case("string")) {
                let tcls = o.borrow().class.clone();
                let tostr = self.find_method_in(&tcls, "__tostring").is_some();
                if tostr {
                    let sv =
                        self.method_invoke(o.clone(), "__toString", CallArgs::positional(vec![]))?;
                    return Ok(sv);
                }
            }
        }
        let tys = &resolved;
        if self.ty_exact(tys, &v) {
            // int stored into a `float` prop widens to a float even in
            // strict mode (typed_properties_031) — but only when no
            // `int` member takes it exactly (`int|float` keeps int,
            // legal_default_values).
            if tys.iter().any(|t| t.eq_ignore_ascii_case("float"))
                && !tys.iter().any(|t| t.eq_ignore_ascii_case("int"))
            {
                if let Value::Int(i) = v {
                    return Ok(Value::Float(i as f64));
                }
            }
            return Ok(v);
        }
        if !self.exec_file_strict() {
            if let Some(c) = weak_ty_coerce(tys, &v) {
                self.deprecate_lossy_int(tys, &v, &c);
                return Ok(c);
            }
        }
        let mut e = PhpError::uncaught(
            "TypeError",
            format!(
                "Cannot assign {} to property {}::${} of type {}",
                self.zval_type_name(&v),
                dcls.name(),
                p.name,
                ty_disp(p.ty.as_deref().unwrap_or(&[]))
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
                self.diag_file(),
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
                            Value::Str(s) => crate::value::lossy(&s).into_owned(),
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

    /// Native bodies for the ArrayIterator stub. Iteration state lives in
    /// the `ArrayIter` object internal; unknown methods return None so
    /// the generic dispatch can report `Call to undefined method`.
    fn array_iter_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        args: &CallArgs,
    ) -> Result<Option<Value>, PhpError> {
        let lname = name.to_lowercase();
        let mk_arr = |ob: &mut PhpObject, a: Rc<RefCell<PhpArray>>, flags: i64| {
            ob.internal = Some(ObjectInternal::ArrayIter {
                arr: a,
                pos: 0,
                flags,
            });
            Value::Null
        };
        match lname.as_str() {
            "__construct" => {
                let flags = args.cells.get(1).map(|c| c.borrow().to_int()).unwrap_or(0);
                let mut ob = obj.borrow_mut();
                let a = match args.cells.first().map(|c| c.borrow().clone()) {
                    Some(Value::Array(a)) => {
                        let mut copy = PhpArray::new();
                        for (k, c) in &a.borrow().entries {
                            copy.set(k.clone(), c.borrow().clone());
                        }
                        Rc::new(RefCell::new(copy))
                    }
                    // Objects iterate their prop cells BY REFERENCE —
                    // writes through $v update the prop (typed gate
                    // still applies); deprecated since 8.5
                    // (typed_properties_113/114/115).
                    Some(Value::Object(o)) => {
                        drop(ob);
                        self.deprecated(
                            "ArrayIterator::__construct(): Using an object as a backing array for ArrayIterator is deprecated, as it allows violating class constraints and invariants",
                        )?;
                        ob = obj.borrow_mut();
                        let mut copy = PhpArray::new();
                        copy.is_ref = true;
                        let pairs: Vec<(String, Cell)> = o
                            .borrow()
                            .props
                            .iter()
                            .map(|(k, c)| (k.clone(), c.clone()))
                            .collect();
                        for (k, c) in pairs {
                            let pn = k.rsplit('\0').next().unwrap_or(&k).to_string();
                            if let Some((pd, dcls)) = self.decl_prop(&o, &pn) {
                                let p = Rc::as_ptr(&c) as usize;
                                if let Some(tys) = &pd.ty {
                                    // Writes through the shared cell
                                    // still hit the typed gate.
                                    self.typed_slots.insert(
                                        p,
                                        (
                                            c.clone(),
                                            tys.clone(),
                                            dcls.name().to_string(),
                                            pn.clone(),
                                        ),
                                    );
                                    self.slot_anchor
                                        .insert(p, SlotAnchor::Obj(Rc::downgrade(&o), k.clone()));
                                    self.slot_owners.entry(p).or_default().push((
                                        tys.clone(),
                                        dcls.name().to_string(),
                                        pn.clone(),
                                        SlotAnchor::Obj(Rc::downgrade(&o), k.clone()),
                                    ));
                                }
                                // Remember readonly cells — by-ref
                                // acquisition must fail (115).
                                if pd.readonly {
                                    self.readonly_cells
                                        .insert(p, (dcls.name().to_string(), pn.clone()));
                                }
                            }
                            copy.bind_cell(ArrKey::Str(Rc::from(k.as_str())), c);
                        }
                        Rc::new(RefCell::new(copy))
                    }
                    _ => Rc::new(RefCell::new(PhpArray::new())),
                };
                Ok(Some(mk_arr(&mut ob, a, flags)))
            }
            _ => {
                // All remaining methods need initialized state.
                let (arr, pos) = {
                    let ob = obj.borrow();
                    match &ob.internal {
                        Some(ObjectInternal::ArrayIter { arr, pos, .. }) => (arr.clone(), *pos),
                        _ => return Ok(None),
                    }
                };
                let v = match lname.as_str() {
                    "rewind" => {
                        if let Some(ObjectInternal::ArrayIter { pos: p, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *p = 0;
                        }
                        Value::Null
                    }
                    "valid" => Value::Bool(pos < arr.borrow().entries.len()),
                    "current" => arr
                        .borrow()
                        .entries
                        .get(pos)
                        .map(|(_, c)| c.borrow().clone())
                        .unwrap_or(Value::Bool(false)),
                    "key" => arr
                        .borrow()
                        .entries
                        .get(pos)
                        .map(|(k, _)| key_value(k))
                        .unwrap_or(Value::Null),
                    "next" => {
                        if let Some(ObjectInternal::ArrayIter { pos: p, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *p += 1;
                        }
                        Value::Null
                    }
                    "seek" => {
                        let i = args.cells.first().map(|c| c.borrow().to_int()).unwrap_or(0);
                        let len = arr.borrow().entries.len() as i64;
                        if i < 0 || i >= len.max(1) && !(i == 0 && len == 0) {
                            return self.fail(PhpError::uncaught(
                                "OutOfBoundsException",
                                format!("Seek position {} is out of range", i),
                                0,
                            ));
                        }
                        if let Some(ObjectInternal::ArrayIter { pos: p, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *p = i as usize;
                        }
                        Value::Null
                    }
                    "count" => Value::Int(arr.borrow().entries.len() as i64),
                    "getarraycopy" => Value::Array(arr.clone()),
                    "offsetget" => {
                        let k = args
                            .cells
                            .first()
                            .map(|c| to_key(&c.borrow()))
                            .unwrap_or(ArrKey::Int(0));
                        match arr.borrow().get(&k) {
                            Some(v) => v,
                            None => {
                                let kn = key_value(&k).to_php_string();
                                let _ = self.warn(&format!("Undefined array key {}", kn));
                                Value::Null
                            }
                        }
                    }
                    "offsetexists" => {
                        let k = args
                            .cells
                            .first()
                            .map(|c| to_key(&c.borrow()))
                            .unwrap_or(ArrKey::Int(0));
                        Value::Bool(arr.borrow().get(&k).is_some())
                    }
                    "offsetset" => {
                        let v = args
                            .cells
                            .get(1)
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Null);
                        match args.cells.first().map(|c| c.borrow().clone()) {
                            Some(Value::Null) | None => arr.borrow_mut().push(v),
                            Some(kv) => arr.borrow_mut().set(to_key(&kv), v),
                        }
                        Value::Null
                    }
                    "offsetunset" => {
                        let k = args
                            .cells
                            .first()
                            .map(|c| to_key(&c.borrow()))
                            .unwrap_or(ArrKey::Int(0));
                        arr.borrow_mut().unset(&k);
                        Value::Null
                    }
                    "getflags" => Value::Int({
                        let ob = obj.borrow();
                        match &ob.internal {
                            Some(ObjectInternal::ArrayIter { flags, .. }) => *flags,
                            _ => 0,
                        }
                    }),
                    "setflags" => {
                        let f = args.cells.first().map(|c| c.borrow().to_int()).unwrap_or(0);
                        if let Some(ObjectInternal::ArrayIter { flags, .. }) =
                            &mut obj.borrow_mut().internal
                        {
                            *flags = f;
                        }
                        Value::Null
                    }
                    "asort" | "ksort" => {
                        let mut a = arr.borrow_mut();
                        if lname == "asort" {
                            a.entries
                                .sort_by(|(_, x), (_, y)| compare(&x.borrow(), &y.borrow()));
                        } else {
                            a.entries.sort_by(|(x, _), (y, _)| match (x, y) {
                                (ArrKey::Int(a), ArrKey::Int(b)) => a.cmp(b),
                                _ => compare(&key_value(x), &key_value(y)),
                            });
                        }
                        Value::Bool(true)
                    }
                    "natsort" | "natcasesort" => {
                        let ci = lname == "natcasesort";
                        arr.borrow_mut().entries.sort_by(|(_, x), (_, y)| {
                            let mut a = x.borrow().to_php_string();
                            let mut b = y.borrow().to_php_string();
                            if ci {
                                a = a.to_lowercase();
                                b = b.to_lowercase();
                            }
                            compare(&Value::str(a), &Value::str(b))
                        });
                        Value::Bool(true)
                    }
                    _ => return Ok(None),
                };
                Ok(Some(v))
            }
        }
    }

    /// Call-arg list for `invokeArgs`/`newInstanceArgs`: array entries
    /// with string keys become named args (named_params/call_user_func).
    fn args_from_array(&mut self, v: &Value) -> CallArgs {
        let mut ca = CallArgs::empty();
        if let Value::Array(a) = v {
            for (k, c) in a.borrow().iter() {
                match k {
                    ArrKey::Str(s) => ca.named.push((s.to_string(), c.clone(), true, false)),
                    _ => ca.cells.push(c.clone()),
                }
            }
        }
        ca
    }

    /// Native bodies for the Reflection* stubs. The reflected
    /// class/function/prop names live under `\0rc\0` prop keys.
    fn reflection_method(
        &mut self,
        obj: &Rc<RefCell<PhpObject>>,
        name: &str,
        args: &CallArgs,
    ) -> Result<Option<Value>, PhpError> {
        let lname = name.to_lowercase();
        match lname.as_str() {
            "__construct" => {
                let mut ob = obj.borrow_mut();
                let cls = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                // Zend reflectors keep the class NAME, not the object —
                // `new ReflectionClass(new T)` drops the arg temp so
                // its __destruct runs at statement end (bug29368_2).
                let cls = match &cls {
                    Value::Object(o) => Value::str(o.borrow().class.name()),
                    _ => cls,
                };
                let prop = args
                    .get(1)
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                ob.props.insert("\0rc\0class".into(), cell(cls.clone()));
                ob.props.insert("\0rc\0prop".into(), cell(prop.clone()));
                // Public metadata props the real reflectors expose:
                // ReflectionProperty::{class,name}, ReflectionMethod::
                // {class,name}, ReflectionClass/Function::name. `class`
                // keeps the canonical (declared-case) class name.
                let cname = match &cls {
                    Value::Object(o) => o.borrow().class.name().to_string(),
                    Value::Str(s) => {
                        let raw = String::from_utf8_lossy(s).to_string();
                        let resolved = self.resolve_class(&raw).unwrap_or_else(|| raw.clone());
                        self.classes
                            .get(&resolved.to_lowercase())
                            .map(|c| c.decl.name.clone())
                            .unwrap_or(resolved)
                    }
                    // A closure first arg is a Closure object to Zend
                    // (bug69802_2).
                    Value::Callable(_) => "Closure".into(),
                    _ => String::new(),
                };
                match ob.class.name().to_lowercase().as_str() {
                    "reflectionproperty" | "reflectionmethod" | "reflectionclassconstant" => {
                        ob.props.insert("name".into(), cell(prop));
                        ob.props.insert("class".into(), cell(Value::str(&cname)));
                        // Public metadata props render in var_dump in
                        // declaration order: name, then class.
                        for k in ["name", "class"] {
                            if !ob.prop_order.contains(&k.into()) {
                                ob.prop_order.push(k.into());
                            }
                        }
                    }
                    "reflectionclass" | "reflectionfunction" => {
                        // A closure reflector's `name` is its Zend name
                        // `{closure:enclosing():L}` (closure_065).
                        let nm = match &cls {
                            Value::Callable(c) => match &c.kind {
                                CallableKind::Closure(d) => Value::str(&d.name),
                                CallableKind::Named(n) => Value::str(n),
                                CallableKind::Method { name, .. } => Value::str(name),
                            },
                            _ => cls,
                        };
                        ob.props.insert("name".into(), cell(nm));
                        if !ob.prop_order.contains(&"name".into()) {
                            ob.prop_order.push("name".into());
                        }
                    }
                    _ => {}
                }
                Ok(Some(Value::Null))
            }
            // ReflectionFunction::invoke(...$args) and
            // ReflectionMethod::invoke($object, ...$args) forward named
            // args to the target (named_params/call_user_func).
            "invoke" => {
                let is_method = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                if is_method {
                    let target = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = self.conv_str(&mn)?.to_string();
                    let ca = CallArgs {
                        cells: args.cells[1.min(args.cells.len())..].to_vec(),
                        named: args.named.clone(),
                        trav_cells: Vec::new(),
                        nonref_cells: Vec::new(),
                    };
                    match target {
                        Value::Object(o) => Ok(Some(self.method_invoke(o, &mn, ca)?)),
                        Value::Null => {
                            // Static context: Class::method or null $this.
                            let cn = obj
                                .borrow()
                                .props
                                .get("\0rc\0class")
                                .map(|c| c.borrow().clone())
                                .unwrap_or(Value::Null);
                            let cn = self.conv_str(&cn)?.to_string();
                            Ok(Some(self.call_named(&format!("{}::{}", cn, mn), &[])?))
                        }
                        _ => Ok(Some(Value::Null)),
                    }
                } else {
                    let cb = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let ca = CallArgs {
                        cells: args.cells.clone(),
                        named: args.named.clone(),
                        trav_cells: args.trav_cells.clone(),
                        nonref_cells: args.nonref_cells.clone(),
                    };
                    Ok(Some(self.call_value(&cb, ca)?))
                }
            }
            "invokeargs" => {
                let is_method = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                if is_method {
                    let target = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let arr = args
                        .get(1)
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = self.conv_str(&mn)?.to_string();
                    let ca = self.args_from_array(&arr);
                    match target {
                        Value::Object(o) => Ok(Some(self.method_invoke(o, &mn, ca)?)),
                        _ => Ok(Some(Value::Null)),
                    }
                } else {
                    let arr = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let cb = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let ca = self.args_from_array(&arr);
                    Ok(Some(self.call_value(&cb, ca)?))
                }
            }
            // Name introspection shared by function/class reflectors
            // (closure_067/068): closures report their zend name.
            "getshortname" | "getnamespacename" | "innamespace" | "isanonymous" => {
                let stored = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let (fname, anon) = match &stored {
                    Value::Callable(c) => (
                        self.callable_ctx_name(&stored),
                        matches!(c.kind, CallableKind::Closure(_)),
                    ),
                    _ => (self.conv_str(&stored)?.to_string(), false),
                };
                // A closure's "short name" is its whole zend name —
                // the `\` inside `{closure:Foo\Bar::baz():N}` is part
                // of the literal (closure_067).
                let (short, ns) = if anon {
                    (fname.clone(), String::new())
                } else {
                    (
                        fname.rsplit('\\').next().unwrap_or(&fname).to_string(),
                        match fname.rfind('\\') {
                            Some(i) => fname[..i].to_string(),
                            None => String::new(),
                        },
                    )
                };
                Ok(Some(match lname.as_str() {
                    "getshortname" => Value::str(short),
                    "getnamespacename" => Value::str(ns),
                    "innamespace" => Value::Bool(!anon && fname.contains('\\')),
                    _ => Value::Bool(anon),
                }))
            }
            // ReflectionFunctionAbstract closure accessors
            // (closure_031/042). A function reflector keeps the
            // callable under \0rc\0class; a method reflector keeps
            // {class-name, method-name} — resolve it for scope.
            "isclosure"
            | "getclosure"
            | "getclosurescopeclass"
            | "getclosurecalledclass"
            | "getclosurethis" => {
                let stored = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let is_method = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                let cb = match &stored {
                    Value::Callable(c) => Some(c.clone()),
                    _ => None,
                };
                if lname == "isclosure" {
                    return Ok(Some(Value::Bool(matches!(
                        cb.as_ref().map(|c| &c.kind),
                        Some(CallableKind::Closure(_))
                    ))));
                }
                if lname == "getclosure" {
                    if cb.is_some() {
                        return Ok(Some(stored));
                    }
                    if is_method {
                        let cn = self.conv_str(&stored)?.to_string();
                        let mn = obj
                            .borrow()
                            .props
                            .get("\0rc\0prop")
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Null);
                        let mn = self.conv_str(&mn)?.to_string();
                        let target = args
                            .first()
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Null);
                        let (mc, tc) = match target {
                            Value::Object(o) => (o.borrow().class.clone(), Some(o.clone())),
                            _ => (
                                self.classes
                                    .get(&cn.to_lowercase())
                                    .cloned()
                                    .ok_or_else(|| {
                                        PhpError::uncaught(
                                            "ReflectionException",
                                            format!("Class {} does not exist", cn),
                                            0,
                                        )
                                    })?,
                                None,
                            ),
                        };
                        let (mdecl, decl_cls) = self
                            .find_method_in(&mc, &mn)
                            .map(|(m, dc)| (Some(m), dc))
                            .unwrap_or_else(|| (None, mc.clone()));
                        return Ok(Some(Value::Callable(self.new_callable(PhpCallable {
                            id: std::cell::Cell::new(0),
                            kind: CallableKind::Method {
                                obj: tc,
                                class: Some(mc.clone()),
                                name: mn,
                            },
                            captures: Vec::new(),
                            this_obj: None,
                            scope_class: Some(decl_cls.clone()),
                            called_class: Some(mc),
                            // Static methods produce static closures —
                            // rebinding an instance warns (closure_061).
                            is_static: mdecl.map(|m| m.is_static).unwrap_or(false),
                        }))));
                    }
                    // A function reflector's getClosure is a named
                    // callable — builtins included (bug70630).
                    let fname = self.conv_str(&stored)?.to_string();
                    if !fname.is_empty() {
                        return Ok(Some(Value::Callable(self.new_callable(PhpCallable {
                            id: std::cell::Cell::new(0),
                            kind: CallableKind::Named(fname),
                            captures: Vec::new(),
                            this_obj: None,
                            scope_class: None,
                            called_class: None,
                            is_static: false,
                        }))));
                    }
                    return Ok(Some(Value::Null));
                }
                // Scope/this accessors.
                let (scope, called, this) = match &cb {
                    Some(c) => (
                        c.scope_class.clone(),
                        c.called_class.clone(),
                        c.this_obj.clone(),
                    ),
                    None if is_method => {
                        let cn = self.conv_str(&stored)?.to_string();
                        let mc = self.classes.get(&cn.to_lowercase()).cloned();
                        (mc.clone(), mc, None)
                    }
                    None => (None, None, None),
                };
                match lname.as_str() {
                    "getclosurethis" => Ok(Some(match this {
                        Some(o) => Value::Object(o),
                        None => Value::Null,
                    })),
                    _ => {
                        let rc = if lname == "getclosurescopeclass" {
                            scope
                        } else {
                            called
                        };
                        // Dummy scope: a closure bound to $this with
                        // no real scope reflects class Closure
                        // (closure_042).
                        let nm = rc.map(|c| c.name().to_string()).or_else(|| {
                            if lname == "getclosurescopeclass" && this.is_some() {
                                Some("Closure".to_string())
                            } else {
                                None
                            }
                        });
                        match nm {
                            Some(n) => {
                                let r = self.instantiate("reflectionclass", &[])?;
                                if let Value::Object(o) = &r {
                                    let mut ob = o.borrow_mut();
                                    ob.props.insert("name".into(), cell(Value::str(&n)));
                                    ob.props.insert("\0rc\0class".into(), cell(Value::str(&n)));
                                }
                                Ok(Some(r))
                            }
                            None => Ok(Some(Value::Null)),
                        }
                    }
                }
            }
            // ReflectionClass::getProperty($name) -> ReflectionProperty
            // carrying {\0rc\0class, \0rc\0prop} (typed_properties_018).
            "getproperty" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let pn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                let cls = self.classes.get(&cn.to_lowercase()).cloned();
                let found = cls.as_ref().and_then(|c| self.find_prop_decl(c, &pn));
                match found {
                    Some((_, dcls)) => {
                        let rp = self.instantiate("reflectionproperty", &[])?;
                        if let Value::Object(o) = &rp {
                            let mut ob = o.borrow_mut();
                            ob.props
                                .insert("\0rc\0class".into(), cell(Value::str(dcls.name())));
                            ob.props.insert("\0rc\0prop".into(), cell(Value::str(&pn)));
                            ob.props.insert("name".into(), cell(Value::str(&pn)));
                            ob.props
                                .insert("class".into(), cell(Value::str(dcls.name())));
                        }
                        Ok(Some(rp))
                    }
                    None => self.fail(PhpError::uncaught(
                        "ReflectionException",
                        format!("Property {}::${} does not exist", cn, pn),
                        0,
                    )),
                }
            }
            "hasproperty" => {
                let pn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                let target = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                // ReflectionObject wraps the object itself; check its
                // live + declared props (bug50146). Closures never have
                // props.
                match &target {
                    Value::Object(t) => {
                        let has =
                            t.borrow().props.contains_key(&pn) || self.decl_prop(t, &pn).is_some();
                        return Ok(Some(Value::Bool(has)));
                    }
                    Value::Callable(_) | Value::Null => {
                        return Ok(Some(Value::Bool(false)));
                    }
                    _ => {}
                }
                let cn = self.conv_str(&target)?.to_string();
                let has = self
                    .classes
                    .get(&cn.to_lowercase())
                    .cloned()
                    .and_then(|c| self.find_prop_decl(&c, &pn))
                    .is_some();
                Ok(Some(Value::Bool(has)))
            }
            // ReflectionProperty::getType() -> ReflectionNamedType with
            // the declared members under \0rp\0ty (same convention as
            // ReflectionParameter).
            "gettype" => {
                let is_prop = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionproperty");
                if is_prop {
                    let cn = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let cn = self.conv_str(&cn)?.to_string();
                    let pn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let pn = self.conv_str(&pn)?.to_string();
                    let ty = self
                        .classes
                        .get(&cn.to_lowercase())
                        .cloned()
                        .and_then(|c| self.find_prop_decl(&c, &pn))
                        .and_then(|(pd, _)| pd.ty);
                    match ty {
                        Some(tys) => {
                            let nt = self.instantiate("reflectionnamedtype", &[])?;
                            if let Value::Object(o) = &nt {
                                let mut ta = PhpArray::default();
                                for m in &tys {
                                    ta.push(Value::str(m));
                                }
                                let mut ob = o.borrow_mut();
                                ob.props.insert(
                                    "\0rp\0ty".into(),
                                    cell(Value::Array(Rc::new(RefCell::new(ta)))),
                                );
                                ob.props
                                    .insert("name".into(), cell(Value::str(tys.first().unwrap())));
                            }
                            Ok(Some(nt))
                        }
                        None => Ok(Some(Value::Null)),
                    }
                } else {
                    // ReflectionParameter::getType() — members stored
                    // under \0rp\0ty by getParameters()
                    // (trampoline_closure_named_arguments).
                    let is_param = obj
                        .borrow()
                        .class
                        .name()
                        .eq_ignore_ascii_case("reflectionparameter");
                    let tys = if is_param {
                        obj.borrow()
                            .props
                            .get("\0rp\0ty")
                            .map(|c| c.borrow().clone())
                            .and_then(|v| match v {
                                Value::Array(a) => Some(a),
                                _ => None,
                            })
                    } else {
                        None
                    };
                    match tys {
                        Some(ta) => {
                            let members: Vec<String> = ta
                                .borrow()
                                .entries
                                .iter()
                                .map(|(_, c)| c.borrow().to_php_string())
                                .collect();
                            let nt = self.instantiate("reflectionnamedtype", &[])?;
                            if let Value::Object(o) = &nt {
                                let mut ob = o.borrow_mut();
                                ob.props
                                    .insert("\0rp\0ty".into(), cell(Value::Array(ta.clone())));
                                if let Some(first) = members.first() {
                                    ob.props
                                        .insert("name".into(), cell(Value::str(first.clone())));
                                }
                            }
                            Ok(Some(nt))
                        }
                        None => Ok(Some(Value::Null)),
                    }
                }
            }
            // ReflectionClass::getDefaultProperties(): prop defaults
            // keyed by prop name; typed props without defaults are
            // absent (bug #77673 — typed_properties_105).
            "getdefaultproperties" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mut arr = PhpArray::default();
                if let Some(cls) = self.classes.get(&cn.to_lowercase()).cloned() {
                    let mut chain: Vec<Rc<PhpClass>> = Vec::new();
                    let mut cur = Some(cls);
                    while let Some(c) = cur {
                        chain.push(c.clone());
                        cur = c
                            .decl
                            .parent
                            .as_ref()
                            .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
                    }
                    for c in chain.iter().rev() {
                        for p in &c.decl.props {
                            if p.ty.is_some() && p.default.is_none() {
                                continue;
                            }
                            let dv = match &p.default {
                                Some(d) => {
                                    let old = self.const_self.replace(c.clone());
                                    self.class_const_ctx += 1;
                                    let r = self.eval_decl_const(d, &c.decl.file);
                                    self.class_const_ctx -= 1;
                                    self.const_self = old;
                                    match r {
                                        Ok(v) => v,
                                        Err(_) => continue,
                                    }
                                }
                                None => Value::Null,
                            };
                            arr.set(ArrKey::Str(p.name.clone().into()), dv);
                        }
                    }
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "getparameters" => {
                // Each param becomes a ReflectionParameter carrying its
                // declared type members under \0rp\0ty (callable_002).
                let is_method = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionmethod");
                let decl: Option<Rc<crate::ast::FunctionDecl>> = if is_method {
                    let cn = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mn = self.conv_str(&mn)?.to_string();
                    if matches!(&cn, Value::Callable(_)) {
                        // new ReflectionMethod($closure, '__invoke') —
                        // Closure::__invoke carries the wrapped
                        // function's signature (bug69802_2).
                        if mn.eq_ignore_ascii_case("__invoke") {
                            self.callable_decl(&cn)
                        } else {
                            None
                        }
                    } else {
                        let cn = self.conv_str(&cn)?.to_string();
                        let c = self.classes.get(&cn.to_lowercase()).cloned();
                        match c {
                            Some(c) => self
                                .find_method_in(&c, &mn)
                                .map(|(m, _)| Rc::new(m.decl.clone())),
                            None => None,
                        }
                    }
                } else {
                    let cb = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    self.callable_decl(&cb)
                };
                let mut arr = PhpArray::default();
                if let Some(d) = decl {
                    for p in &d.params {
                        let rp = self.instantiate("reflectionparameter", &[])?;
                        if let Value::Object(o) = &rp {
                            o.borrow_mut()
                                .props
                                .insert("\0rp\0name".into(), cell(Value::str(&p.name)));
                            // Zend's ReflectionParameter exposes the name
                            // as a public prop rendered by var_dump.
                            let mut ob = o.borrow_mut();
                            ob.props.insert("name".into(), cell(Value::str(&p.name)));
                            if !ob.prop_order.contains(&"name".into()) {
                                ob.prop_order.push("name".into());
                            }
                            drop(ob);
                            o.borrow_mut()
                                .props
                                .insert("\0rp\0variadic".into(), cell(Value::Bool(p.variadic)));
                            let mut ta = PhpArray::default();
                            if let Some(ty) = &p.ty {
                                for m in ty {
                                    ta.push(Value::str(m));
                                }
                            }
                            o.borrow_mut().props.insert(
                                "\0rp\0ty".into(),
                                cell(Value::Array(Rc::new(RefCell::new(ta)))),
                            );
                        }
                        arr.push(rp);
                    }
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "isvariadic" => Ok(Some(Value::Bool(
                obj.borrow()
                    .props
                    .get("\0rp\0variadic")
                    .is_some_and(|c| c.borrow().is_truthy()),
            ))),
            "hastype" => {
                // ReflectionParameter::hasType() — \0rp\0ty members
                // populated by getParameters().
                let has = obj
                    .borrow()
                    .props
                    .get("\0rp\0ty")
                    .map(|c| c.borrow().clone())
                    .and_then(|v| match v {
                        Value::Array(a) => Some(!a.borrow().entries.is_empty()),
                        _ => None,
                    })
                    .unwrap_or(false);
                Ok(Some(Value::Bool(has)))
            }
            "getclass" => {
                // Deprecated since 8.0 — returns a ReflectionClass for
                // the first class/interface member of the declared
                // type (a union picks the class part — bug69802_2).
                self.deprecated(
                    "Method ReflectionParameter::getClass() is deprecated since 8.0, use ReflectionParameter::getType() instead",
                )?;
                let ty = obj
                    .borrow()
                    .props
                    .get("\0rp\0ty")
                    .map(|c| c.borrow().clone())
                    .and_then(|v| match v {
                        Value::Array(a) => Some(a),
                        _ => None,
                    });
                let class_ty = ty.and_then(|ta| {
                    ta.borrow().entries.iter().find_map(|(_, c)| {
                        let n = c.borrow().to_php_string();
                        self.classes
                            .get(&n.to_lowercase())
                            .map(|cl| cl.decl.name.clone())
                            .or_else(|| {
                                self.interfaces
                                    .get(&n.to_lowercase())
                                    .map(|d| d.name.clone())
                            })
                    })
                });
                match class_ty {
                    Some(n) => {
                        let rc = self.instantiate("reflectionclass", &[])?;
                        if let Value::Object(o) = &rc {
                            let mut ob = o.borrow_mut();
                            ob.props.insert("\0rc\0class".into(), cell(Value::str(&n)));
                            ob.props.insert("name".into(), cell(Value::str(&n)));
                            if !ob.prop_order.contains(&"name".into()) {
                                ob.prop_order.push("name".into());
                            }
                        }
                        Ok(Some(rc))
                    }
                    None => Ok(Some(Value::Null)),
                }
            }
            "iscallable" => {
                self.deprecated(
                    "Method ReflectionParameter::isCallable() is deprecated since 8.0, use ReflectionParameter::getType() instead",
                )?;
                let has = obj
                    .borrow()
                    .props
                    .get("\0rp\0ty")
                    .map(|c| c.borrow().clone())
                    .and_then(|v| match v {
                        Value::Array(a) => Some(a),
                        _ => None,
                    })
                    .map(|a| {
                        a.borrow().entries.iter().any(|(_, c)| {
                            matches!(&*c.borrow(), Value::Str(s) if s.eq_ignore_ascii_case(b"callable"))
                        })
                    })
                    .unwrap_or(false);
                Ok(Some(Value::Bool(has)))
            }
            "getattributes" => {
                let is_fn = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionfunction");
                let tn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let tn = self.conv_str(&tn)?.to_string();
                let is_cc = obj
                    .borrow()
                    .class
                    .name()
                    .eq_ignore_ascii_case("reflectionclassconstant");
                if is_cc {
                    let cn = obj
                        .borrow()
                        .props
                        .get("\0rc\0class")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let cn = self.conv_str(&cn)?.to_string();
                    let pn = obj
                        .borrow()
                        .props
                        .get("\0rc\0prop")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let pn = self.conv_str(&pn)?.to_string();
                    let decls = self
                        .find_const_decl(&cn, &pn)
                        .map(|(cd, _)| cd.attrs.clone())
                        .unwrap_or_default();
                    let fname = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .map(|v| self.conv_str(&v).map(|s| s.to_string()))
                        .transpose()?
                        .unwrap_or_default();
                    let mut arr = PhpArray::default();
                    for a in decls {
                        if !fname.is_empty() && !a.name.eq_ignore_ascii_case(&fname) {
                            continue;
                        }
                        let v = self.instantiate("reflectionattribute", &[])?;
                        if let Value::Object(o) = &v {
                            o.borrow_mut().internal = Some(ObjectInternal::ReflectionAttribute {
                                name: a.name.clone(),
                                args: Rc::new(a.args.clone()),
                                target: 16,
                            });
                        }
                        arr.push(v);
                    }
                    return Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))));
                }
                let (decls, target): (Vec<crate::ast::AttrDecl>, i64) = if is_fn {
                    (
                        self.functions
                            .get(&tn.to_lowercase())
                            .map(|d| d.attrs.clone())
                            .unwrap_or_default(),
                        2,
                    )
                } else {
                    (
                        self.classes
                            .get(&tn.to_lowercase())
                            .map(|c| c.decl.attrs.clone())
                            .unwrap_or_default(),
                        1,
                    )
                };
                // Optional class-name filter arg.
                let fname = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .map(|v| self.conv_str(&v).map(|s| s.to_string()))
                    .transpose()?
                    .unwrap_or_default();
                let mut arr = PhpArray::default();
                for a in decls {
                    if !fname.is_empty() && !a.name.eq_ignore_ascii_case(&fname) {
                        continue;
                    }
                    let v = self.instantiate("reflectionattribute", &[])?;
                    if let Value::Object(o) = &v {
                        o.borrow_mut().internal = Some(ObjectInternal::ReflectionAttribute {
                            name: a.name.clone(),
                            args: Rc::new(a.args.clone()),
                            target,
                        });
                    }
                    arr.push(v);
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "getarguments" => {
                let exprs = match &obj.borrow().internal {
                    Some(ObjectInternal::ReflectionAttribute { args, .. }) => args.clone(),
                    _ => return Ok(Some(Value::Null)),
                };
                let mut arr = PhpArray::default();
                for e in exprs.iter() {
                    if let Expr::Binary { op: "named", l, r } = e {
                        if let Expr::Str(n) = l.as_ref() {
                            let v = self.eval_const(r)?;
                            arr.set(ArrKey::Str(n.as_str().into()), v);
                            continue;
                        }
                    }
                    arr.push(self.eval_const(e)?);
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "newinstance" | "newinstanceargs" => {
                if let Some(ObjectInternal::ReflectionAttribute {
                    name,
                    args: aexprs,
                    target,
                }) = &obj.borrow().internal
                {
                    let (name, aexprs, target) = (name.clone(), aexprs.clone(), *target);
                    let lname = name.trim_start_matches('\\').to_lowercase();
                    let cls = match self.classes.get(&lname).cloned() {
                        Some(c) => c,
                        None => {
                            // `new ReflectionClass` may not have loaded
                            // the attribute class yet — trigger autoload.
                            let resolved = self
                                .resolve_class(name.trim_start_matches('\\'))
                                .unwrap_or_else(|| name.clone());
                            match self.classes.get(&resolved.to_lowercase()).cloned() {
                                Some(c) => c,
                                None => {
                                    return self
                                        .fail::<Option<Value>>(PhpError::uncaught(
                                            "Error",
                                            format!("Class \"{}\" not found", name),
                                            0,
                                        ))
                                        .map(|_| None);
                                }
                            }
                        }
                    };
                    let short = |n: &str| n.rsplit('\\').next().unwrap_or(n).to_string();
                    let marker = cls
                        .decl
                        .attrs
                        .iter()
                        .find(|a| short(&a.name).eq_ignore_ascii_case("attribute"));
                    let Some(marker) = marker else {
                        return self
                            .fail::<Option<Value>>(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Attempting to use non-attribute class \"{}\" as attribute",
                                    name
                                ),
                                0,
                            ))
                            .map(|_| None);
                    };
                    // `#[Attribute(flags: N)]` (or first positional) gates
                    // which declarations the attribute may target.
                    let mut mask = 63i64;
                    let flags_e = marker
                        .args
                        .iter()
                        .find_map(|a| {
                            if let Expr::Binary { op: "named", l, r } = a {
                                if matches!(l.as_ref(), Expr::Str(n) if n == "flags") {
                                    return Some(r.as_ref());
                                }
                                None
                            } else {
                                None
                            }
                        })
                        .or_else(|| marker.args.first());
                    if let Some(e) = flags_e {
                        mask = self.eval_const(e)?.to_int();
                    }
                    if mask & target == 0 {
                        let tn = match target {
                            1 => "class",
                            2 => "function",
                            4 => "method",
                            8 => "property",
                            16 => "class constant",
                            32 => "parameter",
                            _ => "unknown",
                        };
                        let allowed: Vec<&str> = [
                            (1i64, "class"),
                            (2, "function"),
                            (4, "method"),
                            (8, "property"),
                            (16, "class constant"),
                            (32, "parameter"),
                        ]
                        .iter()
                        .filter(|(b, _)| mask & b != 0)
                        .map(|(_, n)| *n)
                        .collect();
                        return self
                            .fail::<Option<Value>>(PhpError::uncaught(
                                "Error",
                                format!(
                                    "Attribute \"{}\" cannot target {} (allowed targets: {})",
                                    name,
                                    tn,
                                    allowed.join(", ")
                                ),
                                0,
                            ))
                            .map(|_| None);
                    }
                    let mut cells = Vec::new();
                    let mut named = Vec::new();
                    for e in aexprs.iter() {
                        if let Expr::Binary { op: "named", l, r } = e {
                            if let Expr::Str(n) = l.as_ref() {
                                named.push((n.clone(), cell(self.eval_const(r)?), true, false));
                                continue;
                            }
                        }
                        cells.push(cell(self.eval_const(e)?));
                    }
                    let ca = CallArgs {
                        cells,
                        named,
                        trav_cells: Vec::new(),
                        nonref_cells: Vec::new(),
                    };
                    return self.new_instance(&name, ca).map(Some);
                }
                let ca = if lname == "newinstanceargs" {
                    let arr = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    self.args_from_array(&arr)
                } else {
                    CallArgs {
                        cells: args.cells.clone(),
                        named: args.named.clone(),
                        trav_cells: args.trav_cells.clone(),
                        nonref_cells: args.nonref_cells.clone(),
                    }
                };
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                Ok(Some(self.new_instance(&cn, ca)?))
            }
            "getname" => {
                if let Some(ObjectInternal::ReflectionAttribute { name, .. }) =
                    &obj.borrow().internal
                {
                    return Ok(Some(Value::str(name.clone())));
                }
                let clsname = obj.borrow().class.name().to_lowercase();
                // ReflectionNamedType::getName() -> the stored type name.
                if clsname == "reflectionnamedtype" {
                    return Ok(Some(
                        obj.borrow()
                            .props
                            .get("name")
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Null),
                    ));
                }
                // ReflectionClassConstant::getName() is the const name;
                // ReflectionProperty::getName() the prop name; every
                // other reflector reports its class/subject.
                let key = match clsname.as_str() {
                    "reflectionclassconstant"
                    | "reflectionclass"
                    | "reflectionfunction"
                    | "reflectionmethod" => "name",
                    "reflectionproperty" => "\0rc\0prop",
                    "reflectionparameter" => "\0rp\0name",
                    _ => "\0rc\0class",
                };
                Ok(Some(
                    obj.borrow()
                        .props
                        .get(key)
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null),
                ))
            }
            "newinstancewithoutconstructor" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?;
                Ok(Some(self.instantiate(&cn.to_lowercase(), &[])?))
            }
            "isfinal" | "isabstract" | "isstatic" | "ispublic" | "isprotected" | "isprivate" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let mn = self.conv_str(&mn)?.to_string();
                let m = self
                    .classes
                    .get(&cn.to_lowercase())
                    .cloned()
                    .and_then(|c| self.find_method_in(&c, &mn).map(|(m, _)| m));
                let b = match m {
                    Some(m) => match lname.as_str() {
                        "isfinal" => m.is_final,
                        "isabstract" => m.is_abstract,
                        "isstatic" => m.is_static,
                        "ispublic" => m.visibility == crate::ast::Visibility::Public,
                        "isprotected" => m.visibility == crate::ast::Visibility::Protected,
                        _ => m.visibility == crate::ast::Visibility::Private,
                    },
                    None => false,
                };
                Ok(Some(Value::Bool(b)))
            }
            "gettraitaliases" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mut arr = PhpArray::default();
                if let Some(c) = self.classes.get(&cn.to_lowercase()).cloned() {
                    for m in &c.decl.methods {
                        if let Some(orig) = &m.trait_alias_of {
                            let v =
                                format!("{}::{}", m.decl.decl_in.clone().unwrap_or_default(), orig);
                            arr.set(ArrKey::Str(m.decl.name.as_str().into()), Value::str(v));
                        }
                    }
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            // ReflectionClass::getInterfaceNames() — declared-case
            // interface names; getInterfaces() returns the reflectors.
            "getinterfacenames" | "getinterfaces" => {
                let cv = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = match &cv {
                    Value::Object(o) => o.borrow().class.name().to_string(),
                    _ => self.conv_str(&cv)?.to_string(),
                };
                let l = self
                    .resolve_class(&cn)
                    .unwrap_or_else(|| cn.clone())
                    .to_lowercase();
                let decl = self
                    .classes
                    .get(&l)
                    .map(|c| c.decl.clone())
                    .or_else(|| self.interfaces.get(&l).cloned())
                    .or_else(|| self.traits.get(&l).cloned());
                let mut arr = PhpArray::default();
                if let Some(d) = decl {
                    for i in &d.implements {
                        if lname == "getinterfaces" {
                            let r = self.instantiate("reflectionclass", &[Value::str(i)])?;
                            arr.set(ArrKey::Str(i.as_str().into()), r);
                        } else {
                            arr.push(Value::str(i.clone()));
                        }
                    }
                }
                Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))))
            }
            "getconstant" | "getconstants" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                if lname == "getconstants" {
                    let mut arr = PhpArray::default();
                    for (n, cd) in self.all_const_decls(&cn) {
                        let f = cd.1.clone();
                        if let Ok(v) = self.eval_decl_const(&cd.0.value, &f) {
                            arr.set(ArrKey::Str(n.into()), v);
                        }
                    }
                    return Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))));
                }
                let pn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                match self.find_const_decl(&cn, &pn) {
                    Some((cd, f)) => {
                        let v = self.eval_decl_const(&cd.value, &f)?;
                        Ok(Some(v))
                    }
                    None => Ok(Some(Value::Bool(false))),
                }
            }
            "getreflectionconstant" | "getreflectionconstants" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let mk = |it: &mut Self, cname: &str, n: &str| -> Result<Value, PhpError> {
                    let v = it.instantiate("reflectionclassconstant", &[])?;
                    if let Value::Object(o) = &v {
                        o.borrow_mut()
                            .props
                            .insert("\0rc\0class".into(), cell(Value::str(cname)));
                        o.borrow_mut()
                            .props
                            .insert("\0rc\0prop".into(), cell(Value::str(n)));
                        o.borrow_mut()
                            .props
                            .insert("name".into(), cell(Value::str(n)));
                        o.borrow_mut()
                            .props
                            .insert("class".into(), cell(Value::str(cname)));
                    }
                    Ok(v)
                };
                if lname == "getreflectionconstants" {
                    let mut arr = PhpArray::default();
                    for (n, _) in self.all_const_decls(&cn) {
                        arr.push(mk(self, &cn, &n)?);
                    }
                    return Ok(Some(Value::Array(Rc::new(RefCell::new(arr)))));
                }
                let pn = args
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                if self.find_const_decl(&cn, &pn).is_none() {
                    return Ok(Some(Value::Bool(false)));
                }
                Ok(Some(mk(self, &cn, &pn)?))
            }
            "getvalue" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let pn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                match self.find_const_decl(&cn, &pn) {
                    Some((cd, f)) => Ok(Some(self.eval_decl_const(&cd.value, &f)?)),
                    None => Ok(Some(Value::Null)),
                }
            }
            "setvalue" => {
                // ReflectionProperty::setValue($object, $value) —
                // bypasses prop visibility without __set (bug72177).
                let pn = obj
                    .borrow()
                    .props
                    .get("\0rc\0prop")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let pn = self.conv_str(&pn)?.to_string();
                let target = args.first().map(|c| c.borrow().clone());
                let val = args
                    .get(1)
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                if let Some(Value::Object(t)) = target {
                    let mut tb = t.borrow_mut();
                    if !tb.props.contains_key(&pn) && !tb.prop_order.iter().any(|k| k == &pn) {
                        tb.prop_order.push(pn.clone());
                    }
                    tb.props.insert(pn.clone(), cell(val));
                    tb.unset_props.remove(&pn);
                }
                Ok(Some(Value::Null))
            }
            "getdeclaringclass" => {
                let cn = obj
                    .borrow()
                    .props
                    .get("\0rc\0class")
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                let cn = self.conv_str(&cn)?.to_string();
                let v = self.instantiate("reflectionclass", &[])?;
                if let Value::Object(o) = &v {
                    o.borrow_mut()
                        .props
                        .insert("\0rc\0class".into(), cell(Value::str(cn.clone())));
                    o.borrow_mut()
                        .props
                        .insert("name".into(), cell(Value::str(cn)));
                }
                Ok(Some(v))
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
        self.prop_read_value(ov, &pn, nullsafe)
    }

    /// prop_read with a pre-bound name — zend binds the operand once,
    /// so a name mutation inside __isset/__get doesn't re-evaluate it
    /// (bug75420).
    fn prop_read_value(&mut self, ov: Value, pn: &str, nullsafe: bool) -> Result<Value, PhpError> {
        match ov {
            Value::Null if nullsafe => Ok(Value::Null),
            Value::Object(o) => {
                let cls = o.borrow().class.clone();
                if !self.in_own_hook(&o, pn) {
                    if let Some((pd, hs)) = self.hooked_prop(&o, pn) {
                        return self.hook_read(&o, &pd, &hs);
                    }
                }
                if let Some(k) = self.obj_prop_key(&o, pn) {
                    // Declared but not visible from this scope →
                    // __get territory (bug37667).
                    if self.prop_visible(&cls, pn) {
                        return Ok(o.borrow().props.get(&k).unwrap().borrow().clone());
                    }
                }
                // Typed prop whose slot was never initialized → Error
                // (not __get, not a warning): parent_get_plain_typed_uninitialized.
                // One that was unset() routes to __get like an
                // undefined property (typed_properties_009).
                let was_unset = {
                    let ob = o.borrow();
                    ob.unset_props.contains(pn)
                        || ob
                            .unset_props
                            .iter()
                            .any(|k| k.ends_with(&format!("\0{}", pn)))
                };
                // Without __get, an unset() declared prop still reads
                // as uninitialized; with __get it routes to magic
                // (typed_properties_047 vs _009).
                let has_get = self.find_method_in(&cls, "__get").is_some();
                if !was_unset || !has_get {
                    if let Some((tpd, tdcls)) = self.decl_prop(&o, pn) {
                        if tpd.ty.is_some() {
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
                // __get magic — the (obj, prop) in-get guard keeps a
                // re-entrant `$this->$pn` inside __get on real storage
                // (bug63462/bug66609).
                if has_get {
                    let gkey = (Rc::as_ptr(&o) as usize, 0u8, pn.to_string());
                    if !self.magic_guards.insert(gkey.clone()) {
                        // Re-entrant access to a DECLARED prop the
                        // magic scope can't see is a hard Error, not
                        // an undefined-prop warning (bug48248).
                        if let Some(e) = self.hidden_decl_error(&o, pn) {
                            return self.fail(e);
                        }
                        self.check_prop_name(pn)?;
                        self.warn(&format!("Undefined property: {}::${}", cls.name(), pn))?;
                        return Ok(Value::Null);
                    }
                    let res = self.method_invoke(
                        o.clone(),
                        "__get",
                        CallArgs::positional(vec![cell(Value::str(pn))]),
                    );
                    self.magic_guards.remove(&gkey);
                    let rv = res?;
                    // A __get result for an unset() declared-typed prop
                    // must satisfy the declared type
                    // (typed_properties_030).
                    if was_unset {
                        if let Some((tpd, tdcls)) = self.decl_prop(&o, pn) {
                            if let Some(tys) = &tpd.ty {
                                if !self.ty_exact(tys, &rv) {
                                    if let Some(cv) = weak_ty_coerce(tys, &rv) {
                                        return Ok(cv);
                                    }
                                }
                                let ok =
                                    self.ty_exact(tys, &rv) || weak_ty_coerce(tys, &rv).is_some();
                                if !ok {
                                    let mut e = PhpError::uncaught(
                                        "TypeError",
                                        format!(
                                            "Value of type {} returned from {}::__get() must be compatible with unset property {}::${} of type {}",
                                            self.zval_type_name(&rv),
                                            tdcls.name(),
                                            tdcls.name(),
                                            pn,
                                            ty_disp(tys)
                                        ),
                                        0,
                                    );
                                    e.thrown_line = Some(self.cur_line);
                                    return self.fail(e);
                                }
                            }
                        }
                    }
                    return Ok(rv);
                }
                // A declared prop this scope can't see raises
                // `Cannot access private/protected property`, not the
                // undefined-property warning (closure_020).
                if let Some(e) = self.hidden_decl_error(&o, pn) {
                    return self.fail(e);
                }
                self.check_prop_name(pn)?;
                self.warn(&format!("Undefined property: {}::${}", cls.name(), pn))?;
                Ok(Value::Null)
            }
            Value::Callable(_) => {
                // Closure is a real class with no declared props —
                // reads warn "Undefined property: Closure::$a"
                // (closure_031).
                self.check_prop_name(pn)?;
                self.warn(&format!("Undefined property: Closure::${}", pn))?;
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
                // A slot that exists but isn't visible from this scope
                // is *inaccessible*: cell ops route to __get like a
                // missing prop, and the write dies in the temp
                // (bug37667 — appends to a protected prop).
                let key = match key {
                    Some(k) if self.prop_visible(&o.borrow().class.clone(), &pn) => Some(k),
                    _ => None,
                };
                if key.is_none() {
                    if let Some((tpd, tdcls)) = self.decl_prop(&o, &pn) {
                        if tpd.ty.is_some() && tpd.default.is_none() {
                            let nullable = tpd
                                .ty
                                .as_ref()
                                .map(|t| t.iter().any(|m| m.eq_ignore_ascii_case("null")))
                                .unwrap_or(false);
                            if !nullable {
                                // `=&` on an uninit typed prop routes to
                                // `&__get` when it exists — the bound
                                // ref sees __get's cell (073).
                                let cls = o.borrow().class.clone();
                                let get_by_ref = self
                                    .find_method_in(&cls, "__get")
                                    .map(|(m, _)| m.decl.by_ref)
                                    .unwrap_or(false);
                                if get_by_ref {
                                    let gkey = (Rc::as_ptr(&o) as usize, 0u8, pn.clone());
                                    if !self.magic_guards.insert(gkey.clone()) {
                                        return self.fail(PhpError::uncaught(
                                            "Error",
                                            format!(
                                                "Cannot access uninitialized non-nullable property {}::${} by reference",
                                                tdcls.name(),
                                                pn
                                            ),
                                            0,
                                        ));
                                    }
                                    self.last_ret_cell = None;
                                    let res = self.method_invoke(
                                        o.clone(),
                                        "__get",
                                        CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                                    );
                                    self.magic_guards.remove(&gkey);
                                    let rv = res?;
                                    let got = self.last_ret_cell.take().unwrap_or_else(|| cell(rv));
                                    // The bound ref IS __get's cell —
                                    // its value is cast to the declared
                                    // type in place and the prop itself
                                    // stays uninitialized (073).
                                    let cv = self.prop_typed_write_check(
                                        &tpd,
                                        &tdcls,
                                        got.borrow().clone(),
                                    )?;
                                    *got.borrow_mut() = cv;
                                    self.ref_cells.insert(Rc::as_ptr(&got) as usize);
                                    return Ok(got);
                                }
                                return self.fail(PhpError::uncaught(
                                    "Error",
                                    format!(
                                        "Cannot access uninitialized non-nullable property {}::${} by reference",
                                        tdcls.name(),
                                        pn
                                    ),
                                    0,
                                ));
                            }
                            // Nullable uninit slots byref-init to null.
                            let nc = cell(Value::Null);
                            self.last_fresh_cell = Some(Rc::as_ptr(&nc) as usize);
                            o.borrow_mut().props.insert(pn.clone(), nc);
                        }
                    }
                }
                if key.is_none() {
                    // A missing prop on a class with __get is
                    // *overloaded*: cell ops (`[]`, `=&`, `++`) fetch
                    // through __get. `&__get` returns a real cell the
                    // write binds; a plain __get yields a temp — the
                    // write dies with an "Indirect modification"
                    // notice (bug32660, bug37667, bug43201).
                    let cls = o.borrow().class.clone();
                    if let Some(gm) = self.find_method_in(&cls, "__get").map(|(m, _)| m) {
                        let gkey = (Rc::as_ptr(&o) as usize, 0u8, pn.clone());
                        if self.magic_guards.insert(gkey.clone()) {
                            self.last_ret_cell = None;
                            let res = self.method_invoke(
                                o.clone(),
                                "__get",
                                CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                            );
                            self.magic_guards.remove(&gkey);
                            let rv = res?;
                            if gm.decl.by_ref {
                                return Ok(self.last_ret_cell.take().unwrap_or_else(|| cell(rv)));
                            }
                            self.notice(&format!(
                                "Indirect modification of overloaded property {}::${} has no effect",
                                cls.name(),
                                pn
                            ))?;
                            return Ok(cell(rv));
                        }
                        // Re-entrant `&`-fetch of a declared prop the
                        // magic scope can't see is a hard Error
                        // (bug48248 `&__get` returning `$this->priv`).
                        if let Some(e) = self.hidden_decl_error(&o, &pn) {
                            return self.fail(e);
                        }
                        return Ok(cell(Value::Null));
                    }
                }
                let key = key.unwrap_or_else(|| pn.clone());
                let mut ob = o.borrow_mut();
                if !ob.props.contains_key(&key) {
                    if !ob.prop_order.contains(&key) {
                        ob.prop_order.push(key.clone());
                    }
                    let nc = cell(Value::Null);
                    self.last_fresh_cell = Some(Rc::as_ptr(&nc) as usize);
                    ob.props.insert(key.clone(), nc);
                }
                let slot = ob.props.get(&key).unwrap().clone();
                drop(ob);
                if let Some((pd, dcls)) = self.decl_prop(&o, &pn) {
                    if let Some(tys) = &pd.ty {
                        let p = Rc::as_ptr(&slot) as usize;
                        self.typed_slots.insert(
                            p,
                            (
                                slot.clone(),
                                tys.clone(),
                                dcls.name().to_string(),
                                pn.clone(),
                            ),
                        );
                        self.slot_anchor
                            .insert(p, SlotAnchor::Obj(Rc::downgrade(&o), key.clone()));
                    }
                }
                Ok(slot)
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
                    let mut ob = o.borrow_mut();
                    let prune = if let Some(c) = ob.props.remove(&k) {
                        // unset() severs the typed slot: refs bound to it
                        // become plain variables again (typed_properties_090).
                        let ptr = Rc::as_ptr(&c) as usize;
                        if let Some(os) = self.slot_owners.get_mut(&ptr) {
                            os.retain(|(_, _, op, _)| op != &pn);
                            if os.is_empty() {
                                self.slot_owners.remove(&ptr);
                            }
                        }
                        if self
                            .typed_slots
                            .get(&ptr)
                            .map(|(_, _, _, sp)| sp == &pn)
                            .unwrap_or(false)
                        {
                            self.typed_slots.remove(&ptr);
                            self.slot_anchor.remove(&ptr);
                        }
                        Some(ptr)
                    } else {
                        None
                    };
                    ob.unset_props.insert(k);
                    drop(ob);
                    if let Some(ptr) = prune {
                        self.prune_typed_slot(ptr);
                    }
                } else if let Some((pd, dcls)) = self.decl_prop(&o, &pn) {
                    // unset() on an uninitialized declared prop still
                    // marks it unset — reads then route to __get
                    // (typed_properties_009/040).
                    let k = if pd.visibility == crate::ast::Visibility::Private {
                        format!("\0{}\0{}", dcls.name(), pd.name)
                    } else {
                        pd.name.clone()
                    };
                    o.borrow_mut().unset_props.insert(k);
                    // __unset only fires for UNDECLARED props — a
                    // declared one is simply marked uninitialized
                    // (typed_properties_magic_set).
                } else if self.find_method_in(&cls, "__unset").is_some()
                    && self
                        .magic_guards
                        .insert((Rc::as_ptr(&o) as usize, 3u8, pn.clone()))
                {
                    let res = self.method_invoke(
                        o.clone(),
                        "__unset",
                        CallArgs::positional(vec![cell(Value::str(pn.clone()))]),
                    );
                    self.magic_guards
                        .remove(&(Rc::as_ptr(&o) as usize, 3u8, pn.clone()));
                    res?;
                }
                // Real-storage tail: `\0` names error here — magic
                // already dispatched above when __unset existed
                // (bug52484).
                self.check_prop_name(&pn)?;
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
        let mn = Self::nul_trunc(&self.prop_name(name)?);
        let ov = self.eval(obj)?;
        match ov {
            Value::Null if nullsafe => Ok(Value::Null),
            Value::Object(o) => {
                let params = self
                    .find_method_in(&o.borrow().class.clone(), &mn)
                    .map(|m| m.0.decl.params.clone())
                    .unwrap_or_default();
                let argvals = self.arg_cells(args, &params, &format!("{}()", mn), false)?;
                // method_invoke handles builtin (Throwable), __call, undefined.
                self.method_invoke_vis(o.clone(), &mn, argvals)
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
            // Closures expose Closure::__invoke/call/bindTo
            // (named_params/call_user_func's `$closure->__invoke(...)`).
            Value::Callable(c) if mn.eq_ignore_ascii_case("__invoke") => {
                let params = match &c.kind {
                    CallableKind::Closure(d) => d.params.clone(),
                    _ => vec![],
                };
                let argvals = self.arg_cells(args, &params, &format!("{}()", mn), false)?;
                // `$f->__invoke()` runs the internal Closure::__invoke —
                // diagnostics name `Closure::__invoke` and drop the
                // ", called in" suffix (closure_059).
                self.pending_call_alias = Some("Closure::__invoke".into());
                let r = self.call_value(&Value::Callable(c), argvals);
                self.pending_call_alias = None;
                r
            }
            Value::Callable(c) if mn.eq_ignore_ascii_case("call") => {
                // `$fn->call($newThis, ...$args)`: bind with an omitted
                // scope then invoke — previous scope preserved when the
                // new instance is compatible (closure_036/038).
                let argvals = self.arg_cells(args, &[], &format!("{}()", mn), false)?;
                let mut ca = argvals;
                let newthis = ca
                    .cells
                    .first()
                    .map(|c| c.borrow().clone())
                    .unwrap_or(Value::Null);
                if let Value::Object(t) = newthis {
                    // call() scopes to the new instance's class
                    // (unlike bindTo's default 'static' scope).
                    match self.rebind_closure(&c, Some(t.clone()), Some(Value::Object(t)))? {
                        Some(nc) => {
                            ca.cells.remove(0);
                            return self.call_value(&Value::Callable(Rc::new(nc)), ca);
                        }
                        // A failed bind (warned) skips the invocation
                        // (closure_from_callable_rebinding).
                        None => return Ok(Value::Null),
                    }
                }
                self.call_value(&Value::Callable(c), ca)
            }
            Value::Callable(c) if mn.eq_ignore_ascii_case("bindto") => {
                let argvals = self.arg_cells(args, &[], &format!("{}()", mn), false)?;
                let this = argvals.cells.first().map(|c| c.borrow().clone());
                let scope = argvals.cells.get(1).map(|c| c.borrow().clone());
                let new_this = match &this {
                    None | Some(Value::Null) => None,
                    Some(Value::Object(o)) => Some(o.clone()),
                    Some(v) => {
                        let e = self.exception(
                            "TypeError",
                            &format!(
                                "Closure::bindTo(): Argument #1 ($newThis) must be of type ?object, {} given",
                                v.gettype()
                            ),
                        );
                        return Err(self.throw(e));
                    }
                };
                let scope_arg = match &scope {
                    None => None,
                    Some(v @ (Value::Null | Value::Object(_) | Value::Str(_))) => Some(v.clone()),
                    Some(v) => {
                        let e = self.exception(
                            "TypeError",
                            &format!(
                                "Closure::bindTo(): Argument #2 ($newScope) must be of type object|string|null, {} given",
                                v.gettype()
                            ),
                        );
                        return Err(self.throw(e));
                    }
                };
                match self.rebind_closure(&c, new_this, scope_arg)? {
                    Some(nc) => Ok(Value::Callable(Rc::new(nc))),
                    None => Ok(Value::Null),
                }
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
        args: CallArgs,
        dc: Rc<PhpClass>,
    ) -> Result<Value, PhpError> {
        let called = obj.borrow().class.clone();
        self.pending_decl_class = Some(dc.clone());
        self.pending_called_class = Some(called);
        let r = if m.is_static {
            self.invoke_fn(&Rc::new(m.decl.clone()), args, None, Some(dc))
        } else {
            self.invoke_fn(&Rc::new(m.decl.clone()), args, Some(obj), Some(dc))
        };
        self.pending_decl_class = None;
        self.pending_called_class = None;
        r
    }

    /// Dispatch `$obj->name($args)` through `__call(name, args)`.
    /// Zend's $args array for __call/__callStatic: elements that were
    /// references in the caller's send array stay shared (bug50394);
    /// plain zvals are copied so var_dump shows no `&`
    /// (trampoline_closure_named_arguments).
    fn magic_args_array(&self, args: &CallArgs) -> PhpArray {
        let mut arr = PhpArray::new();
        let share = |a: &Cell| {
            if self.ref_cells.contains(&(Rc::as_ptr(a) as usize)) {
                a.clone()
            } else {
                cell(a.borrow().clone())
            }
        };
        for a in &args.cells {
            arr.push_cell(share(a));
        }
        for (n, a, ..) in &args.named {
            arr.set_cell(ArrKey::Str(Rc::from(n.as_str())), share(a));
        }
        arr
    }

    fn call_via_magic(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        m: &Rc<MethodDecl>,
        dc: Rc<PhpClass>,
        name: &str,
        args: CallArgs,
    ) -> Result<Value, PhpError> {
        let arr = self.magic_args_array(&args);
        self.invoke_method(
            obj,
            m,
            CallArgs::positional(vec![
                cell(Value::str(name)),
                cell(Value::Array(Rc::new(RefCell::new(arr)))),
            ]),
            dc,
        )
    }

    /// A private method owned by the calling scope binds statically:
    /// `$this->m()` inside `S::x` always resolves to `S::m`, bypassing
    /// the object's override (zend private methods are not virtual).
    fn scope_private_method(&mut self, name: &str) -> Option<(Rc<MethodDecl>, Rc<PhpClass>)> {
        let scope = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned())
            .or_else(|| self.const_self.clone())?;
        let m = scope.decl.find_method(&name.to_lowercase())?;
        (m.visibility == crate::ast::Visibility::Private).then_some((m, scope))
    }

    /// Userland `Cls::name()` dispatch: gate visibility at the call
    /// site, routing inaccessible methods through __callStatic.
    fn static_invoke_vis(
        &mut self,
        cls: Rc<PhpClass>,
        name: &str,
        args: CallArgs,
        called_class: Option<Rc<PhpClass>>,
        fwd: bool,
    ) -> Result<Value, PhpError> {
        if let Some((m, sc)) = self.scope_private_method(name) {
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
            if !m.is_static && this_obj.is_none() {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Non-static method {}::{}() cannot be called statically",
                        sc.name(),
                        m.decl.name
                    ),
                    0,
                ));
            }
            self.pending_decl_class = Some(sc.clone());
            self.pending_called_class = Some(called_class.unwrap_or(cls.clone()));
            let r = self.invoke_fn(&Rc::new(m.decl.clone()), args, this_obj, Some(sc));
            self.pending_decl_class = None;
            self.pending_called_class = None;
            return r;
        }
        if let Some((m, dc)) = self.find_method_in(&cls, name) {
            if !self.method_access_ok(&m, &dc) {
                // Inaccessible found-method: same magic preference as
                // the missing path — __call first in object context,
                // else __callStatic (bug53826, bug48533).
                let this_obj = self
                    .stack
                    .last()
                    .and_then(|f| f.this_obj.clone())
                    .filter(|o| {
                        let cname = o.borrow().class.name().to_string();
                        self.is_a_str(&cname, cls.name())
                    });
                if let Some(o) = this_obj {
                    if let Some((cm, cdc)) = self.find_method_in(&cls, "__call") {
                        return self.call_via_magic(o, &cm, cdc, name, args);
                    }
                }
                if let Some((cm, cdc)) = self.find_method_in(&cls, "__callstatic") {
                    let arr = self.magic_args_array(&args);
                    self.pending_decl_class = Some(cdc.clone());
                    self.pending_called_class = Some(called_class.unwrap_or(cls.clone()));
                    let r = self.invoke_fn(
                        &Rc::new(cm.decl.clone()),
                        CallArgs::positional(vec![
                            cell(Value::str(name)),
                            cell(Value::Array(Rc::new(RefCell::new(arr)))),
                        ]),
                        None,
                        Some(cdc),
                    );
                    self.pending_decl_class = None;
                    self.pending_called_class = None;
                    return r;
                }
                let e = self.method_vis_error(&m, &dc);
                return self.fail(e);
            }
        }
        self.static_invoke(cls, name, args, called_class, fwd)
    }

    /// Userland `$obj->name()` dispatch: gate visibility at the call
    /// site. Internal invocations (FCC bound scope, engine magic calls)
    /// go through `method_invoke` unchecked.
    fn method_invoke_vis(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        name: &str,
        args: CallArgs,
    ) -> Result<Value, PhpError> {
        let cls = obj.borrow().class.clone();
        // Scope-private binding takes precedence over the object's own
        // method table (private methods are not virtual).
        if let Some((m, sc)) = self.scope_private_method(name) {
            if m.is_abstract {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Cannot call abstract method {}::{}()", sc.name(), name),
                    0,
                ));
            }
            return self.invoke_method(obj, &m, args, sc);
        }
        if let Some((m, dc)) = self.find_method_in(&cls, name) {
            if !self.method_access_ok(&m, &dc) {
                // Inaccessible method routes through __call when
                // defined (zend_std_get_method fallback).
                if let Some((cm, cdc)) = self.find_method_in(&cls, "__call") {
                    return self.call_via_magic(obj, &cm, cdc, name, args);
                }
                let e = self.method_vis_error(&m, &dc);
                return self.fail(e);
            }
        }
        self.method_invoke(obj, name, args)
    }

    /// Method-call visibility against the current calling scope.
    fn method_access_ok(&mut self, m: &MethodDecl, dc: &Rc<PhpClass>) -> bool {
        let scope = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned())
            .or_else(|| self.const_self.clone());
        match m.visibility {
            crate::ast::Visibility::Public => true,
            crate::ast::Visibility::Private => scope
                .as_ref()
                .map(|s| s.name() == dc.name())
                .unwrap_or(false),
            crate::ast::Visibility::Protected => scope
                .as_ref()
                .map(|s| {
                    let proto = self.method_prototype(dc, &m.decl.name.to_lowercase());
                    self.is_a_str(s.name(), dc.name())
                        || self.is_a_str(dc.name(), s.name())
                        // Sibling access: protected `B::m()` callable from
                        // scope A when A is a descendant of the method's
                        // PROTOTYPE owner (the ancestor that first
                        // declared it — gh14009: A and B both extend P,
                        // P first declared `common`).
                        || self.is_a_str(s.name(), &proto)
                })
                .unwrap_or(false),
        }
    }

    /// The ancestor class that first declared `lname` (non-private) —
    /// the prototype owner for protected-member access rules.
    fn method_prototype(&mut self, cls: &Rc<PhpClass>, lname: &str) -> String {
        let mut owner = cls.name().to_string();
        let mut cur = cls.decl.parent.clone();
        while let Some(p) = cur {
            let Some(pc) = self.classes.get(&p.to_lowercase()).cloned() else {
                break;
            };
            if pc.decl.methods.iter().any(|m| {
                m.decl.name.to_lowercase() == lname
                    && !matches!(m.visibility, crate::ast::Visibility::Private)
            }) {
                owner = pc.decl.name.clone();
            }
            cur = pc.decl.parent.clone();
        }
        owner
    }

    /// `Call to private/protected method X::m() from scope Y` — catchable.
    fn method_vis_error(&mut self, m: &MethodDecl, dc: &Rc<PhpClass>) -> PhpError {
        let vis = match m.visibility {
            crate::ast::Visibility::Public => "public",
            crate::ast::Visibility::Protected => "protected",
            crate::ast::Visibility::Private => "private",
        };
        let scope = self
            .stack
            .last()
            .and_then(|f| f.decl_class.as_ref().or(f.scope_class.as_ref()).cloned())
            .or_else(|| self.const_self.clone());
        let from = match &scope {
            Some(s) => format!("scope {}", s.name()),
            None => "global scope".to_string(),
        };
        PhpError::uncaught(
            "Error",
            format!(
                "Call to {} method {}::{}() from {}",
                vis,
                dc.name(),
                m.decl.name,
                from
            ),
            0,
        )
    }

    /// Calls a method by name through an object cell (magic methods, __call).
    pub fn method_invoke(
        &mut self,
        obj: Rc<RefCell<PhpObject>>,
        name: &str,
        args: CallArgs,
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
        if cls.name().starts_with("Reflection") || cls.name().starts_with("reflection") {
            let stub = self
                .find_method_in(&cls, name)
                .map(|(m, _)| m.decl.body.is_empty() && m.decl.line == 0)
                .unwrap_or(false);
            if stub {
                // Native Reflection calls leave a `Cls->m()` frame in
                // uncaught traces (named_params/attributes_named_flags).
                self.call_trace.push(TraceFrame {
                    file: self.diag_file(),
                    line: self.cur_line as u32,
                    function: name.to_string(),
                    class: Some(cls.name().to_string()),
                    ty: "->".into(),
                    args: Vec::new(),
                    named_args: Vec::new(),
                    internal: false,
                });
                let r = self.reflection_method(&obj, name, &args);
                self.call_trace.pop();
                if let Some(v) = r? {
                    return Ok(v);
                }
            }
        }
        // SplDoublyLinkedList / SplStack — list state on \0dll\0items.
        if matches!(
            cls.name().to_lowercase().as_str(),
            "spldoublylinkedlist" | "splstack" | "splqueue"
        ) {
            let dll_method = |o: &Rc<RefCell<PhpObject>>| -> Vec<Value> {
                match o.borrow().props.get("\0dll\0items") {
                    Some(c) => match &*c.borrow() {
                        Value::Array(a) => {
                            a.borrow().iter().map(|(_, c)| c.borrow().clone()).collect()
                        }
                        _ => Vec::new(),
                    },
                    None => Vec::new(),
                }
            };
            match name.to_lowercase().as_str() {
                "count" => return Ok(Value::Int(dll_method(&obj).len() as i64)),
                "isempty" => return Ok(Value::Bool(dll_method(&obj).is_empty())),
                "top" => return Ok(dll_method(&obj).first().cloned().unwrap_or(Value::Null)),
                "bottom" => return Ok(dll_method(&obj).last().cloned().unwrap_or(Value::Null)),
                "push" => {
                    let v = args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    let mut items = dll_method(&obj);
                    items.push(v);
                    let mut a = PhpArray::new();
                    for (i, iv) in items.into_iter().enumerate() {
                        a.set(ArrKey::Int(i as i64), iv);
                    }
                    obj.borrow_mut().props.insert(
                        "\0dll\0items".into(),
                        cell(Value::Array(Rc::new(RefCell::new(a)))),
                    );
                    return Ok(Value::Null);
                }
                "pop" | "shift" => {
                    let mut items = dll_method(&obj);
                    let r = if name.eq_ignore_ascii_case("pop") {
                        items.pop()
                    } else {
                        if items.is_empty() {
                            None
                        } else {
                            Some(items.remove(0))
                        }
                    };
                    let mut a = PhpArray::new();
                    for (i, iv) in items.into_iter().enumerate() {
                        a.set(ArrKey::Int(i as i64), iv);
                    }
                    obj.borrow_mut().props.insert(
                        "\0dll\0items".into(),
                        cell(Value::Array(Rc::new(RefCell::new(a)))),
                    );
                    return Ok(r.unwrap_or(Value::Null));
                }
                _ => {}
            }
        }
        // DateTime: minimal native clock — the ctor stores the parsed
        // timestamp so getTimestamp/diff can read it back
        // (closure_call_internal).
        if matches!(
            cls.name().to_lowercase().as_str(),
            "datetime" | "datetimeimmutable"
        ) {
            match name.to_lowercase().as_str() {
                "__construct" => {
                    let ts = match args
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null)
                    {
                        Value::Str(s) => {
                            let s = crate::value::lossy(&s);
                            match s.strip_prefix('@') {
                                Some(num) => num.trim().parse::<i64>().unwrap_or(0),
                                // Relative formats beyond '@N' are
                                // stubs — treat as epoch for now.
                                None => 0,
                            }
                        }
                        _ => 0,
                    };
                    obj.borrow_mut()
                        .props
                        .insert("\0dt\0ts".into(), cell(Value::Int(ts)));
                    return Ok(Value::Null);
                }
                "gettimestamp" => {
                    return Ok(obj
                        .borrow()
                        .props
                        .get("\0dt\0ts")
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Int(0)));
                }
                _ => {}
            }
        }
        // ArrayIterator: native iteration state on the object internal.
        if cls.name().eq_ignore_ascii_case("arrayiterator") {
            if let Some(v) = self.array_iter_method(&obj, name, &args)? {
                return Ok(v);
            }
        }
        // Generator: same pattern — native iteration state.
        if cls.name().eq_ignore_ascii_case("generator") {
            if let Some(v) = self.generator_method(&obj, name, &args)? {
                return Ok(v);
            }
        }
        // SplFileInfo / DirectoryIterator: SPL filesystem objects.
        if matches!(
            cls.name().to_lowercase().as_str(),
            "splfileinfo" | "directoryiterator" | "filesystemiterator"
        ) {
            if let Some(v) = self.spl_method(&obj, name, &args)? {
                return Ok(v);
            }
        }
        // PDO / PDOStatement: sqlite-backed storage surface (#15 spike).
        if cls.name().eq_ignore_ascii_case("pdo") {
            if let Some(v) = crate::pdo::pdo_method(self, &obj, name, &args)? {
                return Ok(v);
            }
        }
        if cls.name().eq_ignore_ascii_case("pdostatement") {
            if let Some(v) = crate::pdo::pdostmt_method(self, &obj, name, &args)? {
                return Ok(v);
            }
        }
        match self.find_method_in(&cls, name) {
            Some((m, dc)) => {
                if m.is_abstract {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot call abstract method {}::{}()", dc.name(), name),
                        0,
                    ));
                }
                self.invoke_method(obj, &m, args, dc)
            }
            None => {
                if let Some((m, dc)) = self.find_method_in(&cls, "__call") {
                    return self.call_via_magic(obj, &m, dc, name, args);
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
                _ => Some(Value::str(self.diag_file())),
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
                    for (n, av) in &fr.named_args {
                        a.set(ArrKey::Str(n.clone().into()), av.borrow().clone());
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
                let (file, line, trace, full) = match &ob.internal {
                    Some(ObjectInternal::Exception {
                        file,
                        line,
                        trace,
                        frames,
                        full_msg,
                        ..
                    }) => {
                        let t = if !trace.is_empty() {
                            trace.clone()
                        } else if !frames.is_empty() {
                            format_trace(frames)
                        } else {
                            "#0 {main}".into()
                        };
                        (file.clone(), *line, t, full_msg.clone())
                    }
                    _ => (
                        self.diag_file(),
                        self.cur_line as u32,
                        "#0 {main}".into(),
                        String::new(),
                    ),
                };
                let msg = if full.is_empty() { msg } else { full };
                Some(Value::str(format!(
                    "{}: {} in {}:{}\nStack trace:\n{}",
                    ob.class.name(),
                    msg,
                    file,
                    line,
                    trace
                )))
            }
            "__construct" => {
                // Builtin ctor: props from args message/code.
                drop(ob);
                let mut ob = obj.borrow_mut();
                let msg = _args
                    .first()
                    .map(|c| c.borrow().to_php_string())
                    .unwrap_or_default();
                let code = _args.get(1).map(|c| c.borrow().to_int()).unwrap_or(0);
                ob.props.insert("message".into(), cell(Value::str(msg)));
                ob.props.insert("code".into(), cell(Value::Int(code)));
                if !ob.prop_order.contains(&"message".into()) {
                    ob.prop_order.push("message".into());
                    ob.prop_order.push("code".into());
                }
                Some(Value::Null)
            }
            _ => None,
        }
    }

    /// `X::` member access where X may be a trait: traits resolve to a
    /// synthesized class holding their statics; direct trait member
    /// access is deprecated (direct_static_member_access). Returns the
    /// resolved class plus the trait's display name when it is one.
    fn member_class_of(
        &mut self,
        class: &Expr,
    ) -> Result<(Rc<PhpClass>, Option<String>), PhpError> {
        let name = self.class_name_of(class)?;
        if let Some(td) = self.traits.get(&name.to_lowercase()).cloned() {
            let key = td.name.to_lowercase();
            let cls = self
                .trait_statics
                .entry(key)
                .or_insert_with(|| {
                    Rc::new(PhpClass {
                        decl: td.clone(),
                        statics: RefCell::new(HashMap::new()),
                        statics_init: RefCell::new(false),
                    })
                })
                .clone();
            return Ok((cls, Some(td.name.clone())));
        }
        Ok((self.class_of(class)?, None))
    }

    fn static_prop_read(&mut self, class: &Expr, name: &PropName) -> Result<Value, PhpError> {
        let name = self.prop_name(name)?;
        let (cls, tname) = self.member_class_of(class)?;
        if let Some(t) = tname {
            self.deprecated(&format!(
                "Accessing static trait property {}::${} is deprecated, it should only be accessed on a class using the trait",
                t, name
            ))?;
        }
        self.statics_init(&cls);
        let v = cls.statics.borrow().get(&name).map(|c| c.borrow().clone());
        match v {
            Some(v) => Ok(v),
            None => {
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, &name) {
                    if pd.ty.is_some() {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            format!(
                                "Typed static property {}::${} must not be accessed before initialization",
                                dcls.name(),
                                name
                            ),
                            0,
                        ));
                    }
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Access to undeclared static property {}::${}",
                        cls.name(),
                        name
                    ),
                    0,
                ))
            }
        }
    }

    fn static_prop_cell(&mut self, class: &Expr, name: &PropName) -> Result<Cell, PhpError> {
        let name = self.prop_name(name)?;
        self.static_prop_named(class, &name)
    }

    fn static_prop_named(&mut self, class: &Expr, name: &str) -> Result<Cell, PhpError> {
        let (cls, tname) = self.member_class_of(class)?;
        if let Some(t) = tname {
            self.deprecated(&format!(
                "Accessing static trait property {}::${} is deprecated, it should only be accessed on a class using the trait",
                t, name
            ))?;
        }
        self.statics_init(&cls);
        let found = cls.statics.borrow().get(name).cloned();
        match found {
            Some(c) => {
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, name) {
                    if let Some(tys) = &pd.ty {
                        let p = Rc::as_ptr(&c) as usize;
                        self.typed_slots.insert(
                            p,
                            (
                                c.clone(),
                                tys.clone(),
                                dcls.name().to_string(),
                                name.to_string(),
                            ),
                        );
                        self.slot_anchor.insert(
                            p,
                            SlotAnchor::Statics(dcls.name().to_string(), name.to_string()),
                        );
                    }
                }
                Ok(c)
            }
            None => {
                // Write/cell path materializes a declared static; an
                // undeclared one is an Error.
                if let Some((pd, dcls)) = self.find_static_prop_decl(&cls, name) {
                    let c = cell(Value::Null);
                    cls.statics.borrow_mut().insert(name.to_string(), c.clone());
                    if let Some(tys) = &pd.ty {
                        self.last_fresh_cell = Some(Rc::as_ptr(&c) as usize);
                        let p = Rc::as_ptr(&c) as usize;
                        self.typed_slots.insert(
                            p,
                            (
                                c.clone(),
                                tys.clone(),
                                dcls.name().to_string(),
                                name.to_string(),
                            ),
                        );
                        self.slot_anchor.insert(
                            p,
                            SlotAnchor::Statics(dcls.name().to_string(), name.to_string()),
                        );
                    }
                    return Ok(c);
                }
                self.fail(PhpError::uncaught(
                    "Error",
                    format!(
                        "Access to undeclared static property {}::${}",
                        cls.name(),
                        name
                    ),
                    0,
                ))
            }
        }
    }

    /// Lazily initialize static prop defaults.
    fn statics_init(&mut self, cls: &Rc<PhpClass>) {
        if *cls.statics_init.borrow() {
            return;
        }
        *cls.statics_init.borrow_mut() = true;
        // Inherited statics: PHP snapshots the parent's static-prop
        // values into the child's table at link time, so `static::$p`
        // on the child resolves parent defaults.
        if let Some(pname) = &cls.decl.parent {
            if let Some(p) = self.classes.get(&pname.to_lowercase()).cloned() {
                self.statics_init(&p);
                for (k, v) in p.statics.borrow().iter() {
                    cls.statics
                        .borrow_mut()
                        .entry(k.clone())
                        .or_insert_with(|| cell(v.borrow().clone()));
                }
            }
        }
        for p in &cls.decl.props {
            if !p.is_static {
                continue;
            }
            let mut default = match &p.default {
                Some(d) => {
                    let old = self.const_self.replace(cls.clone());
                    self.class_const_ctx += 1;
                    let v = self
                        .eval_decl_const(d, &cls.decl.file)
                        .unwrap_or(Value::Null);
                    self.class_const_ctx -= 1;
                    self.const_self = old;
                    v
                }
                None => Value::Null,
            };
            if p.ty.is_some() {
                if let Ok(d) = self.prop_typed_write_check(p, cls, default.clone()) {
                    default = d;
                }
            }
            // A typed static without a default stays *uninitialized*:
            // reads raise the uninit Error until first assignment.
            if p.ty.is_some() && p.default.is_none() {
                continue;
            }
            cls.statics
                .borrow_mut()
                .insert(p.name.clone(), cell(default));
        }
    }

    fn static_call(&mut self, class: &Expr, name: &str, args: &[Expr]) -> Result<Value, PhpError> {
        let (cls, tname) = self.member_class_of(class)?;
        if let Some(t) = tname {
            self.deprecated(&format!(
                "Calling static trait method {}::{} is deprecated, it should only be called on a class using the trait",
                t, name
            ))?;
        }
        let params = self
            .find_method_in(&cls, name)
            .map(|m| m.0.decl.params.clone())
            .unwrap_or_default();
        let argvals = self.arg_cells(args, &params, &format!("{}()", name), false)?;
        // Only a syntactic class ref (self/parent/static/Foo) is a
        // forwarding call; `$x::m()` is not (bug48533).
        let fwd = matches!(class, Expr::Const(_) | Expr::Str(_) | Expr::AnonClass(_));
        // Forwarding calls (self::/parent::/static::) preserve the
        // current late-static-binding class instead of resetting it to
        // the resolved target: `parent::__construct()` on a subclass
        // still sees the subclass via `static::` inside the parent ctor.
        let called = match class {
            Expr::Const(n) | Expr::Str(n)
                if matches!(
                    n.trim_start_matches('\\').to_lowercase().as_str(),
                    "self" | "parent" | "static"
                ) =>
            {
                self.stack.last().and_then(|f| {
                    f.called_class
                        .clone()
                        .or_else(|| f.this_obj.as_ref().map(|o| o.borrow().class.clone()))
                })
            }
            _ => None,
        };
        self.static_invoke_vis(cls, name, argvals, called, fwd)
    }

    fn static_invoke(
        &mut self,
        cls: Rc<PhpClass>,
        name: &str,
        args: CallArgs,
        called_class: Option<Rc<PhpClass>>,
        fwd: bool,
    ) -> Result<Value, PhpError> {
        // Closure::{bind,fromCallable}: native callable rebinding.
        if cls.name().eq_ignore_ascii_case("closure") {
            let lname = name.to_lowercase();
            match lname.as_str() {
                "getcurrent" => {
                    // Current frame must itself be executing a closure
                    // body (closure_get_current).
                    return match self.stack.last().and_then(|f| f.closure_rc.clone()) {
                        Some(rc) => Ok(Value::Callable(rc)),
                        None => self.fail(PhpError::uncaught(
                            "Error",
                            "Current function is not a closure",
                            0,
                        )),
                    };
                }
                "bind" | "bindto" => {
                    // `Closure::bind($closure, $newThis, $newScope = ?)`.
                    let c = args.cells.first().map(|c| c.borrow().clone());
                    let this = args.cells.get(1).map(|c| c.borrow().clone());
                    let scope = args.cells.get(2).map(|c| c.borrow().clone());
                    let Value::Callable(cb) = c.unwrap_or(Value::Null) else {
                        return Ok(Value::Null);
                    };
                    let new_this = match &this {
                        None | Some(Value::Null) => None,
                        Some(Value::Object(o)) => Some(o.clone()),
                        Some(v) => {
                            let e = self.exception(
                                "TypeError",
                                &format!(
                                    "Closure::bind(): Argument #2 ($newThis) must be of type ?object, {} given",
                                    v.gettype()
                                ),
                            );
                            return Err(self.throw(e));
                        }
                    };
                    let scope_arg = match &scope {
                        None => None,
                        Some(v @ (Value::Null | Value::Object(_) | Value::Str(_))) => {
                            Some(v.clone())
                        }
                        Some(v) => {
                            let e = self.exception(
                                "TypeError",
                                &format!(
                                    "Closure::bind(): Argument #3 ($newScope) must be of type object|string|null, {} given",
                                    v.gettype()
                                ),
                            );
                            return Err(self.throw(e));
                        }
                    };
                    match self.rebind_closure(&cb, new_this, scope_arg)? {
                        Some(nc) => return Ok(Value::Callable(Rc::new(nc))),
                        None => return Ok(Value::Null),
                    }
                }
                "fromcallable" => {
                    let v = args
                        .cells
                        .first()
                        .map(|c| c.borrow().clone())
                        .unwrap_or(Value::Null);
                    match self.callable_to_closure(&v) {
                        Ok(c) => return Ok(c),
                        Err(fail) => {
                            // Zend appends the reason:
                            // "Failed to create closure from callable:
                            // non-static method A::m() cannot be called
                            // statically" (from_callable_non_static).
                            let reason = if !fail.message.is_empty() {
                                fail.message
                                    .replacen("Non-static method", "non-static method", 1)
                            } else {
                                self.pending_exception
                                    .as_ref()
                                    .and_then(|e| match e {
                                        Value::Object(o) => o
                                            .borrow()
                                            .props
                                            .get("message")
                                            .map(|c| c.borrow().to_php_string()),
                                        _ => None,
                                    })
                                    .unwrap_or_default()
                            };
                            let e = self.exception(
                                "TypeError",
                                &format!("Failed to create closure from callable: {}", reason),
                            );
                            self.pending_exception = Some(e);
                            return Err(PhpError {
                                trace: None,
                                thrown_line: None,
                                display_msg: None,
                                kind: ErrorKind::Throw,
                                message: "fromCallable".into(),
                                line: 0,
                            });
                        }
                    }
                }
                _ => {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Call to undefined method Closure::{}()", name),
                        0,
                    ));
                }
            }
        }
        // Throwable methods are instance-only; look up incl. parents.
        match self.find_method_in(&cls, name) {
            Some((m, dc)) => {
                if m.is_abstract {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!("Cannot call abstract method {}::{}()", dc.name(), name),
                        0,
                    ));
                }
                // Forwarding call: a non-static method invoked
                // statically still receives $this when the caller's
                // $this is an instance of the callee's class
                // (bug21961) — but only via a syntactic class ref;
                // `$obj::m()` is not a forwarding call (bug48533).
                let this_obj = if m.is_static || !fwd {
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
                if !m.is_static && this_obj.is_none() {
                    return self.fail(PhpError::uncaught(
                        "Error",
                        format!(
                            "Non-static method {}::{}() cannot be called statically",
                            dc.name(),
                            m.decl.name
                        ),
                        0,
                    ));
                }
                self.pending_decl_class = Some(dc.clone());
                self.pending_called_class = Some(called_class.clone().unwrap_or(cls.clone()));
                let r = self.invoke_fn(&Rc::new(m.decl.clone()), args, this_obj, Some(dc));
                self.pending_decl_class = None;
                self.pending_called_class = None;
                r
            }
            None => {
                // A missing __construct never reaches magic —
                // `Foo::__construct()` is "Cannot call constructor"
                // (call_static_006). __destruct etc. dispatch normally.
                if name.eq_ignore_ascii_case("__construct") {
                    return self.fail(PhpError::uncaught("Error", "Cannot call constructor", 0));
                }
                let mut arr = PhpArray::new();
                for a in &args.cells {
                    arr.push(a.borrow().clone());
                }
                for (n, a, ..) in &args.named {
                    arr.set(ArrKey::Str(Rc::from(n.as_str())), a.borrow().clone());
                }
                let magic_args = CallArgs::positional(vec![
                    cell(Value::str(name)),
                    cell(Value::Array(Rc::new(RefCell::new(arr)))),
                ]);
                // Object context prefers __call over __callStatic when
                // the caller's $this is an instance of the callee —
                // `self::x()` inside a method acts as an instance call
                // (call_static_003/007, bug45186).
                let this_obj = self
                    .stack
                    .last()
                    .and_then(|f| f.this_obj.clone())
                    .filter(|o| {
                        let cname = o.borrow().class.name().to_string();
                        self.is_a_str(&cname, cls.name())
                    });
                if let Some(o) = this_obj {
                    if self.find_method_in(&cls, "__call").is_some() {
                        return self.method_invoke(o, "__call", magic_args);
                    }
                }
                if let Some((m, dc)) = self.find_method_in(&cls, "__callstatic") {
                    self.pending_decl_class = Some(dc.clone());
                    self.pending_called_class = Some(called_class.clone().unwrap_or(cls.clone()));
                    let r = self.invoke_fn(&Rc::new(m.decl.clone()), magic_args, None, Some(dc));
                    self.pending_decl_class = None;
                    self.pending_called_class = None;
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
                    // A private decl's real slot is the mangled
                    // `\0C\0name` key — it can never own a PLAIN-name
                    // slot, which is then a dynamic prop instead
                    // (bug60536_001).
                    if p.visibility == crate::ast::Visibility::Private {
                        continue;
                    }
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

    /// `Error` when `pn` resolves to a DECLARED prop the current scope
    /// can't see — zend throws `Cannot access private/protected
    /// property` rather than creating a dynamic prop or warning
    /// (bug38461/bug48248). None for undeclared or visible names.
    fn hidden_decl_error(&mut self, o: &Rc<RefCell<PhpObject>>, pn: &str) -> Option<PhpError> {
        // Zend checks the object's own class table for a same-named
        // declaration: an inherited public/protected decl hides the
        // name (Cannot-access when invisible), and a PRIVATE decl
        // hides it only when the object's own class declares it —
        // ancestor-private names still admit a dynamic prop
        // (bug38461/bug48248 vs bug60536_001).
        let (pd, dcls) = self.decl_prop(o, pn).or_else(|| {
            let ob = o.borrow();
            ob.class
                .decl
                .props
                .iter()
                .find(|p| {
                    p.name == pn && !p.is_static && p.visibility == crate::ast::Visibility::Private
                })
                .map(|p| (p.clone(), ob.class.clone()))
        })?;
        let word = match pd.visibility {
            crate::ast::Visibility::Private => "private",
            crate::ast::Visibility::Protected => "protected",
            crate::ast::Visibility::Public => return None,
        };
        let scope = self.stack.last().and_then(|f| {
            f.decl_class
                .as_ref()
                .or(f.scope_class.as_ref())
                .map(|s| s.decl.name.clone())
        });
        let dn = dcls.name().to_string();
        let allows = match (pd.visibility, scope.as_deref()) {
            (crate::ast::Visibility::Private, Some(s)) => s == dn,
            (crate::ast::Visibility::Protected, Some(s)) => {
                self.is_a_str(s, &dn) || self.is_a_str(&dn, s)
            }
            _ => false,
        };
        if allows {
            return None;
        }
        Some(PhpError::uncaught(
            "Error",
            format!("Cannot access {} property {}::${}", word, dn, pn),
            0,
        ))
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
        let mut out = Vec::new();
        for n in &self.decl_order {
            let name = match kind {
                crate::ast::ClassKind::Trait => self.traits.get(n).map(|d| d.name.clone()),
                crate::ast::ClassKind::Interface => self.interfaces.get(n).map(|d| d.name.clone()),
                _ => self
                    .classes
                    .get(n)
                    .filter(|c| c.decl.kind == kind)
                    .map(|c| c.name().to_string()),
            };
            if let Some(nm) = name {
                out.push(nm);
            }
        }
        for (k, a) in &self.decl_aliases {
            if *k == kind {
                out.push(a.clone());
            }
        }
        out
    }

    /// class_alias($name, $alias): alias entries resolve like the
    /// original (classes, interfaces and traits alike).
    pub fn class_alias(&mut self, name: &str, alias: &str) -> Result<bool, PhpError> {
        // `class_alias($cls, 'int')` — the alias may not be a reserved
        // scalar type name (scalar_reserved*_class_alias).
        let short = alias
            .trim_start_matches('\\')
            .rsplit('\\')
            .next()
            .unwrap_or(alias)
            .to_lowercase();
        const RESERVED_ALS: &[&str] = &[
            "int", "float", "string", "bool", "void", "iterable", "object", "mixed", "never",
            "null", "false", "true",
        ];
        if RESERVED_ALS.contains(&short.as_str()) {
            return Err(PhpError::fatal(
                format!(
                    "Cannot use \"{}\" as a class alias as it is reserved",
                    short
                ),
                self.cur_line,
            ));
        }
        let alias_l = alias.trim_start_matches('\\').to_lowercase();
        let key = name.trim_start_matches('\\').to_lowercase();
        if let Some(c) = self.classes.get(&key).cloned() {
            self.classes.insert(alias_l.clone(), c);
            self.decl_aliases
                .push((crate::ast::ClassKind::Class, alias_l));
            return Ok(true);
        }
        if let Some(i) = self.interfaces.get(&key).cloned() {
            self.interfaces.insert(alias_l.clone(), i);
            self.decl_aliases
                .push((crate::ast::ClassKind::Interface, alias_l));
            return Ok(true);
        }
        if let Some(t) = self.traits.get(&key).cloned() {
            self.traits.insert(alias_l.clone(), t);
            self.decl_aliases
                .push((crate::ast::ClassKind::Trait, alias_l));
            return Ok(true);
        }
        self.warn(&format!("Class \"{}\" not found", name))?;
        Ok(false)
    }

    /// Does this object's class implement `iname` (transitively)?
    /// Used by serialize() for the Serializable C:-format branch.
    pub fn obj_implements(&mut self, o: &Rc<RefCell<PhpObject>>, iname: &str) -> bool {
        self.is_a(&o.borrow().class, iname)
    }

    /// Does `d` (a class decl) implement interface `iname`, directly or
    /// through the implements chain of interfaces it names?
    fn implements_iface(&self, d: &ClassDecl, iname: &str) -> bool {
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<String> = d.implements.clone();
        while let Some(i) = stack.pop() {
            let l = i.trim_start_matches('\\').to_lowercase();
            if l == iname {
                return true;
            }
            if !seen.insert(l.clone()) {
                continue;
            }
            if let Some(id) = self.interfaces.get(&l) {
                stack.extend(id.implements.iter().cloned());
            }
        }
        false
    }

    /// Locate a class/interface/trait const decl by name — walks the
    /// class chain, used traits' merged consts and implemented
    /// interfaces (reflection APIs see trait consts on both sides).
    fn find_const_decl(
        &mut self,
        cname: &str,
        name: &str,
    ) -> Option<(crate::ast::ConstDecl, String)> {
        let key = cname.trim_start_matches('\\').to_lowercase();
        if let Some(td) = self.traits.get(&key) {
            for cd in &td.consts {
                if cd.name == name {
                    return Some((cd.clone(), td.file.clone()));
                }
            }
            return None;
        }
        if let Some(id) = self.interfaces.get(&key) {
            for cd in &id.consts {
                if cd.name == name {
                    return Some((cd.clone(), id.file.clone()));
                }
            }
            return None;
        }
        let mut cur = self.classes.get(&key).cloned();
        let mut ifaces: Vec<String> = Vec::new();
        while let Some(c) = cur {
            for cd in &c.decl.consts {
                if cd.name == name {
                    return Some((cd.clone(), c.decl.file.clone()));
                }
            }
            ifaces.extend(c.decl.implements.iter().cloned());
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        for iname in ifaces {
            if let Some(id) = self.interfaces.get(&iname.to_lowercase()) {
                for cd in &id.consts {
                    if cd.name == name {
                        return Some((cd.clone(), id.file.clone()));
                    }
                }
            }
        }
        None
    }

    /// All (name, (decl, file)) consts visible on `cname` — class chain
    /// + interfaces; trait members appear via the class's merged decl.
    fn all_const_decls(&mut self, cname: &str) -> Vec<(String, (crate::ast::ConstDecl, String))> {
        let mut out: Vec<(String, (crate::ast::ConstDecl, String))> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let key = cname.trim_start_matches('\\').to_lowercase();
        if let Some(td) = self.traits.get(&key).cloned() {
            for cd in &td.consts {
                if seen.insert(cd.name.clone()) {
                    out.push((cd.name.clone(), (cd.clone(), td.file.clone())));
                }
            }
            return out;
        }
        if let Some(id) = self.interfaces.get(&key).cloned() {
            for cd in &id.consts {
                if seen.insert(cd.name.clone()) {
                    out.push((cd.name.clone(), (cd.clone(), id.file.clone())));
                }
            }
            return out;
        }
        let mut cur = self.classes.get(&key).cloned();
        let mut ifaces: Vec<String> = Vec::new();
        while let Some(c) = cur {
            for cd in &c.decl.consts {
                if seen.insert(cd.name.clone()) {
                    out.push((cd.name.clone(), (cd.clone(), c.decl.file.clone())));
                }
            }
            ifaces.extend(c.decl.implements.iter().cloned());
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        for iname in ifaces {
            if let Some(id) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                for cd in &id.consts {
                    if seen.insert(cd.name.clone()) {
                        out.push((cd.name.clone(), (cd.clone(), id.file.clone())));
                    }
                }
            }
        }
        out
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
        // Type-member checks during signature verification autoload the
        // compared classes — Zend verifies covariance with the real
        // hierarchy, so `C::m(): D` inside an autoloaded class sees `D`
        // even when it is declared later (abstract_method_9). A name
        // mid-link counts as resolvable without loading
        // (infinite_recursion — `class C extends Z implements C`).
        if !self.classes.contains_key(&a.to_lowercase())
            && !self.linking.iter().any(|c| c.name.eq_ignore_ascii_case(a))
        {
            // The check creates a delayed variance dependency either
            // way; a name mid-registration is still unlinked — the
            // decl answers it by name, but the obligation is recorded
            // so the check re-verifies after it links
            // (variance/loading_exception*).
            self.note_variance_obligation();
            if !self
                .declaring
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(a))
            {
                if let Err(e) = self.run_autoload(a) {
                    // An autoload failure is a compile-time fatal everywhere
                    // else; inside a signature check a missing class just
                    // means "not a subtype" — but the fatal itself must
                    // still reach the checking context (error3 cascade).
                    self.sig_fatal.get_or_insert(e);
                    self.pending_exception = None;
                }
            }
        }
        // Aliases canonicalize through the class table: `Bar` (an
        // alias of Foo) compares as `Foo` (typed_properties_084).
        let canon_b = self
            .classes
            .get(&b.trim_start_matches('\\').to_lowercase())
            .map(|c| c.name().to_string());
        let b = canon_b.as_deref().unwrap_or(b);
        match self.classes.get(&a.to_lowercase()).cloned() {
            Some(c) => self.is_a(&c, b),
            None => self.is_a_unresolved(a, b, 0),
        }
    }

    /// Ancestry check by NAME for a class not (yet) in `self.classes`:
    /// walks parent/implements names through `classes` and `linking`.
    fn is_a_unresolved(&mut self, a: &str, b: &str, depth: u8) -> bool {
        if a.trim_start_matches('\\').eq_ignore_ascii_case(b) {
            return true;
        }
        if depth > 16 {
            return false;
        }
        let d = self
            .classes
            .get(&a.to_lowercase())
            .map(|c| c.decl.clone())
            .or_else(|| {
                self.linking
                    .iter()
                    .rev()
                    .find(|d| d.name.eq_ignore_ascii_case(a))
                    .cloned()
            })
            .or_else(|| {
                self.declaring
                    .iter()
                    .rev()
                    .find(|d| d.name.eq_ignore_ascii_case(a))
                    .cloned()
            })
            .or_else(|| {
                self.interfaces
                    .get(&a.to_lowercase())
                    .cloned()
                    .or_else(|| self.traits.get(&a.to_lowercase()).cloned())
            });
        let Some(d) = d else {
            return false;
        };
        if let Some(p) = &d.parent {
            if self.is_a_unresolved(p, b, depth + 1) {
                return true;
            }
        }
        for i in &d.implements {
            if self.is_a_unresolved(i, b, depth + 1) {
                return true;
            }
        }
        false
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
        if !self.classes.contains_key(&name.to_lowercase()) {
            self.run_autoload(&name)?;
        }
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
        self.class_const_named(&cname, name)
    }

    /// is_callable(['Cls'|$obj, 'm']): 'parent'/'self'/'static' names
    /// resolve against the caller's scope; "parent"/"self" string
    /// callables are deprecated once they resolve (bug76773-deprecated).
    pub fn is_callable_arr(&mut self, first: &Value, mname: &str) -> bool {
        let cls = match first {
            Value::Callable(_) => return mname.eq_ignore_ascii_case("__invoke"),
            Value::Object(o) => Some(o.borrow().class.clone()),
            Value::Str(n) => {
                let n = crate::value::lossy(n);
                let ln = n.trim_start_matches('\\').to_lowercase();
                match ln.as_str() {
                    "parent" | "self" | "static" => {
                        let Some(scope) = self.caller_scope_name() else {
                            return false;
                        };
                        let Some(sc) = self.classes.get(&scope.to_lowercase()).cloned() else {
                            return false;
                        };
                        let target = match ln.as_str() {
                            "parent" => sc
                                .decl
                                .parent
                                .as_ref()
                                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned()),
                            "static" => self
                                .stack
                                .last()
                                .and_then(|f| f.called_class.clone())
                                .or(Some(sc)),
                            _ => Some(sc),
                        };
                        if let Some(c) = &target {
                            if self.find_method_in(c, mname).is_some() {
                                self.deprecated(&format!(
                                    "Use of \"{}\" in callables is deprecated",
                                    ln
                                ))
                                .ok();
                                return true;
                            }
                        }
                        return false;
                    }
                    _ => {
                        if !self.classes.contains_key(&ln) {
                            let _ = self.run_autoload(&n);
                            self.pending_exception = None;
                        }
                        self.classes.get(&ln).cloned()
                    }
                }
            }
            _ => None,
        };
        match cls {
            Some(c) => self.find_method_in(&c, mname).is_some(),
            None => false,
        }
    }

    /// `Cls::CONST` lookup by plain class-name string (shared with the
    /// constant() builtin so `constant('T::X')` honours the trait-const
    /// rule — constant_018).
    pub fn class_const_named(&mut self, cname: &str, name: &str) -> Result<Value, PhpError> {
        let cname = cname.to_string();
        if name == "class" {
            return Ok(Value::str(cname));
        }
        // `X::CONST` on an unloaded class runs the autoloaders (real
        // psr-4 code hits this constantly — e.g. `Language::ENGLISH`).
        let ckey = self
            .resolve_class(&cname)
            .unwrap_or_else(|| cname.clone())
            .to_lowercase();
        if let Some(td) = self.traits.get(&ckey).cloned() {
            return self.fail(PhpError::uncaught(
                "Error",
                format!(
                    "Cannot access trait constant {}::{} directly",
                    td.name, name
                ),
                0,
            ));
        }
        if let Some(iface) = self.interfaces.get(&ckey).cloned() {
            // Const on an interface (e.g. `FastRoute\Dispatcher::FOUND`):
            // walk it and its extended interfaces.
            let mut seen = std::collections::HashSet::new();
            let mut stack = vec![iface];
            while let Some(c) = stack.pop() {
                if !seen.insert(c.name.to_lowercase()) {
                    continue;
                }
                for cd in &c.consts {
                    if cd.name == name {
                        self.class_const_ctx += 1;
                        let r = self.eval_decl_const(&cd.value, &c.file);
                        self.class_const_ctx -= 1;
                        return match r {
                            Ok(v) => self.const_apply_ty(cd, &c.name, v),
                            Err(e) => Err(e),
                        };
                    }
                }
                for i in &c.implements {
                    if let Some(f) = self.interfaces.get(&i.to_lowercase()).cloned() {
                        stack.push(f);
                    }
                }
                if let Some(p) = &c.parent {
                    if let Some(f) = self.interfaces.get(&p.to_lowercase()).cloned() {
                        stack.push(f);
                    }
                }
            }
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Undefined constant {}", name),
                0,
            ));
        }
        let cls = match self.classes.get(&ckey) {
            Some(c) => c.clone(),
            None => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Class \"{}\" not found", cname),
                    0,
                ))
            }
        };
        // Walk chain for the const (class first, then implemented
        // interfaces transitively — interface consts are inherited).
        let mut cur = Some(cls.clone());
        let mut ifaces: Vec<String> = Vec::new();
        while let Some(c) = cur {
            for cd in &c.decl.consts {
                if cd.name == name {
                    if cd.enum_case {
                        return self.enum_case_value(&c.decl.name, &cd.name, cd);
                    }
                    let old = self.const_self.replace(c.clone());
                    self.class_const_ctx += 1;
                    let r = self.eval_decl_const(&cd.value, &c.decl.file);
                    self.class_const_ctx -= 1;
                    self.const_self = old;
                    return match r {
                        Ok(v) => self.const_apply_ty(cd, &c.decl.name, v),
                        Err(e) => Err(e),
                    };
                }
            }
            ifaces.extend(c.decl.implements.iter().cloned());
            cur = c
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        let mut seen = std::collections::HashSet::new();
        let mut queue: Vec<String> = ifaces;
        while let Some(iname) = queue.pop() {
            if !seen.insert(iname.to_lowercase()) {
                continue;
            }
            if let Some(c) = self.interfaces.get(&iname.to_lowercase()).cloned() {
                for cd in &c.consts {
                    if cd.name == name {
                        let old = self.const_self.replace(cls.clone());
                        self.class_const_ctx += 1;
                        let r = self
                            .eval_decl_const(&cd.value, &c.file)
                            .and_then(|v| self.const_apply_ty(cd, &c.name, v));
                        self.class_const_ctx -= 1;
                        self.const_self = old;
                        return r;
                    }
                }
                queue.extend(c.implements.iter().cloned());
                if let Some(p) = &c.parent {
                    queue.push(p.clone());
                }
            }
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

    /// Flush all output buffers at script end, innermost first so each
    /// level's handler output lands in its parent's buffer (bug24951).
    fn flush_ob_all(&mut self) {
        while !self.ob_stack.is_empty() {
            let r = self.ob_invoke(8);
            self.ob_stack.pop();
            if let Ok(Some(s)) = r {
                self.emit_bytes(&s);
            }
        }
    }

    /// Invoke the top level's handler with `mode | START` on first call,
    /// clearing the buffer first (bug24951 flag semantics:
    /// START=1, CLEAN=2, FLUSH=4, FINAL=8). Returns the handler's output
    /// — or the raw buffer when there is no handler.
    fn ob_invoke(&mut self, mode: i64) -> Result<Option<Vec<u8>>, PhpError> {
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
                let out = self.call_value(
                    &h,
                    CallArgs::positional(vec![cell(Value::bytes(buf)), cell(Value::Int(m))]),
                );
                self.internal_cb -= 1;
                Ok(Some(out?.to_php_bytes()))
            }
            None => Ok(Some(buf)),
        }
    }

    /// String conversion for builtins (__toString-aware, never errors → "" on failure).
    pub fn to_string_of(&mut self, v: &Value) -> String {
        self.conv_str(v).unwrap_or_else(|_| v.to_php_string())
    }
    /// Byte-faithful variant — for binary-safe builtins.
    pub fn to_bytes_of(&mut self, v: &Value) -> Vec<u8> {
        self.conv_bytes(v).unwrap_or_else(|_| v.to_php_bytes())
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
            buf: Vec::new(),
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
            self.emit_bytes(&s);
        }
        Ok(())
    }
    /// ob_flush: handler(mode=FLUSH) result emitted to the PARENT level
    /// (the level is briefly popped so emit can't feed back into it),
    /// buffer cleared, level stays open (bug24951).
    pub fn ob_flush(&mut self) -> Result<(), PhpError> {
        if let Some(s) = self.ob_invoke(4)? {
            let level = self.ob_stack.pop();
            self.emit_bytes(&s);
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
            .map(|l| Value::bytes(l.buf))
            .unwrap_or(Value::Bool(false))
    }
    /// ob_get_flush: handler(mode=FINAL) result emitted, RAW buffer
    /// returned, level popped.
    pub fn ob_get_flush(&mut self) -> Result<Value, PhpError> {
        let raw = self.ob_stack.last().map(|l| l.buf.clone());
        let r = self.ob_invoke(8)?;
        self.ob_stack.pop();
        if let Some(s) = r {
            self.emit_bytes(&s);
        }
        Ok(raw.map(Value::bytes).unwrap_or(Value::Bool(false)))
    }
    pub fn ob_top(&self) -> Option<&Vec<u8>> {
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
    /// get_called_class(): late-static-binding class of the current
    /// frame, `false` outside a called-class context (static_get_called_class).
    pub fn called_class_name(&mut self) -> Value {
        match self.stack.last().and_then(|f| f.called_class.clone()) {
            Some(c) => Value::str(c.name().to_string()),
            None => Value::Bool(false),
        }
    }

    /// property_exists(): instance prop declared on the class or any
    /// ancestor (property002).
    pub fn class_has_prop(&self, c: &Rc<PhpClass>, name: &str) -> bool {
        let mut cur = Some(c.clone());
        while let Some(k) = cur {
            if k.decl.props.iter().any(|p| p.name == name) {
                return true;
            }
            cur = k
                .decl
                .parent
                .as_ref()
                .and_then(|p| self.classes.get(&p.to_lowercase()).cloned());
        }
        false
    }
    pub fn instantiate_class(&mut self, name: &str, args: Vec<Cell>) -> Result<Value, PhpError> {
        self.new_instance(name, CallArgs::positional(args))
    }
    pub fn call_closure(
        &mut self,
        c: &Rc<PhpCallable>,
        args: Vec<Cell>,
    ) -> Result<Value, PhpError> {
        self.call_value(&Value::Callable(c.clone()), CallArgs::positional(args))
    }
    pub fn var_name_set(&mut self, name: &str, v: Value) {
        self.var_set(name, v);
    }
    pub fn warn_pub(&mut self, msg: &str) -> Result<(), PhpError> {
        self.warn(msg)
    }

    pub fn deprecated_pub(&mut self, msg: &str) -> Result<(), PhpError> {
        self.deprecated(msg)
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
        self.call_value(&Value::str(name), CallArgs::positional(args))
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

/// Zend-style render for the `assert(<args>)` AssertionError message.
fn assert_arg_repr(v: &Value) -> String {
    match v {
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Null => "NULL".into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => crate::value::trace_arg(&Value::Float(*f)),
        Value::Str(s) => format!("'{}'", crate::value::lossy(&s)),
        Value::Array(_) => "Array".into(),
        Value::Object(o) => format!("Object({})", o.borrow().class.name()),
        Value::Callable(_) => "Object(Closure)".into(),
        Value::Resource(_) => "Resource id #1".into(),
    }
}

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
fn perl_inc(s: &[u8]) -> Vec<u8> {
    let mut bytes = s.to_vec();
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
    bytes
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

fn bitwise_str(op: &str, a: &[u8], b: &[u8]) -> Vec<u8> {
    // `|` pads the shorter operand with NUL; `&`/`^` truncate to min length.
    let (x, y) = (a, b);
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
    out
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
        "preg_match" | "preg_match_all" => &[false, false, true],
        "preg_replace"
        | "preg_replace_callback"
        | "preg_filter"
        | "str_replace"
        | "str_ireplace" => &[false, false, false, false, true],
        "preg_replace_callback_array" => &[false, false, false, true],
        "parse_str" => &[false, true],
        "is_callable" => &[false, false, true],
        "sscanf" | "fscanf" => &[false, false],
        "exec" => &[false, true, true],
        "passthru" | "system" => &[false, true],
        "preg_grep" => &[false],
        _ => return None,
    })
}

/// Weak-mode scalar coercion used by typed-property writes and hook
/// type checks ("C::$p: Return value must be of type int" family).
fn weak_ty_coerce(tys: &[String], v: &Value) -> Option<Value> {
    for t in tys {
        let coerced = match (t.as_str(), v) {
            ("int", Value::Str(s)) => {
                let tr = crate::value::lossy(s);
                let tr = tr.trim();
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
            ("int", Value::Float(f)) => {
                // Zend refuses out-of-range float->int coercions
                // (NaN/Inf/|f| >= 2^63 → TypeError, no saturation).
                if f.is_finite() && *f < 9.223372036854776e18 && *f >= -9.223372036854776e18 {
                    Some(Value::Int(*f as i64))
                } else {
                    None
                }
            }
            ("int", Value::Bool(b)) => Some(Value::Int(*b as i64)),
            ("string", Value::Int(i)) => Some(Value::str(i.to_string())),
            ("string", Value::Float(f)) => Some(Value::str(format_float_repr(*f))),
            ("string", Value::Bool(b)) => Some(Value::str(if *b { "1" } else { "" })),
            ("float", Value::Int(i)) => Some(Value::Float(*i as f64)),
            ("float", Value::Str(s)) => crate::value::lossy(s)
                .trim()
                .parse::<f64>()
                .ok()
                .map(Value::Float),
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

/// Render a parsed type member list the way Zend prints it — a union
/// containing `null` displays as `?T`.
fn ty_disp(ty: &[String]) -> String {
    let mut nullable = false;
    let mut rest: Vec<String> = Vec::new();
    for m in ty {
        if m.eq_ignore_ascii_case("null") {
            nullable = true;
        } else {
            rest.push(
                m.split('&')
                    .map(|p| {
                        let p = p.trim_start_matches('\\');
                        if let Some(pos) = p.find("@anonymous$") {
                            format!("{}@anonymous", &p[..pos])
                        } else {
                            p.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("&"),
            );
        }
    }
    let joined = if rest.len() > 1 || nullable {
        rest.iter()
            .map(|m| {
                if m.contains('&') {
                    format!("({m})")
                } else {
                    m.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("|")
    } else {
        rest.join("|")
    };
    if nullable && rest.is_empty() {
        "null".to_string()
    } else if nullable && rest.len() == 1 && !rest[0].contains('&') {
        format!("?{}", joined)
    } else if nullable {
        // `(X&Y)|null` — intersections can't take the ? shortcut.
        format!("{}|null", joined)
    } else {
        joined
    }
}

/// Typed-const compat in trait composition: same member list
/// (case-insensitive; both `None` is compatible).
fn ty_list_eq(a: &Option<Vec<String>>, b: &Option<Vec<String>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|(m, n)| m.eq_ignore_ascii_case(n))
        }
        _ => false,
    }
}

/// Whether a const initializer is a pure compile-time expression
/// (literals and operators over them — no fetches, calls, `new`).
/// Only these get Zend's eager "Cannot use ... as value" fatal at
/// class registration; everything else type-checks lazily at access.
fn is_compile_const(e: &Expr) -> bool {
    match e {
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::Str(_) => true,
        Expr::Interp(parts) => parts
            .iter()
            .all(|p| matches!(p, crate::lexer::StringPart::Lit(_))),
        Expr::ArrayLit(items) => items.iter().all(|(_, v)| is_compile_const(v)),
        Expr::Unary { e, .. } => is_compile_const(e),
        Expr::Binary { l, r, .. } => is_compile_const(l) && is_compile_const(r),
        Expr::Cast { e, .. } => is_compile_const(e),
        Expr::Ternary { c, t, f } => {
            is_compile_const(c)
                && t.as_ref().map(|x| is_compile_const(x)).unwrap_or(true)
                && is_compile_const(f)
        }
        _ => false,
    }
}

/// Zend's normalized union display for redundancy errors: iterable
/// expands to its members, class names first (written order), then
/// `object`, then `array`, then remaining builtins, `null` last.
fn ty_norm_disp(ty: &[String]) -> String {
    let mut classes: Vec<String> = Vec::new();
    let mut scalars: Vec<String> = Vec::new();
    let mut obj = false;
    let mut arr = false;
    let mut nul = false;
    for m in ty {
        let mut members: Vec<String> = if m.eq_ignore_ascii_case("iterable") {
            vec!["Traversable".into(), "array".into()]
        } else {
            vec![m.clone()]
        };
        for e in members.drain(..) {
            let el = e.to_lowercase();
            match el.as_str() {
                "null" => nul = true,
                "object" => obj = true,
                "array" => arr = true,
                "self" | "static" | "parent" => {
                    if !classes.iter().any(|c| c.eq_ignore_ascii_case(&e)) {
                        classes.push(e);
                    }
                }
                "int" | "float" | "string" | "bool" | "callable" | "iterable" | "mixed"
                | "void" | "never" | "false" | "true" => {
                    if !scalars.iter().any(|c| c == &el) {
                        scalars.push(el);
                    }
                }
                _ => {
                    if !classes.iter().any(|c| c.eq_ignore_ascii_case(&e)) {
                        if e.contains('&') {
                            classes.push(format!("({})", e));
                        } else {
                            classes.push(e);
                        }
                    }
                }
            }
        }
    }
    let mut out = classes;
    if obj {
        out.push("object".into());
    }
    if arr {
        out.push("array".into());
    }
    out.extend(scalars);
    if nul {
        out.push("null".into());
    }
    out.join("|")
}
