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
    PhpCallable, PhpClass, PhpObject, PhpResource, PhpStr, TraceFrame, Value,
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
    /// Temporaries that must outlive the call (an unpacked `...`
    /// source array holds its elements' cells — zend keeps the zval
    /// alive until the call returns, so its table stays charged
    /// through the builtin's own allocs).
    pub hold: Vec<Value>,
    /// Vm-site tokens — one per vm_stack push this call made (the
    /// frame push plus each `...` unpack site's arg span). Each token's
    /// MemCharge carries the push's arena bookkeeping (see `vm_stack`),
    /// repaid when the owning frame pops or a never-dispatched call's
    /// tokens die at sweep.
    pub vm_sites: Vec<Rc<VmSite>>,
    /// Arena slots this call has pushed so far — frame overhead plus
    /// args — used by `vm_call_push` to compute the extend delta and
    /// the span a copy_call_frame moves.
    pub vm_slots: u64,
    /// Diagnostic line of the last-evaluated argument — the deepest
    /// line marker reached while building this list. Zend sites the
    /// diagnostics of a compile-specialized literal call (sprintf rope)
    /// at the line of its final operand, not the call's first token.
    pub end_line: usize,
    /// The args arrived as a verbatim array send (`call_user_func_array`,
    /// Reflection invokeArgs/newInstanceArgs) — element cells ARE the
    /// source array's buckets, so by-value packs (`__call`'s $a) keep
    /// IS_REFERENCE elements (bug50394). A `...` unpack or normal send
    /// separates them instead (oracle: `...[&$w]` packs `string`, cufa
    /// packs `&string`).
    pub verbatim_elems: bool,
}

/// Token anchoring a vm_stack arena record — one per call push (on the
/// CallArgs/Frame's vm_sites list) and one per live segment (on its
/// VmSeg). The payload lives on the token's MemCharge.
pub struct VmSite;

/// One live segment of zend's request-scoped vm_stack arena.
struct VmSeg {
    /// emalloc'd bytes — the request a limit trip on this segment
    /// reports (256KB pages, aligned up for oversized calls).
    size: u64,
    /// Occupied bytes including the 32B segment header.
    used: u64,
    /// This segment's emalloc charge — dies with the segment (emptied
    /// by a copy_call_frame, or freed at its owning frame's pop).
    tok: Rc<VmSite>,
}

/// ZEND_VM_STACK_PAGE_SIZE — zend vm_stack pages are 256KB.
const VM_PAGE: u64 = 262_144;
/// Arena slots one call frame costs — zend's ZEND_CALL_FRAME_SLOT plus
/// the callee's op_array CV/TMP span. ponytail: flat estimate — the
/// real span varies per decl, so a call landing near a page edge can
/// cross a page one push late.
const VM_FRAME_SLOTS: u64 = 5;

impl CallArgs {
    pub fn positional(cells: Vec<Cell>) -> Self {
        Self {
            cells,
            named: Vec::new(),
            trav_cells: Vec::new(),
            nonref_cells: Vec::new(),
            hold: Vec::new(),
            vm_sites: Vec::new(),
            vm_slots: 0,
            end_line: 0,
            verbatim_elems: false,
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
    vars: crate::value::FxMap<String, Cell>,
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
    /// Anchor identifying the fn/method DECL this frame executes —
    /// `Rc::as_ptr` of the registered decl (stable across the per-call
    /// `m.decl.clone()` method dispatch takes, so `static` site
    /// identity survives cloned bodies).
    decl_site: usize,
    /// This frame is a generator body invoked by the engine's resume —
    /// Zend renders it `[internal function]: fn(args)` in backtraces
    /// (the resume call, not a userland call, carries the visible frame).
    gen_body: bool,
    /// vm_stack arg-page tokens moved here from CallArgs at bind —
    /// zend keeps a call's arg zvals on vm_stack until the FRAME is
    /// destroyed, so the charges outlive arg binding and die with
    /// this frame's pop.
    vm_sites: Vec<Rc<VmSite>>,
}

impl Frame {
    fn new(fn_name: String) -> Self {
        Self {
            vars: crate::value::FxMap::default(),
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
            decl_site: 0,
            gen_body: false,
            vm_sites: Vec::new(),
        }
    }
}

/// stream_filter_remove's binding record: (stream res id, filter name,
/// the filter resource itself, the stream resource for ->stream props).
pub(crate) type FilterBinding = (
    u64,
    String,
    Rc<RefCell<PhpResource>>,
    Rc<RefCell<PhpResource>>,
);

/// Liveness probe for a charged allocation: weak ref to the owning
/// Rc — returns false once every strong handle died (zend's efree).
pub(crate) type MemProbe = Box<dyn Fn() -> bool>;

/// A charge booked against a live Rc. `inner` is footprint inside
/// committed chunks, `huge` footprint in dedicated huge segments
/// (zend serves requests past ~2MB from their own mmap'ed segment),
/// `table_req` remembers the last arData-capacity request so realloc
/// accounting can retire it.
struct MemCharge {
    inner: u64,
    huge: u64,
    table_req: u64,
    /// vm_stack rollback for a call's arena push — (segment token key,
    /// slots pushed, minted-segment token key or 0). Applied when the
    /// owning frame pops or the token dies.
    vm: Option<(usize, u64, usize)>,
    /// Address space a huge segment may still extend into: zend's
    /// mremap grows a mapping only until the next occupied range —
    /// modelled as its footprint at (re)placement plus the freed
    /// predecessor's hole and the mapping slack a fresh mmap leaves
    /// below the next VMA. Unused for in-chunk charges.
    seg_cap: u64,
    /// Index into `mem_chunks` of the chunk holding this charge's
    /// `inner` run (usize::MAX when it lives outside the chunks).
    chunk: usize,
    probe: MemProbe,
}

/// A slot in `mem_seg_order`: either a live huge segment (`hole` = 0,
/// `key` = its charge's tracked key) or the still-free span a freed
/// segment left at that placement (`hole` > 0). The credit is lazy —
/// zend's top-down mmap reoccupies a fitting hole with the next fresh
/// segment before the segment below can ever extend into it — so a
/// hole waits in its slot for seg_place to steal or seg_drain to
/// merge at the heir's own grow.
#[derive(Clone, Copy)]
struct SegSlot {
    /// The owning charge's tracked key — `usize::MAX` marks a hole
    /// slot (no live owner).
    key: usize,
    hole: u64,
}

/// zend_mm chunk: ZEND_MM_CHUNK_SIZE (2MB, 512 pages).
const MM_CHUNK: u64 = 2 * 1024 * 1024;
/// zend_mm_max_large_size: largest request served from chunk page
/// runs; bigger requests get a dedicated segment.
const MM_MAX_LARGE: u64 = 2_093_056;
/// zend_mm_max_small_size: largest bin-bucketed request.
const MM_SMALL: u64 = 3072;
/// Heap usage a fresh script observes under memory_get_usage()
/// (~450-470KB of engine state) and its in-chunk share. ponytail:
/// lumped constants — PR #88's canonical accounting models the true
/// baseline from actual engine state.
const MM_BASE_USED: u64 = 463_136;
/// Page-space the engine state occupies inside the first chunk —
/// small-bin usage fragments across page runs, so this exceeds the
/// usage figure. Calibrated to the oracle boundary: a fresh ~1.43MB
/// string still fits the first chunk, ~1.45MB forces a new one.
const MM_BASE_CHUNK: u64 = 655_360;
/// zend's ob smart_string starts each level with a 16KB buffer
/// (php_output's initial capacity) and ereallocs on append: one
/// extra 16KB page per incremental crossing, or straight to the
/// page-aligned content on a single big write — calibrated to the
/// oracle's capacity curve (16384→32768 at len 16384, align4096
/// beyond).
const OB_INIT_CAP: u64 = 16384;
/// Per open level's non-buffer bookkeeping (php_output_handler
/// struct and friends) — folded into the boot charge so the level's
/// erealloc figure stays the buffer's own request.
const OB_LEVEL_STRUCT: u64 = 128;
/// ob machinery booked while at least one real level is open — the
/// output runtime stack plus handler slots (~2.6K).
const OB_OPEN_STACK: u64 = 2656;
/// Stub retained after the last level pops — oracle keeps ~128 once
/// ob has ever been used.
/// ponytail: lumped — post-pop residuals jitter ±32 across scripts
/// (freed-run fragmentation); the real page map PR #88 owns could
/// only pick one.
const OB_RESID: u64 = 128;
/// Output machinery zend retains once any bytes reached the real
/// stdout sink (sapi write path) — +32 on the first emit, none on
/// stderr.
const EMIT_RESID: u64 = 32;
/// emalloc request for a new dynamic-property bucket in an object's
/// slot table.
pub(crate) const OBJ_SLOT_REQ: u64 = 32;

/// `static` decl site registry: fn-statics key → var name → set of
/// (compile-unit serial, decl-origin anchor, stmt line). See
/// Stmt::Static for the duplicate-declaration rule it enforces.
type StaticDeclSites =
    HashMap<String, HashMap<String, std::collections::HashSet<(u64, usize, usize)>>>;

pub struct Interp<'a> {
    pub file: &'a str,
    globals: Frame,
    stack: Vec<Frame>,
    pub functions: crate::value::FxMap<String, Rc<FunctionDecl>>,
    classes: crate::value::FxMap<String, Rc<PhpClass>>,
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
    /// binding): name → (compile unit, AST decl ptr), so only the SAME
    /// decl site in the SAME unit no-ops on execution — a different
    /// decl site claiming the name still hits the 'Cannot redeclare'
    /// check (namespaces/ns_060). The unit guards against a freed AST
    /// Vec recycling the node ptr across re-parses (a second eval's
    /// decl can land on the freed allocation of the first's).
    early_bound_classes: HashMap<String, (u64, usize)>,
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
    constants: crate::value::FxMap<String, Value>,
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
    /// (file, line) of the lazy class-const/prop/static decl currently
    /// evaluating — an uncaught Error from inside it attributes to the
    /// declaration site (zend reports the decl's own file+line, with
    /// the [constant expression] pseudo-frame pointing at resolution).
    const_decl_ctx: Option<(String, u32)>,
    /// Resolution line of the const-expr currently evaluating — the
    /// `[constant expression]` pseudo-frame sites here (the access that
    /// triggered the lazy init), not inside the decl being evaluated
    /// (gh8821: `#0 file(11): [constant expression]()` where 11 is the
    /// `new` call, not the const decl).
    const_init_site: Option<usize>,
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
    /// re-validating it (bug72685). Weak keys drop when the storage
    /// dies — a recycled pointer then fails upgrade() and re-validates,
    /// so nothing stays pinned and the map can't grow without bound.
    pub valid_utf8: std::collections::HashMap<usize, std::rc::Weak<[u8]>>,
    /// Raw request body for php://input — serve mode fills it.
    pub php_input: std::rc::Rc<Vec<u8>>,
    /// Real upload tmp paths created this request — is_uploaded_file()
    /// and move_uploaded_file() check membership.
    pub uploads: Vec<std::path::PathBuf>,
    /// Per-stream chunk size set by stream_set_chunk_size(), keyed by
    /// resource id — the function returns the PREVIOUS size (zend
    /// default 8192).
    pub stream_chunk_sizes: std::collections::HashMap<u64, i64>,
    /// Filters attached by stream_filter_append/prepend, keyed by the
    /// STREAM resource id — zend's readfilters/writefilters chains.
    /// A stream with any entry is 'filtered' and every non-STDIO cast
    /// fails (cast.c:300).
    pub stream_filters: std::collections::HashMap<u64, Vec<crate::value::StreamFilter>>,
    /// Filter RESOURCE id → (stream resource id, filter name, the
    /// filter resource, the STREAM resource) — stream_filter_remove()
    /// detaches the right chain entry, fclose() invalidates held
    /// filter handles when their stream dies, and the flush in
    /// remove() hands filter() callbacks their ->stream prop (zend's
    /// stream zval on the filter call).
    pub stream_filter_bindings: std::collections::HashMap<u64, FilterBinding>,
    /// stream_filter_register() — name → userland class name (zend's
    /// BG(user_filter_map); the class is resolved lazily at attach).
    /// Insertion-ordered so stream_get_filters() lists them in the
    /// order they were registered.
    pub user_filter_map: Vec<(String, String)>,
    /// userfilter.bucket brigade resource id → the brigade's queue of
    /// bucket payloads — populated while a php_user_filter::filter()
    /// call is in flight.
    pub stream_brigades: std::collections::HashMap<u64, std::collections::VecDeque<Vec<u8>>>,
    /// userfilter.bucket resource id → the bucket's raw bytes — the
    /// backing for StreamBucket::$bucket.
    pub stream_buckets: std::collections::HashMap<u64, Vec<u8>>,
    /// The stream resource id a php_user_filter::filter() call is
    /// running on (zend's PHP_STREAM_FLAG_NO_FCLOSE): an fclose() on
    /// it from inside the callback warns 'cannot close the provided
    /// stream' and returns false.
    pub filter_no_fclose: Option<u64>,
    /// The builtin name zend's php_error_docref would use for warnings
    /// raised inside filter() calls ('fread(): Unprocessed filter
    /// buckets...'). fs::dispatch refreshes it per call.
    pub filter_warn_ctx: String,
    /// Stream resource ids whose php_user_filter::filter() call is in
    /// flight — zend fails stream reads re-entered on the stream the
    /// fill loop owns.
    pub stream_filter_busy: std::collections::HashSet<u64>,
    /// Streaming codec objects for FilterState::Codec entries, keyed
    /// by filter id (compressors can't clone). Seeded at attach and
    /// dropped with the chain entry.
    /// Live codec objects keyed (filter res id, read-chain flag) —
    /// an ALL-mode attach shares one res id across its two chain
    /// entries, so the direction disambiguates them.
    pub codec_states: std::collections::HashMap<(u64, bool), crate::builtins::fs::CodecState>,
    /// Live WeakReference wrapper per target object id — zend keeps a
    /// per-handle weakref list so repeated create() calls on the same
    /// live object return the identical wrapper (`===` true).
    pub weakrefs: std::collections::HashMap<u64, std::rc::Weak<RefCell<crate::value::PhpObject>>>,
    /// Output buffer stack for ob_*().
    ob_stack: Vec<ObLevel>,
    /// Buffers opened inside a generator body past a yield — they
    /// leave the real stack while the body is suspended and
    /// rematerialize when the consumer's cursor passes each open tag
    /// (Zend's buffers are global across suspends).
    suspended_obs: Vec<ObLevel>,
    /// One-time token for zend's retained ob machinery (the output
    /// stack and per-level handler structs) — created on the first
    /// real level, never released: oracle keeps ~128 after the last
    /// pop.
    ob_boot: Option<Rc<()>>,
    /// One-time token for the output machinery zend retains once
    /// bytes reach the real stdout sink — EMIT_RESID, never released.
    emit_boot: Option<Rc<()>>,
    /// Bytes reached the real stdout sink at least once (live_io's
    /// writes bypass `self.out`, so the sink can't be probed there).
    pub(crate) emit_seen: bool,
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
    globals_synced: crate::value::FxSet<String>,
    /// Set while a dim-read runs in a by-ref context (`$x =& $o['k']`):
    /// zend's read_dimension(BP_VAR_RW) silently creates missing
    /// buckets instead of warning.
    dim_by_ref: bool,
    /// `foreach ($x as &$v)` source fetch — zend treats it as a
    /// write-reference bind (uninit non-nullable typed props error
    /// 'by reference'; uninit *nullable* statics report 'undeclared').
    foreach_by_ref: bool,
    /// Autoload/lookup error swallowed by the last `is_callable_value`
    /// probe — re-raised when a `callable` param type rejects the arg.
    callable_probe_err: Option<(Value, PhpError)>,
    /// Function-scoped static storage: scope key → var → cell. The key
    /// is fn_statics_key() for a function's own op_array; eval/include
    /// unit code executing inside a frame suffixes `\0u{unit}` so each
    /// unit gets a fresh table, and top-level code uses the executing
    /// unit under the global scope key (Zend: static vars live in the
    /// op_array that declared them).
    pub(crate) statics: crate::value::FxMap<String, crate::value::FxMap<String, Cell>>,
    /// static-decl sites per function scope (fn key → var → decl
    /// (unit serial, stmt ptr)) — PHP fatals on a same-unit
    /// redeclaration at a different statement site. The unit serial is
    /// bumped at every parse boundary (include/eval/run) since Zend
    /// compiles each into a fresh op_array — a freed Vec may recycle
    /// the same stmt ptr across re-parses.
    static_decls: StaticDeclSites,
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
    /// putenv() overrides read back by getenv() (no real process-env
    /// mutation). `None` = tombstone from `putenv("KEY")` (unset),
    /// which shadows a same-named var in the real environment.
    env_overrides: HashMap<String, Option<String>>,
    /// Raw argv entries after the script path, for `getopt()`.
    pub script_args: Vec<String>,
    exception_handler: Option<Value>,
    in_handler: bool,
    /// The current dim write is detached — a handler mid-key-eval
    /// rebound or mutated the container, so the pending write lands on
    /// the stale slot: invisible and silent (assign_dim_014).
    detached_dim: bool,
    /// A throwable raised by a compound op's dim fetch engine call
    /// (offsetGet's own throw or its missing-key warn's handler) —
    /// zend's assign op keeps running with EG(exception) pending, so
    /// the write still lands before the throwable surfaces at op end.
    /// `(thrown value, error, live)`: the value rides the pair so a
    /// `?`-exit can't strand it in `pending_exception`; `live` marks a
    /// defer from the write op's own last-dim read — anything earlier
    /// means zend's opcode-boundary check already killed the write.
    // (thrown value, its raise error, live-final-level, plain-AA gate)
    dim_throw: Option<(crate::value::Value, PhpError, bool, bool)>,
    /// `++`/`--` overflow context while the pending dim write stores:
    /// a ref held by a typed-int prop reports `Cannot
    /// increment/decrement a reference held by property ... past its
    /// {maximal,minimal} value` instead of the assign TypeError
    /// (typed_properties_064). `(direction, bound)`.
    incdec_ref_ctx: Option<(&'static str, &'static str)>,
    /// Inside `unset()`: null dim keys convert to "" without the
    /// 'Using null as an array offset' deprecation (zend's UNSET_DIM
    /// maps IS_NULL silently — float/resource/illegal still diagnose).
    unset_ctx: bool,
    /// Dim-key conversions already emitted for this assign op — zend
    /// casts each dim operand once: the compound read, the write gate
    /// and the write itself reuse it without re-warning (`.=`/`|=`
    /// probe: oracle prints the null-offset deprecation exactly once).
    /// Per-dim-op cache of each operand cell's offset conversion —
    /// `(cell, ArrKey)` keeps the Rc alive so a dropped cell's address
    /// can't be reused and mis-key a later conversion (ABA).
    dim_key_conv: crate::value::FxMap<usize, (Cell, ArrKey)>,
    /// Per-dim-op CV-key bindings — zend reads each CV operand once
    /// per op, so `$a[$u] += v` warns 'Undefined variable' once even
    /// though the bound cell feeds both the read and the write
    /// (`??=` is two ops: its assign pass re-reads the CV).
    dim_cv_bound: crate::value::FxMap<String, Cell>,
    /// Dim operand cells bound to a fresh Null by an UNDEFINED var —
    /// zend keeps them IS_UNDEF so a later fetch's CV re-read warns
    /// again (`??=`'s ASSIGN_DIM is a second fetch). Reset with the
    /// other dim-op caches.
    dim_undef_cells: crate::value::FxSet<usize>,
    /// Current line estimate for error messages (best-effort).
    pub cur_line: usize,
    /// Site a pending `=`'s folded `${expr}` value reads at — zend's
    /// delayed-compile RHS takes the ASSIGN op's own lineno instead of
    /// the varvar's `}` line. Set in `assign()` only when the paren/
    /// marker-stripped value root is a folded VarVar; consumed by the
    /// VarVar eval arm.
    pub(in crate::interp) vv_rhs_site: Option<usize>,
    /// The match/switch subject's compiled-end line while a scanned
    /// (unfolded) cond evaluates — zend stamped every const-scan
    /// folded leaf with it, so each op in the cond takes its line
    /// from its rightmost leaf (`unfold_tail_site` instead of the
    /// operand's own first-token line). `None` outside the scan.
    pub(in crate::interp) scan_stamp: Option<usize>,
    /// Source line of the innermost call currently dispatching — Zend
    /// sites a pushed frame at the call's own line (the DO_FCALL op
    /// line: callee-name/`(` token for `f(...)`, member-name for
    /// `->m(...)`/`::m(...)`, class expr for `new X(...)`). Set by the
    /// call dispatchers right after their args evaluate, cleared at
    /// each `Stmt::Line`.
    pub(in crate::interp) send_line: Option<usize>,
    /// Active generator body's yield collector — `Expr::Yield` pushes
    /// (key, value) here while a generator function's body runs.
    gen_sink: Option<Rc<RefCell<Vec<crate::value::GenItem>>>>,
    /// send() queue feeding `yield`-expr results in the running body
    /// — (yield index, value) entries.
    gen_sends: std::collections::VecDeque<(usize, Value)>,
    /// throw() queue for the running body — (yield index, throwable)
    /// entries the `yield` expression raises as its result when the
    /// replay reaches them, so the body's own try/catch/finally does
    /// the unwind.
    gen_throws: std::collections::VecDeque<(usize, Value)>,
    /// Yield indices a queued throw actually fired at during the
    /// current gen run — `Generator->throw()` checks this to tell a
    /// delivered injection from one that couldn't land (exhausted
    /// gen, suspension point inside a `yield from` splice).
    gen_throws_fired: Vec<usize>,
    /// Auto-key counter for keyless `yield $v` — counts keyless yields
    /// only (explicit keys and `yield from` items don't advance it).
    gen_auto: i64,
    /// The generator whose body is currently running — output produced
    /// after a yield suspends is tagged with that yield's item index
    /// and buffered on the GenState until the consumer resumes past
    /// it (closure_call_leak_with_exception).
    gen_run_state: Option<Rc<RefCell<crate::value::GenState>>>,
    /// Nonzero while a method call is driven by foreach's internal
    /// iteration — a deferred gen-body death raised under it keeps
    /// the body's original call-frame trace (`FILE(n): g()`), while a
    /// userland `Generator->next()`-style resume renders the engine's
    /// internal resume stack instead.
    iter_calls: u32,
    /// Nonzero while an internal materializer (iterator_to_array,
    /// iterator_count) drives the iteration — a deferred death renders
    /// the resume stack WITHOUT the `Generator->{method}()`
    /// pseudo-frame (`[internal function]: g()` then the caller).
    gen_internal_resume: u32,
    /// Invocation line of the outermost live Generator-method dispatch
    /// — Zend frees a finished gen's execute_data inside the resume
    /// call, so destructors it runs cite the resume's line (the
    /// `->next()` call / foreach header), not the body's last line.
    gen_resume_site: Option<usize>,
    /// The `foreach` statement's own line while `exec_foreach_iter`
    /// drives an iterator — a deferred body death cites it for the
    /// gen's suspended frame (Zend's FE ops carry the header line,
    /// not the loop-body line `cur_line` has drifted to).
    gen_iter_site: Option<usize>,
    /// A fatal surfaced by err_flow while a generator body runs —
    /// stored instead of printed so the deferred death restamps the
    /// resume-stack trace and prints once at the consumer's resume.
    gen_pending_fatal: Option<PhpError>,
    /// call_trace snapshot at the last raise while a gen body ran —
    /// the suspended throw-site context (eval()/include() pseudo-
    /// frames, userland calls) the deferred death renders ahead of
    /// the resume stack.
    gen_raise_ctx: Vec<crate::value::TraceFrame>,
    /// While a unit's compile gate runs (eval()/include() flow_gate +
    /// hoisting), the callsite that triggered compilation — the user
    /// error handler invoked for a compile diagnostic reports its
    /// call frame there (Zend runs the handler at the caller site),
    /// not at the diagnostic's in-unit position.
    compile_callsite: Option<(String, u32)>,
    /// Depth of `finally` regions executing inside a generator body —
    /// their output is the gen's death-time output (Zend replays it
    /// when the suspended gen is destroyed), so emit_bytes tags it
    /// for fin_q.
    gen_fin_depth: u32,
    /// Set while a gen body runs and a `finally` region's own flow
    /// died — marks the body's terminal error as finally-region, so
    /// a force-close surfaces it at destruction (not the resume).
    /// Saved/restored at gen_start like the other gen-run slots.
    gen_fin_err: bool,
    /// send()'s/throw()'s re-run horizon: the body replays with the
    /// new send/throw queued, and output belonging to yields the
    /// consumer already observed (tag < horizon) is suppressed —
    /// Zend's lazy resume produces only post-resume bytes. Scoped to
    /// the re-running gen's own frames — a nested gen's run shares
    /// the interpreter, not the horizon.
    gen_replay_horizon: Option<(usize, Rc<RefCell<crate::value::GenState>>)>,
    /// A `yield from` snapshots the inner gen's destruction journal
    /// right after its start (before the drain prunes it) so the
    /// splice can merge it into the OUTER gen's journal — inner+outer
    /// finally replay together at the outer's destruction.
    gen_yield_from_fin: Option<crate::value::GenFinData>,
    /// While a `yield from` drains an inner iterator inside a gen
    /// run: the outer item index the inner items splice at. The
    /// inner's flushed stream bytes retag into the OUTER's deferred
    /// queue at `base + inner_tag` — a live emit would echo inner
    /// output before the consumer reached it.
    gen_collect_base: Option<usize>,
    /// Items the in-flight `yield from` collection has produced so
    /// far — a plain delegate's side-effect output (its next()
    /// echoing, etc.) journals at `base + seen - 1`, matching Zend
    /// driving the delegate's next() lazily on each resume.
    gen_collect_seen: usize,
    /// The gen whose `yield from` collection is in flight — the
    /// seen count belongs to that gen's sink; a delegate's own run
    /// must not fold it into its `done`.
    gen_collect_run: Option<Rc<RefCell<crate::value::GenState>>>,
    /// The running gen's fin_q (mirrors its GenState.fin_q; a stack-
    /// style save/restore like gen_sink).
    gen_fin_q: Option<crate::value::FinQueue>,
    /// Every generator object minted this run, as (weak state, fin_q).
    /// A dead weak means the object was released (unset()/overwrite) —
    /// Zend then runs the suspended body's finally chains, replayed
    /// from fin_q; a sweep at unit end models the shutdown GC.
    live_gens: Vec<(
        std::rc::Weak<RefCell<crate::value::GenState>>,
        crate::value::FinQueue,
    )>,
    /// Declaring class of the method about to be invoked (set by
    /// invoke_method, consumed by invoke_fn to fill Frame::decl_class).
    pending_decl_class: Option<Rc<PhpClass>>,
    /// Called-scope (LSB) for the next invoke_fn frame — set by
    /// invoke_method/static_invoke, consumed like pending_decl_class.
    pending_called_class: Option<Rc<PhpClass>>,
    /// Origin anchor for the next invoke_fn frame — set by method
    /// dispatch sites that pass `Rc::new(m.decl.clone())` (whose
    /// cloned bodies would otherwise get fresh `vars.as_ptr()` sites
    /// per call). Consumed like pending_decl_class.
    pending_decl_site: Option<usize>,
    /// (object id, prop, is_get, owner) whose hook is about to run —
    /// consumed by invoke_fn to fill Frame::hook_prop.
    pending_hook_prop: Option<(u64, String, bool, String)>,
    /// Live object handles for PHP's var_dump `#N` id: the lowest freed
    /// slot is reused, matching Zend's object store recycling.
    obj_handles: Vec<ObjHandle>,
    /// Birth order stamp per handle slot (parallel to `obj_handles`):
    /// the shutdown destruct sweep visits objects in creation order
    /// like Zend — a recycled low slot must not let a newborn object
    /// jump ahead of older live ones (bug74053).
    obj_born: Vec<u64>,
    /// Death order stamp per handle slot: Zend's object store frees a
    /// slot when the zval decrefs — a dtor-triggered death inside
    /// another dtor lands BEFORE the outer object's own free, so the
    /// free list hands the OUTER slot to the next `new` first
    /// (gh10168). 0 = alive / silently dead (order unknown).
    obj_died: Vec<u64>,
    /// Freed handle slots in death order (Zend's LIFO free list) —
    /// `mark_obj_died` pushes, `push_handle` pops the freshest.
    dead_slots: Vec<usize>,
    /// Weak registry of arrays that gained a reference (`=&`) element
    /// — only those can join a cycle, so the GC pass scans just them
    /// (Zend roots every refcounted array, this is the cheap subset
    /// that matters for `gc_collect_cycles` counting).
    arr_handles: Vec<std::rc::Weak<RefCell<PhpArray>>>,
    /// Approximation of Zend's root buffer: potential-cycle entries
    /// added since the last collect (`=&` binds, object allocation,
    /// displaced zvals that stayed alive). At 10_000 the collector
    /// auto-runs like `gc_collect_roots` on buffer overflow
    /// (gc/bug70805).
    gc_pending: usize,
    /// Re-entrancy guard — a collect runs userland `__destruct`s whose
    /// own unsets/displaces must not nest a second collect (Zend's
    /// GC_GCOLLECTED flag).
    gc_collecting: bool,
    /// Values already rooted (Zend's "already purple" bit): binds and
    /// surviving decrefs each count a zval once — cleared on collect.
    pub(crate) gc_purpled: HashSet<usize>,
    /// In-flight slot decrefs from `gc_note_dying` walks: cell ptr →
    /// count. A dying container still holds its cells while a collect
    /// fires mid-walk — Zend's destructor has already decremented the
    /// member refcounts, so the root test subtracts these holds.
    gc_dying: HashMap<usize, usize>,
    /// `gc_status()` counters: collect runs so far and roots collected
    /// across them (Zend reports both verbatim), plus wall-clock
    /// seconds spent collecting / running dtors / engine uptime.
    pub(crate) gc_runs: u64,
    pub(crate) gc_collected: u64,
    pub(crate) gc_collector_time: f64,
    pub(crate) gc_destructor_time: f64,
    pub(crate) t0: std::time::Instant,
    spawn_seq: u64,
    /// Per-callsite unqualified fn resolution cache — Zend resolves
    /// `ns\f -> f` once per call site (constexpr/namespace_004).
    fcc_fn_cache: HashMap<(String, String, String), Option<String>>,
    /// Objects whose __destruct already ran (shutdown pass). Weak
    /// handles don't inflate strong_count (an Rc pin kept every
    /// destructed object alive forever) and go stale with the object
    /// — a dead entry at a recycled address is simply unmarked
    /// (bug74053).
    destructed: HashMap<usize, std::rc::Weak<RefCell<PhpObject>>>,
    /// Next `destructed` size that triggers a stale-weak prune —
    /// doubling growth like `ref_cells_prune`.
    destructed_prune: usize,
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
    strict_files: crate::value::FxSet<String>,
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
    /// Cells bound to storage owned outside the executing frame —
    /// `static $s` aliases its function's statics table, so the cell
    /// outlives every call. Like is_ref cells, a suspended gen frame's
    /// release decrefs the binding but must not null the referent:
    /// the table keeps it live for sibling calls/gen instances
    /// (bug64979). Same Weak-pin ABA scheme as `ref_cells`.
    pub shared_cells: std::collections::HashMap<usize, std::rc::Weak<RefCell<Value>>>,
    /// Next `mark_shared` inserts past this size first sweep dead marks.
    shared_cells_prune: usize,
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
    /// zend heap->size: baseline plus the footprint (page/bucket
    /// units, not raw request bytes) of live tracked and untracked
    /// charges — memory_get_usage() input.
    pub mem_used: u64,
    /// 'tried to allocate N' figure of the charge that overflowed.
    mem_last: u64,
    /// Cached ini_bytes("memory_limit"): reparsed only when the raw
    /// ini string changes, since ini_set writes land in self.ini and
    /// this read runs on every tracked alloc.
    mem_limit_ck: (i64, Option<String>),
    /// Set by gen_start so invoke_fn_run marks the gen-body frame —
    /// its TraceFrame sites `[internal function]` (Zend's resume
    /// isn't a userland call).
    pending_gen_body: bool,
    /// Raised once the memory_limit fatal fired — buffers are dropped
    /// at shutdown instead of flushed (bug45392).
    pub mem_exceeded: bool,
    /// Call site (line + rendered backtrace) where the limit check
    /// first failed — zend's OOM bailout reports the allocating call,
    /// not the stmt boundary that raises the fatal.
    oom_at: Option<(usize, Vec<String>)>,
    /// heap->real_size's chunk share: zend commits whole 2MB chunks
    /// up front; grows when the in-chunk footprint overflows.
    mem_committed: u64,
    /// Live footprint inside committed chunks — the sum of
    /// `mem_chunks` occupancy, kept for cheap arithmetic.
    mem_in_chunk: u64,
    /// zend_mm's committed chunk list — used bytes per 2MB chunk.
    /// A run first-fits the oldest chunk with room; a freed run's
    /// pages stay committed for reuse (chunks only unmap once the
    /// whole chunk is empty — zend_mm_delete_chunk, except the last
    /// non-main chunk which zend keeps mapped on its cached list).
    /// An entry at 0 bytes is a dead slot unless it is `mem_cached`.
    /// Chunk-level fidelity, not page-run-level: freed space is
    /// treated as contiguous, so a badly fragmented tail can pin a
    /// chunk the sim thinks reusable.
    mem_chunks: Vec<u64>,
    /// zend_mm chunk->num per `mem_chunks` slot — a monotonically
    /// increasing identity assigned at each fresh commit: a reused
    /// slot is a NEWER chunk than its index suggests, so the
    /// emptied-vs-cached ordering can't key on position.
    mem_chunk_nums: Vec<u64>,
    mem_chunk_num_next: u64,
    /// Emptied non-main chunks zend keeps mapped on cached_chunks
    /// (delay deletion — get_chunk pops one back without running the
    /// limit check). LIFO stack of `mem_chunks` indices; zend holds at
    /// most a couple intra-request.
    mem_cached: Vec<usize>,
    /// zend_mm_delete_chunk hysteresis: chunks_count at the last real
    /// unmap and the consecutive unmaps at that same boundary — the
    /// 4th+ deletion at one boundary caches instead of unmapping.
    chunks_del_boundary: usize,
    chunks_del_count: usize,
    /// Page-aligned size of live huge allocs — each is its own
    /// segment in real_size and is released when its owner dies.
    mem_huge: u64,
    /// Huge-segment placement order — kernel top-down mmap lands each
    /// new segment below the previous lowest VMA, so position order
    /// approximates the VA stack (highest first). A freed segment's
    /// span stays a hole in its slot until a fitting placement steals
    /// it or the live segment below drains it at grow time — eager
    /// credit to the heir was wrong: a transient temp reoccupies the
    /// same span every iteration and the heir never sees it.
    /// ponytail: interleaved chunks and non-zend VMAs break the
    /// ordering assumption.
    mem_seg_order: Vec<SegSlot>,
    /// High-water mark of real_size — memory_get_peak_usage(true).
    pub(crate) mem_real_peak: u64,
    /// Allocations whose charge is tied to a live Rc — data pointer
    /// → charge. zend tracks emalloc/efree: when every strong ref to
    /// a tracked alloc dies its footprint releases back into the
    /// heap, so reclaimable churn never trips the limit while
    /// genuinely-growing structures do.
    mem_tracked: crate::value::FxMap<usize, MemCharge>,
    /// High-water mark of mem_used — memory_get_peak_usage().
    pub(crate) mem_peak: u64,
    /// zend's ONE request-scoped vm_stack arena: 256KB-page segments
    /// holding call frames' arg spans. A call that overflows the top
    /// segment copies into a fresh one sized for its whole span — that
    /// segment's emalloc is what a limit trip reports. Unwinding is
    /// LIFO: a frame frees its minted segment at pop.
    vm_stack: Vec<VmSeg>,
    /// Registry size that trips the next dead-entry sweep — bounds the
    /// tracker footprint for alloc-churn loops.
    mem_sweep_at: usize,
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
    /// Alloc figure of `buf`'s zend smart_string — `mem_sync`
    /// reconciles it after every mutation; `ob_meter_sync` books it
    /// into the zend_mm sim against `mem_tok`, whose death releases
    /// the charge on level teardown (zend frees the buffer then).
    pub charged: i64,
    /// The smart_string's allocated capacity `a`: a clean/flush
    /// resets `len` only — the allocation survives until the level
    /// ends, so the charge tracks `cap`, not `buf.len()`.
    pub cap: u64,
    /// Token binding this level's buffer charge to its lifetime in
    /// the zend_mm sim — `mem_realloc`'s weak probe dies with the
    /// level, releasing the bytes at the next sweep.
    pub mem_tok: Rc<()>,
    pub handler: Option<Value>,
    /// Set after the handler's first invocation — PHP's
    /// PHP_OUTPUT_HANDLER_START bit is only passed once (bug24951).
    pub started: bool,
    /// When the buffer was opened inside a generator body past its
    /// first yield: the owning gen's fin queue, whose mirrored `pos`
    /// tracks how far the consumer advanced — Zend buffers live on
    /// the global stack and survive the suspend, so deferred bytes
    /// merge into `buf` in cursor order rather than echoing raw at
    /// replay.
    pub gen_q: Option<crate::value::FinQueue>,
    /// The consumer cursor position at which the body's `ob_start`
    /// ran — the buffer exists consumer-side only once `pos` reaches
    /// it; before that it lives in `suspended_obs`.
    pub gen_open: Option<usize>,
    /// When the body's ob-pop op ran (ob_get_clean & friends), in
    /// consumer cursor space — the buffer keeps capturing consumer
    /// writes while `pos` sits inside [gen_open, gen_close), like
    /// Zend's still-live global buffer.
    pub gen_close: Option<usize>,
    /// Deferred gen bytes captured while this buffer is open, tagged
    /// by the yield index they follow — merged into `buf` when the
    /// cursor passes them (or in full inside the body itself, where
    /// they already ran).
    pub gen_pending: Vec<(usize, Vec<u8>)>,
    /// Deferred bytes drained into `buf` so far — the tail of a
    /// body-pop value (buf[..len-drained] is pre-pop writes, the rest
    /// is the resume segment it captured).
    pub gen_drained: usize,
    /// A body-popped window mirror: the pop value's head — the
    /// direct (pre-first-yield) writes that precede every tagged
    /// segment. Consumer captures splice into the segment stream
    /// when the journaled pop echo is rewritten at window close.
    pub pop_head: Option<Vec<u8>>,
    /// The pop value's per-tag segments (bytes the body's resumes
    /// appended, keyed by the item index each followed) — a consumer
    /// write at cursor `pos` splices ahead of every segment whose
    /// tag is >= pos, matching the global stack's write order.
    pub pop_segs: Vec<(usize, Vec<u8>)>,
    /// Segment boundaries of journaled bytes drained into `buf`, as
    /// (tag, buf offset, byte len) per entry — lets a later pop split
    /// `buf`'s drained regions back into segments for `pop_segs` and
    /// lets teardown drop a killed gen's un-run tail in place.
    pub drained_segs: Vec<(usize, usize, usize)>,
    /// Consumer writes this mirror captured, each tagged by the
    /// journal cursor they arrived at — the splice input for
    /// `ob_mirror_close`.
    pub caps: Vec<(usize, Vec<u8>)>,
    /// Buf ranges holding consumer captures, as (tag, offset, len)
    /// per entry — lets a content rebuild exclude cap bytes from the
    /// direct-write head and re-insert them at cursor position
    /// (`ob_level_content`, end-of-request patches).
    pub cap_segs: Vec<(usize, usize, usize)>,
    /// Buffer views the body materialized eagerly — (yield index,
    /// head, per-tag segments) per read. Consumer captures that
    /// arrive between the suspend and the read's resume belong
    /// inside them (Zend runs the read lazily); the window's close
    /// rewrites the stored values to the resolved content.
    pub read_vals: Vec<ObReadVal>,
    /// Stack slot the level occupied when the body suspended —
    /// consumer levels pushed during the suspension stay above it
    /// in Zend's shared stack, so promotion re-inserts here rather
    /// than at the top.
    pub suspend_base: usize,
    /// The owning generator's state — stale-drop rewrites its
    /// deferred journal entries still holding the stale pop value.
    pub gen_state: Option<std::rc::Weak<RefCell<crate::value::GenState>>>,
}

/// A buffer view a gen body materialized eagerly: (yield index,
/// direct-write head, per-tag journaled segments). Window close
/// rewrites the stored value against the captures its resume saw.
pub(in crate::interp) type ObReadVal = (usize, Vec<u8>, Vec<(usize, Vec<u8>)>);

/// Result of a top-level program run.
pub struct RunResult {
    pub exit_code: i32,
    /// Set when a fatal error terminated execution.
    pub fatal: Option<PhpError>,
}

/// Rebuild the bytes one shared output buffer held: the body's
/// per-tag segments in tag order, with each consumer capture
/// spliced ahead of the segment whose tag is >= its arrival cursor
/// — the global stack's real write order across suspends.
impl ObLevel {
    /// Reconcile `charged` with the buffer's zend-alloc size after a
    /// mutation. zend's smart_string never shrinks while the level
    /// lives — a clean/flush resets `len` but keeps `a` — so the
    /// charge follows capacity, ratcheting up when a write crosses
    /// it (one 16KB page per incremental crossing, or straight to
    /// the aligned content on a single big append).
    /// `ob_meter_sync` applies `charged` to the zend_mm sim.
    pub(in crate::interp) fn mem_sync(&mut self) {
        if self.pop_head.is_some() {
            // A pop mirror is journaled bookkeeping, not a real
            // output-buffer allocation.
            self.charged = 0;
            return;
        }
        let len = self.buf.len() as u64;
        if len >= self.cap {
            self.cap = (self.cap + OB_INIT_CAP).max((len + 4095) & !4095);
        }
        self.charged = self.cap as i64;
    }
}

pub(in crate::interp) fn ob_splice(
    head: &[u8],
    segs: &[(usize, Vec<u8>)],
    caps: &[(usize, Vec<u8>)],
) -> Vec<u8> {
    let mut v = head.to_vec();
    let mut ci = 0;
    for (t, s) in segs {
        while ci < caps.len() && caps[ci].0 <= *t {
            v.extend_from_slice(&caps[ci].1);
            ci += 1;
        }
        v.extend_from_slice(s);
    }
    while ci < caps.len() {
        v.extend_from_slice(&caps[ci].1);
        ci += 1;
    }
    v
}

/// Split a buffer view at the level's recorded drain offsets into a
/// direct-write head and per-tag journaled segments — same layout
/// `ob_splice` consumes.
pub(in crate::interp) fn ob_split_view(
    l: &ObLevel,
    content: &[u8],
) -> (Vec<u8>, Vec<(usize, Vec<u8>)>) {
    let first = l
        .drained_segs
        .first()
        .map(|(_, s, _)| (*s).min(content.len()))
        .unwrap_or(content.len());
    let head = content[..first].to_vec();
    let mut segs: Vec<(usize, Vec<u8>)> = Vec::new();
    let mut off = first;
    for &(t, s, n) in &l.drained_segs {
        let s = s.min(content.len());
        let e = (s + n).min(content.len());
        if s < off {
            continue;
        }
        // Real writes between two journaled segs ride with the
        // following one — they ran later.
        if s > off {
            if let Some(prev) = segs.last_mut() {
                prev.1.extend_from_slice(&content[off..s]);
            } else {
                segs.push((t, content[off..s].to_vec()));
            }
        }
        segs.push((t, content[s..e].to_vec()));
        off = e;
    }
    if off < content.len() {
        segs.push((usize::MAX, content[off..].to_vec()));
    }
    (head, segs)
}

/// Working maps for one `gc_cycle_collect_pass` scan: the
/// reachability graph plus the cell-level accounting that decides
/// which refcount holds are internal to the candidate universe.
#[derive(Default)]
struct GcScan {
    /// Container ptr → direct refcounted targets (flood-fill edges);
    /// covers unregistered containers reached transitively.
    edges: HashMap<usize, Vec<usize>>,
    /// Cell ptr → slots the cell occupies inside scanned containers.
    cell_slots: HashMap<usize, usize>,
    /// Cell ptr → the cell (kept for strong-count checks).
    cells: HashMap<usize, Cell>,
    /// Node ptr → cells directly holding a clone of it.
    cell_edges: HashMap<usize, HashSet<usize>>,
    /// Node ptr → bare clones held in non-cell positions (gen
    /// sends/throws, buffered item keys, saved call setup).
    raw_edges: HashMap<usize, usize>,
    /// Universe node ptrs (objects + registered arrays) — the sweep
    /// set; these hold their bookkeeping clone in the scan vectors.
    universe: HashSet<usize>,
    /// Non-universe containers discovered mid-scan — plain arrays and
    /// closures; kept so their strong counts can be checked like the
    /// nodes' (the map clone is each one's bookkeeping +1).
    nodes: HashMap<usize, Value>,
    /// In-flight slot decrefs (`gc_note_dying` walks) — a snapshot of
    /// `Interp::gc_dying` taken at pass start.
    dying: HashMap<usize, usize>,
    /// Containers reached through internal-machinery prop cells only
    /// (`\0Cls\0prop` on GC_PROPLESS_CLASSES): engine-modeled storage,
    /// not zvals — dead ones don't count toward the collect total.
    internal: HashSet<usize>,
    /// Containers reached through at least one real zval edge — an
    /// `internal`-marked container with a real edge counts normally.
    plain: HashSet<usize>,
    /// Cell ptr → engine-bookkeeping clone count. `typed_slots` pins a
    /// clone of every typed prop's cell for write-gating — like
    /// `dying`, those aren't zval holders, so they can't root the cell's
    /// payload (gc_048's typed `$cycleRef` prop cell).
    pins: HashMap<usize, usize>,
}

impl<'a> Interp<'a> {
    pub fn new(file: &'a str) -> Self {
        let mut constants = crate::value::FxMap::default();
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
                    pos: 0,
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
        for (name, n) in [
            ("SIGHUP", 1),
            ("SIGINT", 2),
            ("SIGQUIT", 3),
            ("SIGILL", 4),
            ("SIGTRAP", 5),
            ("SIGABRT", 6),
            ("SIGIOT", 6),
            ("SIGBUS", 7),
            ("SIGFPE", 8),
            ("SIGKILL", 9),
            ("SIGUSR1", 10),
            ("SIGSEGV", 11),
            ("SIGUSR2", 12),
            ("SIGPIPE", 13),
            ("SIGALRM", 14),
            ("SIGTERM", 15),
            ("SIGSTKFLT", 16),
            ("SIGCHLD", 17),
            ("SIGCLD", 17),
            ("SIGCONT", 18),
            ("SIGSTOP", 19),
            ("SIGTSTP", 20),
            ("SIGTTIN", 21),
            ("SIGTTOU", 22),
            ("SIGURG", 23),
            ("SIGXCPU", 24),
            ("SIGXFSZ", 25),
            ("SIGVTALRM", 26),
            ("SIGPROF", 27),
            ("SIGWINCH", 28),
            ("SIGIO", 29),
            ("SIGPOLL", 29),
            ("SIGPWR", 30),
            ("SIGSYS", 31),
            ("SIG_BLOCK", 0),
            ("SIG_UNBLOCK", 1),
            ("SIG_SETMASK", 2),
            ("SIG_DFL", 0),
            ("SIG_IGN", 1),
            ("SIG_ERR", -1),
            ("SIGBABY", 31),
            ("WNOHANG", 1),
            ("WUNTRACED", 2),
            ("WCONTINUED", 8),
            ("PRIO_PROCESS", 0),
        ] {
            constants.insert(name.into(), Value::Int(n));
        }
        constants.insert("SEEK_SET".into(), Value::Int(0));
        constants.insert("SEEK_CUR".into(), Value::Int(1));
        constants.insert("SEEK_END".into(), Value::Int(2));
        constants.insert("STREAM_FILTER_READ".into(), Value::Int(1));
        constants.insert("STREAM_FILTER_WRITE".into(), Value::Int(2));
        constants.insert("STREAM_FILTER_ALL".into(), Value::Int(3));
        // php_stream_filter_status_t — php_user_filter::filter() returns.
        constants.insert("PSFS_ERR_FATAL".into(), Value::Int(0));
        constants.insert("PSFS_FEED_ME".into(), Value::Int(1));
        constants.insert("PSFS_PASS_ON".into(), Value::Int(2));
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
            globals_synced: crate::value::FxSet::default(),
            dim_by_ref: false,
            foreach_by_ref: false,
            anon_class_names: HashMap::new(),
            anon_class_seq: 0,
            callable_probe_err: None,
            stack: Vec::new(),
            functions: crate::value::FxMap::default(),
            classes: crate::value::FxMap::default(),
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
                // php_user_filter's methods carry tentative return
                // types — overrides without matching types warn
                // (ReturnTypeWillChange suppresses).
                for m in [
                    "filter",
                    "oncreate",
                    "onclose",
                    "onflush",
                    "onread",
                    "onwrite",
                    "onappend",
                    "onprepend",
                    "onstart",
                    "onstop",
                    "onseek",
                    "onskip",
                    "oneof",
                    "ondetach",
                ] {
                    t.insert(("php_user_filter".into(), m.into()));
                }
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
            const_decl_ctx: None,
            const_init_site: None,
            out_headers: Vec::new(),
            resp_code: 200,
            last_json_error: 0,
            last_preg_error: 0,
            valid_utf8: std::collections::HashMap::new(),
            php_input: std::rc::Rc::new(Vec::new()),
            uploads: Vec::new(),
            stream_chunk_sizes: std::collections::HashMap::new(),
            stream_filters: std::collections::HashMap::new(),
            stream_filter_bindings: std::collections::HashMap::new(),
            user_filter_map: Vec::new(),
            stream_brigades: std::collections::HashMap::new(),
            stream_buckets: std::collections::HashMap::new(),
            filter_no_fclose: None,
            filter_warn_ctx: String::new(),
            stream_filter_busy: std::collections::HashSet::new(),
            codec_states: std::collections::HashMap::new(),
            weakrefs: std::collections::HashMap::new(),
            ob_stack: Vec::new(),
            suspended_obs: Vec::new(),
            ob_boot: None,
            emit_boot: None,
            emit_seen: false,
            silence: 0,
            isset_quiet: 0,
            statics: crate::value::FxMap::default(),
            static_decls: StaticDeclSites::new(),
            cur_unit_id: 0,
            next_unit_id: 1,
            included: HashSet::new(),
            pending_exception: None,
            call_trace: Vec::new(),
            // zend's regular_list already holds stdin/stdout/stderr
            // plus the default stream context, so the first userland
            // resource is id 5.
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
            incdec_ref_ctx: None,
            detached_dim: false,
            dim_throw: None,
            unset_ctx: false,
            dim_key_conv: crate::value::FxMap::default(),
            dim_undef_cells: crate::value::FxSet::default(),
            dim_cv_bound: crate::value::FxMap::default(),
            cur_line: 1,
            vv_rhs_site: None,
            scan_stamp: None,
            send_line: None,
            gen_sink: None,
            pending_gen_captures: Vec::new(),
            pending_gen_body: false,
            gen_sends: std::collections::VecDeque::new(),
            gen_throws: std::collections::VecDeque::new(),
            gen_throws_fired: Vec::new(),
            gen_auto: 0,
            gen_run_state: None,
            iter_calls: 0,
            gen_internal_resume: 0,
            gen_resume_site: None,
            gen_iter_site: None,
            gen_pending_fatal: None,
            gen_raise_ctx: Vec::new(),
            compile_callsite: None,
            gen_fin_depth: 0,
            gen_fin_err: false,
            gen_replay_horizon: None,
            gen_yield_from_fin: None,
            gen_collect_base: None,
            gen_collect_seen: 0,
            gen_collect_run: None,
            gen_fin_q: None,
            live_gens: Vec::new(),
            pending_decl_class: None,
            pending_called_class: None,
            pending_decl_site: None,
            pending_hook_prop: None,
            in_const_expr: 0,
            class_const_ctx: 0,
            const_self: None,
            param_bind_ctx: None,
            engine_consts,
            autoload_fns: Vec::new(),
            obj_handles: Vec::new(),
            obj_born: Vec::new(),
            obj_died: Vec::new(),
            dead_slots: Vec::new(),
            arr_handles: Vec::new(),
            gc_pending: 0,
            gc_collecting: false,
            gc_purpled: HashSet::new(),
            gc_dying: HashMap::new(),
            gc_runs: 0,
            gc_collected: 0,
            gc_collector_time: 0.0,
            gc_destructor_time: 0.0,
            t0: std::time::Instant::now(),
            spawn_seq: 0,
            fcc_fn_cache: HashMap::new(),
            destructed: HashMap::new(),
            destructed_prune: 1024,
            expr_temps: Vec::new(),
            last_popped_frame: None,
            dump_stack: std::collections::HashSet::new(),
            internal_cb: 0,
            cur_file: file.to_string(),
            strict_files: crate::value::FxSet::default(),
            typed_slots: std::collections::HashMap::new(),
            slot_owners: std::collections::HashMap::new(),
            slot_merged: std::collections::HashMap::new(),
            slot_anchor: std::collections::HashMap::new(),
            ref_cells: std::collections::HashMap::new(),
            ref_cells_prune: 1024,
            shared_cells: std::collections::HashMap::new(),
            shared_cells_prune: 1024,
            magic_guards: std::collections::HashSet::new(),
            readonly_cells: std::collections::HashMap::new(),
            clone_write: false,
            last_fresh_cell: None,
            builtin_ifaces: std::collections::HashSet::new(),
            dep_seen: std::collections::HashSet::new(),
            last_err_file: String::new(),
            loop_depth: 0,
            assert_src: String::new(),
            mem_used: MM_BASE_USED,
            mem_last: 0,
            mem_limit_ck: (0, None),
            mem_exceeded: false,
            oom_at: None,
            // zend_mm_init commits the first 2MB chunk eagerly.
            mem_committed: MM_CHUNK,
            mem_in_chunk: MM_BASE_CHUNK,
            mem_chunks: vec![MM_BASE_CHUNK],
            mem_chunk_nums: vec![0],
            mem_chunk_num_next: 1,
            mem_cached: Vec::new(),
            chunks_del_boundary: 0,
            chunks_del_count: 0,
            mem_huge: 0,
            mem_seg_order: Vec::new(),
            mem_real_peak: MM_CHUNK,
            // Seed: header + the top-level frame's span.
            vm_stack: vec![VmSeg {
                size: VM_PAGE,
                used: 32 + VM_FRAME_SLOTS * 16,
                tok: Rc::new(VmSite),
            }],
            mem_tracked: crate::value::FxMap::default(),
            mem_peak: MM_BASE_USED,
            mem_sweep_at: 4096,
            deadline: None,
            deadline_secs: 0,
            ini: HashMap::from([
                ("error_reporting".to_string(), "30719".to_string()),
                // Zend's compiled-in default (hardcoded in main/php.ini).
                ("memory_limit".to_string(), "128M".to_string()),
                ("zend.enable_gc".to_string(), "1".to_string()),
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
    fn eval_decl_const(
        &mut self,
        e: &Expr,
        decl_file: &str,
        decl_line: usize,
    ) -> Result<Value, PhpError> {
        if decl_file.is_empty() {
            return self.eval_const(e);
        }
        let old = self.decl_file_ctx.replace(decl_file.to_string());
        let old_ctx = if decl_line > 0 {
            self.const_decl_ctx
                .replace((decl_file.to_string(), decl_line as u32))
        } else {
            None
        };
        // Save the resolution line before the decl's own lines take over
        // cur_line — a fail() mid-eval builds the pseudo-frame here.
        let old_site = self.const_init_site.replace(self.cur_line);
        let r = self.eval_const(e);
        self.decl_file_ctx = old;
        self.const_init_site = old_site;
        if decl_line > 0 {
            self.const_decl_ctx = old_ctx;
        }
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
                for (_, d, ..) in vars {
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
            | Expr::VarVar(e, _)
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
            Expr::Call { name, args, .. } => {
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
            Expr::Match { subject, arms, .. } => {
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
            Expr::StaticCallDyn {
                class, name, args, ..
            } => {
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
            Expr::New { class, args, .. } => {
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
        if let Err(e) = self.decl_type_checks(&d.name, d, None) {
            // A throwable from the user error handler (e.g. throwing
            // on a signature deprecation) escapes at the caller site —
            // the eval()/include() call — and dies uncaught, it is not
            // a compile diagnostic to re-emit at exec time. Compile
            // fatals stay deferred: the exec-time decl arm re-runs
            // the same checks through err_flow.
            if e.kind == ErrorKind::Throw {
                return Err(e);
            }
        }
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
        // Global decl claiming a builtin name dies at the same
        // compile phase (oracle: "Cannot redeclare function strlen()");
        // namespaced decls stay legal.
        if d.ns.is_empty() {
            if let Some(msg) = Self::builtin_redecl_msg(&key, &d.name) {
                return Err(PhpError::compile_fatal(msg, d.line));
            }
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

    /// Message for a global `function <name>()` decl that collides
    /// with a builtin — None when the name is free. Namespaced decls
    /// are legal and never reach here (gated on `d.ns.is_empty()`).
    /// `preg_jit`/`pathinfo_dirname`/`fastcgi_finish_request` are
    /// phpun-internal or FPM-only dispatch entries, not oracle-visible
    /// functions — declaring them is legal, so they're exempt.
    /// `assert` is zend_compile-special-cased ahead of the generic
    /// redeclare text.
    fn builtin_redecl_msg(key: &str, dname: &str) -> Option<String> {
        if matches!(
            key,
            "preg_jit" | "pathinfo_dirname" | "fastcgi_finish_request"
        ) || !crate::builtins::is_builtin(key)
        {
            return None;
        }
        if key == "assert" {
            return Some(
                "Defining a custom assert() function is not allowed, as the function has special semantics"
                    .to_string(),
            );
        }
        Some(format!("Cannot redeclare function {dname}()"))
    }

    fn hoist_funcs_pass(&mut self, stmts: &[Stmt]) -> Result<(), PhpError> {
        for s in stmts {
            // `__halt_compiler()` ends compilation — post-halt code is
            // never compiled in Zend, so its decls must not register
            // (they'd leak into function_exists and trip the
            // builtin-name fatal).
            if matches!(s, Stmt::Expr(Expr::Call { name, .. })
                if matches!(&**name, Expr::Str(n) if n.trim_start_matches('\u{1}').eq_ignore_ascii_case("__halt_compiler")))
            {
                break;
            }
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
                    self.early_bound_classes
                        .insert(key, (self.cur_unit_id, site));
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
            .and_then(|_| Self::yield_gate(stmts))
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
        // A generator destroyed by the unwind (last ref dropped as
        // the error propagated) replays its finally before the fatal
        // renders — Zend tears objects down between diagnosing and
        // displaying it. Gens still referenced stay for the
        // shutdown pass (run_shutdown replays them AFTER the
        // display, in reverse creation order).
        if !matches!(flow, Flow::Normal | Flow::Return(_) | Flow::Exit(_)) {
            let _ = self.gen_gc_sweep(false);
        }
        // Generators still suspended at request end replay their
        // enclosing finally chains during shutdown — Zend renders a
        // terminal error first, then tears objects down (shutdown
        // functions, CV teardown, object store). The sweep inside
        // run_shutdown raises a destruction-time error as a second
        // fatal.
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

    /// Live objects in the store (strong pins) — used to catch drops
    /// inside a shutdown-time dtor.
    fn live_obj_pins(&self) -> Vec<Rc<RefCell<PhpObject>>> {
        self.obj_handles
            .iter()
            .filter_map(|h| match h {
                ObjHandle::Obj(w) => w.upgrade(),
                _ => None,
            })
            .collect()
    }

    /// Invoke a shutdown-time `__destruct`, then fire the dtors of
    /// objects whose last real ref was dropped inside it. Zend decrefs
    /// a zval's contents the moment it is overwritten — `self::$b =
    /// new b` inside a dtor runs the old object's __destruct
    /// immediately, at any depth (bug74053). Pinning the store around
    /// the call keeps such drops reclaimable (a dead Weak could never
    /// be destructed); the drain recurses since a dropped object's own
    /// dtor can drop further objects.
    fn shutdown_dtor_invoke(&mut self, o: Rc<RefCell<PhpObject>>) -> Result<(), PhpError> {
        let mut pins = self.live_obj_pins();
        // Drops fire oldest-first — same creation order as the sweep.
        pins.sort_by_key(|p| {
            self.obj_born
                .get(p.borrow().id.saturating_sub(1) as usize)
                .copied()
                .unwrap_or(0)
        });
        self.method_invoke(o, "__destruct", CallArgs::empty())?;
        for p in pins {
            // count==1: only `pins` still holds it — every real ref
            // died during the dtor that just ran.
            if Rc::strong_count(&p) != 1
                || self.was_destructed(Rc::as_ptr(&p) as usize)
                || self
                    .find_method_in(&p.borrow().class, "__destruct")
                    .is_none()
                || !self.mark_destructed(&p)
            {
                continue;
            }
            self.shutdown_dtor_invoke(p)?;
        }
        Ok(())
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
            // A suspended generator torn down here replays its
            // finally journal at its own slot in the teardown order
            // — Zend kills the generator handle when the global var
            // frees it, interleaved with real __destruct calls.
            let gen_q = match &o.borrow().internal {
                Some(crate::value::ObjectInternal::Generator(st)) => {
                    Some(st.borrow().fin_q.clone())
                }
                _ => None,
            };
            if let Some(q) = gen_q {
                if let Some(e) = self.gen_fin_replay(&q, true) {
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
                continue;
            }
            if self
                .find_method_in(&o.borrow().class, "__destruct")
                .is_some()
                && self.mark_destructed(&o)
            {
                if let Err(e) = self.shutdown_dtor_invoke(o.clone()) {
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
            // Prop cells free with the object — a suspended
            // generator held only by them force-closes at this same
            // teardown slot (Zend frees the prop table alongside the
            // dtor, interleaved with the CV pass).
            if let Some(e) = self.gen_prop_sweep(&o) {
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
        // zend's resource-list teardown rides the symbol-table free:
        // each still-filtered stream flushes its write chain and runs
        // the userfilter dtor with ->stream NULL (the stream zval is
        // already dead). An error stops the sweep like a dtor failure.
        if !dtor_stop {
            let mut sres = Vec::new();
            for c in self.globals.vars.values() {
                if let Value::Resource(r) = &*c.borrow() {
                    if self.stream_filters.contains_key(&r.borrow().id()) {
                        sres.push(r.clone());
                    }
                }
            }
            for r in sres {
                if let Err(e) = crate::builtins::fs::stream_dtor_flush(self, &r) {
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
        }
        // Objects a dtor spawns may land in already-visited recycled
        // handle slots — rescan until a full pass runs nothing new
        // (bug51822/bug74053).
        if !dtor_stop {
            'sweep: loop {
                // Zend destructs objects in creation order; handle
                // slots recycle mid-dtor, so slot order must not
                // decide which spawned object runs next (bug74053).
                let mut todo: Vec<(u64, Rc<RefCell<PhpObject>>)> = self
                    .obj_handles
                    .iter()
                    .filter_map(|h| match h {
                        ObjHandle::Obj(w) => w.upgrade(),
                        _ => None,
                    })
                    .map(|o| {
                        let born = self
                            .obj_born
                            .get(o.borrow().id.saturating_sub(1) as usize)
                            .copied()
                            .unwrap_or(0);
                        (born, o)
                    })
                    .collect();
                todo.sort_by_key(|(b, _)| *b);
                let mut progressed = false;
                for (_, o) in todo {
                    let key = Rc::as_ptr(&o) as usize;
                    if self.was_destructed(key) {
                        continue;
                    }
                    if self
                        .find_method_in(&o.borrow().class, "__destruct")
                        .is_some()
                    {
                        self.mark_destructed(&o);
                        progressed = true;
                        if let Err(e) = self.shutdown_dtor_invoke(o.clone()) {
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
                    // Same prop-free rule as the CV pass — a
                    // prop-held generator force-closes at this
                    // object's slot in the store pass.
                    if let Some(e) = self.gen_prop_sweep(&o) {
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
                if !progressed {
                    break;
                }
            }
        }
        // The symbol table frees only after every shutdown dtor ran —
        // a dtor's `global $x` still binds the (dead) object zval like
        // Zend, whose store destructors all precede CV teardown
        // (gh10168 with_prop_ref variants).
        self.globals.vars.clear();
        // Generators still suspended — not torn down through the CV
        // pass above (locals, containers, live references) — force
        // close now: Zend kills every remaining generator handle at
        // request end, newest first.
        if let Err(e) = self.gen_gc_sweep(true) {
            shutdown_code = Some(match self.err_flow(e) {
                Flow::Exit(c) => c,
                Flow::Throw(v) => {
                    self.uncaught(&v);
                    255
                }
                _ => 255,
            });
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
            if self.was_destructed(key) {
                continue;
            }
            if self
                .find_method_in(&o.borrow().class, "__destruct")
                .is_some()
            {
                self.mark_destructed(&o);
                self.method_invoke(o.clone(), "__destruct", CallArgs::empty())?;
                self.mark_obj_died(&o);
            }
        }
        Ok(())
    }

    /// Store into a live cell: the new value lands first, then the
    /// displaced zval decrefs — its __destruct (when this was the
    /// last ref) sees the new value already in place (gh10168).
    pub(in crate::interp) fn cell_store(&mut self, c: &Cell, v: Value) -> Result<(), PhpError> {
        let old = std::mem::replace(&mut *c.borrow_mut(), v);
        self.destruct_dying_value(&old)
    }

    /// Objects whose last refs live inside a dropped value run
    /// __destruct — `unset($closure)` decrefs the closure's bound
    /// $this and captures (Zend refcount semantics — closure_005).
    pub(in crate::interp) fn destruct_dying_value(&mut self, v: &Value) -> Result<(), PhpError> {
        // A displaced zval that stays alive is Zend's purple-add — a
        // potential cycle root entering the buffer (gc/bug70805).
        let purple = match v {
            Value::Object(o) if Rc::strong_count(o) > 1 => Some(Rc::as_ptr(o) as usize),
            Value::Array(a) if Rc::strong_count(a) > 1 => Some(Rc::as_ptr(a) as usize),
            Value::Callable(c) if Rc::strong_count(c) > 1 => Some(Rc::as_ptr(c) as usize),
            _ => None,
        };
        if let Some(k) = purple {
            self.gc_note_purple(k);
        }
        // A container taking its last ref decrefs every held slot —
        // Zend buffers each surviving payload as a root candidate
        // (gc_023's `unset($a)`); Rust's Drop would stay silent.
        if matches!(v, Value::Array(_) | Value::Object(_) | Value::Callable(_)) {
            self.gc_note_dying(&v.clone(), 8);
        }
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
                // The slot frees when the dtor returns — nested dtor
                // deaths (stamped inside) precede it in Zend's free
                // list (gh10168 handle reuse).
                self.mark_obj_died(&o);
            }
        }
        Ok(())
    }

    /// Decref the running frame's CVs (vars/args/$this): an object
    /// whose strong refs are exactly the cells this frame is about
    /// to drop runs its __destruct now — Zend's behavior at function
    /// exit and exception unwind (bug52361).
    fn destruct_frame_objs(&mut self, f: &Frame) -> Result<(), PhpError> {
        let mut cells: Vec<Cell> = f.vars.values().cloned().collect();
        cells.extend(f.args.iter().cloned());
        if let Some(o) = &f.this_obj {
            cells.push(cell(Value::Object(o.clone())));
        }
        self.destruct_cells(&cells)
    }

    /// `destruct_frame_objs` over a bare cell list — the suspended
    /// generator frame's stashed CVs decref the same way when its
    /// execute_data is freed.
    pub(in crate::interp) fn destruct_cells(&mut self, cells: &[Cell]) -> Result<(), PhpError> {
        // Only a cell DYING with this batch decrefs its zval: a shared
        // cell (`=&` alias still owned by props/statics/another var)
        // keeps its content — the frame's handle dropping is not a
        // zval decref (gh10168: call1's $tmp survives in $box->value).
        let mut cell_refs: HashMap<usize, usize> = HashMap::new();
        for c in cells {
            *cell_refs.entry(Rc::as_ptr(c) as usize).or_default() += 1;
        }
        let mut held: HashMap<usize, (usize, Rc<RefCell<PhpObject>>)> = HashMap::new();
        for c in cells {
            let seen = cell_refs[&(Rc::as_ptr(c) as usize)];
            // +1 = the owner's map slot being torn down after us.
            if Rc::strong_count(c) > seen + 1 {
                continue;
            }
            if let Value::Object(o) = &*c.borrow() {
                held.entry(Rc::as_ptr(o) as usize)
                    .or_insert_with(|| (0, o.clone()))
                    .0 += 1;
            }
        }
        for (_, (n, o)) in held {
            // +1 for the `o` clone sitting in `held` itself.
            if Rc::strong_count(&o) != n + 1 {
                continue;
            }
            let key = Rc::as_ptr(&o) as usize;
            if !self.was_destructed(key)
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

    /// `php -r <code>` mode: the source is tagless PHP parsed in-script
    /// like eval()'d code — a `<?php`/`<?` sequence is a syntax error
    /// (`unexpected token "<", expecting end of file`), never an open
    /// tag; the pseudo-path "Command line code" stays the label.
    pub fn run_code(&mut self, src: &str) -> RunResult {
        if self.ini.contains_key("error_reporting") {
            let lv = self.ini_error_level();
            self.error_level = lv;
        }
        match parser::parse_eval(src, self.ini_on("short_open_tag")) {
            Ok(stmts) => self.run(&stmts),
            Err(e) => {
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
                    .and_then(|_| Self::yield_gate(&stmts))
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
        self.suspended_obs.clear();
        self.silence = 0;
        self.isset_quiet = 0;
        self.detached_dim = false;
        self.dim_key_conv.clear();
        self.dim_undef_cells.clear();
        self.pending_exception = None;
        self.call_trace.clear();
        self.deadline = None;
        self.loop_depth = 0;
    }

    /// Record an object as destructed: true iff newly marked. A
    /// stale weak (its object freed) counts as unmarked — the slot
    /// at that address is simply a different object now.
    fn mark_destructed(&mut self, o: &Rc<RefCell<PhpObject>>) -> bool {
        let key = Rc::as_ptr(o) as usize;
        if self.was_destructed(key) {
            return false;
        }
        self.destructed.insert(key, Rc::downgrade(o));
        if self.destructed.len() > self.destructed_prune {
            self.destructed.retain(|_, w| w.strong_count() > 0);
            self.destructed_prune = (self.destructed.len() * 2).max(1024);
        }
        true
    }

    /// `destructed` membership liveness-aware: a weak whose object
    /// is gone is a stale entry, not a live mark.
    fn was_destructed(&self, key: usize) -> bool {
        self.destructed
            .get(&key)
            .is_some_and(|w| w.upgrade().is_some())
    }

    /// Worker mode: objects created during boot are application state and
    /// must not be destructed at request end. Call once after the boot
    /// phase so per-request shutdown only sweeps request objects.
    pub fn seal_boot_objects(&mut self) {
        self.obj_handles.clear();
        self.obj_born.clear();
        self.dead_slots.clear();
        self.gc_pending = 0;
        self.gc_collecting = false;
        self.gc_purpled.clear();
        self.arr_handles.clear();
        self.obj_died.clear();
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

    /// Mark `c` as bound to storage outside the frame (statics table).
    pub(crate) fn mark_shared(&mut self, c: &Cell) {
        self.shared_cells
            .insert(Rc::as_ptr(c) as usize, Rc::downgrade(c));
        if self.shared_cells.len() > self.shared_cells_prune {
            self.shared_cells.retain(|_, w| w.strong_count() > 0);
            self.shared_cells_prune = (self.shared_cells.len() * 2).max(1024);
        }
    }

    /// Is `c` a live frame-external binding? Same Weak-pin proof as
    /// `is_ref_cell`.
    pub(crate) fn is_shared_cell(&self, c: &Cell) -> bool {
        self.shared_cells
            .get(&(Rc::as_ptr(c) as usize))
            .and_then(|w| w.upgrade())
            .map(|u| Rc::ptr_eq(&u, c))
            .unwrap_or(false)
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
                return Ok(c.borrow().clone());
            }
            if let Some(c) = self.superglobal_cell(name) {
                return Ok(c.borrow().clone());
            }
            if !self.is_quiet() {
                self.warn(&format!("Undefined variable ${}", name))?;
            }
            return Ok(Value::Null);
        }
        let found = self.cur().vars.get(name).cloned();
        match found {
            Some(c) => Ok(c.borrow().clone()),
            None => match self.superglobal_cell(name) {
                Some(c) => Ok(c.borrow().clone()),
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

    /// Write a var slot: the displaced zval decrefs first — its
    /// __destruct (when this was the last ref) runs immediately and
    /// sees the NEW value already in place (gh10168).
    fn var_set(&mut self, name: &str, v: Value) -> Result<(), PhpError> {
        match self.var_cell_opt(name) {
            Some(c) => {
                let old = std::mem::replace(&mut *c.borrow_mut(), v);
                self.destruct_dying_value(&old)
            }
            None => {
                crate::interp::util::alloc_hit(5);
                self.cur()
                    .vars
                    .insert(name.to_string(), Rc::new(RefCell::new(v)));
                Ok(())
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
                let old = std::mem::replace(&mut *c.borrow_mut(), nv);
                self.destruct_dying_value(&old)
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

    /// zend_mm footprint of an emalloc request: bin slots for small
    /// allocs (approximated as align8 — zend's bins step 8..3072),
    /// whole 4KB pages for large/huge ones.
    fn mem_fp(req: u64) -> u64 {
        if req > MM_SMALL {
            (req + 4095) & !4095
        } else {
            (req + 7) & !7
        }
    }

    /// zend arData request for `len` live entries: next-pow2 capacity
    /// — packed arrays cost 16B/slot + header, mixed HTs ~40B/slot
    /// (32B bucket + hash/data overhead; calibrated to oracle 'tried
    /// to allocate' reports, e.g. 1310720 = 32768*40).
    pub(crate) fn ht_req(len: usize, packed: bool) -> u64 {
        let cap = (len.max(1) as u64).next_power_of_two().max(8);
        if packed {
            cap * 16 + 8
        } else {
            cap * 40
        }
    }

    /// heap->real_size: committed 2MB chunks + live huge segments —
    /// memory_get_usage(true) input.
    pub(crate) fn mem_real(&self) -> u64 {
        self.mem_committed + self.mem_huge
    }

    /// Whether a charge already tripped the limit — callers simulating
    /// multi-alloc sequences stop at the first failure like zend's
    /// emalloc bailout (the stmt boundary raises the same fatal).
    pub(crate) fn mem_tripped(&self) -> bool {
        self.oom_at.is_some()
    }

    /// zend's memory_limit check runs inside the allocator where a
    /// request forces newly committed memory: a huge alloc needs its
    /// own segment (real_size + aligned request vs limit), anything
    /// else needs a fresh 2MB chunk once the in-chunk footprint
    /// overflows the committed span (get_chunk's branch). The first
    /// failure records the allocating call site for the deferred
    /// fatal and the 'tried to allocate' figure zend reports — the
    /// raw request for huge allocs, page-aligned for chunk-served
    /// ones. Returns that figure when the commit would overflow.
    /// ponytail: chunk-granular — zend checks exact free page runs
    /// per size class; the in-chunk counter conservatively assumes no
    /// reusable runs, so fragmented heaps can trip a bit earlier.
    /// PR #88 owns the canonical page-level model.
    pub(crate) fn mem_check(&mut self, req: u64) -> Option<u64> {
        let raw = self.ini.get("memory_limit").map(String::as_str);
        if raw != self.mem_limit_ck.1.as_deref() {
            let v = self.ini_bytes("memory_limit");
            self.mem_limit_ck = (v, raw.map(str::to_string));
        }
        let limit = self.mem_limit_ck.0;
        if limit <= 0 {
            return None;
        }
        let limit = limit as u64;
        let fp = Self::mem_fp(req);
        // A run needs a fresh chunk when no in-list chunk can host
        // it — zend then commits another 2MB, and the limit check is
        // that commit's overflow, not the run's own bytes. Cached
        // and unmapped slots aren't in zend's chunk list, so only
        // occupied runs (and the always-mapped main chunk) host.
        let needs_chunk = |s: &Self| {
            req <= MM_MAX_LARGE
                && !s
                    .mem_chunks
                    .iter()
                    .enumerate()
                    .any(|(i, &u)| (u > 0 || i == 0) && u + fp <= MM_CHUNK)
        };
        // Crossing a commit boundary — reclaim dead charges first
        // (zend frees blocks at efree; the Weak probes catch up here
        // so a just-died large alloc never trips the limit).
        if ((req > MM_MAX_LARGE && self.mem_real().saturating_add(fp) > limit) || needs_chunk(self))
            && !self.mem_tracked.is_empty()
        {
            self.mem_sweep();
        }
        let report = if req > MM_MAX_LARGE {
            if self.mem_real().saturating_add(fp) > limit {
                // huge segments report the 8-aligned request (zend_mm
                // safe_error gets the header-adjusted size).
                Some((req + 7) & !7)
            } else {
                None
            }
        } else if needs_chunk(self)
            // get_chunk pops the cached chunk first — that path skips
            // the limit check entirely (zend_mm_get_chunk).
            && self.mem_cached.is_empty()
            && self.mem_real().saturating_add(MM_CHUNK) > limit
        {
            Some(fp)
        } else {
            None
        };
        if let Some(report) = report {
            self.oom_record(report);
        }
        report
    }

    /// zend dies inside the FIRST failed emalloc — later requests in
    /// the same deferred run must not overwrite the fatal's 'tried
    /// to allocate' figure.
    fn oom_record(&mut self, report: u64) {
        if self.oom_at.is_none() {
            self.mem_last = report;
            self.oom_at = Some((self.cur_line, self.fatal_frames()));
        }
    }

    /// Extend runway of a freshly (re)placed huge segment — its own
    /// footprint, the freed predecessor's span above it (`hole`:
    /// the old segment's whole cap on reloc — footprint plus
    /// leftover runway — an emptied chunk's 2MB, or 0), and the
    /// alignment gap the kernel's 2MB-aligned top-down placement
    /// leaves below the predecessor's base. Verified against
    /// /proc/self/maps on the oracle: gap = (-fp) mod MM_CHUNK.
    fn seg_stretch(fp: u64, hole: u64) -> u64 {
        fp + (MM_CHUNK - fp % MM_CHUNK) % MM_CHUNK + hole
    }

    /// mem_check for flat emalloc results: zend reports the raw
    /// request for huge allocs on this path (str_repeat's figure is
    /// len+32 unaligned, unlike erealloc-grown buffers' aligned
    /// report).
    pub(crate) fn mem_check_flat(&mut self, req: u64) -> Option<u64> {
        let fresh = self.oom_at.is_none();
        let r = self.mem_check(req);
        if fresh && r.is_some() && req > MM_MAX_LARGE {
            // Same deferred fatal — just restate the figure raw.
            self.mem_last = req;
        }
        r
    }

    /// Commit an emalloc request: run the limit check, then book the
    /// footprint (the alloc exists until the deferred fatal raises).
    /// Returns the booked footprint and, for chunk-served requests,
    /// the chunk the run landed in (usize::MAX for huge segments).
    fn mem_commit(&mut self, req: u64) -> (u64, usize) {
        // efree half of emalloc/efree: dead tracked allocs release
        // their footprint before the fit check — the fatal must not
        // fire on bytes whose owners already died (zend_mm_gc frees
        // blocks first).
        if self.mem_tracked.len() >= self.mem_sweep_at {
            self.mem_sweep();
        }
        let fp = Self::mem_fp(req);
        let _ = self.mem_check(req);
        self.mem_used += fp;
        let chunk = if req > MM_MAX_LARGE {
            self.mem_huge += fp;
            usize::MAX
        } else {
            self.mem_in_chunk += fp;
            self.chunk_place(fp)
        };
        if self.mem_used > self.mem_peak {
            self.mem_peak = self.mem_used;
        }
        let real = self.mem_real();
        if real > self.mem_real_peak {
            self.mem_real_peak = real;
        }
        (fp, chunk)
    }

    /// First-fit an in-chunk run: the oldest in-list chunk with room
    /// takes it (zend scans committed chunks for a free page run —
    /// dead and cached slots aren't in that list); on a miss,
    /// get_chunk pops the cached chunk back into service before
    /// committing a fresh one.
    fn chunk_place(&mut self, fp: u64) -> usize {
        for (i, u) in self.mem_chunks.iter_mut().enumerate() {
            if (*u > 0 || i == 0) && *u + fp <= MM_CHUNK {
                *u += fp;
                return i;
            }
        }
        if let Some(i) = self.mem_cached.pop() {
            debug_assert_eq!(self.mem_chunks[i], 0);
            self.mem_chunks[i] = fp;
            // zend_mm_chunk_init runs on the cached-pop path too —
            // the chunk gets a fresh ->num like a new commit.
            self.mem_chunk_nums[i] = self.mem_chunk_num_next;
            self.mem_chunk_num_next += 1;
            return i;
        }
        // Fresh 2MB commit — reuse a dead slot or append.
        for (i, u) in self.mem_chunks.iter_mut().enumerate() {
            if *u == 0 && i != 0 {
                *u = fp;
                self.mem_chunk_nums[i] = self.mem_chunk_num_next;
                self.mem_chunk_num_next += 1;
                self.mem_committed += MM_CHUNK;
                return i;
            }
        }
        self.mem_chunks.push(fp);
        self.mem_chunk_nums.push(self.mem_chunk_num_next);
        self.mem_chunk_num_next += 1;
        self.mem_committed += MM_CHUNK;
        self.mem_chunks.len() - 1
    }

    /// zend_mm_delete_chunk: an emptied non-main chunk leaves the
    /// committed list. It goes onto cached_chunks — stays mapped and
    /// get_chunk pops it back skipping the limit check — while the
    /// heap is collapsing (chunks+cached < avg+0.1, and zend only
    /// recomputes avg at request end so it is ~1 intra-request) or
    /// once 4+ consecutive unmaps hit the same chunks_count boundary
    /// (last_chunks_delete_count hysteresis). Otherwise a chunk
    /// unmaps: the emptied one when it is newer than the cached head,
    /// else the head (zend_mm chunk->num ordering — `nums` tracks it,
    /// the slot index can't: a reused slot is newer than it looks).
    /// The boundary/count update runs only with an empty
    /// cache, like zend. Caller has already zeroed the occupancy.
    /// Field-level args keep this callable while a mem_tracked entry
    /// borrow is outstanding. Returns true when the emptied chunk
    /// stays mapped (cached directly, or swapped in for the head).
    fn chunk_vacate(
        chunks: &mut [u64],
        nums: &[u64],
        committed: &mut u64,
        cached: &mut Vec<usize>,
        del_boundary: &mut usize,
        del_count: &mut usize,
        idx: usize,
    ) -> bool {
        let live = chunks
            .iter()
            .enumerate()
            .filter(|(j, &u)| *j != idx && !cached.contains(j) && (u > 0 || *j == 0))
            .count();
        if live + cached.len() <= 1 || (live == *del_boundary && *del_count >= 4) {
            cached.push(idx);
            return true;
        }
        *committed = committed.saturating_sub(MM_CHUNK);
        if cached.is_empty() {
            if live != *del_boundary {
                *del_boundary = live;
                *del_count = 0;
            } else {
                *del_count += 1;
            }
        }
        match cached.last() {
            // Emptied is older than the cached head — zend unmaps the
            // head and caches this one instead (its span stays
            // committed).
            Some(&head) if nums[idx] < nums[head] => {
                *cached.last_mut().unwrap() = idx;
                true
            }
            // Nothing cached, or the emptied chunk is the newer one
            // — it unmaps itself.
            _ => false,
        }
    }

    /// Return fp bytes to the recorded chunk. Occupancy is
    /// chunk-level, so a release just lowers the tally; a chunk that
    /// hits zero goes through chunk_vacate (zend frees fully-empty
    /// chunks — only the main chunk, index 0 here, survives).
    fn chunk_release(&mut self, idx: usize, fp: u64) {
        if let Some(u) = self.mem_chunks.get_mut(idx) {
            let nu = u.saturating_sub(fp);
            let was = *u;
            *u = nu;
            if nu == 0 && was != 0 && idx != 0 {
                Self::chunk_vacate(
                    &mut self.mem_chunks,
                    &self.mem_chunk_nums,
                    &mut self.mem_committed,
                    &mut self.mem_cached,
                    &mut self.chunks_del_boundary,
                    &mut self.chunks_del_count,
                    idx,
                );
            }
        }
    }

    /// Free VA a placement slot currently offers — a marked hole's
    /// span, or a zombie's: a slot still keyed by a dead charge is
    /// already unmapped in zend (efree munmaps at the free, ahead of
    /// the sim's lazy probe sweep), so its range is fair game for
    /// both fresh placement and a neighbor's extension.
    fn slot_span(&self, s: SegSlot) -> u64 {
        if s.hole > 0 {
            return s.hole;
        }
        match self.mem_tracked.get(&s.key) {
            Some(c) if !(c.probe)() => c.seg_cap,
            _ => 0,
        }
    }

    /// A freed huge segment's span stays a hole in its placement
    /// slot — never eager credit to the segment below: the kernel's
    /// top-down mmap reoccupies a fitting span with the next fresh
    /// segment (a same-size temp cycles through its predecessor's
    /// range every iteration), so the credit lands lazily — seg_place
    /// steals a fitting hole, seg_drain merges what is still free
    /// into the segment below at its own grow.
    fn seg_free(&mut self, key: usize, span: u64) {
        if let Some(s) = self
            .mem_seg_order
            .iter_mut()
            .find(|s| s.key == key && s.hole == 0)
        {
            *s = SegSlot {
                key: usize::MAX,
                hole: span,
            };
        }
    }

    /// Place a fresh huge segment: the kernel's top-down mmap drops
    /// it at the top of the highest free span big enough for the
    /// placement (footprint + alignment tail — the maps-verified
    /// stretch), interior holes included; a smaller hole means the
    /// segment stacks at the bottom canyon below every live segment.
    /// The landing sits adjacent below the mapping above, so its own
    /// runway is just the trim tail — the leftover of the stolen span
    /// stays a hole below it.
    fn seg_place(&mut self, key: usize, fp: u64) -> u64 {
        self.seg_place_at(key, fp).1
    }

    /// seg_place that also returns the placed index, so a reloc
    /// caller can free the predecessor slot by position when the new
    /// segment's key repeats the old one's.
    fn seg_place_at(&mut self, key: usize, fp: u64) -> (usize, u64) {
        // The gap must hold the whole reservation: zend mmaps
        // size + alignment - page before trimming — an exactly
        // fitting gap does not qualify.
        let need = fp + MM_CHUNK - 4096;
        // Topmost contiguous free run big enough for the placement —
        // the kernel's gap is every contiguous free span, interior
        // holes included. The landing sits at the run's top, adjacent
        // below the mapping that bounds it.
        let mut i = 0;
        while i < self.mem_seg_order.len() {
            let mut total = 0u64;
            let mut j = i;
            while j < self.mem_seg_order.len() {
                let span = self.slot_span(self.mem_seg_order[j]);
                if span == 0 {
                    break;
                }
                total = total.saturating_add(span);
                j += 1;
            }
            if total > need {
                // Consume the run top-down; leftovers stay below.
                let mut left = need;
                while left > 0 {
                    let span = self.slot_span(self.mem_seg_order[i]);
                    if span <= left {
                        left -= span;
                        self.mem_seg_order.remove(i);
                    } else {
                        self.mem_seg_order[i].hole = span - left;
                        break;
                    }
                }
                self.mem_seg_order.insert(i, SegSlot { key, hole: 0 });
                return (i, Self::seg_stretch(fp, 0));
            }
            i = if j > i { j } else { i + 1 };
        }
        self.mem_seg_order.push(SegSlot { key, hole: 0 });
        (self.mem_seg_order.len() - 1, Self::seg_stretch(fp, 0))
    }

    /// seg_free variant keyed by position — the relocated charge keeps
    /// its key, so the predecessor slot is whichever match is NOT `at`.
    fn seg_free_except(&mut self, key: usize, span: u64, at: usize) {
        if let Some((i, _)) = self
            .mem_seg_order
            .iter()
            .enumerate()
            .find(|(i, s)| *i != at && s.key == key && s.hole == 0)
        {
            self.mem_seg_order[i] = SegSlot {
                key: usize::MAX,
                hole: span,
            };
        }
    }

    /// Free space directly above a live segment — the contiguous run
    /// of still-free slots ending right on top of it. This is the
    /// mremap headroom zend can extend into; it is shared with fresh
    /// placement (seg_place steals from the same runs), so it is
    /// recomputed rather than merged: a span a temp reoccupied in the
    /// meantime never counts.
    fn seg_above(&self, key: usize) -> u64 {
        let Some(q) = self
            .mem_seg_order
            .iter()
            .position(|s| s.key == key && s.hole == 0)
        else {
            return 0;
        };
        let mut span = 0u64;
        for i in (0..q).rev() {
            let s = self.slot_span(self.mem_seg_order[i]);
            if s == 0 {
                break;
            }
            span = span.saturating_add(s);
        }
        span
    }

    /// Consume `need` bytes of the free space directly above a segment
    /// — the mremap extension eats its own tail first (inside seg_cap)
    /// then the contiguous run from the nearest slot up.
    fn seg_consume_above(&mut self, key: usize, need: u64) {
        let Some(q) = self
            .mem_seg_order
            .iter()
            .position(|s| s.key == key && s.hole == 0)
        else {
            return;
        };
        let mut left = need;
        for i in (0..q).rev() {
            if left == 0 {
                break;
            }
            let span = self.slot_span(self.mem_seg_order[i]);
            if span == 0 {
                break;
            }
            if span <= left {
                left -= span;
                self.mem_seg_order.remove(i);
            } else {
                self.mem_seg_order[i].hole = span - left;
                break;
            }
        }
    }

    /// Charge `req` bytes and tie the footprint to `rc`'s lifetime —
    /// released when every strong ref dies (zend's efree).
    pub(crate) fn mem_track<T: ?Sized + 'static>(&mut self, rc: &Rc<T>, req: u64) {
        crate::interp::util::alloc_hit(2);
        let (fp, chunk) = self.mem_commit(req);
        let (inner, huge) = if req > MM_MAX_LARGE { (0, fp) } else { (fp, 0) };
        let key = Rc::as_ptr(rc) as *const u8 as usize;
        let mut stale_cap = 0u64;
        match self.mem_tracked.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if !(e.get().probe)() {
                    // Allocator recycled a dead owner's pointer —
                    // release the stale charge, then re-register.
                    let dead = e.get();
                    self.mem_in_chunk = self.mem_in_chunk.saturating_sub(dead.inner);
                    self.mem_huge = self.mem_huge.saturating_sub(dead.huge);
                    self.mem_used = self.mem_used.saturating_sub(dead.inner + dead.huge);
                    if let Some((seg, slots, own)) = dead.vm {
                        Self::vm_stack_apply(&mut self.vm_stack, seg, slots, own);
                    }
                    let dead_chunk = dead.chunk;
                    let dead_inner = dead.inner;
                    if dead.huge > 0 {
                        stale_cap = dead.seg_cap;
                    }
                    let c = e.get_mut();
                    c.inner = 0;
                    c.huge = 0;
                    c.table_req = 0;
                    c.vm = None;
                    c.seg_cap = 0;
                    c.chunk = usize::MAX;
                    let emptied = self.mem_chunks.get_mut(dead_chunk).is_some_and(|u| {
                        let nu = u.saturating_sub(dead_inner);
                        let emptied = nu == 0 && *u != 0 && dead_chunk != 0;
                        *u = nu;
                        emptied
                    });
                    if emptied {
                        Self::chunk_vacate(
                            &mut self.mem_chunks,
                            &self.mem_chunk_nums,
                            &mut self.mem_committed,
                            &mut self.mem_cached,
                            &mut self.chunks_del_boundary,
                            &mut self.chunks_del_count,
                            dead_chunk,
                        );
                    }
                    let weak = Rc::downgrade(rc);
                    c.probe = Box::new(move || weak.strong_count() > 0);
                }
                if inner > 0 {
                    e.get_mut().chunk = chunk;
                }
                if huge > 0 {
                    e.get_mut().seg_cap = Self::seg_stretch(fp, 0);
                }
                e.get_mut().inner += inner;
                e.get_mut().huge += huge;
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                let weak = Rc::downgrade(rc);
                e.insert(MemCharge {
                    inner,
                    huge,
                    table_req: 0,
                    vm: None,
                    seg_cap: if req > MM_MAX_LARGE {
                        Self::seg_stretch(fp, 0)
                    } else {
                        0
                    },
                    chunk: if inner > 0 { chunk } else { usize::MAX },
                    probe: Box::new(move || weak.strong_count() > 0),
                });
            }
        }
        if stale_cap > 0 {
            // The recycled pointer's dead segment frees its span —
            // a hole the fresh placement below reoccupies when it
            // fits (a same-size temp cycles through the range).
            self.seg_free(key, stale_cap);
        }
        if huge > 0
            && !self
                .mem_seg_order
                .iter()
                .any(|s| s.key == key && s.hole == 0)
        {
            let cap = self.seg_place(key, fp);
            if let Some(c) = self.mem_tracked.get_mut(&key) {
                c.seg_cap = cap;
            }
        }
    }

    /// zend_vm_stack_extend_call_frame — grow the arena for a call
    /// whose span is `out`'s current arg total plus the frame's
    /// overhead. Fits: bump the top segment's used. Overflow: copy
    /// the in-progress call into a fresh segment sized for its whole
    /// span — zend_vm_stack_new_page emallocs it, and that request
    /// is the figure zend's OOM reports.
    fn vm_call_push(&mut self, out: &mut CallArgs) {
        let n = (out.cells.len() + out.named.len()) as u64;
        self.vm_call_reserve(out, n);
    }

    /// Reserve `VM_FRAME_SLOTS + slots` on the call's arena span once —
    /// arg_cells calls it upfront with the arg count so the end-of-args
    /// catch-up (and one VmSite token + mem_track pair) drops away for
    /// the common no-unpack call.
    fn vm_call_reserve(&mut self, out: &mut CallArgs, slots: u64) {
        let want = VM_FRAME_SLOTS + slots;
        if want <= out.vm_slots {
            return;
        }
        let extra_slots = want - out.vm_slots;
        let extra = extra_slots * 16;
        let top = self.vm_stack.last().unwrap();
        if top.size.saturating_sub(top.used) < extra {
            // zend_vm_stack_copy_call_frame: the whole in-progress
            // call moves into the new segment; prev's top rolls back
            // to the call's base and the segment frees when emptied —
            // while the new segment's charge is already committed.
            let used = want * 16;
            let size = (used + 32 + VM_PAGE - 1) & !(VM_PAGE - 1);
            let tok: Rc<VmSite> = Rc::new(VmSite);
            self.mem_track(&tok, size);
            let own = Rc::as_ptr(&tok) as *const u8 as usize;
            let prev = self.vm_stack.len() - 1;
            self.vm_stack[prev].used = self.vm_stack[prev]
                .used
                .saturating_sub(out.vm_slots * 16)
                .max(32);
            if self.vm_stack[prev].used <= 32 {
                self.vm_stack.remove(prev);
            }
            self.vm_stack.push(VmSeg {
                size,
                used: 32 + used,
                tok,
            });
            // The call's earlier pushes moved into the new segment —
            // their rollbacks were settled by the prev rollback above.
            for arc in &out.vm_sites {
                let key = Rc::as_ptr(arc) as *const u8 as usize;
                if let Some(c) = self.mem_tracked.get_mut(&key) {
                    c.vm = None;
                }
            }
            out.vm_slots = want;
            self.vm_site(out, own, extra_slots, own);
            return;
        }
        let seg_key = Rc::as_ptr(&top.tok) as *const u8 as usize;
        self.vm_stack.last_mut().unwrap().used += extra;
        out.vm_slots = want;
        self.vm_site(out, seg_key, extra_slots, 0);
    }

    /// Record one arena push: the site token's MemCharge carries the
    /// rollback — repaid at the owning frame's pop (vm_frame_free) or
    /// at sweep once a never-dispatched call's token dies.
    fn vm_site(&mut self, out: &mut CallArgs, seg: usize, slots: u64, own: usize) {
        crate::interp::util::alloc_hit(1);
        let site: Rc<VmSite> = Rc::new(VmSite);
        self.mem_track(&site, 0);
        let key = Rc::as_ptr(&site) as *const u8 as usize;
        if let Some(c) = self.mem_tracked.get_mut(&key) {
            c.vm = Some((seg, slots, own));
        }
        out.vm_sites.push(site);
    }

    /// Repay one pushed span's `used` and free its minted segment —
    /// the frame pop frees the segment it extended (ZEND_CALL_
    /// ALLOCATED); earlier segments freed by a copy are already gone,
    /// so lookup misses are benign.
    fn vm_stack_apply(stack: &mut Vec<VmSeg>, seg: usize, slots: u64, own: usize) {
        let key_of = |s: &VmSeg| Rc::as_ptr(&s.tok) as *const u8 as usize;
        if let Some(i) = stack.iter().rposition(|s| key_of(s) == seg) {
            stack[i].used = stack[i].used.saturating_sub(slots * 16).max(32);
        }
        if own != 0 {
            if let Some(i) = stack.iter().rposition(|s| key_of(s) == own) {
                stack.remove(i);
            }
        }
    }

    /// Frame teardown frees the call's arena span — zend releases it
    /// at pop, not when the next frame pops.
    fn vm_frame_free(&mut self, sites: &[Rc<VmSite>]) {
        for arc in sites {
            let key = Rc::as_ptr(arc) as *const u8 as usize;
            let vm = self.mem_tracked.get_mut(&key).and_then(|c| c.vm.take());
            if let Some((seg, slots, own)) = vm {
                Self::vm_stack_apply(&mut self.vm_stack, seg, slots, own);
            }
        }
    }

    /// Pop a call frame and free its vm_stack span.
    fn stack_pop(&mut self) -> Option<Frame> {
        let f = self.stack.pop();
        if let Some(f) = &f {
            self.vm_frame_free(&f.vm_sites);
        }
        f
    }

    /// arData realloc accounting for table growth: book the new
    /// capacity request and retire the previous one's footprint —
    /// zend's erealloc transiently holds both, so charge-then-credit.
    /// A same-capacity call is a no-op (zend only reallocs when the
    /// pow2 class advances).
    pub(crate) fn mem_realloc<T: ?Sized + 'static>(&mut self, rc: &Rc<T>, req: u64) {
        let key = Rc::as_ptr(rc) as *const u8 as usize;
        let mut old_live = None;
        if let Some(c) = self.mem_tracked.get(&key) {
            if (c.probe)() && c.table_req == req {
                return;
            }
            if (c.probe)() {
                old_live = Some((c.table_req, c.chunk));
            }
        }
        // Huge→huge erealloc (zend_mm_realloc_huge): extend the
        // segment in place when the request stays inside its stretch
        // of free address space — real_size still counts the old
        // segment and the limit check is the growth *delta* against
        // headroom. Past seg_cap the kernel can't extend, so zend
        // relocates: the new segment is sized while the old one is
        // still held (the stricter check).
        if let Some((old, _)) = old_live {
            if req > MM_MAX_LARGE && old > MM_MAX_LARGE {
                let ofp = Self::mem_fp(old);
                let fp = Self::mem_fp(req);
                if fp <= ofp {
                    // Shrink (or same size): the tail unmaps — zend
                    // runs no limit check on that path.
                    let d = ofp - fp;
                    self.mem_huge = self.mem_huge.saturating_sub(d);
                    self.mem_used = self.mem_used.saturating_sub(d);
                    if let Some(c) = self.mem_tracked.get_mut(&key) {
                        c.huge = c.huge.saturating_sub(d);
                        c.table_req = req;
                    }
                    let real = self.mem_real();
                    if real > self.mem_real_peak {
                        self.mem_real_peak = real;
                    }
                    return;
                }
                // The extendable bound is the seg's own extent plus
                // the free space still directly above it — recomputed
                // so a span a temp reoccupied meanwhile never counts.
                let cap = self.mem_tracked.get(&key).map(|c| c.seg_cap).unwrap_or(0)
                    + self.seg_above(key);
                let reloc = fp > cap;
                let limit = self.ini_bytes("memory_limit");
                let over = |s: &Self| -> bool {
                    if limit <= 0 {
                        return false;
                    }
                    let real = s.mem_real();
                    if reloc {
                        // alloc-before-free: old seg still held.
                        real.saturating_add(fp) > limit as u64
                    } else {
                        // in-place: only the delta is committed.
                        real.saturating_add(fp - ofp) > limit as u64
                    }
                };
                // A failing commit frees dead charges and retries
                // (zend_mm_gc) — sweep before reporting.
                if over(self) && !self.mem_tracked.is_empty() {
                    self.mem_sweep();
                }
                let failed = over(self);
                if failed {
                    self.oom_record((req + 7) & !7);
                }
                self.mem_huge += fp - ofp;
                self.mem_used += fp - ofp;
                // Relocated: the new segment lands at the bottom of
                // the address space while the old one is still held;
                // the old span frees in place for the segment below
                // to drain at its grow — same as mem_grow_str.
                let ncap = if reloc && !failed {
                    // The old slot's charge is still live — it can't
                    // be a steal target; the new segment lands
                    // elsewhere and the old span frees in place.
                    let (at, n) = self.seg_place_at(key, fp);
                    let old = self.mem_tracked.get(&key).map(|c| c.seg_cap).unwrap_or(0);
                    if old > 0 {
                        self.seg_free_except(key, old, at);
                    }
                    Some(n)
                } else {
                    None
                };
                if !reloc && !failed {
                    // In-place: the extent eats `fp - ofp` of the free
                    // space above — its own tail first, then the run.
                    let tail = cap.saturating_sub(ofp).saturating_sub(self.seg_above(key));
                    let on_chain = (fp - ofp).saturating_sub(tail);
                    if on_chain > 0 {
                        self.seg_consume_above(key, on_chain);
                    }
                }
                if let Some(c) = self.mem_tracked.get_mut(&key) {
                    c.huge = fp;
                    c.table_req = req;
                    if !failed {
                        c.seg_cap = fp.max(ncap.unwrap_or(c.seg_cap));
                    }
                }
                if self.mem_used > self.mem_peak {
                    self.mem_peak = self.mem_used;
                }
                let real = self.mem_real();
                if real > self.mem_real_peak {
                    self.mem_real_peak = real;
                }
                return;
            }
        }
        // In-place growth: the grown run still fits the old run's
        // chunk once the old bytes are freed there — zend merges the
        // adjacent free pages into the run, no new chunk commits and
        // no limit check runs at all.
        if let Some((old, ci)) = old_live {
            if req <= MM_MAX_LARGE && old > 0 && old <= MM_MAX_LARGE {
                let ofp = Self::mem_fp(old);
                let fp = Self::mem_fp(req);
                if let Some(u) = self.mem_chunks.get_mut(ci) {
                    if *u - ofp + fp <= MM_CHUNK {
                        *u = *u - ofp + fp;
                        self.mem_in_chunk = self.mem_in_chunk - ofp + fp;
                        self.mem_used = self.mem_used - ofp + fp;
                        if let Some(c) = self.mem_tracked.get_mut(&key) {
                            c.inner = c.inner - ofp + fp;
                            c.table_req = req;
                        }
                        if self.mem_used > self.mem_peak {
                            self.mem_peak = self.mem_used;
                        }
                        let real = self.mem_real();
                        if real > self.mem_real_peak {
                            self.mem_real_peak = real;
                        }
                        return;
                    }
                }
            }
        }
        let (fp, chunk) = self.mem_commit(req);
        let (inner, huge) = if req > MM_MAX_LARGE { (0, fp) } else { (fp, 0) };
        let mut top_credit = 0u64;
        let mut old_seg_cap = 0u64;
        match self.mem_tracked.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let old = std::mem::replace(&mut e.get_mut().table_req, req);
                let mut hole = 0;
                if old > 0 {
                    let ofp = Self::mem_fp(old);
                    if old > MM_MAX_LARGE {
                        old_seg_cap = e.get().seg_cap;
                        e.get_mut().huge = e.get().huge.saturating_sub(ofp);
                        self.mem_huge = self.mem_huge.saturating_sub(ofp);
                        hole = old_seg_cap;
                    } else {
                        let ci = e.get().chunk;
                        e.get_mut().inner = e.get().inner.saturating_sub(ofp);
                        self.mem_in_chunk = self.mem_in_chunk.saturating_sub(ofp);
                        let emptied = self.mem_chunks.get_mut(ci).is_some_and(|u| {
                            let nu = u.saturating_sub(ofp);
                            let emptied = nu == 0 && *u != 0 && ci != 0;
                            *u = nu;
                            emptied
                        });
                        if emptied
                            && !Self::chunk_vacate(
                                &mut self.mem_chunks,
                                &self.mem_chunk_nums,
                                &mut self.mem_committed,
                                &mut self.mem_cached,
                                &mut self.chunks_del_boundary,
                                &mut self.chunks_del_count,
                                ci,
                            )
                        {
                            // Unmapped — the freed span extends the
                            // segment directly below it: this new
                            // segment when nothing stood between
                            // (the ob buffer's first huge
                            // transition), else the topmost live
                            // segment.
                            if self.mem_seg_order.is_empty() {
                                hole = MM_CHUNK;
                            } else {
                                top_credit = MM_CHUNK;
                            }
                        }
                    }
                    self.mem_used = self.mem_used.saturating_sub(ofp);
                }
                if inner > 0 {
                    e.get_mut().chunk = chunk;
                }
                if req > MM_MAX_LARGE {
                    // Freshly-placed huge segment (dead entry, or
                    // grown up from in-chunk) — new extend runway.
                    e.get_mut().seg_cap = Self::seg_stretch(fp, hole);
                }
                e.get_mut().inner += inner;
                e.get_mut().huge += huge;
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                let weak = Rc::downgrade(rc);
                e.insert(MemCharge {
                    inner,
                    huge,
                    table_req: req,
                    vm: None,
                    seg_cap: if req > MM_MAX_LARGE {
                        Self::seg_stretch(fp, 0)
                    } else {
                        0
                    },
                    chunk: if inner > 0 { chunk } else { usize::MAX },
                    probe: Box::new(move || weak.strong_count() > 0),
                });
            }
        }
        if old_seg_cap > 0 {
            // The retired table was a huge segment — its freed span
            // becomes a hole in its slot (lazy credit: whoever below
            // grows into it while it stays free takes it).
            self.seg_free(key, old_seg_cap);
        }
        if top_credit > 0 {
            let top = self
                .mem_seg_order
                .iter()
                .find(|&s| self.slot_span(*s) == 0)
                .map(|s| s.key);
            if let Some(top) = top {
                if let Some(c) = self.mem_tracked.get_mut(&top) {
                    c.seg_cap = c.seg_cap.saturating_add(top_credit);
                }
            }
        }
        if huge > 0
            && !self
                .mem_seg_order
                .iter()
                .any(|s| s.key == key && s.hole == 0)
        {
            self.mem_seg_order.push(SegSlot { key, hole: 0 });
        }
    }

    /// Book every ob level's smart_string alloc into the sim —
    /// buffered bytes are otherwise invisible to memory_get_usage,
    /// ini_set's usage compare, and the limit trip. Runs at stmt
    /// boundaries, on reconcile, and inline at every buffer mutation
    /// — zend checks the limit inside the erealloc that grows the
    /// buffer, while the write's producing temp is still held, so
    /// the charge can't wait for the boundary.
    /// `mem_realloc`'s table_req dedupe makes unchanged levels free.
    pub(crate) fn ob_meter_sync(&mut self) {
        // The request-lifetime ob stack: booted by the first real
        // level, plus each open level's small handler struct. Never
        // released — oracle retains ~128 after the last pop.
        if self.ob_boot.is_none() {
            let real = self
                .ob_stack
                .iter()
                .chain(self.suspended_obs.iter())
                .any(|l| l.pop_head.is_none());
            if real {
                self.ob_boot = Some(std::rc::Rc::new(()));
            }
        }
        if let Some(boot) = self.ob_boot.clone() {
            let n = self
                .ob_stack
                .iter()
                .chain(self.suspended_obs.iter())
                .filter(|l| l.pop_head.is_none())
                .count() as u64;
            // The ob runtime stack + handler structs live while any
            // real level is open (~2.6K + 128/level); the last pop
            // frees them except a ~128 stub.
            let want = if n > 0 {
                OB_OPEN_STACK + OB_LEVEL_STRUCT * n
            } else {
                OB_RESID
            };
            self.ob_mem_apply(&boot, want);
        }
        // zend's sapi-write machinery is retained for the request
        // once any bytes reached real stdout (+32, stderr excluded).
        if self.emit_boot.is_none() && (self.emit_seen || !self.out.is_empty()) {
            self.emit_boot = Some(std::rc::Rc::new(()));
        }
        if let Some(eb) = self.emit_boot.clone() {
            self.ob_mem_apply(&eb, EMIT_RESID);
        }
        for i in 0..self.ob_stack.len() {
            let (tok, want) = (self.ob_stack[i].mem_tok.clone(), self.ob_stack[i].charged);
            self.ob_mem_apply(&tok, want.max(0) as u64);
        }
        for i in 0..self.suspended_obs.len() {
            let (tok, want) = (
                self.suspended_obs[i].mem_tok.clone(),
                self.suspended_obs[i].charged,
            );
            self.ob_mem_apply(&tok, want.max(0) as u64);
        }
    }

    /// Route one level's current alloc figure through erealloc
    /// accounting: mem_realloc's commit sizes the grown request while
    /// the old segment is still counted, then releases it — zend dies
    /// inside the crossing erealloc with old and new both held.
    fn ob_mem_apply(&mut self, tok: &Rc<()>, want: u64) {
        let key = Rc::as_ptr(tok) as *const u8 as usize;
        if self.mem_tracked.get(&key).map(|c| c.table_req) == Some(want) {
            return;
        }
        self.mem_realloc(tok, want);
    }

    /// Release charges whose owning Rc died — the efree counterpart
    /// of the tracked charges.
    pub(crate) fn mem_sweep(&mut self) {
        let mut inner = 0u64;
        let mut huge = 0u64;
        let mut freed = Vec::new();
        // Freed segments leave their span as a hole in their
        // placement slot — the credit stays lazy: seg_place steals a
        // fitting hole, seg_drain merges what is still free into the
        // segment below at its own grow.
        let mut order = std::mem::take(&mut self.mem_seg_order);
        let tracked = &mut self.mem_tracked;
        order.retain_mut(|s| {
            if s.hole > 0 {
                return true;
            }
            match tracked.get(&s.key) {
                Some(c) if (c.probe)() && c.huge > 0 => true,
                Some(c) => {
                    s.key = usize::MAX;
                    s.hole = c.seg_cap;
                    s.hole > 0
                }
                None => false,
            }
        });
        self.mem_seg_order = order;
        // Releases land on each charge's recorded chunk — a chunk
        // the release empties goes through chunk_vacate (zend frees
        // fully-empty non-main chunks, caching the last one).
        let chunks = &mut self.mem_chunks;
        let mut vacate = Vec::new();
        self.mem_tracked.retain(|_, c| {
            if (c.probe)() {
                true
            } else {
                inner += c.inner;
                huge += c.huge;
                if let Some(v) = c.vm {
                    freed.push(v);
                }
                if let Some(u) = chunks.get_mut(c.chunk) {
                    let nu = u.saturating_sub(c.inner);
                    let was = *u;
                    *u = nu;
                    if nu == 0 && was != 0 && c.chunk != 0 {
                        vacate.push(c.chunk);
                    }
                }
                false
            }
        });
        for ci in vacate {
            Self::chunk_vacate(
                &mut self.mem_chunks,
                &self.mem_chunk_nums,
                &mut self.mem_committed,
                &mut self.mem_cached,
                &mut self.chunks_del_boundary,
                &mut self.chunks_del_count,
                ci,
            );
        }
        self.mem_in_chunk = self.mem_in_chunk.saturating_sub(inner);
        self.mem_huge = self.mem_huge.saturating_sub(huge);
        self.mem_used = self.mem_used.saturating_sub(inner + huge);
        for (seg, slots, own) in freed {
            Self::vm_stack_apply(&mut self.vm_stack, seg, slots, own);
        }
        self.mem_sweep_at = self.mem_tracked.len() + 4096;
    }

    /// Partial efree inside a *living* container: zend frees a
    /// bucket's payload on unset/evict while the table survives —
    /// subtract `req`'s small footprint from the container's charge.
    /// Entries for containers never charged through mem_track are
    /// absent, so untracked owners neither pay nor get refunded.
    pub(crate) fn mem_credit<T: ?Sized + 'static>(&mut self, rc: &Rc<T>, req: u64) {
        let fp = Self::mem_fp(req);
        let key = Rc::as_ptr(rc) as *const u8 as usize;
        if let std::collections::hash_map::Entry::Occupied(mut e) = self.mem_tracked.entry(key) {
            if !(e.get().probe)() {
                // Stale entry of a dead owner at a recycled pointer —
                // release it now; the caller's container was never
                // charged for it.
                let dead = e.remove();
                self.mem_in_chunk = self.mem_in_chunk.saturating_sub(dead.inner);
                self.mem_huge = self.mem_huge.saturating_sub(dead.huge);
                self.mem_used = self.mem_used.saturating_sub(dead.inner + dead.huge);
                if let Some((seg, slots, own)) = dead.vm {
                    Self::vm_stack_apply(&mut self.vm_stack, seg, slots, own);
                }
                if dead.huge > 0 {
                    self.seg_free(key, dead.seg_cap);
                }
            } else {
                let sub = fp.min(e.get().inner);
                let ci = e.get().chunk;
                e.get_mut().inner -= sub;
                self.mem_in_chunk = self.mem_in_chunk.saturating_sub(sub);
                self.mem_used = self.mem_used.saturating_sub(sub);
                self.chunk_release(ci, sub);
            }
        }
    }

    /// Retire a still-live tracked alloc's charge — erealloc growth
    /// semantics: zend replaces the old buffer's pages with the grown
    /// request, so the peak must not double-count the pair.
    pub(crate) fn mem_retire<T: ?Sized + 'static>(&mut self, rc: &Rc<T>) {
        let key = Rc::as_ptr(rc) as *const u8 as usize;
        self.mem_retire_key(key, true);
    }

    /// Drop a tracked charge by key. `inherit` feeds a freed
    /// segment's span to the segment directly below it — false when
    /// the caller replaces the segment in place (a `.=`/erealloc
    /// grow books the hole into its own new cap instead).
    fn mem_retire_key(&mut self, key: usize, inherit: bool) {
        if let Some(c) = self.mem_tracked.remove(&key) {
            self.mem_in_chunk = self.mem_in_chunk.saturating_sub(c.inner);
            self.mem_huge = self.mem_huge.saturating_sub(c.huge);
            self.mem_used = self.mem_used.saturating_sub(c.inner + c.huge);
            if let Some((seg, slots, own)) = c.vm {
                Self::vm_stack_apply(&mut self.vm_stack, seg, slots, own);
            }
            if c.huge > 0 {
                if inherit {
                    self.seg_free(key, c.seg_cap);
                } else {
                    self.mem_seg_order.retain(|s| s.key != key);
                }
            }
            self.chunk_release(c.chunk, c.inner);
        }
    }

    /// erealloc accounting for `.=` on an unshared string, mirroring
    /// zend_mm_realloc: a huge segment mremaps (the old segment is
    /// unmapped before the grown one is sized against the limit), a
    /// page-run extends in place when it still fits the committed
    /// span, and otherwise zend mallocs the grown run while the old
    /// one is held — the limit check sees both.
    /// ponytail: in-place extension uses a 7/8-of-chunk heuristic —
    /// zend checks whether the pages immediately after the old run
    /// are free, which needs the real page map PR #88 owns; without
    /// it the boundary can land one growth step off.
    pub(crate) fn mem_grow_str(&mut self, old: &Rc<[u8]>, new: &Rc<[u8]>, req: u64) {
        // zend frees dead allocs before sizing the grow — sweep
        // unconditionally so freed runs feed the extend test.
        self.mem_sweep();
        let fp = Self::mem_fp(req);
        let (old_inner, old_huge, old_chunk) = self
            .mem_tracked
            .get(&(Rc::as_ptr(old) as *const u8 as usize))
            .map(|c| (c.inner, c.huge, c.chunk))
            .unwrap_or((0, 0, usize::MAX));
        // In-place growth: the grown run fits the same chunk once the
        // old bytes are freed — no chunk commits, no limit check.
        let extend = req <= MM_MAX_LARGE
            && old_inner > 0
            && self
                .mem_chunks
                .get(old_chunk)
                .is_some_and(|&u| u - old_inner + fp <= MM_CHUNK);
        // Whether the grown segment relocates: past its stretch of
        // free address space mremap can't extend, so zend allocs the
        // new segment while the old one is still held (the stricter
        // check). Otherwise the old segment unmaps and only the
        // growth delta is checked. Untracked/dead olds relocate.
        let old_key = Rc::as_ptr(old) as *const u8 as usize;
        // The extendable bound is the seg's own extent plus the free
        // space still directly above it — recomputed, so a span a
        // temp reoccupied meanwhile never counts.
        let old_seg = self
            .mem_tracked
            .get(&old_key)
            .filter(|c| (c.probe)())
            .map(|c| c.seg_cap);
        // ponytail: slot spans approximate the freed region each seg
        // leaves (extent + its own tail); slack between slots (head
        // frags, neighbor tails) isn't tracked, so the boundary can
        // drift ±1 grow step past ~4 extents of runway.
        let reloc = old_seg.is_none_or(|cap| fp > cap + self.seg_above(old_key));
        let mut new_seg_cap = 0u64;
        if req > MM_MAX_LARGE {
            if reloc {
                // alloc+copy+free: size the new segment with old held.
                let _ = self.mem_check(req);
            }
            // Retire the old charge without freeing its placement —
            // the grown segment takes over the slot (reloc lands it
            // directly below the old base, in-place keeps the
            // mapping), so its span is booked in new_seg_cap, not
            // inherited by the segment below.
            if let Some(c) = self.mem_tracked.remove(&old_key) {
                self.mem_in_chunk = self.mem_in_chunk.saturating_sub(c.inner);
                self.mem_huge = self.mem_huge.saturating_sub(c.huge);
                self.mem_used = self.mem_used.saturating_sub(c.inner + c.huge);
                if let Some((seg, slots, own)) = c.vm {
                    Self::vm_stack_apply(&mut self.vm_stack, seg, slots, own);
                }
                self.chunk_release(c.chunk, c.inner);
            }
            if !reloc {
                // in-place extend: old unmaps → delta-only check.
                let _ = self.mem_check(req);
                new_seg_cap = old_seg.unwrap_or(0);
            }
        } else if !extend {
            let _ = self.mem_check(req);
        }
        self.mem_used += fp;
        let mut new_chunk = usize::MAX;
        if req > MM_MAX_LARGE {
            self.mem_huge += fp;
        } else {
            self.mem_in_chunk += fp;
            if extend {
                // Replace the old run in place inside its chunk:
                // occupancy changes by the grown fp minus the freed
                // run, like mem_realloc's in-place arm — adding the
                // raw fp leaks the old bytes every grow.
                if let Some(u) = self.mem_chunks.get_mut(old_chunk) {
                    *u = *u - old_inner + fp;
                }
                self.mem_in_chunk = self.mem_in_chunk.saturating_sub(old_inner);
                self.mem_used = self.mem_used.saturating_sub(old_inner);
                self.mem_tracked
                    .remove(&(Rc::as_ptr(old) as *const u8 as usize));
                new_chunk = old_chunk;
            } else {
                new_chunk = self.chunk_place(fp);
                self.mem_retire(old);
            }
        }
        if self.mem_used > self.mem_peak {
            self.mem_peak = self.mem_used;
        }
        let real = self.mem_real();
        if real > self.mem_real_peak {
            self.mem_real_peak = real;
        }
        let key = Rc::as_ptr(new) as *const u8 as usize;
        if let Some(c) = self.mem_tracked.remove(&key) {
            // Allocator recycled a dead owner's pointer — release the
            // stale charge before re-registering.
            self.mem_in_chunk = self.mem_in_chunk.saturating_sub(c.inner);
            self.mem_huge = self.mem_huge.saturating_sub(c.huge);
            self.mem_used = self.mem_used.saturating_sub(c.inner + c.huge);
            if let Some((seg, slots, own)) = c.vm {
                Self::vm_stack_apply(&mut self.vm_stack, seg, slots, own);
            }
            if c.huge > 0 {
                self.seg_free(key, c.seg_cap);
            }
            self.chunk_release(c.chunk, c.inner);
        }
        if req > MM_MAX_LARGE {
            if reloc {
                // The grown segment lands at the bottom of the
                // address space while the old one is still held —
                // its slot can't host placement (the retired charge
                // is untracked, so the slot still reads occupied) —
                // then the old span frees in place as a hole for the
                // segment below it to drain at its grow.
                self.seg_place_at(key, fp);
                self.seg_free(old_key, old_seg.unwrap_or(0));
                new_seg_cap = Self::seg_stretch(fp, 0);
            } else if let Some(p) = self
                .mem_seg_order
                .iter()
                .position(|s| s.key == old_key && s.hole == 0)
            {
                // In-place extension stands where its predecessor
                // stood in the placement order; it eats the delta
                // of free space above — its own tail first, then
                // the contiguous run from the nearest slot up.
                self.mem_seg_order[p].key = key;
                let cap = old_seg.unwrap_or(0);
                let tail = cap.saturating_sub(old_huge);
                let delta = fp.saturating_sub(old_huge);
                let on_tail = tail.min(delta);
                let on_chain = delta - on_tail;
                new_seg_cap = fp + (tail - on_tail);
                if on_chain > 0 {
                    self.seg_consume_above(key, on_chain);
                }
            } else {
                self.mem_seg_order.push(SegSlot { key, hole: 0 });
            }
        }
        let (inner, huge) = if req > MM_MAX_LARGE { (0, fp) } else { (fp, 0) };
        let weak = Rc::downgrade(new);
        self.mem_tracked.insert(
            key,
            MemCharge {
                inner,
                huge,
                table_req: 0,
                vm: None,
                seg_cap: new_seg_cap,
                chunk: new_chunk,
                probe: Box::new(move || weak.strong_count() > 0),
            },
        );
    }

    /// Reconcile then report the live usage — memory_get_usage()
    /// reads it straight, so sweep dead tracked allocs first.
    pub(crate) fn mem_reconcile(&mut self) -> u64 {
        self.ob_meter_sync();
        self.mem_sweep();
        self.mem_used
    }

    /// The memory_limit fatal as raised mid-call by an oversized alloc
    /// — zend bails out inside emalloc, so a builtin that cannot afford
    /// the real allocation (huge result buffers) returns this itself
    /// instead of waiting for the stmt boundary.
    pub fn oom_fatal(&self) -> PhpError {
        let limit = self.ini_bytes("memory_limit");
        let (line, frames) = self
            .oom_at
            .clone()
            .unwrap_or((self.cur_line, self.fatal_frames()));
        let mut e = PhpError::fatal(
            format!(
                "Allowed memory size of {} bytes exhausted (tried to allocate {} bytes)",
                limit, self.mem_last
            ),
            line,
        );
        e.trace = Some(frames);
        e
    }

    /// Byte-faithful emit — program output is bytes (echo of binary
    /// strings, file reads, preg results must not be UTF-8 validated).
    pub fn emit_bytes(&mut self, b: &[u8]) {
        // Emitted output is free — zend charges only ob-buffered
        // bytes (the ObLevel::mem_sync figure, applied to the zend_mm
        // sim at stmt boundaries by ob_meter_sync).
        // Inside a generator run, output after a yield is deferred to
        // resume — `f(yield)` must not observe the call (nor its echo)
        // until the consumer advances past that yield.
        if self.gen_run_state.is_some() {
            let done = self
                .gen_sink
                .as_ref()
                .map(|s| s.borrow().len())
                .unwrap_or(0)
                + if self
                    .gen_collect_run
                    .as_ref()
                    .zip(self.gen_run_state.as_ref())
                    .is_some_and(|(a, b)| Rc::ptr_eq(a, b))
                {
                    self.gen_collect_seen
                } else {
                    0
                };
            // send()/throw() re-runs replay the prefix the consumer
            // already echoed — suppress live echo AND re-journal for
            // the covered span: the prior run journaled (and mostly
            // emitted) those bytes, and the restart dropped only the
            // stale run's un-emitted tail.
            if self.gen_horizon_suppresses(done) {
                return;
            }
            if done == 0 {
                // A delegate re-collected under an outer's replay
                // horizon: its pre-first-yield bytes were already
                // echoed by the run the consumer saw.
                let suppress = self
                    .gen_run_state
                    .as_ref()
                    .is_some_and(|s| s.borrow().suppress_prefix);
                if suppress {
                    return;
                }
            }
            if done > 0 {
                // An ob opened inside this gen captures the deferred
                // output like Zend's global buffer — journaled per
                // tag so it merges with consumer writes in cursor
                // order instead of echoing raw at replay.
                if let Some(l) = self.ob_stack.last_mut() {
                    // A popped window's mirror must not take the
                    // re-run's writes — those belong to the body's
                    // own deferred journal (the pop already closed
                    // the real buffer).
                    let owns = match (&l.gen_q, &self.gen_run_state) {
                        (Some(q), Some(s)) => {
                            std::rc::Rc::ptr_eq(q, &s.borrow().fin_q) && l.gen_close.is_none()
                        }
                        _ => false,
                    };
                    if owns {
                        // finally-region output belongs to the
                        // destruction journal — a gen-owned capture
                        // window dies before it could replay them.
                        if self.gen_fin_depth == 0 {
                            l.gen_pending.push((done - 1, b.to_vec()));
                            return;
                        }
                    }
                }

                let is_fin = self.gen_fin_depth > 0;
                self.gen_buf_out(done - 1, b, false, is_fin);
                return;
            }
        }
        self.ob_promote();
        self.emit_routed(b);
    }

    /// Zend-arena `size` — live bytes (runtime baseline + object
    /// shells + array tables + string payloads). memory_get_usage
    /// reports this.
    pub(crate) fn mem_total(&self) -> i64 {
        crate::value::MEM_BASE_BYTES + crate::value::mem_live_bytes()
    }

    /// Emit journaled/replayed bytes at their materialization point:
    /// inside another gen's run they join its deferred journal (an
    /// inner's death bytes attribute to the outer's cursor window);
    /// consumer-side they go through emit_passthrough.
    fn emit_replay(&mut self, b: &[u8], tag: usize) {
        if self.gen_run_state.is_some() {
            self.emit_bytes(b);
        } else {
            self.emit_passthrough(b, tag);
        }
    }

    /// Emit already-journaled bytes bound for real output. Replayed
    /// entries capture into the topmost level that was open at the
    /// entry's logical write time (a live consumer `ob_start`, or a
    /// gen window whose open tag the entry reaches) — Zend's shared
    /// stack routes the resume echo the same way. Gen windows the
    /// entry predates never see it: those bytes were journaled into
    /// the level's own `gen_pending`, not `pending_out`.
    fn emit_passthrough(&mut self, b: &[u8], tag: usize) {
        if let Some(l) = self
            .ob_stack
            .iter_mut()
            .rev()
            .find(|l| l.gen_q.is_none() || l.gen_open.is_some_and(|o| tag >= o))
        {
            if l.gen_q.is_some() {
                // A gen window's journaled captures splice by tag like
                // consumer writes — keep the bookkeeping in step.
                let pos = l.gen_q.as_ref().map(|q| q.borrow().vis_pos).unwrap_or(0);
                l.caps.push((pos, b.to_vec()));
                l.cap_segs.push((pos, l.buf.len(), b.len()));
            }
            l.buf.extend_from_slice(b);
            l.mem_sync();
            self.ob_meter_sync();
            return;
        }
        self.emit_seen = true;
        if self.live_io {
            use std::io::Write;
            let mut so = std::io::stdout().lock();
            let _ = so.write_all(b);
            let _ = so.flush();
        } else {
            self.out.extend_from_slice(b);
        }
    }

    fn emit_routed(&mut self, b: &[u8]) {
        if let Some(buf) = self.ob_stack.last_mut() {
            // Cursor-past journaled captures precede this write.
            Self::ob_drain_level(buf, false);
            if buf.gen_q.is_some() {
                // A gen-owned level (live window or pop mirror):
                // tag the write by the cursor it arrived at — kill
                // teardown and mirror-close splices need it.
                let pos = buf.gen_q.as_ref().map(|q| q.borrow().vis_pos).unwrap_or(0);
                buf.caps.push((pos, b.to_vec()));
                buf.cap_segs.push((pos, buf.buf.len(), b.len()));
            }
            buf.buf.extend_from_slice(b);
            buf.mem_sync();
            self.ob_meter_sync();
        } else {
            self.emit_seen = true;
            if self.live_io {
                use std::io::Write;
                let mut so = std::io::stdout().lock();
                let _ = so.write_all(b);
                let _ = so.flush();
            } else {
                self.out.extend_from_slice(b);
            }
        }
    }

    /// A send()/throw() restart replaces the body's earlier run: the
    /// post-yield bytes that run journaled into its own ob windows
    /// (`gen_pending`, plus any tail already drained into `buf`) are
    /// stale — the re-run produces the window's real contents.
    /// Consumer captures (`caps`) are real writes and stay.
    pub(in crate::interp) fn ob_gen_restart(&mut self, fq: &crate::value::FinQueue) {
        let owned = |l: &ObLevel| l.gen_q.as_ref().is_some_and(|q| std::rc::Rc::ptr_eq(q, fq));
        let sweep = |l: &mut ObLevel| {
            if l.pop_head.is_some() {
                // A pop mirror is pure replay bookkeeping — the pop
                // will re-run in the fresh body pass.
                return true;
            }
            l.gen_pending.clear();
            if l.gen_drained > 0 {
                // Drop journaled bytes already merged into buf —
                // the drained tail is the stale run's writes. They
                // interleave with consumer captures, so cut each
                // recorded seg range instead of truncating the tail.
                let mut segs = std::mem::take(&mut l.drained_segs);
                segs.sort_by_key(|(_, off, _)| *off);
                let mut v = Vec::with_capacity(l.buf.len());
                let mut off = 0;
                for &(_, s, e) in &segs {
                    v.extend_from_slice(&l.buf[off..s.min(l.buf.len())]);
                    off = (s + e).min(l.buf.len());
                }
                v.extend_from_slice(&l.buf[off..]);
                // Cap offsets ride on the same buf — shift each by
                // the bytes the excision cut before it.
                for c in l.cap_segs.iter_mut() {
                    c.1 = segs.iter().fold(c.1, |o, (_, s, e)| {
                        o.saturating_sub((*e).min(o.saturating_sub(*s)))
                    });
                }
                l.buf = v;
                l.mem_sync();
                l.gen_drained = 0;
            }
            false
        };
        let mut i = 0;
        while i < self.suspended_obs.len() {
            if owned(&self.suspended_obs[i]) && sweep(&mut self.suspended_obs[i]) {
                self.suspended_obs.remove(i);
            } else {
                i += 1;
            }
        }
        let mut i = 0;
        while i < self.ob_stack.len() {
            if owned(&self.ob_stack[i]) && sweep(&mut self.ob_stack[i]) {
                self.ob_stack.remove(i);
            } else {
                i += 1;
            }
        }
        self.ob_meter_sync();
    }

    /// Move the suspended gen-owned buffers whose open tag the
    /// consumer's cursor passed back onto the real stack — Zend's
    /// buffers are global, so they materialize at resume time,
    /// below anything the consumer pushed while suspended.
    fn ob_promote(&mut self) {
        // A promoted capture window closes once the cursor passes
        // the body's pop point — its consumer writes splice into
        // the journaled pop value before it drops.
        let mut i = 0;
        while i < self.ob_stack.len() {
            let stale = self.ob_stack[i].gen_close.is_some_and(|c| {
                self.ob_stack[i]
                    .gen_q
                    .as_ref()
                    .is_some_and(|q| q.borrow().vis_pos >= c)
            });
            if stale {
                let l = self.ob_stack.remove(i);
                self.ob_mirror_close(&l);
            } else {
                i += 1;
            }
        }
        // Dead mirrors (cursor at/past the body's pop) stay dead —
        // same splice for one demoted back to suspended_obs by a
        // body re-run before it could re-materialize.
        let mut i = 0;
        while i < self.suspended_obs.len() {
            let dead = self.suspended_obs[i].gen_close.is_some_and(|c| {
                self.suspended_obs[i]
                    .gen_q
                    .as_ref()
                    .is_some_and(|q| q.borrow().vis_pos >= c)
            });
            if dead {
                let l = self.suspended_obs.remove(i);
                self.ob_mirror_close(&l);
            } else {
                i += 1;
            }
        }
        let mut i = 0;
        while i < self.suspended_obs.len() {
            let l = &self.suspended_obs[i];
            let ready = l
                .gen_open
                .is_some_and(|t| l.gen_q.as_ref().is_some_and(|q| q.borrow().vis_pos >= t))
                && l.gen_close
                    .is_none_or(|c| l.gen_q.as_ref().is_some_and(|q| q.borrow().vis_pos < c))
                // Pop mirrors are consumer-side bookkeeping, not real
                // stack levels — during the body's own run they stay
                // parked so a body pop reaches the level it pushed.
                && (self.gen_run_state.is_none() || l.pop_head.is_none());
            if ready {
                let l = self.suspended_obs.remove(i);
                let at = l.suspend_base.min(self.ob_stack.len());
                self.ob_stack.insert(at, l);
            } else {
                i += 1;
            }
        }
    }

    /// The body's own buffers leave the real stack when it suspends
    /// (they were opened inside the eager run but exist consumer-side
    /// only once its yield index passes) — park them keyed by open
    /// tag until `ob_promote` restores them.
    fn ob_suspend(&mut self, fq: &crate::value::FinQueue) {
        let mut moved = Vec::new();
        let mut i = self.ob_stack.len();
        while i > 0 {
            i -= 1;
            let owned = self.ob_stack[i].gen_open.is_some()
                && self.ob_stack[i]
                    .gen_q
                    .as_ref()
                    .is_some_and(|q| std::rc::Rc::ptr_eq(q, fq));
            if owned {
                let mut l = self.ob_stack.remove(i);
                l.suspend_base = i;
                moved.push(l);
            }
        }
        moved.reverse();
        self.suspended_obs.extend(moved);
    }

    /// A gen window mirror that just closed: the consumer writes it
    /// captured splice into the pop value Zend's shared buffer held
    /// — rewrite the gen's deferred journal entries still carrying
    /// the stale value (head+tail) to the corrected
    /// (head+captures+tail).
    fn ob_mirror_close(&mut self, l: &ObLevel) {
        let Some(head) = &l.pop_head else {
            return;
        };
        // Body-pop retarget: the body's pop popped whatever sat on
        // top of Zend's global stack at resume time. A consumer level
        // still on the stack at window close means the pop took IT —
        // its content replaces the journaled pop value and the level
        // is consumed.
        let stolen = self
            .ob_stack
            .iter()
            .rposition(|x| x.gen_q.is_none() && x.pop_head.is_none())
            .map(|i| self.ob_stack.remove(i));
        if let Some(mut st) = stolen {
            let mut old_v = head.clone();
            for (_, s) in &l.pop_segs {
                old_v.extend_from_slice(s);
            }
            let min_tag = l.gen_close.unwrap_or(0).saturating_sub(1);
            if !old_v.is_empty() {
                let new_v = std::mem::take(&mut st.buf);
                st.mem_sync();
                if let Some(gs) = &l.gen_state {
                    if let Some(st2) = gs.upgrade() {
                        let mut st2 = st2.borrow_mut();
                        for (t, b, ..) in &mut st2.pending_out {
                            if *t >= min_tag {
                                Self::bytes_replace(b, &old_v, &new_v);
                            }
                        }
                        Self::gen_patch_values(&mut st2, &old_v, &new_v);
                    }
                }
                if let Some(q) = &l.gen_q {
                    for (t, b, _) in &mut q.borrow_mut().bytes {
                        if *t >= min_tag {
                            Self::bytes_replace(b, &old_v, &new_v);
                        }
                    }
                }
            }
            // The body's pop took the consumer level — the gen's own
            // buffer was never popped: it stays open on the shared
            // stack and keeps capturing (request-end flush).
            if let Some(head) = &l.pop_head {
                let mut buf = head.clone();
                for (_, s) in &l.pop_segs {
                    buf.extend_from_slice(s);
                }
                for (_, c) in &l.caps {
                    buf.extend_from_slice(c);
                }
                let mut level = ObLevel {
                    buf,
                    charged: 0,
                    cap: OB_INIT_CAP,
                    mem_tok: std::rc::Rc::new(()),
                    handler: l.handler.clone(),
                    started: l.started,
                    gen_q: l.gen_q.clone(),
                    gen_open: l.gen_open,
                    gen_close: None,
                    gen_pending: Vec::new(),
                    gen_drained: 0,
                    pop_head: None,
                    pop_segs: Vec::new(),
                    // Keep the journaled segment and capture
                    // positions so later consumer writes still
                    // splice at cursor order.
                    drained_segs: {
                        let mut off = head.len();
                        l.pop_segs
                            .iter()
                            .map(|(t, s)| {
                                let e = (*t, off, s.len());
                                off += s.len();
                                e
                            })
                            .collect()
                    },
                    caps: l.caps.clone(),
                    cap_segs: {
                        let mut off = l.pop_head.as_ref().map(|h| h.len()).unwrap_or(0)
                            + l.pop_segs.iter().map(|(_, s)| s.len()).sum::<usize>();
                        l.caps
                            .iter()
                            .map(|(t, c)| {
                                let e = (*t, off, c.len());
                                off += c.len();
                                e
                            })
                            .collect()
                    },
                    read_vals: l.read_vals.clone(),
                    suspend_base: l.suspend_base,
                    gen_state: l.gen_state.clone(),
                };
                level.mem_sync();
                self.ob_stack.push(level);
                self.ob_meter_sync();
            }
            return;
        }
        if l.caps.is_empty() {
            return;
        }
        let merged = ob_splice(&[], &l.pop_segs, &l.caps);
        let mut old_v = head.clone();
        for (_, s) in &l.pop_segs {
            old_v.extend_from_slice(s);
        }
        // The pop ran inside the resume that produced item close-1 —
        // only deferred entries from that segment on can carry the
        // pop value.
        let min_tag = l.gen_close.unwrap_or(0).saturating_sub(1);
        // Buffer views the body materialized eagerly — each read
        // resolves against the captures that arrived while the
        // cursor sat at tags below its resume.
        if !l.read_vals.is_empty() {
            if let Some(gs) = &l.gen_state {
                if let Some(st) = gs.upgrade() {
                    let mut st = st.borrow_mut();
                    for (k, rhead, rsegs) in &l.read_vals {
                        let caps: Vec<(usize, Vec<u8>)> =
                            l.caps.iter().filter(|(t, _)| *t < *k).cloned().collect();
                        let mut stale = rhead.clone();
                        for (_, s) in rsegs {
                            stale.extend_from_slice(s);
                        }
                        let mut v = rhead.clone();
                        v.extend_from_slice(&ob_splice(&[], rsegs, &caps));
                        Self::gen_patch_values(&mut st, &stale, &v);
                    }
                }
            }
        }
        if old_v.is_empty() {
            // The journaled pop value is the empty string — no anchor
            // to rewrite. Splice into the body's first deferred echo
            // after the pop: that is where the popped value's print
            // lands (typical `echo "...$c\n"` shape).
            let mut done = false;
            if let Some(gs) = &l.gen_state {
                if let Some(st) = gs.upgrade() {
                    for (t, b, ..) in &mut st.borrow_mut().pending_out {
                        if !done && *t >= min_tag {
                            if let Some(p) = b.iter().rposition(|c| *c == b'\n') {
                                b.splice(p..p, merged.iter().copied());
                            } else {
                                b.extend_from_slice(&merged);
                            }
                            done = true;
                        }
                    }
                }
            }
            if !done {
                if let Some(q) = &l.gen_q {
                    for (t, b, _) in &mut q.borrow_mut().bytes {
                        if !done && *t >= min_tag {
                            if let Some(p) = b.iter().rposition(|c| *c == b'\n') {
                                b.splice(p..p, merged.iter().copied());
                            } else {
                                b.extend_from_slice(&merged);
                            }
                            done = true;
                        }
                    }
                }
            }
            return;
        }
        let mut new_v = head.clone();
        new_v.extend_from_slice(&merged);
        if let Some(gs) = &l.gen_state {
            if let Some(st) = gs.upgrade() {
                for (t, b, ..) in &mut st.borrow_mut().pending_out {
                    if *t >= min_tag {
                        Self::bytes_replace(b, &old_v, &new_v);
                    }
                }
            }
        }
        if let Some(q) = &l.gen_q {
            for (t, b, _) in &mut q.borrow_mut().bytes {
                if *t >= min_tag {
                    Self::bytes_replace(b, &old_v, &new_v);
                }
            }
        }
        // Values the body already materialized from the window's
        // stale content (ob_get_contents reads stored into CVs,
        // yielded items, the return value) — Zend ran those reads at
        // resume with the captures inside.
        if let Some(gs) = &l.gen_state {
            if let Some(st) = gs.upgrade() {
                Self::gen_patch_values(&mut st.borrow_mut(), &old_v, &new_v);
            }
        }
    }

    /// Rewrite a gen's stored values that materialized a window's
    /// stale content: string cells equal to `old` become `new`, and
    /// an Int cell equal to `old`'s byte length inside a container
    /// that held such a string becomes `new`'s (ob_get_length reads
    /// pair with ob_get_contents ones).
    fn gen_patch_values(st: &mut crate::value::GenState, old: &[u8], new: &[u8]) {
        if old == new || old.is_empty() {
            return;
        }
        let patch = |v: &mut Value| Self::value_replace(v, old, new);
        patch(&mut st.return_val);
        for (k, c) in &mut st.items {
            patch(k);
            patch(&mut c.borrow_mut());
        }
        let fin = st.fin_q.clone();
        let mut f = fin.borrow_mut();
        for (_, c) in &mut f.suspended {
            patch(&mut c.borrow_mut());
        }
    }

    /// Deep cell rewrite for `gen_patch_values` — returns the number
    /// of string cells replaced so the enclosing array can also patch
    /// sibling length reads.
    fn value_replace(v: &mut Value, old: &[u8], new: &[u8]) -> usize {
        match v {
            Value::Str(s) if s.as_ref() == old => {
                *v = Value::bytes(new.to_vec());
                1
            }
            Value::Array(a) => {
                let cells: Vec<crate::value::Cell> =
                    a.borrow().iter().map(|(_, c)| c.clone()).collect();
                let mut n = 0;
                for c in &cells {
                    n += Self::value_replace(&mut c.borrow_mut(), old, new);
                }
                if n > 0 {
                    for c in &cells {
                        if let Value::Int(i) = &mut *c.borrow_mut() {
                            if *i == old.len() as i64 {
                                *i = new.len() as i64;
                            }
                        }
                    }
                }
                n
            }
            _ => 0,
        }
    }

    /// Replace every occurrence of `old` in `b` with `new`.
    fn bytes_replace(b: &mut Vec<u8>, old: &[u8], new: &[u8]) {
        if old.is_empty() {
            return;
        }
        let mut i = 0;
        while i + old.len() <= b.len() {
            match b[i..].windows(old.len()).position(|w| w == old) {
                Some(p) => {
                    let at = i + p;
                    b.splice(at..at + old.len(), new.iter().copied());
                    i = at + new.len();
                }
                None => break,
            }
        }
    }

    /// Merge a gen-opened buffer's journaled deferred bytes into its
    /// buf: an entry tagged `t` ran inside the resume that produced
    /// item `t`, so it lands in the buffer once the cursor passes
    /// item `t` (`t < pos`) — or all of them for a read inside the
    /// body itself (`all`), or once the body's gen finished (every
    /// tag is stream-past).
    fn ob_drain_level(level: &mut ObLevel, all: bool) {
        if level.gen_pending.is_empty() {
            return;
        }
        let (pos, fin, killed) = level
            .gen_q
            .as_ref()
            .map(|q| {
                let f = q.borrow();
                // The whole journal is confirmed once the body ran
                // to its end AND the consumer's cursor reached it —
                // a mid-consumption gen's post-yield tails still wait
                // on resume confirmation. Delegate snapshots freeze
                // at pos=0 so this stays false for them.
                (f.vis_pos, f.finished && f.vis_pos >= f.total, f.killed)
            })
            .unwrap_or((usize::MAX, true, false));
        // An orphaned journal (owning state displaced/freed) whose
        // stream was never consumed is a kill — the un-run tail can
        // never confirm. A consumed gen ran those writes; Zend
        // emitted them.
        let killed = killed
            || (!fin
                && level
                    .gen_state
                    .as_ref()
                    .is_some_and(|w| w.upgrade().is_none()));
        // A tail entry tagged `t` (written after yield index `t`)
        // ran in Zend's frame only once a resume delivered item
        // `t + 1` — a consumed-then-unclosed gen's whole journal is
        // confirmed; a killed gen's cursor froze at the kill so its
        // un-run tail never materializes.
        let take = level
            .gen_pending
            .iter()
            .take_while(|(t, _)| all || (fin && !killed) || *t < pos)
            .count();

        for (t, b) in level.gen_pending.drain(..take) {
            level.gen_drained += b.len();
            level.drained_segs.push((t, level.buf.len(), b.len()));
            level.buf.extend_from_slice(&b);
        }
        level.mem_sync();
    }

    /// Drain the top buffer's journaled gen captures — `all` when the
    /// read runs inside the owning gen's body (its tags are source-
    /// ordered already); otherwise by the mirrored consumer cursor.
    pub(in crate::interp) fn ob_drain_pending(&mut self) {
        self.ob_promote();
        if let Some(l) = self.ob_stack.last_mut() {
            let all = match (&l.gen_q, &self.gen_run_state) {
                (Some(q), Some(s)) => std::rc::Rc::ptr_eq(q, &s.borrow().fin_q),
                _ => false,
            };
            Self::ob_drain_level(l, all);
        }
        self.ob_meter_sync();
    }

    /// Stack entries the consumer can see through the suspended-gen
    /// windows — used by ob_get_level & friends so a detached gen
    /// buffer still counts like Zend's shared stack.
    pub(in crate::interp) fn ob_suspended_visible(&self) -> usize {
        self.suspended_obs
            .iter()
            .filter(|l| {
                l.gen_open
                    .is_some_and(|t| l.gen_q.as_ref().is_some_and(|q| q.borrow().vis_pos > t))
                    && l.gen_close
                        .is_none_or(|c| l.gen_q.as_ref().is_some_and(|q| q.borrow().vis_pos <= c))
            })
            .count()
    }

    /// Emit generator-deferred output whose suspending yield the
    /// consumer has now advanced past (`pos > tag`). Pass
    /// `usize::MAX` to flush everything (getReturn runs to the end).
    fn gen_flush_out(&mut self, state: &Rc<RefCell<crate::value::GenState>>, pos: usize) {
        // Buffers the body opened past a yield materialize once the
        // cursor passes their open tag.
        self.ob_promote();
        let ready = {
            let mut st = state.borrow_mut();
            let split = st
                .pending_out
                .iter()
                .position(|(t, ..)| *t >= pos)
                .unwrap_or(st.pending_out.len());
            let mut rest = st.pending_out.split_off(split);
            std::mem::swap(&mut st.pending_out, &mut rest);
            rest
        };
        for (t, b, is_err, is_fin) in ready {
            // Inside a `yield from` drain the inner's flushed bytes
            // retag into the OUTER gen's deferred queue — emitting
            // live would echo inner output before the consumer
            // reached it.
            if let (Some(base), Some(run)) = (self.gen_collect_base, &self.gen_run_state) {
                run.borrow_mut()
                    .pending_out
                    .push((base + t, b, is_err, is_fin));
                continue;
            }
            if is_err {
                self.diag_stderr(&String::from_utf8_lossy(&b));
            } else {
                self.emit_replay(&b, t);
            }
        }
        // Entries just shown are no longer part of the suspended
        // region's death-time finally output.
        state
            .borrow()
            .fin_q
            .borrow_mut()
            .bytes
            .retain(|(t, ..)| *t >= pos);
    }

    /// Buffer output inside a running gen body — the deferred stream
    /// (pending_out) plus, for finally-region bytes, the destruction
    /// journal (fin_q). Shared by emit_bytes and diag_stderr.
    fn gen_buf_out(&mut self, tag: usize, b: &[u8], is_err: bool, is_fin: bool) {
        if let Some(run) = &self.gen_run_state {
            run.borrow_mut()
                .pending_out
                .push((tag, b.to_vec(), is_err, is_fin));
        }
        if is_fin {
            if let Some(q) = &self.gen_fin_q {
                q.borrow_mut().bytes.push((tag, b.to_vec(), is_err));
            }
        }
    }

    /// Whether a re-run's prefix suppression covers the current emit:
    /// only the re-running gen's own frames replay the consumer-seen
    /// prefix — a nested gen's body (created inside the re-run, run
    /// by its own gen_start) emits and journals normally.
    pub(in crate::interp) fn gen_horizon_suppresses(&self, done: usize) -> bool {
        let r = match &self.gen_replay_horizon {
            Some((k, tgt)) => {
                done <= *k
                    && self
                        .gen_run_state
                        .as_ref()
                        .is_some_and(|s| Rc::ptr_eq(s, tgt))
            }
            None => false,
        };

        r
    }

    /// Rebuild a level's real buffer content in write order: the
    /// direct-write head pieces stay inline, journaled segs emit
    /// once the consumer cursor confirms them, and consumer captures
    /// splice ahead of the first seg whose tag is >= their arrival
    /// cursor — the shared stack's real ordering. A killed gen's
    /// tail entries (`t + 1 >= pos`) never ran in Zend's frame and
    /// drop out.
    fn ob_level_content(l: &ObLevel, pos: usize, killed: bool) -> Vec<u8> {
        if let Some(head) = &l.pop_head {
            // Pop mirror — the body's pop ran only in the eager
            // re-run; at teardown the real buffer's content is
            // its pre-window head plus whatever the consumer
            // captured while suspended.
            let mut v = head.clone();
            for (_, c) in &l.caps {
                v.extend_from_slice(c);
            }
            return v;
        }
        // Walk the recorded seg/cap ranges in buf order: head slices
        // copy through, cap bytes hold for their tag position, and
        // each confirmed seg first emits every capture that arrived
        // before its resume.
        let mut events: Vec<(usize, usize, usize, bool)> = Vec::new();
        for &(t, s, n) in &l.drained_segs {
            events.push((s, t, n, false));
        }
        for &(t, s, n) in &l.cap_segs {
            events.push((s, t, n, true));
        }
        events.sort_by_key(|(s, _, _, c)| (*s, *c));
        let mut v = Vec::new();
        let mut off = 0usize;
        let mut ci = 0usize;
        for (s, t, n, is_cap) in events {
            let s = s.min(l.buf.len());
            let e = (s + n).min(l.buf.len());
            if s < off {
                continue;
            }
            v.extend_from_slice(&l.buf[off..s]);
            if !is_cap && t + usize::from(killed) < pos {
                while ci < l.caps.len() && l.caps[ci].0 <= t {
                    v.extend_from_slice(&l.caps[ci].1);
                    ci += 1;
                }
                v.extend_from_slice(&l.buf[s..e]);
            }
            off = e;
        }
        v.extend_from_slice(&l.buf[off..]);
        while ci < l.caps.len() {
            v.extend_from_slice(&l.caps[ci].1);
            ci += 1;
        }
        v
    }

    /// A dead gen's open output buffers tear down like Zend closing
    /// the frame's levels: their real contents flush to the parent
    /// level (or stdout), while journaled-but-unflushed captures
    /// — the suspended body's writes the frame never ran — drop
    /// with the window. Pop mirrors die silently: their captures
    /// belonged to a window the body already consumed.
    fn ob_dead_gen(&mut self, fq: &crate::value::FinQueue) {
        let owned = |l: &ObLevel| l.gen_q.as_ref().is_some_and(|q| std::rc::Rc::ptr_eq(q, fq));
        let (pos, total) = {
            let q = fq.borrow();
            (q.pos, q.total)
        };
        if pos >= total {
            // The gen ran to its end — Zend leaves its output buffers
            // on the global stack; they flush at request end like any
            // orphaned level. Only a gen destroyed mid-flight has its
            // buffers force-flushed at teardown.
            return;
        }
        // Mid-flight teardown — the eager tail's un-confirmed
        // journaled captures never ran in Zend's frame.
        fq.borrow_mut().kill_tree();
        let killed = true;
        // Zend leaves the dead gen's output buffers ON the global
        // stack: a suspended window's content materializes at the
        // confirmed cursor, consumer writes keep capturing into it,
        // and its handler fires at the final flush like any orphaned
        // level. Promote each owned level to a real consumer level at
        // the slot it occupied (suspend_base for parked windows).
        // An owned pop mirror is a window the body's pop consumed —
        // consumer-side that pop hasn't run, so its real content
        // (pre-pop head + consumer captures) materializes into an
        // ordinary poppable level; a mirror whose window never
        // opened consumer-side dies silently.
        let mut promote: Vec<(usize, ObLevel)> = Vec::new();
        let mut i = self.suspended_obs.len();
        while i > 0 {
            i -= 1;
            if !owned(&self.suspended_obs[i]) {
                continue;
            }
            let mut l = self.suspended_obs.remove(i);
            let never_opened = l.pop_head.is_some()
                && l.gen_open
                    .is_some_and(|o| l.gen_q.as_ref().is_some_and(|q| q.borrow().vis_pos < o));
            if never_opened {
                continue;
            }
            Self::ob_dead_level(&mut l, pos, killed);
            promote.push((l.suspend_base, l));
        }
        for l in self.ob_stack.iter_mut().filter(|l| owned(l)) {
            Self::ob_dead_level(l, pos, killed);
        }
        promote.sort_by_key(|(b, _)| *b);
        for (base, l) in promote {
            self.ob_stack.insert(base.min(self.ob_stack.len()), l);
        }
    }

    /// Materialize a dead gen's level into an ordinary consumer
    /// level: real content at the confirmed cursor, all gen/mirror
    /// bookkeeping cleared — a pop mirror becomes a normal poppable
    /// level instead of a phantom that flushes raw and can't be
    /// popped.
    fn ob_dead_level(l: &mut ObLevel, pos: usize, killed: bool) {
        Self::ob_drain_level(l, false);
        l.buf = Self::ob_level_content(l, pos, killed);
        l.mem_sync();
        l.drained_segs.clear();
        l.cap_segs.clear();
        l.caps.clear();
        l.gen_pending.clear();
        l.read_vals.clear();
        l.gen_drained = 0;
        l.gen_q = None;
        l.gen_open = None;
        l.gen_close = None;
        l.pop_head = None;
        l.pop_segs.clear();
    }

    /// Zend destroys a suspended generator by running the finally
    /// chains of the try-regions enclosing its suspension point. The
    /// eager body already buffered those bytes plus the markers a
    /// force-close raises after them (yield-inside-finally fatal, a
    /// parked `throw()` throwable, the body's own finally-region
    /// death) — replay when the gen dies (dead weak ref) and once at
    /// unit end for gens still suspended (request shutdown).
    pub(in crate::interp) fn gen_gc_sweep(&mut self, at_unit_end: bool) -> Result<(), PhpError> {
        if self.live_gens.is_empty() {
            return Ok(());
        }
        let mut entries = std::mem::take(&mut self.live_gens);
        // Request teardown destroys newest handles first.
        if at_unit_end {
            entries.reverse();
        }
        let mut terminal = None;
        for (weak, q) in entries {
            let dead = weak.upgrade().is_none();
            if !dead && !at_unit_end {
                // Still suspended — keep watching it.
                self.live_gens.push((weak, q));
                continue;
            }
            if terminal.is_none() {
                terminal = self.gen_fin_replay(&q, at_unit_end);
            } else if !dead {
                // Teardown stopped at the first raise — keep the
                // rest watched for a later destruction point.
                self.live_gens.push((weak, q));
            }
        }
        match terminal {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// An object freed at this teardown slot drops its prop cells
    /// with it — a suspended generator whose only owners are those
    /// cells force-closes right here (Zend frees the property table
    /// alongside the object, interleaved with the destruct pass,
    /// not at the unit-end gen sweep). Returns the first terminal
    /// raise, like `gen_fin_replay` callers expect.
    fn gen_prop_sweep(&mut self, o: &Rc<RefCell<PhpObject>>) -> Option<PhpError> {
        // Tally the prop cells referencing each generator object —
        // the gen dies with this object iff every strong ref lives
        // in its prop table (plus the clone we hold for the check).
        let mut gens: Vec<(Rc<RefCell<PhpObject>>, usize)> = Vec::new();
        {
            let b = o.borrow();
            for name in &b.prop_order {
                let Some(c) = b.props.get(name) else {
                    continue;
                };
                if let Value::Object(go) = &*c.borrow() {
                    if !matches!(
                        &go.borrow().internal,
                        Some(crate::value::ObjectInternal::Generator(_))
                    ) {
                        continue;
                    }
                    if let Some((_, n)) = gens.iter_mut().find(|(g, _)| Rc::ptr_eq(g, go)) {
                        *n += 1;
                    } else {
                        gens.push((go.clone(), 1));
                    }
                }
            }
        }
        let mut terminal = None;
        for (go, n) in gens {
            if Rc::strong_count(&go) != n + 1 {
                continue;
            }
            let q = match &go.borrow().internal {
                Some(crate::value::ObjectInternal::Generator(st)) => st.borrow().fin_q.clone(),
                _ => continue,
            };
            if terminal.is_none() {
                terminal = self.gen_fin_replay(&q, true);
            }
        }
        terminal
    }

    /// Zend's cycle collector, driven by `gc_collect_cycles()`: an
    /// object reachable only through other objects — the classic
    /// object-prop ↔ suspended-gen-frame cycle (`$a->g = $a->g()`
    /// where the gen's `$this` is `$a`) — is freed at the call, so a
    /// dead cycle's generator force-closes and replays its finally
    /// journal mid-script rather than at shutdown.
    ///
    /// The pass marks every live object, seeds roots as objects with
    /// more strong refs than the refs found inside the candidate
    /// universe (props / gen frames' suspended cells, yielded items,
    /// pending sends and throws, setup args/captures/$this), and
    /// flood-fills through internal edges; what's left is a dead
    /// cycle. Dead gens replay their journal, dead objects run
    /// `__destruct`, then the dead set's held cells release so the
    /// weak handles in `live_gens`/`obj_handles` go stale like Zend
    /// freeing the zvals.
    pub fn gc_cycle_collect(&mut self) -> Result<usize, PhpError> {
        self.gc_cycle_collect_impl(false)
    }

    /// `gc_collect_cycles()` runs the full buffer; the auto-collect
    /// fired by `gc_maybe_collect` (buffer overflow) frees only dead
    /// components holding a buffered root — members still streaming
    /// into the buffer mid-decref wait for the next pass (gc_023's
    /// trailing count lands on the manual call).
    fn gc_cycle_collect_impl(&mut self, buffer_only: bool) -> Result<usize, PhpError> {
        // Zend's collect keeps re-rooting values that dtors unroot
        // mid-pass (gc/bug70805: C's dtor unsets $a, whose cycle then
        // dies inside the same collect). Re-run the mark until a pass
        // finds nothing, accumulating the root count.
        let t = std::time::Instant::now();
        let mut total = 0;
        // Zend counts a collector run only when the root buffer wasn't
        // empty or the pass actually freed something — an idle
        // `gc_collect_cycles()` on a drained buffer doesn't bump
        // `gc_status().runs` (gc_037).
        let buffered = !self.gc_purpled.is_empty();
        for _ in 0..8 {
            let n = self.gc_cycle_collect_pass(buffer_only)?;
            if n == 0 {
                break;
            }
            total += n;
        }
        if buffered || total > 0 {
            self.gc_runs += 1;
            self.gc_collected += total as u64;
            self.gc_collector_time += t.elapsed().as_secs_f64();
        }
        self.gc_purpled.clear();
        // Any collect drains the candidate buffer — the hysteresis
        // counter restarts from zero for the next overflow (F4).
        self.gc_pending = 0;
        Ok(total)
    }

    fn gc_cycle_collect_pass(&mut self, buffer_only: bool) -> Result<usize, PhpError> {
        let mut objs: Vec<Rc<RefCell<PhpObject>>> = Vec::new();
        let mut arrs: Vec<Rc<RefCell<PhpArray>>> = Vec::new();
        // Dedup: a node registered more than once (`reg_arr_ref` fires
        // per `=&` bind) must hold exactly one scan clone — the root
        // test's bookkeeping assumes +1 per node.
        let mut seen: HashSet<usize> = HashSet::new();
        for h in &self.obj_handles {
            if let ObjHandle::Obj(w) = h {
                if let Some(o) = w.upgrade() {
                    if seen.insert(Rc::as_ptr(&o) as usize) {
                        objs.push(o);
                    }
                }
            }
        }
        // Registered arrays are graph nodes too — only arrays that
        // took a `=&` element can join a cycle (`$a[] =& $a`).
        for w in &self.arr_handles {
            if let Some(a) = w.upgrade() {
                if seen.insert(Rc::as_ptr(&a) as usize) {
                    arrs.push(a);
                }
            }
        }
        let mut pins: HashMap<usize, usize> = HashMap::new();
        for (c, ..) in self.typed_slots.values() {
            *pins.entry(Rc::as_ptr(c) as usize).or_insert(0) += 1;
        }
        let mut scan = GcScan {
            universe: seen,
            dying: self.gc_dying.clone(),
            pins,
            ..GcScan::default()
        };
        // Scan the universe once. `cell_edges[t]` is the set of cells
        // directly holding a clone of `t` — one cell contributes one
        // ref no matter how many slots scan it; `raw_edges` counts
        // bare clones in non-cell positions; `cell_slots` counts every
        // slot a cell occupies inside scanned containers, so a cell
        // shared with an unscanned holder (a plain array's slot, a
        // var, a capture) shows an extra strong ref — an external
        // holder that roots the clone's target. Unregistered
        // containers get their own edge lists so reachability
        // traverses them (a registered array inside a plain array
        // inside a live object is reachable).
        let mut visited: HashSet<usize> = HashSet::new();
        for o in &objs {
            let t = Rc::as_ptr(o) as usize;
            if visited.insert(t) {
                let mut out = Vec::new();
                Self::gc_scan_obj_fields(o, &mut scan, &mut out, 16, &mut visited);
                scan.edges.insert(t, out);
            }
        }
        for a in &arrs {
            let t = Rc::as_ptr(a) as usize;
            if visited.insert(t) {
                let mut out = Vec::new();
                for (_, c) in &a.borrow().entries {
                    Self::gc_scan_cell(c, &mut scan, &mut out, 16, &mut visited, false);
                }
                scan.edges.insert(t, out);
            }
        }
        // Roots: a strong count above the refs the scan accounts for
        // — cells held only inside scanned slots + bare clones —
        // means an external holder keeps the node alive. Non-universe
        // containers discovered mid-scan root their targets too (a
        // plain array held by a var is alive).
        let mut reach: HashSet<usize> = HashSet::new();
        for o in &objs {
            let t = Rc::as_ptr(o) as usize;
            if Self::gc_is_root(&scan, t, Rc::strong_count(o)) {
                reach.insert(t);
            }
        }
        for a in &arrs {
            let t = Rc::as_ptr(a) as usize;
            if Self::gc_is_root(&scan, t, Rc::strong_count(a)) {
                reach.insert(t);
            }
        }
        for (t, v) in &scan.nodes {
            let strong = match v {
                Value::Array(a) => Rc::strong_count(a),
                Value::Callable(c) => Rc::strong_count(c),
                Value::Object(o) => Rc::strong_count(o),
                _ => continue,
            };
            if Self::gc_is_root(&scan, *t, strong) {
                reach.insert(*t);
            }
        }
        let mut stack: Vec<usize> = reach.iter().copied().collect();
        while let Some(p) = stack.pop() {
            if let Some(out) = scan.edges.get(&p) {
                for t in out {
                    if reach.insert(*t) {
                        stack.push(*t);
                    }
                }
            }
        }
        let mut dead_arrs: Vec<Rc<RefCell<PhpArray>>> = arrs
            .into_iter()
            .filter(|a| !reach.contains(&(Rc::as_ptr(a) as usize)))
            .collect();
        let mut dead: Vec<Rc<RefCell<PhpObject>>> = objs
            .into_iter()
            .filter(|o| !reach.contains(&(Rc::as_ptr(o) as usize)))
            .collect();
        // Dead non-node containers (a dead gen's captures, an object's
        // nested plain arrays, a self-capturing closure): unreachable
        // and not externally held, they die with the set by refcount
        // drop and count toward the collect total like Zend zvals.
        let mut dead_nodes: HashSet<usize> = HashSet::new();
        for (t, v) in &scan.nodes {
            if reach.contains(t) {
                continue;
            }
            let strong = match v {
                Value::Array(a) => Rc::strong_count(a),
                Value::Callable(c) => Rc::strong_count(c),
                Value::Object(o) => Rc::strong_count(o),
                _ => continue,
            };
            if !Self::gc_is_root(&scan, *t, strong) {
                dead_nodes.insert(*t);
            }
        }
        if buffer_only {
            // Overflow pass: free only dead components a buffered
            // (purpled) root belongs to — Zend's buffer is the
            // candidate set. Components without a buffered member
            // keep waiting for their own decrefs to note them.
            let mut dead_all: HashSet<usize> = dead_nodes.clone();
            for o in &dead {
                dead_all.insert(Rc::as_ptr(o) as usize);
            }
            for a in &dead_arrs {
                dead_all.insert(Rc::as_ptr(a) as usize);
            }
            let mut freed: HashSet<usize> = HashSet::new();
            let mut stack: Vec<usize> = dead_all
                .iter()
                .copied()
                .filter(|t| self.gc_purpled.contains(t))
                .collect();
            for t in &stack {
                freed.insert(*t);
            }
            while let Some(p) = stack.pop() {
                if let Some(out) = scan.edges.get(&p) {
                    for t in out {
                        if dead_all.contains(t) && freed.insert(*t) {
                            stack.push(*t);
                        }
                    }
                }
            }
            dead.retain(|o| freed.contains(&(Rc::as_ptr(o) as usize)));
            dead_arrs.retain(|a| freed.contains(&(Rc::as_ptr(a) as usize)));
            dead_nodes.retain(|t| freed.contains(t));
        }
        // Zend counts every dead root — dead universe nodes plus the
        // dead non-node containers that die with them. Containers reached
        // only through internal-machinery props (SplObjectStorage's
        // `$objs`/`$data`, SplFixedArray's `$data`) aren't zvals in Zend —
        // its storage is C-level — so they don't count (bug69534); their
        // slot contents are still scanned and do.
        let counted = |t: &usize| !scan.internal.contains(t) || scan.plain.contains(t);
        let dead_roots = dead
            .iter()
            .filter(|o| counted(&(Rc::as_ptr(o) as usize)))
            .count()
            + dead_arrs
                .iter()
                .filter(|a| counted(&(Rc::as_ptr(a) as usize)))
                .count()
            + dead_nodes.iter().filter(|t| counted(t)).count();
        drop(scan);
        let mut first_err = None;
        for o in &dead {
            let gen_q = match &o.borrow().internal {
                Some(crate::value::ObjectInternal::Generator(st)) => {
                    Some(st.borrow().fin_q.clone())
                }
                _ => None,
            };
            if let Some(q) = gen_q {
                if let Some(e) = self.gen_fin_replay(&q, false) {
                    first_err = Some(e);
                }
            } else if self
                .find_method_in(&o.borrow().class, "__destruct")
                .is_some()
                && self.mark_destructed(o)
            {
                let dt = std::time::Instant::now();
                if let Err(e) = self.method_invoke(o.clone(), "__destruct", CallArgs::empty()) {
                    first_err = Some(e);
                }
                self.gc_destructor_time += dt.elapsed().as_secs_f64();
            }
        }
        // Release the dead set's cells last — dropping them earlier
        // would let a freed prop's value (a gen handle) run its own
        // close ahead of the journal replay above.
        for a in &dead_arrs {
            a.borrow_mut().entries.clear();
        }
        for o in &dead {
            o.borrow_mut().props.clear();
            if let Some(crate::value::ObjectInternal::Generator(st)) = &o.borrow().internal {
                let mut st = st.borrow_mut();
                st.items.clear();
                st.sends.clear();
                st.throws.clear();
                st.injected_throwable = None;
                let GenSetup::Invoke {
                    args,
                    this_obj,
                    captures,
                    ..
                } = &mut st.setup;
                args.cells.clear();
                *this_obj = None;
                captures.clear();
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(dead_roots),
        }
    }

    /// Graph-node pointer for a value (0 = not a refcounted node).
    fn gc_val_ptr(v: &Value) -> usize {
        match v {
            Value::Array(a) => Rc::as_ptr(a) as usize,
            Value::Callable(c) => Rc::as_ptr(c) as usize,
            Value::Object(o) => Rc::as_ptr(o) as usize,
            _ => 0,
        }
    }

    /// Root test for one scanned node: its real strong count vs the
    /// refs the scan accounts for. A contributing cell is internal
    /// only while the cell itself sits in scanned slots alone (`+1`
    /// is `GcScan::cells`' own bookkeeping clone); a cell shared with
    /// an unscanned holder — a var frame, a plain array's slot, a
    /// closure capture — is an external root hold of its target.
    fn gc_is_root(scan: &GcScan, t: usize, strong: usize) -> bool {
        let (cells, external) = scan
            .cell_edges
            .get(&t)
            .map(|set| {
                let ext = set
                    .iter()
                    .filter(|cp| {
                        // The dying walk's in-flight decrefs don't
                        // count as holders — Zend already subtracted
                        // the dying container's member refcounts
                        // before the collector ran (gc_023).
                        let dying = scan.dying.get(*cp).copied().unwrap_or(0);
                        let pins = scan.pins.get(*cp).copied().unwrap_or(0);
                        Rc::strong_count(&scan.cells[*cp]) > scan.cell_slots[cp] + 1 + dying + pins
                    })
                    .count();
                (set.len(), ext)
            })
            .unwrap_or((0, 0));
        let internal = cells - external + scan.raw_edges.get(&t).copied().unwrap_or(0);
        strong > internal + 1
    }

    /// One universe slot holding cell `c`: tally the slot, then
    /// record the cell value's direct refcounted target as this
    /// cell's single contribution to that node's internal refs — and
    /// recurse into the target's own slots on first visit.
    fn gc_scan_cell(
        c: &Cell,
        scan: &mut GcScan,
        out: &mut Vec<usize>,
        depth: u8,
        visited: &mut HashSet<usize>,
        internal: bool,
    ) {
        if depth == 0 {
            return;
        }
        let cp = Rc::as_ptr(c) as usize;
        *scan.cell_slots.entry(cp).or_insert(0) += 1;
        scan.cells.entry(cp).or_insert_with(|| c.clone());
        Self::gc_scan_held(&c.borrow(), Some(cp), scan, out, depth, visited, internal);
    }

    /// The refcounted target a held `v` points at. A cell-held clone
    /// counts once per holding cell (`holder = Some(cell ptr)`); a
    /// bare clone in a non-cell position counts itself. First visit
    /// scans the container's own slots into its edge list.
    fn gc_scan_held(
        v: &Value,
        holder: Option<usize>,
        scan: &mut GcScan,
        out: &mut Vec<usize>,
        depth: u8,
        visited: &mut HashSet<usize>,
        internal: bool,
    ) {
        if depth == 0 {
            return;
        }
        let t = Self::gc_val_ptr(v);
        if t == 0 {
            return;
        }
        out.push(t);
        if internal {
            scan.internal.insert(t);
        } else {
            scan.plain.insert(t);
        }
        match holder {
            Some(cp) => {
                scan.cell_edges.entry(t).or_default().insert(cp);
            }
            None => {
                *scan.raw_edges.entry(t).or_insert(0) += 1;
            }
        }
        // Non-universe containers need their own bookkeeping clone so
        // their strong counts measure against the scan's +1 too.
        if !scan.universe.contains(&t) {
            scan.nodes.entry(t).or_insert_with(|| v.clone());
        }
        if !visited.insert(t) {
            return;
        }
        let mut inner = Vec::new();
        match v {
            Value::Array(a) => {
                for (_, c) in &a.borrow().entries {
                    Self::gc_scan_cell(c, scan, &mut inner, depth - 1, visited, false);
                }
            }
            Value::Callable(c) => {
                for (_, cap, _) in &c.captures {
                    Self::gc_scan_cell(cap, scan, &mut inner, depth - 1, visited, false);
                }
                // The bound `$this` / method-callable target object is a
                // bare clone — count it as a raw edge so the cycle
                // object ↔ closure closes correctly.
                if let Some(o) = &c.this_obj {
                    let v = Value::Object(o.clone());
                    Self::gc_scan_held(&v, None, scan, &mut inner, depth - 1, visited, false);
                }
                if let crate::value::CallableKind::Method { obj: Some(o), .. } = &c.kind {
                    let v = Value::Object(o.clone());
                    Self::gc_scan_held(&v, None, scan, &mut inner, depth - 1, visited, false);
                }
            }
            Value::Object(o) => {
                Self::gc_scan_obj_fields(o, scan, &mut inner, depth - 1, visited);
            }
            _ => {}
        }
        scan.edges.insert(t, inner);
    }

    /// `\0Cls\0prop` private-prop keys whose owning class keeps storage
    /// in pure-C structures rather than zval slots in Zend — phpun models
    /// them as ordinary prop cells, so containers sitting inside count as
    /// dead member containers while Zend counts none (bug69534 expects
    /// int(2): SplObjectStorage's `$objs`/`$data` arrays aren't real).
    /// Element contents still scan: attached objects are zvals in Zend
    /// too. Only classes whose storage is NOT a zend HashTable/array
    /// qualify — IteratorIterator's `$inner`, RII's `$stack`, CBFI's
    /// `$callback` etc. are real zvals and stay counted.
    const GC_PROPLESS_CLASSES: &'static [&'static str] = &["splobjectstorage", "splfixedarray"];

    /// Whether `name` is a `\0Cls\0prop` private-prop key on a
    /// GC_PROPLESS_CLASSES member (engine-modeled storage, not a zval).
    fn gc_internal_prop(name: &str) -> bool {
        if !name.starts_with('\0') {
            return false;
        }
        let rest = &name[1..];
        let Some(owner) = rest.split('\0').next() else {
            return false;
        };
        Self::GC_PROPLESS_CLASSES
            .iter()
            .any(|c| owner.eq_ignore_ascii_case(c))
    }

    /// An object's in-graph edges: prop cells, and for a generator
    /// the suspended frame's stashed CVs, buffered items, queued
    /// sends/throws, saved call setup, and the destruction journal's
    /// suspended cells. Cells count per cell; bare fields (`sends`,
    /// `throws`, `injected_throwable`, item keys, `this_obj`,
    /// `closure_rc`) count one raw clone each.
    fn gc_scan_obj_fields(
        o: &Rc<RefCell<PhpObject>>,
        scan: &mut GcScan,
        out: &mut Vec<usize>,
        depth: u8,
        visited: &mut HashSet<usize>,
    ) {
        if depth == 0 {
            return;
        }
        let ob = o.borrow();
        for (pname, c) in ob.props.iter() {
            // Private props on classes whose Zend counterpart keeps its
            // storage in pure-C structures (no zval slots): the container
            // in that cell is engine modeling, counted out of the dead
            // set — its contents still scan as real member zvals.
            Self::gc_scan_cell(
                c,
                scan,
                out,
                depth - 1,
                visited,
                Self::gc_internal_prop(pname),
            );
        }
        // ArrayIter's AoStore is zend's `intern->array` slot — a real
        // counted hold of the object (raw edge), not a member zval.
        // Without it a `=&`-bound store self-roots on its unaccounted
        // clone and `$ao[0] =& $ao` never collects. `src` (the object an
        // ArrayObject wraps) is the same kind of bare hold. Shared
        // stores (getIterator siblings) contribute one edge each —
        // reachability then keeps the array alive while any sharer is.
        if let Some(crate::value::ObjectInternal::ArrayIter { store, .. }) = &ob.internal {
            let st = store.borrow();
            let v = Value::Array(st.arr.clone());
            Self::gc_scan_held(&v, None, scan, out, depth - 1, visited, false);
            if let Some(src) = &st.src {
                let v = Value::Object(src.clone());
                Self::gc_scan_held(&v, None, scan, out, depth - 1, visited, false);
            }
            return;
        }
        // A throwable's `previous` chain is a real member-zval hold —
        // engine-chained Errors write the C-field without a prop
        // mirror, so props alone don't cover the edge.
        if let Some(crate::value::ObjectInternal::Exception { previous, .. }) = &ob.internal {
            if let Some(v) = previous {
                Self::gc_scan_held(v, None, scan, out, depth - 1, visited, false);
            }
            return;
        }
        let Some(crate::value::ObjectInternal::Generator(st)) = &ob.internal else {
            return;
        };
        let st = st.borrow();
        for (k, c) in &st.items {
            Self::gc_scan_held(k, None, scan, out, depth - 1, visited, false);
            Self::gc_scan_cell(c, scan, out, depth - 1, visited, false);
        }
        for (_, v) in &st.sends {
            Self::gc_scan_held(v, None, scan, out, depth - 1, visited, false);
        }
        for (_, v) in &st.throws {
            Self::gc_scan_held(v, None, scan, out, depth - 1, visited, false);
        }
        if let Some(v) = &st.injected_throwable {
            Self::gc_scan_held(v, None, scan, out, depth - 1, visited, false);
        }
        let GenSetup::Invoke {
            args,
            this_obj,
            captures,
            closure_rc,
            ..
        } = &st.setup;
        for c in &args.cells {
            Self::gc_scan_cell(c, scan, out, depth - 1, visited, false);
        }
        for (_, c, _) in captures {
            Self::gc_scan_cell(c, scan, out, depth - 1, visited, false);
        }
        if let Some(t) = this_obj {
            let v = Value::Object(t.clone());
            Self::gc_scan_held(&v, None, scan, out, depth - 1, visited, false);
        }
        if let Some(rc) = closure_rc {
            let v = Value::Callable(rc.clone());
            Self::gc_scan_held(&v, None, scan, out, depth - 1, visited, false);
        }
        // Suspended frame CVs — the journal outlives the state, and
        // delegation snapshots carry their own inner frames' cells.
        // A `yield from` chain nests FinDelegate.fin recursively, so walk
        // the delegate tree, not just the first level (gc_with_yield_from:
        // a ≥3-deep chain left the innermost global-holding snapshot
        // unscanned → phantom root kept the whole cycle alive). The
        // Rc-shared `delegate_fins` live journals are skipped — they're
        // owned by their own gens' states and get scanned there.
        {
            let f = st.fin_q.borrow();
            let mut fins: Vec<&crate::value::GenFinData> = vec![&*f];
            while let Some(g) = fins.pop() {
                for (_, c) in &g.suspended {
                    Self::gc_scan_cell(c, scan, out, depth - 1, visited, false);
                }
                for d in &g.delegates {
                    fins.push(&d.fin);
                }
            }
        }
    }

    /// Register an array-valued cell in the GC universe: a cell
    /// holding an array that just got reference-bound into another
    /// container can form a pure-array cycle (`$a[] =& $a`).
    pub(crate) fn reg_arr_ref(&mut self, c: &Cell) {
        if let Value::Array(a) = &*c.borrow() {
            self.arr_handles.push(std::rc::Rc::downgrade(a));
        }
    }

    /// Zend's container destructor decrefs every held zval — each
    /// decref'd payload that survives becomes a root candidate. Rust's
    /// Drop releases them silently, so a container taking its last ref
    /// here walks its slots by hand (gc_023's `unset($a)` of a
    /// 10k-element array buffers every element). `v` arrives as an
    /// owned clone: the strong-count tests subtract that one hold.
    /// No clone may outlive a slot visit — a held clone (cell or
    /// payload) reads as an external root hold to a collect fired
    /// mid-walk, wrongly rooting its own members.
    fn gc_note_dying(&mut self, v: &Value, depth: u8) {
        if depth == 0 {
            return;
        }
        match v {
            Value::Array(a) => {
                if Rc::strong_count(a) - 1 > 1 {
                    self.gc_note_purple(Rc::as_ptr(a) as usize);
                } else {
                    let dying = {
                        let b = a.borrow();
                        self.gc_dying_acquire(b.entries.iter().map(|(_, c)| Rc::as_ptr(c)))
                    };
                    for i in 0.. {
                        let slot = {
                            let b = a.borrow();
                            b.entries
                                .get(i)
                                .map(|(_, c)| (Rc::strong_count(c), c.borrow().clone()))
                        };
                        let Some((cstrong, pv)) = slot else { break };
                        self.gc_note_dying_slot(cstrong, &pv, depth - 1);
                    }
                    self.gc_dying_release(dying);
                }
            }
            Value::Object(o) => {
                if Rc::strong_count(o) - 1 > 1 {
                    self.gc_note_purple(Rc::as_ptr(o) as usize);
                } else {
                    let dying = {
                        let b = o.borrow();
                        self.gc_dying_acquire(b.props.values().map(Rc::as_ptr))
                    };
                    for i in 0.. {
                        let slot = {
                            let b = o.borrow();
                            b.props
                                .values()
                                .nth(i)
                                .map(|c| (Rc::strong_count(c), c.borrow().clone()))
                        };
                        let Some((cstrong, pv)) = slot else { break };
                        self.gc_note_dying_slot(cstrong, &pv, depth - 1);
                    }
                    self.gc_dying_release(dying);
                }
            }
            Value::Callable(c) => {
                if Rc::strong_count(c) - 1 > 1 {
                    self.gc_note_purple(Rc::as_ptr(c) as usize);
                } else {
                    let dying =
                        self.gc_dying_acquire(c.captures.iter().map(|(_, cap, _)| Rc::as_ptr(cap)));
                    for i in 0.. {
                        let slot = c
                            .captures
                            .get(i)
                            .map(|(_, cap, _)| (Rc::strong_count(cap), cap.borrow().clone()));
                        let Some((cstrong, pv)) = slot else { break };
                        self.gc_note_dying_slot(cstrong, &pv, depth - 1);
                    }
                    if let Some(o) = &c.this_obj {
                        self.gc_note_dying(&Value::Object(o.clone()), depth - 1);
                    }
                    self.gc_dying_release(dying);
                }
            }
            _ => {}
        }
    }

    /// A dying container still holds its slot cells while the walk
    /// runs; a collect fired mid-walk would count those holds as
    /// external roots (gc_023's inner arrays stay live through `$a`'s
    /// slots). Zend's destructor has already decremented each member's
    /// refcount before the collector runs — register the in-flight
    /// decrefs so `gc_is_root` can subtract them, then release.
    fn gc_dying_acquire(
        &mut self,
        ptrs: impl Iterator<Item = *const RefCell<Value>>,
    ) -> Vec<usize> {
        let mut held = Vec::new();
        for c in ptrs {
            let p = c as usize;
            *self.gc_dying.entry(p).or_insert(0) += 1;
            held.push(p);
        }
        held
    }

    fn gc_dying_release(&mut self, held: Vec<usize>) {
        for p in held {
            if let Some(n) = self.gc_dying.get_mut(&p) {
                *n -= 1;
                if *n == 0 {
                    self.gc_dying.remove(&p);
                }
            }
        }
    }

    /// One slot of a dying container decref'd: `cstrong` is the cell's
    /// real strong count before the parent's drop and `pv` a clone of
    /// its payload (its +1 is subtracted inside `gc_note_dying`). A
    /// shared cell (`=&` alias) survives the drop — the payload was
    /// decref'd and stays alive, so its node enters the buffer the
    /// way Zend buffers the ref zval (gc_023's self-referencing
    /// elements share cells between `$a` and their own entries). A
    /// dying cell decrefs its payload once more — the same test
    /// applied a level down.
    fn gc_note_dying_slot(&mut self, cstrong: usize, pv: &Value, depth: u8) {
        if depth == 0 {
            return;
        }
        if cstrong > 1 {
            match pv {
                Value::Array(a) => self.gc_note_purple(Rc::as_ptr(a) as usize),
                Value::Object(o) => self.gc_note_purple(Rc::as_ptr(o) as usize),
                Value::Callable(k) => self.gc_note_purple(Rc::as_ptr(k) as usize),
                _ => {}
            }
            return;
        }
        self.gc_note_dying(&pv.clone(), depth - 1);
    }

    /// A zval became a potential cycle root (Zend's purple-add): count
    /// it once per buffer epoch. Zend's root buffer overflows at 10k
    /// entries — the collector runs when the buffer is full BEFORE the
    /// new root lands, so the triggering root isn't part of the pass
    /// it fired (gc_023's trailing int(1)).
    pub(in crate::interp) fn gc_note_purple(&mut self, key: usize) {
        if !self.gc_purpled.contains(&key) {
            self.gc_maybe_collect();
            if self.gc_purpled.insert(key) {
                self.gc_pending += 1;
            }
        }
    }

    /// Zend auto-collects when its 10k-entry root buffer overflows:
    /// run the same pass here once enough potential roots piled up.
    /// Errors raised mid-collect are dropped — Zend likewise collects
    /// silently (its own buffer-overflow call has no error channel).
    pub(in crate::interp) fn gc_maybe_collect(&mut self) {
        if self.gc_pending < 10_000 || self.gc_collecting || !self.ini_on("zend.enable_gc") {
            return;
        }
        self.gc_collecting = true;
        let _ = self.gc_cycle_collect_impl(true);
        // The pass drained the buffer (`gc_purpled` cleared): the
        // next collect waits for a fresh 10k NEW purpled candidates,
        // not the size of the live set.
        self.arr_handles.retain(|w| w.upgrade().is_some());
        self.gc_pending = 0;
        self.gc_collecting = false;
    }

    /// Drain one gen's destruction journal: emit the suspended
    /// delegation chain's queued finally output (innermost level
    /// first) and return the level's own terminal raise — the
    /// `yield`-inside-`finally` fatal or the body's `finally`-region
    /// death. The suspended frame's CVs decref last — Zend frees
    /// execute_data after the finally chain, so locals' `__destruct`
    /// and held gens' own teardown land here, not in the body's
    /// (already-past) output window.
    pub(in crate::interp) fn gen_fin_replay(
        &mut self,
        q: &crate::value::FinQueue,
        at_unit_end: bool,
    ) -> Option<PhpError> {
        {
            let mut f = q.borrow_mut();

            // Every replay path is a destruction — a gen torn down
            // mid-flight or displaced by a re-run leaves an un-run
            // journaled tail; a fully-consumed gen's tail ran. A
            // delegate torn down while suspended inside the outer's
            // yield-from drain is likewise a kill: its pos mirror
            // counts the collect drive's internal resumes, not real
            // consumer resumes.
            if f.pos < f.total || f.suppressed || (!at_unit_end && self.gen_collect_base.is_some())
            {
                f.kill_tree();
            }
        }
        if q.borrow().suppressed {
            // Re-run artifact — the displaced incarnation's close is
            // bookkeeping, not a real generator destruction.
            return None;
        }
        self.ob_dead_gen(q);
        let mut fin = std::mem::take(&mut *q.borrow_mut());
        let pos = fin.pos;
        // The take blanks the journal — parked ob windows still read
        // this fin as their cursor mirror. Restore the bookkeeping
        // fields they gate on.
        {
            let mut f = q.borrow_mut();
            f.pos = fin.pos;
            f.vis_pos = fin.vis_pos;
            f.total = fin.total;
            f.finished = fin.finished;
            f.killed = fin.killed;
            f.suppressed = fin.suppressed;
            f.delegate_fins = fin.delegate_fins.clone();
        }
        let terminal = self.gen_fin_emit(&fin, pos, at_unit_end);
        let dtor = self
            .gen_release_cells(std::mem::take(&mut fin.suspended))
            .err();
        terminal.or(dtor)
    }

    /// Emit a journal's queued bytes for the suspended chain the
    /// consumer is inside, innermost level first, then the level's
    /// own bytes, then its terminal raise.
    fn gen_fin_emit(
        &mut self,
        fin: &crate::value::GenFinData,
        pos: usize,
        at_unit_end: bool,
    ) -> Option<PhpError> {
        for d in fin.active_delegates_at(pos) {
            if let Some(e) = self.gen_fin_emit(&d.fin, pos, at_unit_end) {
                return Some(e);
            }
        }
        // Suspended AT a yield inside `finally` (iteration reached it
        // normally, or a throw()-driven unwind parked there): Zend
        // abandons the gen silently — the yield already suspended,
        // so the finally's tail bytes and any terminal raise never
        // run.
        if fin.yields.iter().any(|(i, _)| *i == pos) {
            return None;
        }
        self.gen_fin_own_bytes(fin, pos);
        self.gen_fin_terminal(fin, pos, at_unit_end)
    }

    /// Bytes-only replay of the suspended chain (innermost first) —
    /// the `throw()` close path, whose terminal is the injected
    /// throwable itself.
    fn gen_fin_bytes(&mut self, fin: &crate::value::GenFinData, pos: usize) {
        for d in fin.active_delegates_at(pos) {
            self.gen_fin_bytes(&d.fin, pos);
        }
        self.gen_fin_own_bytes(fin, pos);
    }

    /// This level's output bytes whose tags the consumer hasn't
    /// passed (already-shown tags were flushed through `pending_out`
    /// during normal iteration). A `yield`-inside-`finally` past the
    /// suspension point ends the unwind — bytes it or anything after
    /// it emitted never replay.
    fn gen_fin_own_bytes(&mut self, fin: &crate::value::GenFinData, pos: usize) {
        let cap = fin
            .yields
            .iter()
            .filter(|(i, _)| *i > pos)
            .map(|(i, _)| *i)
            .min();
        for (t, b, is_err) in &fin.bytes {
            if *t < pos {
                continue;
            }
            if cap.is_some_and(|y| *t >= y) {
                continue;
            }
            if *is_err {
                self.diag_stderr(&String::from_utf8_lossy(b));
            } else {
                self.emit_replay(b, *t);
            }
        }
    }

    /// The destruction-time raise for a force-closed or shutdown gen,
    /// in unwind order: a `yield` past the suspension point inside a
    /// `finally` region fatals; the body's own error replays when it
    /// died inside `finally`.
    fn gen_fin_terminal(
        &mut self,
        fin: &crate::value::GenFinData,
        pos: usize,
        at_unit_end: bool,
    ) -> Option<PhpError> {
        if let Some((_, yline)) = fin.yields.iter().find(|(i, _)| *i > pos) {
            let v = self.exception(
                "Error",
                "Cannot yield from finally in a force-closed generator",
            );
            if let Value::Object(o) = &v {
                let mut b = o.borrow_mut();
                if let Some(crate::value::ObjectInternal::Exception {
                    file, line, thrown, ..
                }) = &mut b.internal
                {
                    *file = if fin.file.is_empty() {
                        self.diag_file()
                    } else {
                        fin.file.clone()
                    };
                    *line = *yline as u32;
                    *thrown = *yline as u32;
                }
            }
            let e = self.throw(v);
            let frames = self.gen_gc_frames(&fin.fn_name, at_unit_end);
            self.rewrite_throwable_trace(&frames);
            return Some(e);
        }
        if let Some((mut e, throwable)) = fin.fin_err.clone() {
            let frames = self.gen_gc_frames(&fin.fn_name, at_unit_end);
            if e.kind == crate::error::ErrorKind::Throw {
                if throwable.is_some() {
                    self.pending_exception = throwable;
                }
                self.rewrite_throwable_trace(&frames);
            } else {
                e.trace = Some(frames);
            }
            return Some(e);
        }
        None
    }

    /// Destruction-site frames for a force-close / shutdown raise:
    /// `FILE(line): g()` at an unset/overwrite point, `[internal
    /// function]: g()` at request shutdown — under the consumer's
    /// own frames.
    fn gen_gc_frames(&mut self, fn_name: &str, at_unit_end: bool) -> Vec<String> {
        let mut frames = vec![if at_unit_end {
            format!("[internal function]: {}()", fn_name)
        } else {
            format!("{}({}): {}()", self.diag_file(), self.cur_line, fn_name)
        }];
        for fr in self.call_trace.iter().rev() {
            if crate::value::trace_frame_hidden(fr) {
                continue;
            }
            frames.push(crate::value::trace_frame_str(fr));
        }
        frames
    }

    /// Swap the pending throwable's trace for a rendered frame list —
    /// the uncaught display reads the Throwable's own trace.
    fn rewrite_throwable_trace(&mut self, frames: &[String]) {
        if let Some(Value::Object(o)) = &self.pending_exception {
            let mut obj = o.borrow_mut();
            if let Some(crate::value::ObjectInternal::Exception {
                trace,
                frames: cframes,
                ..
            }) = &mut obj.internal
            {
                // The throwable's own construction stack leads the
                // render (Zend keeps the frames live at `new` time);
                // construction frames the resume stack already
                // reports drop out — suspended gen bodies surface
                // as `[internal function]: f()` (call-resume) or a
                // `FILE(line): f()` frame (foreach-resume), and an
                // engine error raised while in-body calls were live
                // re-reports those calls in raise_frames.
                // Dedup is by call, not by callee name: two frames
                // may share a callee (`Generator->send` at the body's
                // re-entrant resume vs the consumer's outer resume)
                // and only same-site entries are the same call.
                let mut internal_names: Vec<String> = Vec::new();
                let mut site_names: Vec<String> = Vec::new();
                let mut site_keys: Vec<(String, String)> = Vec::new();
                for f in frames {
                    if let Some(rest) = f.strip_prefix("[internal function]: ") {
                        if let Some(end) = rest.find('(') {
                            internal_names.push(rest[..end].to_string());
                        }
                    } else if let Some(pos) = f.find("): ") {
                        let call = &f[pos + 3..];
                        if let Some(end) = call.find('(') {
                            site_names.push(call[..end].to_string());
                            site_keys.push((call[..end].to_string(), f[..pos + 1].to_string()));
                        }
                    }
                }
                // The OUTERMOST gen-body frame in the construction
                // stack: everything at/below it is the eager run's
                // drive — engine resumes (`IteratorIterator->rewind()`
                // and friends) leave real method frames there citing
                // the FIRST resume, and a `Generator->{m}()` push
                // cites its own call site. Zend constructs the
                // throwable inside the CURRENT resume, so its stored
                // stack only ever holds the live drive — drop that
                // prefix and let `frames` supply it. Nested-gen
                // bodies sit BETWEEN the outermost body and the
                // construction point: they keep their suspended
                // call/drain frames (`f()`, `It->getIterator()` in
                // gh15275).
                let cut = cframes
                    .iter()
                    .position(|f| f.gen_body)
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let mut parts: Vec<String> = Vec::new();
                for fr in cframes.clone()[cut..].iter().rev() {
                    if crate::value::trace_frame_hidden(fr) {
                        continue;
                    }
                    // A `Generator->{m}()` frame snapshotted into the
                    // construction stack is the resume that RAN the
                    // body — Zend's deferred render shows the resume
                    // live at the raise, already in `frames`. Dropping
                    // it here is what keeps a stale first-resume frame
                    // from leading the rewritten trace.
                    if fr.gen_resume {
                        continue;
                    }
                    let callee = fr
                        .class
                        .as_ref()
                        .map(|c| format!("{}{}{}", c, fr.ty, fr.function))
                        .unwrap_or_else(|| fr.function.clone());
                    let dup = if fr.file == "[internal function]" {
                        // Engine-resumed frames (gen bodies, builtin
                        // callbacks) have no call site of their own —
                        // match by callee name against either render
                        // shape the resume stack can carry.
                        internal_names
                            .iter()
                            .chain(site_names.iter())
                            .any(|n| *n == fr.function || *n == callee)
                    } else {
                        let site = format!("{}({})", fr.file, fr.line);
                        site_keys
                            .iter()
                            .any(|(n, s)| (*n == fr.function || *n == callee) && *s == site)
                    };
                    if dup {
                        continue;
                    }
                    parts.push(crate::value::trace_frame_str(fr));
                }
                parts.extend(frames.iter().cloned());
                let mut t = String::new();
                for (i, fr) in parts.iter().enumerate() {
                    t.push_str(&format!("#{} {}\n", i, fr));
                }
                t.push_str(&format!("#{} {{main}}", parts.len()));
                *trace = t;
            }
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
        // zend's shorthand parse reads only the leading integer and
        // ignores trailing junk before the suffix — '2.5M' behaves as
        // '2M' (startup also warns; the value truncation is what
        // callers observe) and 'abc' parses as 0.
        let num = num.trim();
        let mut end = 0;
        for (i, c) in num.char_indices() {
            if i == 0 && (c == '-' || c == '+') {
                end = 1;
            } else if c.is_ascii_digit() {
                end = i + 1;
            } else {
                break;
            }
        }
        num[..end].parse::<i64>().unwrap_or(0) * mul
    }

    /// Public wrapper so builtins can share PHP's float→int coercion.
    pub fn coerce_int_pub(&mut self, v: &Value) -> i64 {
        self.coerce_int(v)
    }

    /// getenv(): putenv() overrides win over the process environment;
    /// an unset tombstone makes the name read back as unset.
    pub fn getenv_pub(&self, name: &str) -> Option<String> {
        match self.env_overrides.get(name) {
            Some(v) => v.clone(),
            None => std::env::var(name).ok(),
        }
    }

    /// getenv() with no args: the whole environment as name → value,
    /// minus tombstoned names.
    pub fn getenv_all_pub(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = std::env::vars()
            .filter(|(k, _)| !matches!(self.env_overrides.get(k), Some(None)))
            .collect();
        for (k, v) in &self.env_overrides {
            let Some(v) = v else { continue };
            match out.iter_mut().find(|(ek, _)| ek == k) {
                Some(e) => e.1 = v.clone(),
                None => out.push((k.clone(), v.clone())),
            }
        }
        out
    }

    /// putenv("K=V") sets, putenv("K") unsets (zend's unsetenv form) —
    /// both return true.
    pub fn putenv_pub(&mut self, s: &str) -> bool {
        match s.split_once('=') {
            Some((k, v)) => {
                self.env_overrides
                    .insert(k.to_string(), Some(v.to_string()));
            }
            None => {
                self.env_overrides.insert(s.to_string(), None);
            }
        }
        true
    }

    /// putenv() state for spawning children: set/overrides/unset pairs
    /// applied on top of the inherited process environment.
    pub fn env_overrides_pub(&self) -> &HashMap<String, Option<String>> {
        &self.env_overrides
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
                line: self.send_line.unwrap_or(self.cur_line) as u32,
                trace: String::new(),
                thrown: self.send_line.unwrap_or(self.cur_line) as u32,
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
        if self.gen_run_state.is_some() {
            self.gen_raise_ctx = self.call_trace.clone();
        }
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
    /// `visible` marks frames Zend keeps in exception traces — every
    /// real call, literal or dynamic; false only for literal calls
    /// compile-specialized into dedicated opcodes (sprintf rope).
    fn call_builtin(
        &mut self,
        name: &str,
        args: &CallArgs,
        visible: bool,
    ) -> Result<Option<Value>, PhpError> {
        // Builtins run without an interp Frame — free the call's
        // vm_stack span at return like a frame pop. Ok(None) means a
        // user function will consume the CallArgs — its frame owns it.
        let r = self.call_builtin_inner(name, args, visible);
        if !matches!(r, Ok(None)) {
            self.vm_frame_free(&args.vm_sites);
        }
        r
    }

    fn call_builtin_inner(
        &mut self,
        name: &str,
        args: &CallArgs,
        visible: bool,
    ) -> Result<Option<Value>, PhpError> {
        // A builtin frame pushed while dispatched from inside another
        // builtin's own machinery (internal_cb: sort/ob/array-callbacks)
        // reports `[internal function]` — Zend emits no file/line for a
        // frame whose caller is internal. call_user_func* trampolines
        // are transparent to the walk (trace_frame_hidden).
        let (site_file, site_line) = self.call_site(false, self.diag_file(), self.cur_line);
        // Trace frame args mirror Zend's bound param array: named args
        // that resolve to a declared fixed param merge into that
        // positional slot (interior unbound slots materialize as NULL);
        // names that don't match land in the variadic tail and keep
        // their `name:` marker (`substr(string: 'x', length: 2)`
        // renders `substr('x', NULL, 2)`).
        let (frame_args, frame_named) = match builtins::builtin_params(name) {
            Some(params) => Self::bind_frame_args(args, params),
            None => (
                args.to_vec(),
                args.named
                    .iter()
                    .map(|(n, c, ..)| (n.clone(), c.clone()))
                    .collect(),
            ),
        };
        self.call_trace.push(TraceFrame {
            function: name.to_string(),
            class: None,
            ty: String::new(),
            file: site_file,
            line: site_line,
            args: frame_args,
            named_args: frame_named,
            internal: true,
            visible,
            // A cufa-family call with named args is a real frame, not
            // a transparent trampoline (zend only inlines positional
            // cufa calls). forward_static_call* always render — they
            // are ordinary internal functions, not trampolines.
            named_dispatch: !args.named.is_empty()
                && matches!(name, "call_user_func" | "call_user_func_array"),
            gen_resume: false,
            gen_body: false,
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
                // fail() captures call_trace — the frame must still be
                // there (Zend keeps the cufa frame in this trace).
                let r = self.fail(PhpError::uncaught(
                    "ArgumentCountError",
                    format!(
                        "{}() expects at least 1 argument, {} given",
                        name,
                        args.cells.len()
                    ),
                    0,
                ));
                self.call_trace.pop();
                return r;
            };
            if fwd && args.named.iter().any(|(n, ..)| n != "callback") {
                // '*' variadic: the reject fires after the arity
                // checks — `forward_static_call(x:)` with no callback
                // reports the missing param first.
                let r = self.fail(PhpError::uncaught(
                    "ArgumentCountError",
                    format!("{}() does not accept unknown named parameters", name),
                    0,
                ));
                self.call_trace.pop();
                return r;
            }
            let ca = CallArgs {
                end_line: args.end_line,
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
                hold: args.hold.clone(),
                vm_sites: args.vm_sites.clone(),
                vm_slots: 0,
                trav_cells: args
                    .trav_cells
                    .iter()
                    .filter(|i| **i >= 1)
                    .map(|i| i - 1)
                    .collect(),
                verbatim_elems: args.verbatim_elems,
            };
            // Zend's `f` flag validates the callback eagerly with a
            // TypeError before any callee work; forward_static_call's
            // autoloader probe propagates instead of wrapping.
            if !self.is_callable_value(&cb) {
                if fwd {
                    if let Some(pe) = self.take_callable_probe_err() {
                        let r = self.fail(pe);
                        self.call_trace.pop();
                        return r;
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
                let r = self.fail(PhpError::uncaught(
                    "Error",
                    "Cannot call forward_static_call() when no class scope is active",
                    0,
                ));
                self.call_trace.pop();
                return r;
            }
            // Forwarded names bind against the CALLEE's params at this
            // frame's level in Zend (zend_call_function resolves the
            // callee's arg array from the caller context): an unknown
            // name on a non-variadic callee errors here —
            // `call_user_func('strlen', x: 'a')` traces the
            // call_user_func frame, not a strlen frame. Variadic
            // callees collect every name into the tail.
            if !ca.named.is_empty() {
                if let Some(bad) = self.callee_unknown_named(&cb, &ca) {
                    // The cufa frame itself is what traces — fail()
                    // must capture before it pops.
                    let r = self.fail(PhpError::uncaught(
                        "Error",
                        format!("Unknown named parameter ${bad}"),
                        0,
                    ));
                    self.call_trace.pop();
                    return r;
                }
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
                let (save_l, save_s) = (self.cur_line, self.send_line);
                if visible {
                    self.internal_cb += 1;
                } else {
                    // A compile-specialized literal call (sprintf rope)
                    // has no DO_FCALL of its own in Zend — the builtin
                    // runs as inline ops of the caller, so conversions
                    // and the callbacks they reach (__toString, thrown
                    // errors) site at the last argument's line, never
                    // `[internal function]`.
                    self.cur_line = args.end_line;
                    self.send_line = Some(args.end_line);
                }
                let r = match self.resolve_named_builtin(name, params, args) {
                    Ok(cells) => builtins::call(self, name, &cells),
                    Err(e) => Err(e),
                };
                if visible {
                    self.internal_cb -= 1;
                }
                // fail() captures call_trace — pop AFTER it so the
                // builtin's own frame shows in the backtrace
                // (`array_multisort(: 1)` in call_user_func_array_variadic).
                // Named-arg resolution errors raised while the
                // callee's param array is still being built
                // mean the frame never existed in Zend — the
                // trace shows `{main}` only (`substr('x',
                // bogus: 3)`, `sprintf('%s', format:)`). Pop it
                // here once and skip the shared pop below.
                let mut frame_popped = false;
                let r = match r {
                    Ok(v) => Ok(v),
                    Err(e) => {
                        if Self::named_init_err(&e) {
                            self.call_trace.pop();
                            frame_popped = true;
                        }
                        self.fail(e)
                    }
                };
                if !frame_popped {
                    self.call_trace.pop();
                }
                let n = self.emit_cmp_notices();
                if !visible {
                    self.cur_line = save_l;
                    self.send_line = save_s;
                }
                n?;
                r
            }
            // Internal fns without a signature accept no named args;
            // names that aren't builtins at all fall through so the
            // userland invoke path sees them.
            None if builtins::is_builtin(name) && !args.named.is_empty() => {
                // Same param-build-phase error: no frame in the trace.
                self.call_trace.pop();
                let r = self.fail(PhpError::uncaught(
                    "Error",
                    format!("Unknown named parameter ${}", args.named[0].0),
                    0,
                ));
                r
            }
            None => {
                let (save_l, save_s) = (self.cur_line, self.send_line);
                if visible {
                    self.internal_cb += 1;
                } else {
                    self.cur_line = args.end_line;
                    self.send_line = Some(args.end_line);
                }
                let r = builtins::call(self, name, args);
                if visible {
                    self.internal_cb -= 1;
                }
                let r = match r {
                    Ok(r) => Ok(r),
                    Err(e) => self.fail(e),
                };
                self.call_trace.pop();
                let n = self.emit_cmp_notices();
                if !visible {
                    self.cur_line = save_l;
                    self.send_line = save_s;
                }
                n?;
                r
            }
        }
    }

    /// The first name in `args.named` that can't bind to a fixed param
    /// of `cb`'s callee signature — used for the cufa-family rule that
    /// forwarded named args resolve at the trampoline's level. None
    /// when the callee is variadic (unknown names collect into the
    /// tail) or its signature is unavailable.
    fn callee_unknown_named(&mut self, cb: &Value, args: &CallArgs) -> Option<String> {
        fn decl_sig(d: &crate::ast::FunctionDecl) -> (Vec<String>, bool) {
            (
                d.params.iter().map(|p| p.name.clone()).collect(),
                d.params.iter().any(|p| p.variadic),
            )
        }
        // (fixed param names, variadic tail?) for a named callable.
        let name_sig = |interp: &mut Self, n: &str| -> Option<(Vec<String>, bool)> {
            let lower = crate::value::lossy(n).to_lowercase();
            let lower = lower.trim_start_matches('\\').to_string();
            if let Some((cn, mn)) = lower.split_once("::") {
                let key = interp.resolve_class(cn).unwrap_or_else(|| cn.to_string());
                if let Some(cls) = interp.classes.get(&key.to_lowercase()).cloned() {
                    return interp
                        .find_method_in(&cls, mn)
                        .map(|(m, _)| decl_sig(&m.decl));
                }
                return None;
            }
            if let Some(params) = builtins::builtin_params(&lower) {
                let variadic = params.iter().any(|(_, d)| matches!(d, builtins::BDef::Var));
                let names = params
                    .iter()
                    .take_while(|(_, d)| !matches!(d, builtins::BDef::Var))
                    .map(|(pn, _)| pn.to_string())
                    .collect();
                return Some((names, variadic));
            }
            interp.functions.get(&lower).map(|d| decl_sig(d))
        };
        let sig: Option<(Vec<String>, bool)> = match cb {
            Value::Str(s) => name_sig(self, &crate::value::lossy(s)),
            Value::Callable(rc) => match &rc.kind {
                crate::value::CallableKind::Closure(d) => Some(decl_sig(d)),
                crate::value::CallableKind::Named(n) => name_sig(self, n),
                crate::value::CallableKind::Method { obj, class, name } => {
                    let cls = obj
                        .as_ref()
                        .map(|o| o.borrow().class.clone())
                        .or_else(|| class.clone());
                    cls.and_then(|c| {
                        self.find_method_in(&c, &name.to_lowercase())
                            .map(|(m, _)| decl_sig(&m.decl))
                    })
                }
            },
            Value::Array(a) => {
                let arr = a.borrow();
                let cn = arr.iter().find_map(|(k, v)| {
                    if !matches!(k, crate::value::ArrKey::Int(0)) {
                        return None;
                    }
                    match &*v.borrow() {
                        Value::Object(o) => Some(o.borrow().class.name().to_string()),
                        Value::Str(s) => Some(crate::value::lossy(s).into_owned()),
                        _ => None,
                    }
                })?;
                let mn = arr.iter().find_map(|(k, v)| {
                    if !matches!(k, crate::value::ArrKey::Int(1)) {
                        return None;
                    }
                    match &*v.borrow() {
                        Value::Str(s) => Some(crate::value::lossy(s).to_lowercase()),
                        _ => None,
                    }
                })?;
                drop(arr);
                let key = self.resolve_class(&cn).unwrap_or(cn);
                self.classes
                    .get(&key.to_lowercase())
                    .cloned()
                    .and_then(|cls| {
                        self.find_method_in(&cls, &mn)
                            .map(|(m, _)| decl_sig(&m.decl))
                    })
            }
            Value::Object(o) => {
                let cls = o.borrow().class.clone();
                self.find_method_in(&cls, "__invoke")
                    .map(|(m, _)| decl_sig(&m.decl))
            }
            _ => None,
        };
        let (names, variadic) = sig?;
        if variadic {
            return None;
        }
        args.named
            .iter()
            .map(|(n, ..)| n)
            .find(|n| !names.iter().any(|p| p == *n))
            .cloned()
    }

    /// Materialize the frame's display args the way Zend's bound param
    /// array does: named args matching a fixed param occupy that slot
    /// (unbound interior slots render as NULL, positional tail args
    /// follow); names that match nothing stay in `named` so traces
    /// render `name: value`. Pure display logic — rejection/overwrite
    /// errors happen later in resolve_named_builtin.
    fn bind_frame_args(
        args: &CallArgs,
        params: &[(&'static str, builtins::BDef)],
    ) -> (Vec<Cell>, Vec<(String, Cell)>) {
        use builtins::BDef;
        let n_fixed = params
            .iter()
            .take_while(|(_, d)| !matches!(d, BDef::Var))
            .count();
        let mut slot: Vec<Option<Cell>> = vec![None; n_fixed];
        for (i, c) in args.cells.iter().enumerate() {
            if i < n_fixed {
                slot[i] = Some(c.clone());
            }
        }
        let mut named: Vec<(String, Cell)> = Vec::new();
        for (n, c, ..) in &args.named {
            match params[..n_fixed]
                .iter()
                .position(|(pn, _)| *pn == n.as_str())
            {
                Some(j) => slot[j] = Some(c.clone()),
                None => named.push((n.clone(), c.clone())),
            }
        }
        let last = slot
            .iter()
            .rposition(|s| s.is_some())
            .map(|i| i + 1)
            .unwrap_or(0)
            .max(args.cells.len().min(n_fixed));
        let mut out: Vec<Cell> = Vec::new();
        for s in &slot[..last] {
            out.push(match s {
                Some(c) => c.clone(),
                None => Rc::new(RefCell::new(Value::Null)),
            });
        }
        out.extend(args.cells.iter().skip(n_fixed).cloned());
        (out, named)
    }

    /// Named-arg binding errors that Zend raises while still building
    /// the callee's param array — the call frame doesn't exist yet,
    /// so traces drop it (`{main}` only). Everything else (arity,
    /// '*' reject, execution) happens inside a live frame.
    fn named_init_err(e: &PhpError) -> bool {
        matches!(&e.kind, ErrorKind::Uncaught { class } if *class == "Error")
            && (e.message.starts_with("Unknown named parameter")
                || e.message.starts_with("Named parameter"))
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
            "array_replace",
            "array_replace_recursive",
            "array_multisort",
            "min",
            "max",
            "sprintf",
            "printf",
            "fprintf",
            "fscanf",
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
        // '*' variadics defer their unknown-name rejection past the
        // arity checks: `min(x: 1)` reports the missing required param
        // ("expects at least 1 argument, 0 given") while
        // `min(value: 1, x: 2)` rejects ("does not accept unknown named
        // parameters"). Unknown names also don't count as "given".
        let reject_named = NAMED_REJECT.contains(&name);
        let mut pending_reject = 0usize;
        let variadic = params.iter().any(|(_, d)| matches!(d, BDef::Var));
        let n_fixed = params
            .iter()
            .take_while(|(_, d)| !matches!(d, BDef::Var))
            .count();
        // `required` counts OptReq params too: arginfo declares them
        // required (reflection/named-arg checks) even though ZPP accepts
        // the call without them (rand/mt_rand).
        let required = params[..n_fixed]
            .iter()
            .filter(|(_, d)| matches!(d, BDef::Req | BDef::OptReq))
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
                None if variadic && reject_named => pending_reject += 1,
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
                None if matches!(params[i].1, BDef::Unk | BDef::OptReq) && i < last_bound => {
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
                // OptReq params accept nothing-or-all: a tail gap when
                // args were given is the arginfo arity error
                // (`rand(1)`/`rand(min: 1)` => "expects exactly 2, 1
                // given"); `rand()` binds no slot at all.
                None if matches!(params[i].1, BDef::OptReq)
                    && (args.cells.len() + args.named.len()) > 0 =>
                {
                    return Err(arity_err(given, false));
                }
                None if i < last_bound => out.push(cell(params[i].1.val())),
                None => break,
            }
        }
        if pending_reject > 0 {
            return Err(PhpError::uncaught(
                "ArgumentCountError",
                format!("{}() does not accept unknown named parameters", name),
                0,
            ));
        }
        out.extend(extra_pos);
        Ok(out)
    }

    #[track_caller]
    fn fail<T>(&mut self, e: PhpError) -> Result<T, PhpError> {
        if std::env::var("PHPUN_DBG_FAIL").is_ok() {
            eprintln!(
                "FAIL@{}: {:?} {:?}",
                std::panic::Location::caller(),
                e.kind,
                e.message
            );
        }
        if self.gen_run_state.is_some() {
            self.gen_raise_ctx = self.call_trace.clone();
        }
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
                    line: self.const_init_site.unwrap_or(self.cur_line) as u32,
                    args: Vec::new(),
                    named_args: Vec::new(),
                    internal: true,
                    // The pseudo-frame renders in zend's traces
                    // (`#0 %s(%d): [constant expression]()`).
                    visible: true,
                    named_dispatch: false,
                    gen_resume: false,
                    gen_body: false,
                });
            }
            // Internal errors raised as exceptions become real throwables so
            // userland `catch` blocks can intercept them.
            let v = self.exception(class, &e.message);
            // zend_throw_exception_internal: a throwable raised while
            // EG(exception) is pending links the pending one as its
            // $previous ('Cannot use object' <- the armed fetch throw,
            // 'Modulo by zero' <- the armed EH — zend_execute.c:183).
            if let Some((tv, _, _, _)) = &self.dim_throw {
                if let Value::Object(o) = &v {
                    if let Some(ObjectInternal::Exception { previous, .. }) =
                        &mut o.borrow_mut().internal
                    {
                        *previous = Some(tv.clone());
                    }
                }
            }
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
                    // A lazy class-const/prop/static init Error
                    // attributes to the DECL site (zend reports the
                    // decl's own file+line — the `FILE(N) : eval()'d
                    // code` composite included — while the pseudo-frame
                    // keeps the resolution site).
                    if self.class_const_ctx > 0 {
                        if let Some((df, dl)) = &self.const_decl_ctx {
                            *file = df.clone();
                            *line = *dl;
                            *thrown = *dl;
                        }
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

    /// `cls` is-a an SPL-prelude delegation class (subclasses included)
    /// — the PHP stand-ins for zend's C-level SPL delegation. A call
    /// made FROM one of their frames sites `[internal function]` like
    /// zend's internal SPL internals.
    pub(in crate::interp) fn class_is_spl_prelude(&self, cls: &Rc<PhpClass>) -> bool {
        const SPL_PRELUDE_CLASSES: &[&str] = &[
            "OuterIterator",
            "IteratorIterator",
            "FilterIterator",
            "RecursiveFilterIterator",
            "CallbackFilterIterator",
            "RecursiveIteratorIterator",
            "AppendIterator",
        ];
        SPL_PRELUDE_CLASSES.iter().any(|n| self.is_a(cls, n))
    }

    /// `class X` is-a `name` (name = class or interface), parents included.
    fn is_a(&self, cls: &Rc<PhpClass>, name: &str) -> bool {
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
        Value::Resource(r) => format!("Resource id #{}", r.borrow().id()),
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
