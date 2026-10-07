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

mod api;
mod calls;
mod classes;
mod diag;
mod exec;
mod expr;
mod flow;
mod gen;
mod include;
mod members;
mod registry;
pub(crate) mod util;

use util::*;

/// Internal control-flow signals.
pub enum Flow {
    Normal,
    Break(u32),
    Continue(u32),
    Return(Value),
    /// `throw` propagating an exception object.
    Throw(Value),
    Exit(i32),
    /// `goto name;` — binds when an enclosing statement list carries a
    /// matching `name:` label; otherwise keeps propagating.
    Goto(String),
}

/// Weak slot in the shared object-store handle space — objects and
/// closures draw ids from the same vector, like Zend's EG(objects_store).
enum ObjHandle {
    Obj(std::rc::Weak<RefCell<PhpObject>>),
    /// Weak callable + its statics-table key prefix (the decl name in
    /// `{name}\0c{id}`) — lets a dead slot's entry be GC'd on reuse.
    Callable(std::rc::Weak<PhpCallable>, Option<String>),
}

impl ObjHandle {
    fn alive(&self) -> bool {
        match self {
            Self::Obj(w) => w.upgrade().is_some(),
            Self::Callable(w, _) => w.upgrade().is_some(),
        }
    }
}

/// Evaluated call arguments: positional cells (call order) plus named
/// entries the callee binds by param name (Zend/tests/named_params).
/// Clone shares the arg cells — generator replay re-binds the same
/// values exactly like zend re-entering the call frame.
#[derive(Clone)]
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
    /// While eval()/include() unit code executes inside this frame: the
    /// executing compile unit. `static` decls in that code store under
    /// `key\0u{unit}` — a fresh table per unit, matching Zend's fresh
    /// op_array (and fresh static_variables) per eval/include call.
    /// None = the frame's own op_array, whose statics persist.
    statics_unit: Option<u64>,
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
            statics_unit: None,
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
    /// Registration is running inside `hoist_funcs` — Zend's
    /// early-binding compile phase — so its link errors carry the
    /// compile-context trace (innermost include/eval frame dropped),
    /// not the live call chain.
    in_hoist: bool,
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
    /// binding): name → AST decl ptr, so only the SAME decl stmt
    /// no-ops on execution — a different decl site claiming the name
    /// still hits the 'Cannot redeclare' check (namespaces/ns_060).
    early_bound_classes: HashMap<String, usize>,
    /// Function decl sites early-bound at compile: lname →
    /// (compile unit, decl node ptr) — reaching that same site at
    /// runtime is a no-op, any OTHER decl into the occupied name is
    /// 'Cannot redeclare'. The unit guards against a freed AST Vec
    /// recycling the node ptr across re-parses.
    early_bound_funcs: HashMap<String, (u64, usize)>,
    /// Per-include top-level namespace: `(stack depth at include,
    /// file's current ns)`. An included file's `namespace` decl governs
    /// ITS top-level code, not the calling frame's (php-parser's
    /// conditional-decl inside a function-context require).
    pub(crate) include_ns: Vec<(usize, String)>,
    constants: HashMap<String, Value>,
    /// Accumulated program output (display_errors prints to stdout under
    /// CLI, and the PHPT harness merges streams via 2>&1).
    pub out: Vec<u8>,
    /// PHP CLI logs every diagnostic to stderr as `PHP <Level>: msg` when
    /// log_errors is on (default); the harness merges stderr after stdout.
    pub err_buf: String,
    /// CLI file runs stream stdout/stderr to the real fds as they're
    /// written so merged output keeps PHP's interleaved order; harness
    /// contexts (phpun test, serve) leave this off and capture instead.
    pub live_io: bool,
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
    /// Zend's IS_STR_VALID_UTF8 flag: string storage (keyed by Rc
    /// pointer) proven fully valid UTF-8 — /u preg calls skip
    /// re-validating it (bug72685). The Rcs stay in the map so the
    /// pointer keys can't be recycled.
    pub valid_utf8: std::collections::HashMap<usize, std::rc::Rc<[u8]>>,
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
    /// isset/empty/?? quiet reads — zend suppresses only the
    /// undefined-family diagnostics there while OFFSET-KEY casts
    /// still surface deprecations/warnings; `_ns` emitters bypass
    /// this counter, `@`'s `silence` suppresses them all.
    isset_quiet: u32,
    /// Cell returned by the last `&fn()` call (returnByReference tests).
    last_ret_cell: Option<Cell>,
    /// The object a write-context prop_cell resolved (lets `=&` reuse
    /// it for the typed-prop decl lookup without re-evaluating the
    /// receiver expr — `$x =& $o->m()->p` must call m() once).
    last_prop_ov: Option<Value>,
    /// Dynamic prop slots materialized by the CURRENT lvalue chain's
    /// write-fetch — (cell ptr, class, name). The compound read then
    /// emits zend's per-level 'Undefined property: C::$p' warning
    /// (finding 13); cleared at the start of each assign target.
    fresh_dyn_props: Vec<(usize, String, String)>,
    /// The last invoked function was declared `&name()` (returns by ref).
    last_call_by_ref: bool,
    /// Set just before invoking `[$closure,'__invoke']` so the callee
    /// frame reports diagnostics as `Closure::__invoke` (zend).
    pending_call_alias: Option<String>,
    /// Insertion order of global vars (for $GLOBALS ordering).
    globals_order: Vec<String>,
    /// Shared PhpArray backing $GLOBALS — same cells as globals.vars.
    globals_arr: Option<Rc<RefCell<PhpArray>>>,
    /// Var names the $GLOBALS table currently manages — a name whose
    /// array entry was tombstoned (unset($GLOBALS['x'])) unsets the
    /// global var on next lookup.
    globals_synced: std::collections::HashSet<String>,
    /// Set while a dim-read runs in a by-ref context (`$x =& $o['k']`):
    /// zend's read_dimension(BP_VAR_RW) silently creates missing
    /// buckets instead of warning.
    dim_by_ref: bool,
    /// `foreach ($x as &$v)` source fetch — zend treats it as a
    /// write-reference bind (uninit non-nullable typed props error
    /// 'by reference'; uninit *nullable* statics report 'undeclared').
    foreach_by_ref: bool,
    /// Inside a whole-target `unset($x)` root fetch — set-visibility
    /// checks stand down so unset's own errors ('Cannot unset
    /// private(set) property', 'Attempt to unset static property')
    /// win over 'Cannot indirectly modify'.
    in_unset: bool,
    /// Autoload/lookup error swallowed by the last `is_callable_value`
    /// probe — re-raised when a `callable` param type rejects the arg.
    callable_probe_err: Option<(Value, PhpError)>,
    /// Function-scoped static storage: scope key → var → cell. The key
    /// is fn_statics_key() for a function's own op_array; eval/include
    /// unit code executing inside a frame suffixes `\0u{unit}` so each
    /// unit gets a fresh table, and top-level code uses the executing
    /// unit under the global scope key (Zend: static vars live in the
    /// op_array that declared them).
    pub(crate) statics: HashMap<String, HashMap<String, Cell>>,
    /// static-decl sites per function scope (fn key → var → decl
    /// (unit serial, stmt ptr)) — PHP fatals on a same-unit
    /// redeclaration at a different statement site. The unit serial is
    /// bumped at every parse boundary (include/eval/run) since Zend
    /// compiles each into a fresh op_array — a freed Vec may recycle
    /// the same stmt ptr across re-parses.
    static_decls: HashMap<String, HashMap<String, std::collections::HashSet<(u64, usize)>>>,
    /// Serial of the compile unit currently executing (see static_decls).
    cur_unit_id: u64,
    /// Next unit serial to hand out — bumps monotonically.
    next_unit_id: u64,
    /// include_once/require_once registry (canonical paths).
    included: HashSet<std::path::PathBuf>,
    /// Pending exception carried across an Err(Throw) return.
    pending_exception: Option<Value>,
    /// zend's `class@anonymous` name table: decl-site (Rc ptr) →
    /// mangled `base@anonymous\0FILE:LINE$SEQ` name.
    anon_class_names: HashMap<usize, String>,
    /// Process-wide anonymous-class sequence (`$0`, `$1`, ...).
    anon_class_seq: u64,
    /// Live call stack (user + builtin) for getTrace() snapshots.
    call_trace: Vec<TraceFrame>,
    /// Pending fatal error message for exceptions raised as PhpError.
    res_counter: u64,
    shutdown_fns: Vec<(Value, Vec<Cell>)>,
    error_handler: Option<Value>,
    error_handler_stack: Vec<Value>,
    exception_handler_stack: Vec<Value>,
    /// error_reporting() level mask (E_* bits).
    pub(crate) error_level: i64,
    /// putenv() overrides read back by getenv() (no real process-env mutation).
    env_overrides: HashMap<String, String>,
    /// Raw argv entries after the script path, for `getopt()`.
    pub script_args: Vec<String>,
    exception_handler: Option<Value>,
    in_handler: bool,
    /// Cells written while an error handler ran — zend binds a dim
    /// write's container slot BEFORE the dim key evaluates, so a
    /// handler that reassigns the container leaves the pending write
    /// on the stale slot: invisible and silent (assign_dim_014). The
    /// stored Rcs pin the allocations so a freed cell's address can't
    /// recycle into a false hit.
    handler_writes: std::collections::HashMap<usize, Cell>,
    /// Cells read while an error handler ran — a prior read links the
    /// binding, so a later write stays on the live slot.
    handler_reads: std::collections::HashMap<usize, Cell>,
    /// The current dim write is detached (see `handler_writes`) —
    /// offset-key conversions stay silent.
    detached_dim: bool,
    /// Dim-key conversions already emitted for this assign op — zend
    /// casts each dim operand once: the compound read, the write gate
    /// and the write itself reuse it without re-warning (`.=`/`|=`
    /// probe: oracle prints the null-offset deprecation exactly once).
    /// Per-dim-op cache of each operand cell's offset conversion —
    /// `(cell, ArrKey)` keeps the Rc alive so a dropped cell's address
    /// can't be reused and mis-key a later conversion (ABA).
    dim_key_conv: std::collections::HashMap<usize, (Cell, ArrKey)>,
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
    /// Ambient `class_const_ctx` level while a PARAM default is being
    /// evaluated — stack-based closure names apply only while it still
    /// matches (a nested class initializer bumps it out from under us).
    param_bind_ctx: Option<u32>,
    /// Engine-provided constants (PHP_VERSION, PHP_EOL, ...) — always
    /// resolvable, so closure `static` defaults naming them bind at
    /// creation like Zend (probe_sv_engine). User consts stay lazy.
    engine_consts: std::collections::HashSet<String>,
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
    /// (typed_properties_034 first vs second foo() call). Weak refs pin
    /// each marked cell's allocation so a freed cell's recycled address
    /// can never inherit the mark (ABA); marks left on cells that
    /// outlive their last alias are inert — dup paths also require
    /// `Rc::strong_count > 1` before re-binding.
    pub ref_cells: std::collections::HashMap<usize, std::rc::Weak<RefCell<Value>>>,
    /// Next `mark_ref` inserts past this size first sweep dead marks.
    ref_cells_prune: usize,
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
    /// Inside `clone($o, [...])` with-property writes: PHP 8.5 lets the
    /// clone overwrite an already-initialized readonly prop — only the
    /// set-visibility scope gate still applies (R3 finding 13).
    pub clone_write: bool,
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
    /// Enclosing loop/switch contexts in the current compile unit
    /// (zend's loop_var_stack depth) — a `break`/`continue` operand
    /// larger than this is the `Cannot 'break' N levels` compile fatal;
    /// reset at function/include/eval boundaries.
    loop_depth: u32,
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

/// Mode for the compile-time const-closure scan: `Const` walks a
/// const-expr slot (closures hit the shape gates; the carried line
/// overrides the error line for attribute args), `Runtime` walks
/// bodies where `function(){}` is legal but decl defaults inside
/// stay gated.
enum GateMode {
    Const(Option<usize>),
    Runtime,
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
        {
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| "phpun".to_string());
            let bindir = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()))
                .unwrap_or_default();
            constants.insert("PHP_BINARY".into(), Value::str(&*exe));
            constants.insert("PHP_BINDIR".into(), Value::str(&*bindir));
        }
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
        constants.insert("PATH_SEPARATOR".into(), Value::str(":"));
        constants.insert("SCANDIR_SORT_ASCENDING".into(), Value::Int(0));
        constants.insert("SCANDIR_SORT_DESCENDING".into(), Value::Int(1));
        constants.insert("SCANDIR_SORT_NONE".into(), Value::Int(2));
        // pathinfo() component selectors.
        constants.insert("PATHINFO_DIRNAME".into(), Value::Int(1));
        constants.insert("PATHINFO_BASENAME".into(), Value::Int(2));
        constants.insert("PATHINFO_EXTENSION".into(), Value::Int(4));
        constants.insert("PATHINFO_FILENAME".into(), Value::Int(8));
        constants.insert("PATHINFO_ALL".into(), Value::Int(15));
        // glob() flags (glibc values, as on Linux PHP builds).
        constants.insert("GLOB_MARK".into(), Value::Int(8));
        constants.insert("GLOB_NOSORT".into(), Value::Int(32));
        constants.insert("GLOB_NOCHECK".into(), Value::Int(16));
        constants.insert("GLOB_NOESCAPE".into(), Value::Int(4096));
        constants.insert("GLOB_BRACE".into(), Value::Int(128));
        constants.insert("GLOB_ONLYDIR".into(), Value::Int(1 << 30));
        constants.insert("GLOB_ERR".into(), Value::Int(4));
        // flock/file flags.
        constants.insert("LOCK_SH".into(), Value::Int(1));
        constants.insert("LOCK_EX".into(), Value::Int(2));
        constants.insert("LOCK_NB".into(), Value::Int(4));
        constants.insert("LOCK_UN".into(), Value::Int(3));
        constants.insert("FILE_USE_INCLUDE_PATH".into(), Value::Int(1));
        constants.insert("FILE_NO_DEFAULT_CONTEXT".into(), Value::Int(16));
        constants.insert("FILE_APPEND".into(), Value::Int(8));
        constants.insert("FILE_IGNORE_NEW_LINES".into(), Value::Int(4));
        constants.insert("FILE_SKIP_EMPTY_LINES".into(), Value::Int(2));
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
        constants.insert("E_ALL".into(), Value::Int(30719));
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
        constants.insert("FILTER_VALIDATE_BOOLEAN".into(), Value::Int(258));
        constants.insert("FILTER_VALIDATE_BOOL".into(), Value::Int(258));
        constants.insert("FILTER_VALIDATE_FLOAT".into(), Value::Int(259));
        constants.insert("FILTER_VALIDATE_REGEXP".into(), Value::Int(272));
        constants.insert("FILTER_VALIDATE_URL".into(), Value::Int(273));
        constants.insert("FILTER_VALIDATE_EMAIL".into(), Value::Int(274));
        constants.insert("FILTER_VALIDATE_IP".into(), Value::Int(275));
        constants.insert("FILTER_VALIDATE_MAC".into(), Value::Int(276));
        constants.insert("FILTER_VALIDATE_DOMAIN".into(), Value::Int(277));
        constants.insert("FILTER_DEFAULT".into(), Value::Int(516));
        constants.insert("FILTER_UNSAFE_RAW".into(), Value::Int(516));
        constants.insert("FILTER_SANITIZE_ENCODED".into(), Value::Int(514));
        constants.insert("FILTER_SANITIZE_SPECIAL_CHARS".into(), Value::Int(515));
        constants.insert("FILTER_SANITIZE_EMAIL".into(), Value::Int(517));
        constants.insert("FILTER_SANITIZE_URL".into(), Value::Int(518));
        constants.insert("FILTER_SANITIZE_NUMBER_INT".into(), Value::Int(519));
        constants.insert("FILTER_SANITIZE_NUMBER_FLOAT".into(), Value::Int(520));
        constants.insert("FILTER_SANITIZE_FULL_SPECIAL_CHARS".into(), Value::Int(522));
        constants.insert("FILTER_SANITIZE_ADD_SLASHES".into(), Value::Int(523));
        constants.insert("FILTER_CALLBACK".into(), Value::Int(1024));
        constants.insert("FILTER_REQUIRE_ARRAY".into(), Value::Int(16777216));
        constants.insert("FILTER_REQUIRE_SCALAR".into(), Value::Int(33554432));
        constants.insert("FILTER_FORCE_ARRAY".into(), Value::Int(67108864));
        constants.insert("FILTER_NULL_ON_FAILURE".into(), Value::Int(134217728));
        constants.insert("FILTER_FLAG_ALLOW_OCTAL".into(), Value::Int(1));
        constants.insert("FILTER_FLAG_ALLOW_HEX".into(), Value::Int(2));
        constants.insert("FILTER_FLAG_STRIP_LOW".into(), Value::Int(4));
        constants.insert("FILTER_FLAG_STRIP_HIGH".into(), Value::Int(8));
        constants.insert("FILTER_FLAG_ENCODE_LOW".into(), Value::Int(16));
        constants.insert("FILTER_FLAG_ENCODE_HIGH".into(), Value::Int(32));
        constants.insert("FILTER_FLAG_ENCODE_AMP".into(), Value::Int(64));
        constants.insert("FILTER_FLAG_NO_ENCODE_QUOTES".into(), Value::Int(128));
        constants.insert("FILTER_FLAG_EMPTY_STRING_NULL".into(), Value::Int(256));
        constants.insert("FILTER_FLAG_STRIP_BACKTICK".into(), Value::Int(512));
        constants.insert("FILTER_FLAG_ALLOW_FRACTION".into(), Value::Int(4096));
        constants.insert("FILTER_FLAG_ALLOW_THOUSAND".into(), Value::Int(8192));
        constants.insert("FILTER_FLAG_ALLOW_SCIENTIFIC".into(), Value::Int(16384));
        constants.insert("FILTER_FLAG_PATH_REQUIRED".into(), Value::Int(262144));
        constants.insert("FILTER_FLAG_QUERY_REQUIRED".into(), Value::Int(524288));
        constants.insert("FILTER_FLAG_IPV4".into(), Value::Int(1048576));
        constants.insert("FILTER_FLAG_IPV6".into(), Value::Int(2097152));
        constants.insert("FILTER_FLAG_HOSTNAME".into(), Value::Int(1048576));
        constants.insert("FILTER_FLAG_EMAIL_UNICODE".into(), Value::Int(1048576));
        constants.insert("FILTER_FLAG_NO_RES_RANGE".into(), Value::Int(4194304));
        constants.insert("FILTER_FLAG_NO_PRIV_RANGE".into(), Value::Int(8388608));
        constants.insert("FILTER_FLAG_GLOBAL_RANGE".into(), Value::Int(536870912));
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
        let engine_consts: std::collections::HashSet<String> = constants.keys().cloned().collect();
        let mut it = Self {
            file,
            globals: Frame::new(String::new()),
            last_ret_cell: None,
            last_prop_ov: None,
            fresh_dyn_props: Vec::new(),
            last_call_by_ref: false,
            pending_call_alias: None,
            globals_order: Vec::new(),
            globals_arr: None,
            globals_synced: std::collections::HashSet::new(),
            dim_by_ref: false,
            foreach_by_ref: false,
            in_unset: false,
            anon_class_names: HashMap::new(),
            anon_class_seq: 0,
            callable_probe_err: None,
            stack: Vec::new(),
            functions: HashMap::new(),
            classes: HashMap::new(),
            traits: HashMap::new(),
            trait_statics: HashMap::new(),
            linking: Vec::new(),
            autoloading: std::collections::HashSet::new(),
            variance_obligations: Vec::new(),
            in_variance_pass: false,
            in_hoist: false,
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
            early_bound_classes: HashMap::new(),
            early_bound_funcs: HashMap::new(),
            include_ns: Vec::new(),
            constants,
            out: Vec::new(),
            err_buf: String::new(),
            live_io: false,
            decl_file_ctx: None,
            out_headers: Vec::new(),
            resp_code: 200,
            last_json_error: 0,
            last_preg_error: 0,
            valid_utf8: std::collections::HashMap::new(),
            php_input: std::rc::Rc::new(Vec::new()),
            uploads: Vec::new(),
            ob_stack: Vec::new(),
            silence: 0,
            isset_quiet: 0,
            statics: HashMap::new(),
            static_decls: HashMap::new(),
            cur_unit_id: 0,
            next_unit_id: 1,
            included: HashSet::new(),
            pending_exception: None,
            call_trace: Vec::new(),
            // Zend burns resource ids 1-4 on STDIN/STDOUT/STDERR plus
            // one internal stream — the first userland resource is #5.
            res_counter: 4,
            shutdown_fns: Vec::new(),
            error_handler: None,
            error_handler_stack: Vec::new(),
            exception_handler_stack: Vec::new(),
            error_level: 30719,
            env_overrides: HashMap::new(),
            script_args: Vec::new(),
            exception_handler: None,
            in_handler: false,
            handler_writes: std::collections::HashMap::new(),
            handler_reads: std::collections::HashMap::new(),
            detached_dim: false,
            dim_key_conv: std::collections::HashMap::new(),
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
            param_bind_ctx: None,
            engine_consts,
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
            ref_cells: std::collections::HashMap::new(),
            ref_cells_prune: 1024,
            magic_guards: std::collections::HashSet::new(),
            readonly_cells: std::collections::HashMap::new(),
            clone_write: false,
            last_fresh_cell: None,
            builtin_ifaces: std::collections::HashSet::new(),
            dep_seen: std::collections::HashSet::new(),
            last_err_file: String::new(),
            loop_depth: 0,
            assert_src: String::new(),
            mem_used: 0,
            mem_last: 0,
            mem_exceeded: false,
            deadline: None,
            deadline_secs: 0,
            ini: HashMap::from([
                ("error_reporting".to_string(), "30719".to_string()),
                // Zend's compiled-in default (hardcoded in main/php.ini).
                ("memory_limit".to_string(), "128M".to_string()),
                // Oracle PHP's compiled-in include_path (brew build) —
                // `get_include_path` and the Failed-opening diagnostics
                // print it verbatim.
                (
                    "include_path".to_string(),
                    ".:/home/linuxbrew/.linuxbrew/Cellar/php/8.5.11/share/php/pear".to_string(),
                ),
            ]),
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
        // SPL iterator wrappers in plain PHP — the delegation layer
        // (OuterIterator, IteratorIterator, FilterIterator,
        // RecursiveIteratorIterator, AppendIterator) needs only
        // Iterator method calls, so a prelude keeps the engine small.
        let _ = it.eval_code(SPL_ITERATOR_PRELUDE);
        it
    }

    /// `phpun file.php a b c` — CLI args after the script name land in
    /// `$argv`/`$argc`/`$_SERVER['argv']` like reference php.
    pub fn set_script_args(&mut self, script: &str, args: &[String]) {
        self.script_args = args.to_vec();
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

    /// zend_compile validates the closure-shape rules of every
    /// const-expr slot in the file eagerly — prop/const/param defaults
    /// and attribute args — even inside `if(false)` or a never-called
    /// function body; `Stmt::Static` defaults are runtime inits and
    /// stay exempt (closure_const_expr/*).
    fn const_closure_gate(stmts: &[Stmt]) -> Result<(), PhpError> {
        for s in stmts {
            Self::gate_stmt(s)?;
        }
        Ok(())
    }

    fn gate_stmt(s: &Stmt) -> Result<(), PhpError> {
        match s {
            Stmt::Class(d) => Self::gate_class_decl(d),
            Stmt::Function(d) => Self::gate_fn_decl(d),
            Stmt::ConstDecl(v) => {
                for (_, e) in v {
                    Self::gate_expr(e, &GateMode::Const(None))?;
                }
                Ok(())
            }
            Stmt::Declare { value, .. } => Self::gate_expr(value, &GateMode::Const(None)),
            Stmt::Expr(e) => Self::gate_expr(e, &GateMode::Runtime),
            Stmt::Echo(es) => {
                for e in es {
                    Self::gate_expr(e, &GateMode::Runtime)?;
                }
                Ok(())
            }
            Stmt::Return(Some(e)) | Stmt::Break(Some(e)) | Stmt::Continue(Some(e)) => {
                Self::gate_expr(e, &GateMode::Runtime)
            }
            Stmt::Block(b) => Self::const_closure_gate(b),
            Stmt::If { cond, then, else_ } => {
                Self::gate_expr(cond, &GateMode::Runtime)?;
                Self::const_closure_gate(then)?;
                Self::const_closure_gate(else_)
            }
            Stmt::While { cond, body } | Stmt::DoWhile { body, cond } => {
                Self::gate_expr(cond, &GateMode::Runtime)?;
                Self::const_closure_gate(body)
            }
            Stmt::For {
                init,
                cond,
                inc,
                body,
            } => {
                for e in init.iter().chain(cond.iter()).chain(inc.iter()) {
                    Self::gate_expr(e, &GateMode::Runtime)?;
                }
                Self::const_closure_gate(body)
            }
            Stmt::Foreach { arr, val, body, .. } => {
                Self::gate_expr(arr, &GateMode::Runtime)?;
                Self::gate_foreach_target(val)?;
                Self::const_closure_gate(body)
            }
            Stmt::Switch { cond, cases } => {
                Self::gate_expr(cond, &GateMode::Runtime)?;
                for (c, b) in cases {
                    if let Some(c) = c {
                        Self::gate_expr(c, &GateMode::Runtime)?;
                    }
                    Self::const_closure_gate(b)?;
                }
                Ok(())
            }
            Stmt::Try {
                body,
                catches,
                finally,
            } => {
                Self::const_closure_gate(body)?;
                for c in catches {
                    Self::const_closure_gate(&c.body)?;
                }
                if let Some(f) = finally {
                    Self::const_closure_gate(f)?;
                }
                Ok(())
            }
            // `static $x = ...` inside a body is a RUNTIME initializer —
            // closure literals are legal there (probe_f4d).
            Stmt::Static { vars, .. } => {
                for (_, d) in vars {
                    if let Some(e) = d {
                        Self::gate_expr(e, &GateMode::Runtime)?;
                    }
                }
                Ok(())
            }
            Stmt::Unset(v) | Stmt::Global(v) => {
                for e in v {
                    Self::gate_expr(e, &GateMode::Runtime)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn gate_foreach_target(t: &ForeachTarget) -> Result<(), PhpError> {
        match t {
            ForeachTarget::ByRef(e) | ForeachTarget::Lvalue(e) => {
                Self::gate_expr(e, &GateMode::Runtime)
            }
            ForeachTarget::List(ts) => {
                for (k, t) in ts.iter().flatten() {
                    if let Some(k) = k {
                        Self::gate_expr(k, &GateMode::Runtime)?;
                    }
                    Self::gate_foreach_target(t)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Const slots shared by fn/method/closure decls: param defaults
    /// and attributes gate eagerly; the body is runtime.
    fn gate_fn_decl(d: &FunctionDecl) -> Result<(), PhpError> {
        for p in &d.params {
            if let Some(def) = &p.default {
                Self::gate_expr(def, &GateMode::Const(None))?;
            }
            Self::gate_hooks(&p.hooks)?;
        }
        Self::gate_attrs(&d.attrs)?;
        Self::const_closure_gate(&d.body)
    }

    fn gate_class_decl(d: &ClassDecl) -> Result<(), PhpError> {
        Self::gate_attrs(&d.attrs)?;
        for p in &d.props {
            if let Some(def) = &p.default {
                Self::gate_expr(def, &GateMode::Const(None))?;
            }
            Self::gate_attrs(&p.attrs)?;
            Self::gate_hooks(&p.hooks)?;
        }
        for c in &d.consts {
            Self::gate_expr(&c.value, &GateMode::Const(None))?;
            Self::gate_attrs(&c.attrs)?;
        }
        for m in &d.methods {
            Self::gate_fn_decl(&m.decl)?;
        }
        Ok(())
    }

    fn gate_hooks(hooks: &Option<Vec<PropHook>>) -> Result<(), PhpError> {
        let Some(hs) = hooks else { return Ok(()) };
        for h in hs {
            for p in &h.params {
                if let Some(d) = &p.default {
                    Self::gate_expr(d, &GateMode::Const(None))?;
                }
            }
            if let Some(b) = &h.body {
                Self::const_closure_gate(b)?;
            }
        }
        Ok(())
    }

    fn gate_attrs(attrs: &[AttrDecl]) -> Result<(), PhpError> {
        for a in attrs {
            for arg in &a.args {
                // Compile fatals inside attribute args attribute to the
                // attributed declaration — Zend reports a line later
                // than the `#[` token (see AttrDecl::line).
                Self::gate_expr(arg, &GateMode::Const(Some(a.line + 1)))?;
            }
        }
        Ok(())
    }

    /// Const-mode walks a const-expr slot (closures hit the shape
    /// gates; `attr` overrides the error line); runtime mode walks
    /// bodies where `function(){}` is legal but decl defaults inside
    /// stay gated.
    fn gate_expr(e: &Expr, m: &GateMode) -> Result<(), PhpError> {
        match e {
            Expr::Closure(c) => {
                if let GateMode::Const(attr) = m {
                    let line = attr.unwrap_or(c.decl.line);
                    if c.arrow {
                        return Err(PhpError::compile_fatal(
                            "Constant expression contains invalid operations",
                            line,
                        ));
                    }
                    if !c.is_static {
                        return Err(PhpError::compile_fatal(
                            "Closures in constant expressions must be static",
                            line,
                        ));
                    }
                    if !c.uses.is_empty() {
                        return Err(PhpError::compile_fatal(
                            "Cannot use(...) variables in constant expression",
                            line,
                        ));
                    }
                }
                Self::gate_fn_decl(&c.decl)
            }
            Expr::AnonClass(d) => Self::gate_class_decl(d),
            Expr::Assign { target, value, .. } => {
                Self::gate_expr(target, m)?;
                Self::gate_expr(value, m)
            }
            Expr::Binary { l, r, .. } => {
                Self::gate_expr(l, m)?;
                Self::gate_expr(r, m)
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
            | Expr::Include { e, .. } => Self::gate_expr(e, m),
            Expr::Ternary { c, t, f } => {
                Self::gate_expr(c, m)?;
                if let Some(t) = t {
                    Self::gate_expr(t, m)?;
                }
                Self::gate_expr(f, m)
            }
            Expr::Call { name, args } => {
                Self::gate_expr(name, m)?;
                for a in args {
                    Self::gate_expr(a, m)?;
                }
                Ok(())
            }
            Expr::Index { e, i } => {
                Self::gate_expr(e, m)?;
                if let Some(i) = i {
                    Self::gate_expr(i, m)?;
                }
                Ok(())
            }
            Expr::Isset(v) => {
                for x in v {
                    Self::gate_expr(x, m)?;
                }
                Ok(())
            }
            Expr::List(v) => {
                for (k, x) in v.iter().flatten() {
                    if let Some(k) = k {
                        Self::gate_expr(k, m)?;
                    }
                    Self::gate_expr(x, m)?;
                }
                Ok(())
            }
            Expr::Exit(Some(e)) => Self::gate_expr(e, m),
            Expr::Yield { key, val } => {
                if let Some(k) = key {
                    Self::gate_expr(k, m)?;
                }
                if let Some(v) = val {
                    Self::gate_expr(v, m)?;
                }
                Ok(())
            }
            Expr::YieldFrom(e) => Self::gate_expr(e, m),
            Expr::Match { subject, arms } => {
                Self::gate_expr(subject, m)?;
                for a in arms {
                    for c in &a.conds {
                        Self::gate_expr(c, m)?;
                    }
                    Self::gate_expr(&a.result, m)?;
                }
                Ok(())
            }
            Expr::MethodCall {
                obj, name, args, ..
            } => {
                Self::gate_expr(obj, m)?;
                if let PropName::Expr(e) = name {
                    Self::gate_expr(e, m)?;
                }
                for a in args {
                    Self::gate_expr(a, m)?;
                }
                Ok(())
            }
            Expr::StaticCall { class, args, .. } => {
                Self::gate_expr(class, m)?;
                for a in args {
                    Self::gate_expr(a, m)?;
                }
                Ok(())
            }
            Expr::StaticCallDyn { class, name, args } => {
                Self::gate_expr(class, m)?;
                Self::gate_expr(name, m)?;
                for a in args {
                    Self::gate_expr(a, m)?;
                }
                Ok(())
            }
            Expr::StaticProp { class, name } => {
                Self::gate_expr(class, m)?;
                if let PropName::Expr(e) = name {
                    Self::gate_expr(e, m)?;
                }
                Ok(())
            }
            Expr::Prop { obj, name, .. } => {
                Self::gate_expr(obj, m)?;
                if let PropName::Expr(e) = name {
                    Self::gate_expr(e, m)?;
                }
                Ok(())
            }
            Expr::New { class, args } => {
                Self::gate_expr(class, m)?;
                for a in args {
                    Self::gate_expr(a, m)?;
                }
                Ok(())
            }
            Expr::ClassConst { class, .. } => Self::gate_expr(class, m),
            Expr::Instanceof { obj, class } => {
                Self::gate_expr(obj, m)?;
                Self::gate_expr(class, m)
            }
            Expr::ArrayLit(items) => {
                for (k, v) in items {
                    if let Some(k) = k {
                        Self::gate_expr(k, m)?;
                    }
                    Self::gate_expr(v, m)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// PHP binds a compilation unit's unconditional top-level function
    /// decls before executing it (bug23279's later-declared handler).
    /// A name collision is PHP's compile-time "Cannot redeclare" fatal.
    /// Early-bind one unconditional top-level `function` decl — Zend
    /// inserts into the function table while compiling the stmt, so a
    /// name collision is the compile-time 'Cannot redeclare' fatal.
    /// Called from the flow gate at the decl's position (flow.rs).
    fn hoist_func(&mut self, d: &FunctionDecl) -> Result<(), PhpError> {
        let _ = self.decl_type_checks(&d.name, d, None);
        let key = d.name.to_lowercase();
        if let Some(prev) = self.functions.get(&key) {
            // Early binding dies at compile time in Zend —
            // the include/eval arms attach the compile-
            // context backtrace to this error.
            return Err(PhpError::compile_fatal(
                format!(
                    "Cannot redeclare function {}() (previously declared in {}:{})",
                    d.name, prev.file, prev.line
                ),
                d.line,
            ));
        }
        let site = std::ptr::from_ref(d) as usize;
        let mut d = d.clone();
        d.file = self.cur_file.clone();
        self.functions.insert(key.clone(), Rc::new(d));
        self.early_bound_funcs.insert(key, (self.cur_unit_id, site));
        Ok(())
    }

    fn hoist_funcs(&mut self, stmts: &[Stmt]) -> Result<(), PhpError> {
        // Registration inside this pass is Zend's early-binding
        // compile phase — its link errors carry the compile-context
        // trace (compile_err_frames), while decls registering at exec
        // keep the live call chain.
        let saved_hoist = std::mem::replace(&mut self.in_hoist, true);
        let r = self.hoist_funcs_pass(stmts);
        self.in_hoist = saved_hoist;
        r
    }

    fn hoist_funcs_pass(&mut self, stmts: &[Stmt]) -> Result<(), PhpError> {
        for s in stmts {
            match s {
                // `namespace X { stmts }` parses as
                // Block[Namespace, Block[stmts]] — decls inside are still
                // unconditional top-level for early binding (ns_085).
                Stmt::Block(v) if matches!(v.first(), Some(Stmt::Namespace(_))) => {
                    for s in &v[1..] {
                        if let Stmt::Block(inner) = s {
                            self.hoist_funcs(inner)?;
                        }
                    }
                }
                // Early binding: unconditional top-level non-enum
                // classes with no parent/interfaces/traits register
                // before execution (namespaces/ns_060). Enums are
                // exec-bound in Zend — `enum A {} class A {}` lets the
                // class win early binding so the ENUM's exec site is
                // where the redeclare fatal lands.
                Stmt::Class(d)
                    if d.traits.is_empty()
                        && d.kind != crate::ast::ClassKind::Enum
                        && (d.implements.is_empty()
                            // `interface Y extends X` also early-binds
                            // once every parent interface is known —
                            // delaying it to exec leaves Y unregistered
                            // for the compile-pass compat checks of
                            // early-bound classes that follow
                            // (set_value_parameter_type_variance_006/007).
                            || (d.kind == crate::ast::ClassKind::Interface
                                && d.implements.iter().all(|i| {
                                    let il = i.to_lowercase();
                                    self.classes.contains_key(&il)
                                        || self.interfaces.contains_key(&il)
                                        || self.traits.contains_key(&il)
                                        || self
                                            .linking
                                            .iter()
                                            .any(|c| c.name.eq_ignore_ascii_case(&il))
                                })))
                        // And every type the decl's own signatures
                        // mention must be checkable — zend refuses
                        // early binding when prop/method/const types
                        // reference unresolved names so their variance
                        // verdicts run at exec-link instead
                        // (property_types_early_bind).
                        && self.decl_types_resolvable(d)
                        && match &d.parent {
                            // zend_try_early_binding: a class with no
                            // dependencies always binds; an extends-only
                            // class binds when its parent is already
                            // registered (no autoload at compile).
                            // Classes with interfaces/traits bind at
                            // exec — their link errors carry the live
                            // trace ('Class B contains N abstract
                            // method' for an interface method keeps the
                            // eval()/include() frame).
                            None => true,
                            Some(p) => {
                                let pl = p.to_lowercase();
                                self.classes.contains_key(&pl)
                                    || self.traits.contains_key(&pl)
                                    || self.interfaces.contains_key(&pl)
                                    || self
                                        .linking
                                        .iter()
                                        .any(|c| c.name.eq_ignore_ascii_case(&pl))
                            }
                        } =>
                {
                    let key = d.name.to_lowercase();
                    // Class-kind redeclares are EXEC-phase fatals in
                    // Zend (unlike function redeclares, which die inside
                    // the unit's compile): an occupied name just leaves
                    // the decl exec-bound so the dup hits the existing
                    // 'Cannot redeclare' check in stmt order — an
                    // earlier exec-bound decl's link error wins first.
                    if self.existing_class_site(&key).is_some() {
                        continue;
                    }
                    let site = Rc::as_ptr(d) as usize;
                    let mut d = (**d).clone();
                    for m in &mut d.methods {
                        let mut mm = (**m).clone();
                        mm.decl.file = self.cur_file.clone();
                        *m = Rc::new(mm);
                    }
                    // Link errors of an early-bound class are compile
                    // errors of this unit — propagate (Zend fails the
                    // whole compile; the decl site still no-ops at
                    // exec via early_bound_classes). Zend reports the
                    // class-decl line for them.
                    let saved_line = self.cur_line;
                    self.cur_line = d.line;
                    let r = self.register_class(Rc::new(d));
                    self.cur_line = saved_line;
                    r?;
                    self.early_bound_classes.insert(key, site);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Stamp a fresh compile-unit serial (a separate Zend op_array —
    /// include/eval/another run) and return the previous one so the
    /// caller can restore it after the unit finishes.
    fn begin_unit(&mut self) -> u64 {
        let id = self.next_unit_id;
        self.next_unit_id += 1;
        std::mem::replace(&mut self.cur_unit_id, id)
    }

    pub fn run(&mut self, stmts: &[Stmt]) -> RunResult {
        self.begin_unit();
        // hard_timeout ini is the absolute deadline (045).
        let ht = self.ini_bytes("hard_timeout");
        if ht > 0 {
            self.deadline =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(ht as u64));
            self.deadline_secs = ht;
        }
        if let Err(e) = Self::const_closure_gate(stmts)
            .and_then(|_| self.flow_gate(stmts))
            .and_then(|_| self.hoist_funcs(stmts))
        {
            let flow = self.err_flow(e);
            let mut result = self.finish(flow);
            if let Some(c) = self.run_shutdown() {
                result.exit_code = c;
            }
            return result;
        }
        let flow = self.exec_block(stmts);
        let mut result = self.finish(flow);
        if let Some(c) = self.run_shutdown() {
            result.exit_code = c;
        }
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
                // entirely — a handled exception exits cleanly
                // (bug23279 only covers the handler firing).
                if let Some(h) = self.exception_handler.clone() {
                    match self.call_value(&h, CallArgs::positional(vec![cell(v)])) {
                        Ok(_) => {
                            return RunResult {
                                exit_code: 0,
                                fatal: None,
                            };
                        }
                        // A handler that itself throws leaves THAT
                        // exception uncaught — still a fatal exit.
                        Err(e) => {
                            if let Some(nv) = self.pending_exception.take() {
                                self.uncaught(&nv);
                            } else {
                                self.print_fatal(&e);
                            }
                        }
                    }
                } else {
                    self.uncaught(&v);
                }
                RunResult {
                    exit_code: 255,
                    fatal: Some(PhpError::fatal("uncaught exception", 0)),
                }
            }
            Flow::Break(_) => {
                let e = PhpError::compile_fatal(
                    "'break' not in the 'loop' or 'switch' context",
                    self.cur_line,
                );
                self.last_err_file = self.diag_file();
                self.print_fatal(&e);
                RunResult {
                    exit_code: 255,
                    fatal: Some(e),
                }
            }
            Flow::Continue(_) => {
                let e = PhpError::compile_fatal(
                    "'continue' not in the 'loop' or 'switch' context",
                    self.cur_line,
                );
                self.last_err_file = self.diag_file();
                self.print_fatal(&e);
                RunResult {
                    exit_code: 255,
                    fatal: Some(e),
                }
            }
            Flow::Goto(l) => {
                let e = PhpError::compile_fatal(
                    format!("'goto' to undefined label '{}'", l),
                    self.cur_line,
                );
                self.last_err_file = self.diag_file();
                self.print_fatal(&e);
                RunResult {
                    exit_code: 255,
                    fatal: Some(e),
                }
            }
        }
    }

    /// Run registered shutdown functions then the deferred __destruct
    /// sweep. A shutdown function that exits or dies stops the rest,
    /// but destructors still run (Zend); the produced exit code —
    /// `exit(N)`'s N or 255 for a fatal — is returned so the caller can
    /// override the script's exit code (a shutdown `exit` rewrites even
    /// a main-path fatal's code).
    fn run_shutdown(&mut self) -> Option<i32> {
        // Shutdown functions and the deferred dtor sweep are invoked by
        // the engine — their callees' trace callsites are `[internal
        // function]`, like callbacks inside builtins.
        self.internal_cb += 1;
        let fns = std::mem::take(&mut self.shutdown_fns);
        let mut shutdown_code = None;
        for (f, args) in fns {
            if let Err(e) = self.call_value(&f, CallArgs::positional(args)) {
                shutdown_code = Some(match self.err_flow(e) {
                    Flow::Exit(c) => c,
                    Flow::Throw(v) => {
                        self.uncaught(&v);
                        255
                    }
                    _ => 255,
                });
                break;
            }
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
        // A __destruct that errors unwinds like a shutdown-function
        // error: exit(N) supplies the exit code, a throw prints its
        // uncaught block — and Zend stops the whole sweep after any
        // shutdown-time error, so later destructors do not run.
        let mut dtor_stop = false;
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
                if let Err(e) = self.method_invoke(o.clone(), "__destruct", CallArgs::empty()) {
                    shutdown_code = Some(match self.err_flow(e) {
                        Flow::Exit(c) => c,
                        Flow::Throw(v) => {
                            self.uncaught(&v);
                            255
                        }
                        _ => 255,
                    });
                    dtor_stop = true;
                    break;
                }
            }
        }
        self.globals.vars.clear();
        // Objects a dtor spawns may land in already-visited recycled
        // handle slots — rescan until a full pass runs nothing new
        // (bug51822/bug74053).
        if !dtor_stop {
            'sweep: loop {
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
                        if let Err(e) =
                            self.method_invoke(o.clone(), "__destruct", CallArgs::empty())
                        {
                            shutdown_code = Some(match self.err_flow(e) {
                                Flow::Exit(c) => c,
                                Flow::Throw(v) => {
                                    self.uncaught(&v);
                                    255
                                }
                                _ => 255,
                            });
                            break 'sweep;
                        }
                    }
                }
                if !progressed {
                    break;
                }
            }
        }
        if !self.mem_exceeded {
            self.flush_ob_all();
        }
        self.internal_cb -= 1;
        shutdown_code
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
                // A fatal/throw inside the dtor aborts the script
                // (Zend). While another exception unwinds Zend chains
                // the dtor error as `Next ...` — not yet modelled, so
                // it stays swallowed there (bug52361).
                if self.pending_exception.is_some() {
                    let _ = self.method_invoke(o.clone(), "__destruct", CallArgs::empty());
                } else {
                    self.method_invoke(o.clone(), "__destruct", CallArgs::empty())?;
                }
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
                // in-flight exception being unwound (bug52361) — Zend
                // chains it as `Next ...` (not yet modelled). On a
                // clean frame exit the dtor's error propagates and
                // aborts the script instead.
                let saved = self.pending_exception.take();
                let r = self.method_invoke(o.clone(), "__destruct", CallArgs::empty());
                let was_unwinding = saved.is_some();
                self.pending_exception = saved.or(self.pending_exception.take());
                if !was_unwinding {
                    r?;
                }
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

    /// `error_reporting` ini value → error_level mask: integer, or an
    /// E_* constant expression (`E_ALL & ~E_DEPRECATED`).
    pub fn ini_error_level(&self) -> i64 {
        let Some(raw) = self.ini.get("error_reporting") else {
            return self.error_level;
        };
        fn cst(name: &str) -> Option<i64> {
            Some(match name.to_ascii_uppercase().as_str() {
                "E_ERROR" => 1,
                "E_WARNING" => 2,
                "E_PARSE" => 4,
                "E_NOTICE" => 8,
                "E_CORE_ERROR" => 16,
                "E_CORE_WARNING" => 32,
                "E_COMPILE_ERROR" => 64,
                "E_COMPILE_WARNING" => 128,
                "E_USER_ERROR" => 256,
                "E_USER_WARNING" => 512,
                "E_USER_NOTICE" => 1024,
                "E_STRICT" => 2048,
                "E_RECOVERABLE_ERROR" => 4096,
                "E_DEPRECATED" => 8192,
                "E_USER_DEPRECATED" => 16384,
                "E_ALL" => 30719,
                _ => return None,
            })
        }
        // Tiny ini-expression evaluator: ~ & | with literals and E_*.
        fn eval(s: &str) -> Option<i64> {
            let s = s.trim();
            if let Ok(n) = s.parse::<i64>() {
                return Some(n);
            }
            // split on the lowest-precedence op not inside parens
            for (ops, f) in [
                ('|', |a: i64, b: i64| a | b),
                ('^', |a: i64, b: i64| a ^ b),
                ('&', |a: i64, b: i64| a & b),
            ] as [(char, fn(i64, i64) -> i64); 3]
            {
                let mut depth = 0i32;
                for (i, ch) in s.char_indices().rev() {
                    match ch {
                        ')' => depth += 1,
                        '(' => depth -= 1,
                        _ if ch == ops && depth == 0 => {
                            let (l, r) = s.split_at(i);
                            return Some(f(eval(l)?, eval(&r[1..])?));
                        }
                        _ => {}
                    }
                }
            }
            let t = s.trim();
            if let Some(inner) = t.strip_prefix('~') {
                return Some(!eval(inner)?);
            }
            if let Some(inner) = t.strip_prefix('!') {
                return Some((eval(inner)? == 0) as i64);
            }
            if t.starts_with('(') && t.ends_with(')') {
                return eval(&t[1..t.len() - 1]);
            }
            cst(t)
        }
        eval(raw).unwrap_or(self.error_level)
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
        // A `-d error_reporting=` ini applies at startup like Zend's
        // ini handler (the PHPT harness sets it per-test).
        if self.ini.contains_key("error_reporting") {
            let lv = self.ini_error_level();
            self.error_level = lv;
        }
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
                self.begin_unit();
                if let Err(e) = Self::const_closure_gate(&stmts)
                    .and_then(|_| self.flow_gate(&stmts))
                    .and_then(|_| self.hoist_funcs(&stmts))
                {
                    let flow = self.err_flow(e);
                    let mut res = self.finish(flow);
                    if let Some(c) = self.run_shutdown() {
                        res.exit_code = c;
                    }
                    return (res, None);
                }
                let flow = self.exec_block(&stmts);
                let rv = match &flow {
                    Flow::Return(v) => Some(v.clone()),
                    _ => None,
                };
                let mut res = self.finish(flow);
                if let Some(c) = self.run_shutdown() {
                    res.exit_code = c;
                }
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
        self.isset_quiet = 0;
        self.handler_writes.clear();
        self.handler_reads.clear();
        self.detached_dim = false;
        self.dim_key_conv.clear();
        self.pending_exception = None;
        self.call_trace.clear();
        self.deadline = None;
        self.loop_depth = 0;
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
        let _ = self.run_shutdown();
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

    /// Mark `c` as an IS_REFERENCE cell (`=&`, by-ref binds). The
    /// Weak pin keeps the cell's allocation alive so its address can
    /// never be recycled under a stale mark.
    pub(crate) fn mark_ref(&mut self, c: &Cell) {
        self.ref_cells
            .insert(Rc::as_ptr(c) as usize, Rc::downgrade(c));
        if self.ref_cells.len() > self.ref_cells_prune {
            self.ref_cells.retain(|_, w| w.strong_count() > 0);
            self.ref_cells_prune = (self.ref_cells.len() * 2).max(1024);
        }
    }

    /// Is `c` a live IS_REFERENCE cell? The Weak pins the allocation,
    /// so an upgraded ptr_eq proves the mark belongs to this cell.
    pub(crate) fn is_ref_cell(&self, c: &Cell) -> bool {
        self.ref_cells
            .get(&(Rc::as_ptr(c) as usize))
            .and_then(|w| w.upgrade())
            .map(|u| Rc::ptr_eq(&u, c))
            .unwrap_or(false)
    }

    /// Ptr-level variant for sites that only kept `Rc::as_ptr`.
    pub(crate) fn is_ref_ptr(&self, ptr: usize) -> bool {
        self.ref_cells
            .get(&ptr)
            .and_then(|w| w.upgrade())
            .map(|u| Rc::as_ptr(&u) as usize == ptr)
            .unwrap_or(false)
    }

    /// Global-scope var cell: a name the $GLOBALS table manages but
    /// globals.vars doesn't have yet materializes out of the table
    /// (a `$GLOBALS['x']=v` write IS `$x=`), while a name whose array
    /// entry died unsets the var on next lookup.
    fn global_var_cell(&mut self, name: &str) -> Option<Cell> {
        if self.globals_synced.contains(name) {
            let live = self
                .globals_arr
                .as_ref()
                .and_then(|a| a.borrow().get_cell(&ArrKey::Str(Rc::from(name))))
                .is_some();
            if !live {
                self.globals_synced.remove(name);
                if let Some(c) = self.globals.vars.remove(name) {
                    let v = c.borrow().clone();
                    drop(c);
                    let _ = self.destruct_dying_value(&v);
                }
                return None;
            }
        }
        if let Some(c) = self.globals.vars.get(name) {
            return Some(c.clone());
        }
        let arr = self.globals_arr.clone()?;
        let c = arr.borrow().get_cell(&ArrKey::Str(Rc::from(name)))?;
        self.globals_synced.insert(name.to_string());
        self.mark_ref(&c);
        self.globals.vars.insert(name.to_string(), c.clone());
        self.globals_order.push(name.to_string());
        Some(c)
    }

    /// Record a read of `c` while an error handler runs — a prior read
    /// links the binding for zend's pending-write semantics.
    pub(crate) fn touch_read(&mut self, c: &Cell) {
        if self.in_handler {
            self.handler_reads.insert(Rc::as_ptr(c) as usize, c.clone());
        }
    }

    /// Record a write of `c` while an error handler runs — a write
    /// without a prior read detaches any pending dim write whose
    /// container was this cell.
    pub(crate) fn touch_write(&mut self, c: &Cell) {
        if self.in_handler {
            let p = Rc::as_ptr(c) as usize;
            if !self.handler_reads.contains_key(&p) {
                self.handler_writes.insert(p, c.clone());
            }
        }
    }

    /// Does the variable name resolve to an existing cell?
    fn var_lookup(&mut self, name: &str) -> Option<Cell> {
        let r = if self.stack.is_empty() {
            if let Some(c) = self.global_var_cell(name) {
                return Some(c);
            }
            self.superglobal_cell(name)
        } else {
            self.cur()
                .vars
                .get(name)
                .cloned()
                .or_else(|| self.superglobal_cell(name))
        };
        if let Some(c) = &r {
            self.touch_read(c);
        }
        r
    }

    fn var_get(&mut self, name: &str) -> Result<Value, PhpError> {
        // `$g = $GLOBALS` snapshots (PHP 8.1): a cloned table whose
        // cells are copies — writes through $g never reach the real
        // globals.
        if name == "GLOBALS" {
            let snap = self.globals_array_val();
            if let Value::Array(a) = &snap {
                return Ok(Value::Array(Rc::new(RefCell::new(a.borrow().clone()))));
            }
            return Ok(snap);
        }
        // Global scope: the $GLOBALS table may know the var (a
        // `$GLOBALS['x']=v` write IS `$x=`), or may have killed a name
        // still sitting in vars (unset($GLOBALS['x'])).
        if self.stack.is_empty() {
            if let Some(c) = self.global_var_cell(name) {
                self.touch_read(&c);
                return Ok(c.borrow().clone());
            }
            if let Some(c) = self.superglobal_cell(name) {
                self.touch_read(&c);
                return Ok(c.borrow().clone());
            }
            if !self.is_quiet() {
                self.warn(&format!("Undefined variable ${}", name))?;
            }
            return Ok(Value::Null);
        }
        let found = self.cur().vars.get(name).cloned();
        match found {
            Some(c) => {
                self.touch_read(&c);
                Ok(c.borrow().clone())
            }
            None => match self.superglobal_cell(name) {
                Some(c) => {
                    self.touch_read(&c);
                    Ok(c.borrow().clone())
                }
                None => {
                    // Inside any function frame, a missing $this is a
                    // hard "Using $this when not in object context"
                    // Error; top-level warns (closure_005).
                    if name == "this" {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Using $this when not in object context",
                            0,
                        ));
                    }
                    if !self.is_quiet() {
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
        // Global scope: a `$GLOBALS['x']=v` write materializes $x's
        // cell (and a dead table entry suppresses a stale var).
        if is_global {
            if let Some(c) = self.global_var_cell(name) {
                return c;
            }
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
            // Entries alias globals.vars cells — the shared table must
            // never CoW-split, so it's the deliberately-shared is_ref
            // kind; the cells are ref-marked so dup paths re-bind.
            a.is_ref = true;
            for n in names {
                if let Some(c) = self.globals.vars.get(&n).cloned() {
                    self.mark_ref(&c);
                    let key = ArrKey::Str(n.clone().into());
                    // The var's cell may have been rebound (`$x =& $y`)
                    // — the table slot follows it rather than having
                    // the new value written into the stale slot cell
                    // (030's aliasing must survive a sync).
                    if a.get_cell(&key)
                        .map(|s| !Rc::ptr_eq(&s, &c))
                        .unwrap_or(false)
                    {
                        a.bind_cell(key, c);
                    } else {
                        a.set_cell(key, c);
                    }
                    self.globals_synced.insert(n);
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
                self.mark_ref(&c);
                self.globals.vars.insert(n.clone(), c);
                self.globals_synced.insert(n.clone());
                self.globals_order.push(n);
            }
        }
        Value::Array(arr)
    }

    /// Peek without creating.
    fn var_cell_opt(&mut self, name: &str) -> Option<Cell> {
        if self.stack.is_empty() {
            if let Some(c) = self.global_var_cell(name) {
                return Some(c);
            }
            return self.superglobal_cell(name);
        }
        match self.stack.last().unwrap().vars.get(name) {
            Some(c) => Some(c.clone()),
            None => self.superglobal_cell(name),
        }
    }

    fn var_set(&mut self, name: &str, v: Value) {
        match self.var_cell_opt(name) {
            Some(c) => {
                self.touch_write(&c);
                *c.borrow_mut() = v;
            }
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
                self.touch_write(&c);
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
                // Closure statics are per-INSTANCE: each `function(){}`
                // eval creates its own table seeded from the decl's
                // defaults (zend_create_closure).
                if let Some(rc) = &f.closure_rc {
                    return format!("{}\u{0}c{}", f.fn_name, rc.id.get());
                }
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
        } else if self.live_io {
            use std::io::Write;
            let mut so = std::io::stdout().lock();
            let _ = so.write_all(b);
            let _ = so.flush();
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
                previous: None,
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
            // `callback:` binds the builtin's own first param like any
            // named arg; the rest of the names behave per the stub
            // variadic — call_user_func forwards them to the callee
            // ('+'), forward_static_call rejects them ('*').
            let fwd = name == "forward_static_call";
            let cb_named = args.named.iter().position(|(n, ..)| n == "callback");
            let cb = if let Some(c) = args.cells.first() {
                if cb_named.is_some() {
                    self.call_trace.pop();
                    return self.fail(PhpError::uncaught(
                        "Error",
                        "Named parameter $callback overwrites previous argument",
                        0,
                    ));
                }
                c.borrow().clone()
            } else if let Some(i) = cb_named {
                args.named[i].1.borrow().clone()
            } else {
                self.call_trace.pop();
                return self.fail(PhpError::uncaught(
                    "ArgumentCountError",
                    format!(
                        "{}() expects at least 1 argument, {} given",
                        name,
                        args.cells.len()
                    ),
                    0,
                ));
            };
            if fwd && args.named.iter().any(|(n, ..)| n != "callback") {
                self.call_trace.pop();
                return self.fail(PhpError::uncaught(
                    "ArgumentCountError",
                    format!("{}() does not accept unknown named parameters", name),
                    0,
                ));
            }
            let ca = CallArgs {
                cells: args.cells[1.min(args.cells.len())..].to_vec(),
                named: args
                    .named
                    .iter()
                    .filter(|(n, ..)| n != "callback")
                    .cloned()
                    .collect(),
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
            // Zend's `f` flag validates the callback eagerly with a
            // TypeError before any callee work; forward_static_call's
            // autoloader probe propagates instead of wrapping.
            if !self.is_callable_value(&cb) {
                if fwd {
                    if let Some(pe) = self.take_callable_probe_err() {
                        self.call_trace.pop();
                        return self.fail(pe);
                    }
                }
                let msg = format!(
                    "{}(): Argument #1 ($callback) must be a valid callback, {}",
                    name,
                    self.zpp_callback_detail(&cb)
                );
                let e = self.exception("TypeError", &msg);
                let te = self.throw(e);
                let r = self.fail(te);
                self.call_trace.pop();
                return r;
            }
            if fwd && self.caller_scope_name().is_none() {
                self.call_trace.pop();
                return self.fail(PhpError::uncaught(
                    "Error",
                    "Cannot call forward_static_call() when no class scope is active",
                    0,
                ));
            }
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
                    // Probe errors belong to THIS param's check only.
                    self.callable_probe_err = None;
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
                            // A throwing autoloader propagates through
                            // the probe (usort/array_map/... — zend
                            // re-raises it rather than TypeError-ing).
                            if let Some(pe) = self.take_callable_probe_err() {
                                self.call_trace.pop();
                                return self.fail(pe);
                            }
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
                self.emit_cmp_notices()?;
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
                self.emit_cmp_notices()?;
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
        // "given" counts positional args plus named args that bound to
        // a fixed param — a name landing in the variadic tail is not
        // counted (call_user_func(x:) reports "0 given").
        let mut given = args.cells.len();
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
                    given += 1;
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
                    // A named arg bound to a LATER param leaves this
                    // required one skipped: `#N not passed`. Nothing
                    // bound after it is just the plain arity error
                    // (`substr(string:)` => "expects at least 2").
                    if slot.iter().skip(i + 1).any(|s| s.is_some()) {
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
                    return Err(arity_err(given, false));
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
            // constant expression itself, innermost on the real stack
            // (property_initializer_scope_002:
            // `#0 %s(%d): [constant expression]()`).
            let const_frame = self.class_const_ctx > 0;
            if const_frame {
                self.call_trace.push(TraceFrame {
                    function: "[constant expression]".to_string(),
                    class: None,
                    ty: String::new(),
                    file: self.diag_file(),
                    line: self.cur_line as u32,
                    args: Vec::new(),
                    named_args: Vec::new(),
                    internal: true,
                });
            }
            // Internal errors raised as exceptions become real throwables so
            // userland `catch` blocks can intercept them.
            let v = self.exception(class, &e.message);
            if const_frame {
                self.call_trace.pop();
            }
            if let Value::Object(o) = &v {
                if let Some(ObjectInternal::Exception {
                    file,
                    line,
                    trace,
                    thrown,
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
                    // A const-expr Error inside eval'd code attributes
                    // to the CALLER file + the eval string's own line —
                    // the `FILE(N) : eval()'d code` composite only
                    // shows in trace frames (probe m9).
                    if self.class_const_ctx > 0 && self.cur_file.contains("eval()'d code") {
                        let caller = self
                            .cur_file
                            .strip_suffix(" : eval()'d code")
                            .and_then(|s| s.rsplit_once('('))
                            .map(|(f, _)| f.to_string())
                            .unwrap_or_else(|| self.cur_file.clone());
                        *file = caller;
                        *line = self.cur_line as u32;
                        *thrown = self.cur_line as u32;
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
}

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

/// SPL iterator-wrapper classes expressed in plain PHP and eval'd once
/// per Interp (Interp::new). Written in PHP because they are pure
/// delegation over Iterator methods; the engine supplies the leaves
/// (DirectoryIterator/FilesystemIterator/RecursiveDirectoryIterator).
const SPL_ITERATOR_PRELUDE: &str = r#"
interface OuterIterator extends Iterator {
    public function getInnerIterator();
}
class IteratorIterator implements OuterIterator {
    protected $inner;
    public function __construct($iterator) {
        $it = $iterator;
        while ($it instanceof IteratorAggregate) {
            $it = $it->getIterator();
        }
        $this->inner = $it;
    }
    public function getInnerIterator() { return $this->inner; }
    public function __call($func, $params) { return $this->inner->$func(...$params); }
    public function rewind() { $this->inner->rewind(); }
    public function valid() { return $this->inner->valid(); }
    public function current() { return $this->inner->current(); }
    public function key() { return $this->inner->key(); }
    public function next() { $this->inner->next(); }
}
abstract class FilterIterator extends IteratorIterator {
    abstract public function accept();
    public function rewind() { $this->inner->rewind(); $this->fetch(); }
    public function next() { $this->inner->next(); $this->fetch(); }
    private function fetch() {
        while ($this->inner->valid() && !$this->accept()) {
            $this->inner->next();
        }
    }
}
abstract class RecursiveFilterIterator extends FilterIterator implements RecursiveIterator {
    public function hasChildren() { return $this->inner->hasChildren(); }
    // SPL: children come back wrapped in the same filter class.
    public function getChildren() {
        $cls = static::class;
        return new $cls($this->inner->getChildren());
    }
}
class CallbackFilterIterator extends FilterIterator {
    private $callback;
    public function __construct($iterator, $callback) {
        parent::__construct($iterator);
        $this->callback = $callback;
    }
    public function accept() {
        return ($this->callback)($this->current(), $this->key(), $this->inner);
    }
}
class RecursiveIteratorIterator implements OuterIterator {
    const LEAVES_ONLY = 0;
    const SELF_FIRST = 1;
    const CHILD_FIRST = 2;
    const CALL_TOSTRING = 4;
    const CATCH_GET_CHILD = 8;
    private $stack = [];
    private $emitted = [];
    private $mode;
    private $flags;
    private $yieldParent = false;
    public function __construct($iterator, $mode = 0, $flags = 0) {
        $it = $iterator;
        while ($it instanceof IteratorAggregate) {
            $it = $it->getIterator();
        }
        $this->mode = $mode;
        $this->flags = $flags;
        $this->stack = [$it];
        $this->emitted = [false];
        $it->rewind();
        $this->descend();
    }
    private function top() { return $this->stack[count($this->stack) - 1]; }
    public function getDepth() { return count($this->stack) - 1; }
    public function getSubIterator($level = null) {
        $i = $level === null ? count($this->stack) - 1 : $level;
        return $this->stack[$i] ?? null;
    }
    public function getInnerIterator() { return $this->top(); }
    private function descend() {
        while (count($this->stack) > 0) {
            $top = $this->top();
            if (!$top->valid()) {
                array_pop($this->stack);
                array_pop($this->emitted);
                $this->yieldParent = false;
                if (count($this->stack) === 0) {
                    return;
                }
                $i = count($this->stack) - 1;
                if ($this->mode === self::CHILD_FIRST && !$this->emitted[$i]) {
                    $this->emitted[$i] = true;
                    $this->yieldParent = true;
                    return;
                }
                $this->top()->next();
                continue;
            }
            if ($top instanceof RecursiveIterator && $top->hasChildren()) {
                $i = count($this->stack) - 1;
                if ($this->mode === self::SELF_FIRST && !$this->emitted[$i]) {
                    $this->emitted[$i] = true;
                    $this->yieldParent = true;
                    return;
                }
                try {
                    $child = $top->getChildren();
                } catch (Throwable $e) {
                    if (!($this->flags & self::CATCH_GET_CHILD)) {
                        throw $e;
                    }
                    $top->next();
                    continue;
                }
                $child->rewind();
                $this->stack[] = $child;
                $this->emitted[] = false;
                continue;
            }
            $this->yieldParent = false;
            return;
        }
        $this->yieldParent = false;
    }
    public function valid() {
        return count($this->stack) > 0 && $this->top()->valid();
    }
    public function current() {
        return count($this->stack) > 0 ? $this->top()->current() : null;
    }
    public function key() {
        return count($this->stack) > 0 ? $this->top()->key() : null;
    }
    public function next() {
        if (count($this->stack) === 0) {
            return;
        }
        if ($this->yieldParent && $this->mode === self::SELF_FIRST) {
            $top = $this->top();
            try {
                $child = $top->getChildren();
            } catch (Throwable $e) {
                if (!($this->flags & self::CATCH_GET_CHILD)) {
                    throw $e;
                }
                $top->next();
                $this->yieldParent = false;
                $this->descend();
                return;
            }
            $child->rewind();
            $i = count($this->stack) - 1;
            $this->emitted[$i] = false;
            $this->stack[] = $child;
            $this->emitted[] = false;
            $this->yieldParent = false;
            $this->descend();
            return;
        }
        if ($this->yieldParent) {
            $i = count($this->stack) - 1;
            $this->emitted[$i] = false;
            $this->yieldParent = false;
            $this->top()->next();
            $this->descend();
            return;
        }
        $this->top()->next();
        $this->descend();
    }
    public function rewind() {
        $this->stack = [$this->stack[0]];
        $this->emitted = [false];
        $this->stack[0]->rewind();
        $this->yieldParent = false;
        $this->descend();
    }
}
class AppendIterator extends IteratorIterator {
    private $its = [];
    private $idx = 0;
    public function __construct() {}
    public function append($it) {
        while ($it instanceof IteratorAggregate) {
            $it = $it->getIterator();
        }
        $this->its[] = $it;
        if ($this->idx === 0 && count($this->its) === 1) {
            $this->inner = $it;
        }
    }
    private function sync() {
        while ($this->idx < count($this->its) && !$this->its[$this->idx]->valid()) {
            $this->idx++;
        }
        $this->inner = $this->idx < count($this->its) ? $this->its[$this->idx] : null;
    }
    public function rewind() {
        foreach ($this->its as $it) {
            $it->rewind();
        }
        $this->idx = 0;
        $this->sync();
    }
    public function valid() {
        return $this->idx < count($this->its) && $this->its[$this->idx]->valid();
    }
    public function next() {
        if ($this->idx < count($this->its)) {
            $this->its[$this->idx]->next();
        }
        $this->sync();
    }
    public function getInnerIterator() {
        return $this->idx < count($this->its) ? $this->its[$this->idx] : null;
    }
}
class EmptyIterator implements Iterator {
    public function current() { return null; }
    public function key() { return null; }
    public function next() {}
    public function rewind() {}
    public function valid() { return false; }
}
class SplObjectStorage implements Countable, Iterator, ArrayAccess {
    private array $objs = [];
    private array $data = [];
    private int $pos = 0;
    private int $idx = 0;
    private function hashOf($obj) {
        if (!is_object($obj)) {
            throw new TypeError('SplObjectStorage::offsetSet(): Argument #1 ($object) must be of type object');
        }
        return spl_object_id($obj);
    }
    public function attach($object, $data = null) { $this->offsetSet($object, $data); }
    public function detach($object) { $this->offsetUnset($object); }
    public function contains($object) { return $this->offsetExists($object); }
    public function offsetExists($obj): bool { return isset($this->objs[$this->hashOf($obj)]); }
    public function offsetSet($obj, $data = null): void {
        $h = $this->hashOf($obj);
        if (!isset($this->objs[$h])) {
            $this->objs[$h] = $obj;
        }
        $this->data[$h] = $data;
    }
    public function offsetGet($obj) {
        $h = $this->hashOf($obj);
        if (!isset($this->objs[$h])) {
            throw new UnexpectedValueException('Object not found');
        }
        return $this->data[$h];
    }
    public function offsetUnset($obj): void {
        $h = $this->hashOf($obj);
        unset($this->objs[$h], $this->data[$h]);
    }
    public function getHash($obj) { return spl_object_hash($obj); }
    public function count(): int { return count($this->objs); }
    // zend's info slot hangs off the CURRENT iterator element.
    public function setInfo($data) {
        $objs = array_values($this->objs);
        if ($this->idx < count($objs)) {
            $this->data[$this->hashOf($objs[$this->idx])] = $data;
        }
    }
    public function getInfo() {
        $objs = array_values($this->objs);
        return $this->idx < count($objs)
            ? ($this->data[$this->hashOf($objs[$this->idx])] ?? null)
            : null;
    }
    // Iteration: key() is a 0-based index, current() the stored object.
    public function rewind(): void { $this->pos = 0; $this->idx = 0; }
    public function valid(): bool { return $this->idx < count($this->objs); }
    public function current() { return array_values($this->objs)[$this->idx]; }
    public function key(): int { return $this->idx; }
    public function next(): void { $this->idx++; }
    public function addAll($storage) {
        foreach ($storage as $obj) { $this->offsetSet($obj, $storage->getInfo()); }
    }
    public function removeAll($storage) {
        foreach ($storage as $obj) { $this->offsetUnset($obj); }
    }
    public function removeAllExcept($storage) {
        foreach ($this->objs as $h => $obj) {
            if (!$storage->offsetExists($obj)) { unset($this->objs[$h], $this->data[$h]); }
        }
    }
    // zend serializes SplObjectStorage as [flat obj,info pairs, dynamic props].
    public function __serialize(): array {
        $st = [];
        foreach ($this->objs as $h => $o) {
            $st[] = $o;
            $st[] = $this->data[$h];
        }
        $props = get_object_vars($this);
        unset($props['objs'], $props['data'], $props['pos'], $props['idx']);
        return [$st, $props];
    }
    public function __unserialize(array $pairs): void {
        $this->objs = []; $this->data = [];
        $this->pos = 0; $this->idx = 0;
        $st = $pairs[0] ?? [];
        for ($i = 0; $i + 1 < count($st); $i += 2) {
            $this->offsetSet($st[$i], $st[$i + 1]);
        }
        foreach (($pairs[1] ?? []) as $k => $v) { $this->$k = $v; }
    }
}
class SplFixedArray implements ArrayAccess, Iterator, Countable {
    private array $data;
    private int $pos = 0;
    public function __construct(int $size = 0) {
        $this->data = array_fill(0, max(0, $size), null);
    }
    public static function fromArray(array $array, bool $preserveKeys = true) {
        $a = new self($preserveKeys ? count($array) : 0);
        if ($preserveKeys) {
            $max = 0;
            foreach ($array as $k => $v) {
                if (!is_int($k) || $k < 0) {
                    throw new InvalidArgumentException('array must contain only positive integer keys');
                }
                $max = max($max, $k + 1);
            }
            $a = new self($max);
            foreach ($array as $k => $v) { $a->data[$k] = $v; }
        } else {
            $a = new self(count($array));
            $i = 0;
            foreach ($array as $v) { $a->data[$i++] = $v; }
        }
        return $a;
    }
    public function toArray(): array { return $this->data; }
    public function getSize(): int { return count($this->data); }
    public function setSize(int $size): bool {
        $size = max(0, $size);
        $cur = count($this->data);
        if ($size > $cur) {
            $this->data = array_merge($this->data, array_fill(0, $size - $cur, null));
        } else {
            $this->data = array_slice($this->data, 0, $size);
        }
        return true;
    }
    private function normKey($key): int {
        if (is_object($key)) {
            throw new TypeError('Illegal SplFixedArray index type');
        }
        return (int) $key;
    }
    public function offsetExists($key): bool {
        $k = $this->normKey($key);
        return $k >= 0 && $k < count($this->data) && $this->data[$k] !== null;
    }
    public function offsetGet($key) {
        $k = $this->normKey($key);
        if ($k < 0 || $k >= count($this->data)) {
            throw new RuntimeException('Index invalid or out of range');
        }
        return $this->data[$k];
    }
    public function offsetSet($key, $value): void {
        $k = $this->normKey($key);
        if ($k < 0 || $k >= count($this->data)) {
            throw new RuntimeException('Index invalid or out of range');
        }
        $this->data[$k] = $value;
    }
    public function offsetUnset($key): void {
        $k = $this->normKey($key);
        if ($k < 0 || $k >= count($this->data)) {
            throw new RuntimeException('Index invalid or out of range');
        }
        $this->data[$k] = null;
    }
    public function count(): int { return count($this->data); }
    public function rewind(): void { $this->pos = 0; }
    public function valid(): bool { return $this->pos < count($this->data); }
    public function current() { return $this->data[$this->pos]; }
    public function key(): int { return $this->pos; }
    public function next(): void { $this->pos++; }
}
"#;
