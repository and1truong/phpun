//! Bytecode control flow and scalar operations with canonical AST bridges.
//! Unsupported bodies retain the AST path; hybrid functions retain binding
//! and frame teardown. Top-level loops reuse globals without a synthetic frame.
//! Oracle probes and regression gates are required for each extension.

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub(in crate::interp) mod native;

mod bool_loop;

use std::cmp::Ordering;
use std::rc::Rc;

use super::calls::builtin_byref;
use super::util::cell;
use crate::ast::{Expr, FunctionDecl, MethodDecl, Param, Stmt};
use crate::builtins;
use crate::error::PhpError;
use crate::value::{
    compare, identical, ArrKey, CallableKind, Cell, FxMap, PhpArray, PhpCallable, PhpClass,
    PhpObject, Value,
};

use super::{Flow, Interp};

/// A local slot: plain Value for body-assigned locals; params share
/// the call's arg Cell so `$a = v` writes through to what
/// func_get_arg()/func_get_args() report — Zend's CV *is* the arg
/// slot, not a copy.
pub(in crate::interp) enum Slot {
    Uninit,
    V(Value),
    C(Cell),
    /// Live parameter in the frame's value vector, with writes visible to args.
    Arg(u16),
}

#[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
pub(in crate::interp) type CompileCacheEntry =
    (Rc<FunctionDecl>, Option<Rc<Compiled>>, &'static str);
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub(in crate::interp) type CompileCacheEntry = (
    Rc<FunctionDecl>,
    Option<Rc<Compiled>>,
    &'static str,
    native::State,
);

/// One compiled function body — op vector + slot layout.
/// Single-scalar pass-proof (`?T`/union-null handled) — see
/// [`Compiled::ret_fast`].
type ScalarGate = fn(&Value) -> bool;

pub(crate) struct Compiled {
    ops: Vec<Op>,
    /// Canonical expression operations retain AST semantics inside VM control flow.
    hybrid: bool,
    top_level: bool,
    loop_body: bool,
    /// Slot count — params occupy the first `decl.params.len()` slots.
    nslots: usize,
    /// Param name → slot index — the bound (typed/variadic/named-arg)
    /// path maps the frame's bound var cells onto slots by name.
    names: FxMap<String, u16>,
    /// Const-folded param defaults for args the caller omitted
    /// (`None` entry = required param). Populated only when
    /// `bind_free` — the bound path evals defaults itself.
    defaults: Vec<Option<Value>>,
    /// Params+return are check-free: the fast path skips the zend
    /// param/return checks entirely. False when any param carries a
    /// type, &/variadic/promotion, or a non-literal default, or the
    /// decl declares a return type — vm_run then runs a miniature of
    /// bind's check prelude, delegating failures to invoke_fn.
    pub(in crate::interp) bind_free: bool,
    /// What the slot path genuinely can't bind: by-ref params (must
    /// alias the caller's cell — argv carries owned Values) and
    /// unsupported promotion binds. Those decls still compile and run
    /// their bodies via vm_bound_exec after the canonical binder.
    pub(in crate::interp) needs_bind: bool,
    /// Cheap pass-proof for single-scalar return types: `Some(f)`
    /// means `f(returned)` proving the type check satisfied lets the
    /// caller skip `vm_ret_apply` entirely. The gate only ever proves
    /// pass, never decides fail — weak-mode coercions and TypeErrors
    /// keep one canonical home in the full path.
    ret_fast: Option<ScalarGate>,
    /// Same proof per param (variadic params get `None` — their
    /// extras take the full gate each).
    param_fast: Vec<Option<ScalarGate>>,
    value_abi: bool,
    /// Bounded, read-only f64 expression leaf; runtime IC/owner guards remain.
    float_leaf: bool,
}

pub(crate) static PROF: [std::sync::atomic::AtomicU64; 7] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
macro_rules! pnow {
    () => {
        if Interp::callprof_on() {
            Some(std::time::Instant::now())
        } else {
            None
        }
    };
}
macro_rules! padd {
    ($i:expr, $t:expr) => {
        if let Some(t) = $t {
            PROF[$i].fetch_add(
                t.elapsed().as_nanos() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    };
}

// Opt-in diagnostics: cache lookups are not calls or elapsed-time shares.
const COVERAGE_NAMES: [&str; 22] = [
    "compiled-lookup",
    "executed-body",
    "reference-return",
    "variable",
    "assignment",
    "call",
    "array",
    "property",
    "closure",
    "interpolation",
    "foreach",
    "switch",
    "match",
    "exception",
    "static",
    "scope",
    "operator",
    "other",
    "hybrid-lookup",
    "hybrid-body",
    "top-level-entry",
    "loop-body-entry",
];
static COVERAGE: [std::sync::atomic::AtomicU64; 22] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 22];

pub(super) fn coverage_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("PHPUN_VMPROF").is_some())
}

fn coverage_hit(reason: &str) {
    if coverage_on() {
        let i = COVERAGE_NAMES
            .iter()
            .position(|n| *n == reason)
            .unwrap_or(17);
        COVERAGE[i].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Cache metadata only; never pin a receiver or a mutable property cell.
/// `slot` is the declared-prop index from `prop_slot` — stable for the
/// cached class, so the hit skips both key resolution and map probing;
/// a `None` slot means the resolved key is a dynamic prop (map path).
struct CachedProp {
    class: Rc<PhpClass>,
    scope: Option<Rc<PhpClass>>,
    key: String,
    slot: Option<usize>,
    private_candidate: Option<String>,
}

enum Op {
    BoolLoop {
        end: usize,
        executions: std::cell::Cell<u64>,
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        native: std::cell::OnceCell<Option<native::Program>>,
    },
    Const(Value),
    /// Shared AST expression implementation; slots and frame vars alias.
    Canonical(Box<Expr>),
    DimCv {
        array: u16,
        key_slot: Option<u16>,
        key_int: i64,
        write: Option<Value>,
        fallback: Box<Expr>,
    },
    ThisProp {
        name: Rc<str>,
        site: usize,
        cache: std::cell::RefCell<Option<CachedProp>>,
        fallback: Box<Expr>,
    },
    Foreach {
        arr: Box<Expr>,
        key: Option<crate::ast::ForeachKey>,
        val: crate::ast::ForeachTarget,
        body: Rc<Compiled>,
        depth: u32,
    },
    CanonicalStmt {
        stmt: Box<Stmt>,
        decl_site: Option<usize>,
        depth: u32,
        normal: usize,
        breaks: Vec<usize>,
        continues: Vec<usize>,
    },
    Load(u16),
    /// Store top-of-stack into a slot, KEEPING it on the stack
    /// (an assignment is an expression).
    Store(u16),
    /// Discard top-of-stack — object/callable temps get the same
    /// decref pass exec's statement end runs (methods_003).
    Pop,
    /// Value-level binary: arith ops go through `Interp::arith`,
    /// compares through compare/identical + the depth/notices
    /// protocol `compare_op` runs.
    Binary(&'static str),
    Not,
    /// Pop a value, push `Bool(is_truthy)` — the `&&`/`||` result
    /// for the evaluated (non-short-circuited) operand.
    Boolify,
    /// Like `Binary` but the left operand is the slot's CURRENT
    /// value, bound at op-exec — zend binds the left CV of a binary
    /// when the op runs, so `$a . ($a = 'B')` yields "BB" not "AB".
    BinaryCv(&'static str, u16),
    AppendCv(u16),
    /// Direct slot + immediate: no RHS value-stack push/pop or separate dispatch.
    BinaryCvConst(&'static str, u16, Value),
    Jump(usize),
    JumpIfFalse(usize),
    JumpIfTrue(usize),
    /// Direct literal-name call with Zend's persistent call-site cache.
    /// `lname` is lowercase and marker-stripped; `raw` keeps spelling
    /// for diagnostics. InitCall pins the target before args run.
    // Resolve and pin the target before any argument op executes.
    InitCall {
        call: usize,
        args: Box<[Expr]>,
    },
    Call {
        lname: Rc<str>,
        raw: Rc<str>,
        argc: u16,
        site: usize,
        callee: usize,
        cache: std::cell::RefCell<Option<CachedFn>>,
    },
    /// `$cv->m()` — receiver binds at INIT (zend evaluates the object
    /// before argument ops): reads the slot, resolves and caches the
    /// monomorphic target. Canonical receivers/methods jump straight
    /// to the fallback expr.
    InitMethod {
        call: usize,
        obj: u16,
    },
    MethodCall {
        name: Rc<str>,
        argc: u16,
        site: usize,
        cache: std::cell::RefCell<Option<CachedMethod>>,
        fallback: Box<Expr>,
    },
    /// `new X(...)` — class resolves at INIT (zend's order: before arg
    /// side effects). Missing entries fall back to the canonical
    /// `Expr::New` (autoload + not-found errors verbatim).
    InitNew {
        call: usize,
        name: Rc<str>,
        fallback: Box<Expr>,
    },
    NewCall {
        lname: Rc<str>,
        argc: u16,
        site: usize,
        cache: std::cell::RefCell<Option<CachedMethod>>,
    },
    /// Statement boundary: decref the temps this statement left in
    /// expr_temps — the AST's Stmt::Expr-end sweep, which object
    /// dtors time against.
    Sweep,
    /// `$slot` pre/post inc/dec by `delta`; `post` returns the old value.
    IncDec {
        slot: u16,
        delta: i64,
        post: bool,
    },
    Echo,
    Line(usize),
    Return,
}

/// Monomorphic method-dispatch cache at a `MethodCall` site: guards on
/// the receiver's class and reuses the class-stable resolution —
/// `core` None means "resolved but not direct-eligible" (stub,
/// abstract, by-ref, generator, needs-bind, uncompiled), which is
/// class-stable too. Context checks (scope-private, visibility,
/// pending hook) still run per call.
struct CachedMethod {
    class: Rc<PhpClass>,
    core: Option<MSiteCore>,
}

/// Class-stable pieces of a direct method target — everything except
/// the receiver itself and per-call context guards.
/// `Op::InitNew` resolution product: class + its found ctor.
type NewTarget = (Rc<PhpClass>, Option<(Rc<MethodDecl>, Rc<PhpClass>)>);

#[derive(Clone)]
struct MSiteCore {
    m: Rc<MethodDecl>,
    fdecl: Rc<FunctionDecl>,
    comp: Rc<Compiled>,
    dc: Rc<PhpClass>,
}

/// Per-call-site resolution (zend's INIT_FCALL cache slot). `Direct`
/// skips not just the name lookup but the whole `invoke_fn` preamble
/// — the compiled callee is run straight from the op.
pub(in crate::interp) enum CachedFn {
    Direct(Rc<FunctionDecl>, Rc<Compiled>),
    Decl(Rc<FunctionDecl>),
    Builtin,
}

impl Clone for CachedFn {
    fn clone(&self) -> Self {
        match self {
            CachedFn::Direct(d, c) => CachedFn::Direct(d.clone(), c.clone()),
            CachedFn::Decl(d) => CachedFn::Decl(d.clone()),
            CachedFn::Builtin => CachedFn::Builtin,
        }
    }
}

/// Saved Interp state around a vm_run — bundled so the rebind
/// rollback stays one argument.
struct VmSaved {
    line: usize,
    prop_ov: Option<Value>,
    dim_by_ref: bool,
}

struct Compiler {
    ops: Vec<Op>,
    slots: FxMap<String, u16>,
    /// Pending `break`/`continue` patches: (op index, is_continue).
    loops: Vec<(Vec<usize>, Vec<usize>)>,
    /// Names the body assigns anywhere — a `Var` read outside this set
    /// (plus params) can hit undefined-variable diagnostics the slot
    /// model can't reproduce, so it bails.
    assigned: std::collections::HashSet<String>,
    fallback: &'static str,
    top_level: bool,
}

impl Compiled {
    /// Single-scalar type gate the ok-path proves with one `matches!`
    /// (`?T`/union-null handled; an Int for `float` widens,
    /// which is the full path's job). `Some(f)` only proves pass —
    /// every miss still runs the canonical check.
    fn scalar_ty_gate(ty: &[String]) -> Option<ScalarGate> {
        let core: Vec<&str> = ty
            .iter()
            .map(|m| m.trim_start_matches('?'))
            .filter(|m| !m.eq_ignore_ascii_case("null"))
            .collect();
        if core.len() != 1 {
            return None;
        }
        let nullable = ty
            .iter()
            .any(|m| m.eq_ignore_ascii_case("null") || m.starts_with('?'));
        Some(match (core[0].to_ascii_lowercase().as_str(), nullable) {
            ("int", false) => |v: &Value| matches!(v, Value::Int(_)),
            ("int", true) => |v: &Value| matches!(v, Value::Int(_) | Value::Null),
            // Only already-float values prove pass; int widening keeps the binder.
            ("float", false) => |v: &Value| matches!(v, Value::Float(_)),
            ("float", true) => |v: &Value| matches!(v, Value::Float(_) | Value::Null),
            ("string", false) => |v: &Value| matches!(v, Value::Str(_)),
            ("string", true) => |v: &Value| matches!(v, Value::Str(_) | Value::Null),
            ("bool", false) => |v: &Value| matches!(v, Value::Bool(_)),
            ("bool", true) => |v: &Value| matches!(v, Value::Bool(_) | Value::Null),
            ("array", false) => |v: &Value| matches!(v, Value::Array(_)),
            ("array", true) => |v: &Value| matches!(v, Value::Array(_) | Value::Null),
            ("null", _) => |v: &Value| matches!(v, Value::Null),
            ("mixed", _) => |_: &Value| true,
            _ => return None,
        })
    }

    pub(crate) fn compile(
        decl: &FunctionDecl,
        body: &[Stmt],
        top_level: bool,
    ) -> Result<Rc<Compiled>, &'static str> {
        // A by-ref return needs cell plumbing on Flow::Return — AST
        // keeps it. Everything else still compiles: typed/variadic/
        // promoted params, declared returns and expr defaults get
        // bound + checked by bind_and_run and only the body runs here.
        if decl.by_ref {
            return Err("reference-return");
        }
        if Interp::decl_contains_yield(body) {
            return Err("generator");
        }
        // ponytail: fixed by-value constructors reuse the slot binder and
        // canonical promoted stores. Named/variadic/reference/default-expr
        // calls keep bind_and_run; no alternative property-write semantics.
        let promoted_slots = decl.name.eq_ignore_ascii_case("__construct")
            && decl.params.iter().all(|p| {
                !p.by_ref
                    && !p.variadic
                    && p.default.as_ref().is_none_or(|d| const_val(d).is_some())
            });
        let has_promoted = decl.params.iter().any(|p| p.promoted);
        let needs_bind = decl
            .params
            .iter()
            .any(|p| p.by_ref || (p.promoted && !promoted_slots));
        let mut bind_free = !needs_bind
            && !has_promoted
            && decl.ret.is_none()
            && decl.params.iter().all(|p| p.ty.is_none() && !p.variadic);
        let mut defaults: Vec<Option<Value>> = Vec::new();
        if !needs_bind {
            for p in &decl.params {
                match &p.default {
                    Some(e) => match const_val(e) {
                        Some(v) => defaults.push(Some(v)),
                        None => {
                            bind_free = false;
                            defaults.push(None);
                        }
                    },
                    None => defaults.push(None),
                }
            }
        }

        let mut assigned: std::collections::HashSet<String> =
            decl.params.iter().map(|p| p.name.clone()).collect();
        collect_assigned(body, &mut assigned);

        let mut c = Compiler {
            ops: Vec::new(),
            slots: FxMap::default(),
            loops: Vec::new(),
            assigned,
            fallback: "other",
            top_level,
        };
        for (i, p) in decl.params.iter().enumerate() {
            c.slots.insert(p.name.clone(), i as u16);
        }
        c.stmts(body).ok_or(c.fallback)?;
        // No trailing Const+Return: pc exhausting the stream is
        // Flow::Normal — the bound path's `Flow::Return` arm treats
        // an explicit `return` differently from fall-off-the-end.
        // Single-scalar type gates the ok-path proves in one match —
        // `int`/`?string`/`bool`/`array`/`null`/`mixed` only.
        let ret_fast: Option<fn(&Value) -> bool> =
            decl.ret.as_deref().and_then(Self::scalar_ty_gate);
        let param_fast: Vec<Option<ScalarGate>> = decl
            .params
            .iter()
            .map(|p| {
                if p.variadic {
                    None
                } else {
                    p.ty.as_deref().and_then(Self::scalar_ty_gate)
                }
            })
            .collect();
        let canonical_binding = c.ops.iter().any(|op| {
            matches!(
                op,
                Op::Canonical(_) | Op::DimCv { .. } | Op::CanonicalStmt { .. } | Op::Foreach { .. }
            )
        });
        let hybrid = canonical_binding || c.ops.iter().any(|op| matches!(op, Op::ThisProp { .. }));
        // ponytail: params bind inline (the frame's value_args are the arg
        // slots; slot writes store through so func_get_args reports them).
        // By-ref/canonical bodies keep cells — needs_bind/hybrid exclude them
        // already. Proven promoted constructors use the same value slots;
        // their early publish pass materializes before observable hooks.
        let value_abi = !hybrid && !needs_bind && (!has_promoted || promoted_slots);
        let float_type = |ty: &Option<Vec<String>>| {
            ty.as_ref()
                .is_some_and(|t| t.len() == 1 && t[0].eq_ignore_ascii_case("float"))
        };
        let float_leaf = !needs_bind
            && decl.attrs.is_empty()
            && float_type(&decl.ret)
            && decl
                .params
                .iter()
                .all(|p| !p.by_ref && !p.variadic && !p.promoted && float_type(&p.ty))
            && c.slots.len() == decl.params.len()
            && c.ops.len() <= 64
            && c.ops.iter().all(|op| {
                matches!(
                    op,
                    Op::Const(Value::Float(_))
                        | Op::Load(_)
                        | Op::ThisProp { .. }
                        | Op::Binary("+" | "-" | "*")
                        | Op::Line(_)
                        | Op::Sweep
                        | Op::InitCall { .. }
                        | Op::Return
                ) || matches!(op, Op::Call { lname, argc: 1, .. } if lname.as_ref() == "sqrt")
            })
            && c.ops.iter().any(|op| matches!(op, Op::Return));
        Ok(Rc::new(Compiled {
            hybrid,
            top_level,
            loop_body: false,
            nslots: c.slots.len(),
            names: c.slots,
            ops: c.ops,
            defaults,
            bind_free,
            // Canonical expressions need the canonical frame lifetime/teardown.
            needs_bind: needs_bind
                || canonical_binding
                || decl.name.eq_ignore_ascii_case("__toString"),
            ret_fast,
            param_fast,
            value_abi,
            float_leaf,
        }))
    }

    fn compile_loop(
        body: &[Stmt],
        assigned: &std::collections::HashSet<String>,
        top_level: bool,
    ) -> Option<Rc<Self>> {
        let mut c = Compiler {
            ops: Vec::new(),
            slots: FxMap::default(),
            loops: Vec::new(),
            assigned: assigned.clone(),
            fallback: "other",
            top_level,
        };
        c.stmts(body)?;
        // ponytail: nonlocal break/continue, scope and unwind statements
        // retain the canonical body. This subset has no relative AST jump
        // targets; the shared iterator still owns reference/cursor semantics.
        if c.slots.len() > u16::MAX as usize + 1
            || c.ops.iter().all(|op| matches!(op, Op::Line(_)))
            || c.ops
                .iter()
                .any(|op| matches!(op, Op::CanonicalStmt { .. }))
        {
            return None;
        }
        Some(Rc::new(Self {
            nslots: c.slots.len(),
            names: c.slots,
            ops: c.ops,
            hybrid: true,
            top_level,
            loop_body: true,
            defaults: Vec::new(),
            bind_free: false,
            needs_bind: true,
            ret_fast: None,
            param_fast: Vec::new(),
            value_abi: false,
            float_leaf: false,
        }))
    }
}

/// `$x = v` / `$x++` targets anywhere in the body — a Var read of a
/// name never assigned can't warn like the cell-model does, so its
/// presence bails the compile.
fn collect_assigned(stmts: &[Stmt], out: &mut std::collections::HashSet<String>) {
    for s in stmts {
        match s {
            Stmt::Expr(e) | Stmt::Return(Some(e)) => collect_assigned_e(e, out),
            Stmt::Echo(es) => es.iter().for_each(|e| collect_assigned_e(e, out)),
            Stmt::If { cond, then, else_ } => {
                collect_assigned_e(cond, out);
                collect_assigned(then, out);
                collect_assigned(else_, out);
            }
            Stmt::While { cond, body } | Stmt::DoWhile { cond, body } => {
                collect_assigned_e(cond, out);
                collect_assigned(body, out);
            }
            Stmt::For {
                init,
                cond,
                inc,
                body,
            } => {
                init.iter().for_each(|e| collect_assigned_e(e, out));
                cond.iter().for_each(|e| collect_assigned_e(e, out));
                inc.iter().for_each(|e| collect_assigned_e(e, out));
                collect_assigned(body, out);
            }
            Stmt::Foreach {
                arr,
                key,
                val,
                body,
            } => {
                collect_assigned_e(arr, out);
                if let Some(crate::ast::ForeachKey::Var(name)) = key {
                    out.insert(name.clone());
                }
                collect_assigned_foreach(val, out);
                collect_assigned(body, out);
            }
            Stmt::Block(b) => collect_assigned(b, out),
            _ => {}
        }
    }
}

fn collect_assigned_foreach(
    target: &crate::ast::ForeachTarget,
    out: &mut std::collections::HashSet<String>,
) {
    use crate::ast::ForeachTarget;
    match target {
        ForeachTarget::Var(name) => {
            out.insert(name.clone());
        }
        ForeachTarget::ByRef(e) | ForeachTarget::Lvalue(e) => {
            if let Expr::Var(name) = &**e {
                out.insert(name.clone());
            }
            collect_assigned_e(e, out);
        }
        ForeachTarget::List(items) => {
            for (key, target) in items.iter().flatten() {
                if let Some(key) = key {
                    collect_assigned_e(key, out);
                }
                collect_assigned_foreach(target, out);
            }
        }
    }
}

fn collect_assigned_e(e: &Expr, out: &mut std::collections::HashSet<String>) {
    match e {
        Expr::Assign { target, value, .. } => {
            if let Expr::Var(n) = &**target {
                out.insert(n.clone());
            }
            collect_assigned_e(target, out);
            collect_assigned_e(value, out);
        }
        Expr::PreInc(t) | Expr::PreDec(t) | Expr::PostInc(t) | Expr::PostDec(t) => {
            if let Expr::Var(n) = &**t {
                out.insert(n.clone());
            }
            collect_assigned_e(t, out);
        }
        Expr::Binary { l, r, .. } => {
            collect_assigned_e(l, out);
            collect_assigned_e(r, out);
        }
        Expr::Unary { e, .. } | Expr::Paren(e) | Expr::Unpack(e) => collect_assigned_e(e, out),
        Expr::Ternary { c, t, f } => {
            collect_assigned_e(c, out);
            if let Some(t) = t {
                collect_assigned_e(t, out);
            }
            collect_assigned_e(f, out);
        }
        Expr::Call { args, .. } => args.iter().for_each(|a| collect_assigned_e(a, out)),
        Expr::Index { e, i } => {
            collect_assigned_e(e, out);
            if let Some(i) = i {
                collect_assigned_e(i, out);
            }
        }
        _ => {}
    }
}

/// Literal/param-default const fold — anything not a plain literal
/// (const refs, expressions) stays None and bails the compile.
fn const_val(e: &Expr) -> Option<Value> {
    match Interp::unmark_arg(e) {
        Expr::Null => Some(Value::Null),
        Expr::Bool(b) => Some(Value::Bool(*b)),
        Expr::Int(i) => Some(Value::Int(*i)),
        Expr::Float(f) => Some(Value::Float(*f)),
        Expr::Str(s) => Some(Value::str(s.clone())),
        Expr::Unary { op: "-", e } => match &**e {
            Expr::Int(i) => Some(Value::Int(i.checked_neg()?)),
            Expr::Float(f) => Some(Value::Float(-f)),
            _ => None,
        },
        _ => None,
    }
}

type Bail = Option<()>;

impl Compiler {
    fn emit(&mut self, op: Op) -> usize {
        self.ops.push(op);
        self.ops.len() - 1
    }

    fn patch(&mut self, at: usize, target: usize) {
        match &mut self.ops[at] {
            Op::Jump(t) | Op::JumpIfFalse(t) | Op::JumpIfTrue(t) => *t = target,
            _ => unreachable!("patch target is always a jump op"),
        }
    }

    fn slot(&mut self, n: &str) -> u16 {
        if let Some(i) = self.slots.get(n) {
            *i
        } else {
            let i = self.slots.len() as u16;
            self.slots.insert(n.to_string(), i);
            i
        }
    }

    fn stmts(&mut self, stmts: &[Stmt]) -> Bail {
        for s in stmts {
            self.stmt(s)?;
        }
        Some(())
    }

    fn stmt(&mut self, s: &Stmt) -> Bail {
        if let Stmt::Foreach {
            arr,
            key,
            val,
            body,
        } = s
        {
            if let Some(body) = Compiled::compile_loop(body, &self.assigned, self.top_level) {
                self.emit(Op::Foreach {
                    arr: Box::new(arr.clone()),
                    key: key.clone(),
                    val: val.clone(),
                    body,
                    depth: self.loops.len() as u32,
                });
                return Some(());
            }
        }
        self.fallback = match s {
            Stmt::Foreach { .. } => "foreach",
            Stmt::Switch { .. } => "switch",
            Stmt::Try { .. } => "exception",
            Stmt::Static { .. } => "static",
            Stmt::Global(_) | Stmt::Unset(_) => "scope",
            _ => "other",
        };
        match s {
            Stmt::Line(l) => {
                self.emit(Op::Line(*l));
            }
            Stmt::Expr(e) => {
                // `$x;` alone is a dead FREE (no undefined warning) —
                // skip emitting its Load so the warning semantics stay.
                match Interp::unmark_rhs(e) {
                    Expr::Var(n) if !self.assigned.contains(n.as_str()) => {}
                    _ => {
                        self.expr(e)?;
                        self.emit(Op::Pop);
                        self.emit(Op::Sweep);
                    }
                }
            }
            Stmt::Echo(args) => {
                for a in args {
                    self.expr(a)?;
                    self.emit(Op::Echo);
                }
                self.emit(Op::Sweep);
            }
            Stmt::Return(v) => {
                match v {
                    Some(e) => self.expr(e)?,
                    None => {
                        self.emit(Op::Const(Value::Null));
                    }
                };
                // Return temporaries stay live through the caller's statement,
                // matching exec(Return); sweeping here loses returned objects.
                self.emit(Op::Return);
            }
            Stmt::Block(b) => self.stmts(b)?,
            Stmt::If { cond, then, else_ } => {
                self.expr(cond)?;
                let jf = self.emit(Op::JumpIfFalse(usize::MAX));
                self.stmts(then)?;
                let j = self.emit(Op::Jump(usize::MAX));
                let else_at = self.ops.len();
                if !else_.is_empty() {
                    self.stmts(else_)?;
                }
                let end = self.ops.len();
                self.patch(jf, else_at);
                self.patch(j, end);
            }
            Stmt::While { cond, body } => {
                let top = self.ops.len();
                self.expr(cond)?;
                let jf = self.emit(Op::JumpIfFalse(usize::MAX));
                self.loops.push((Vec::new(), Vec::new()));
                self.stmts(body)?;
                self.emit(Op::Jump(top));
                let end = self.ops.len();
                let (breaks, continues) = self.loops.pop().unwrap();
                for b in breaks {
                    self.patch(b, end);
                }
                for c in continues {
                    self.patch(c, top);
                }
                self.patch(jf, end);
            }
            Stmt::DoWhile { cond, body } => {
                let top = self.ops.len();
                self.loops.push((Vec::new(), Vec::new()));
                self.stmts(body)?;
                let cond_at = self.ops.len();
                self.expr(cond)?;
                self.emit(Op::JumpIfTrue(top));
                let end = self.ops.len();
                let (breaks, continues) = self.loops.pop().unwrap();
                for b in breaks {
                    self.patch(b, end);
                }
                for c in continues {
                    self.patch(c, cond_at);
                }
            }
            Stmt::For {
                init,
                cond,
                inc,
                body,
            } => {
                let region = self.emit(Op::BoolLoop {
                    end: 0,
                    executions: std::cell::Cell::new(0),
                    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
                    native: std::cell::OnceCell::new(),
                });
                for e in init {
                    self.expr(e)?;
                    self.emit(Op::Pop);
                }
                let top = self.ops.len();
                let jf = if !cond.is_empty() {
                    for (i, e) in cond.iter().enumerate() {
                        self.expr(e)?;
                        if i + 1 < cond.len() {
                            self.emit(Op::Pop);
                        }
                    }
                    Some(self.emit(Op::JumpIfFalse(usize::MAX)))
                } else {
                    None
                };
                self.loops.push((Vec::new(), Vec::new()));
                self.stmts(body)?;
                let inc_at = self.ops.len();
                for e in inc {
                    self.expr(e)?;
                    self.emit(Op::Pop);
                }
                self.emit(Op::Jump(top));
                let end = self.ops.len();
                let (breaks, continues) = self.loops.pop().unwrap();
                for b in breaks {
                    self.patch(b, end);
                }
                for c in continues {
                    self.patch(c, inc_at);
                }
                if let Some(jf) = jf {
                    self.patch(jf, end);
                }
                if bool_loop::eligible(&self.ops[region + 1..end]) {
                    if let Op::BoolLoop { end: target, .. } = &mut self.ops[region] {
                        *target = end;
                    }
                }
            }
            Stmt::Break(level) | Stmt::Continue(level) => {
                let level = match level {
                    None => 1,
                    Some(Expr::Int(n)) if *n > 0 => *n as usize,
                    _ => return None,
                };
                let target = self.loops.len().checked_sub(level)?;
                let jump = self.emit(Op::Jump(usize::MAX));
                if matches!(s, Stmt::Break(_)) {
                    self.loops[target].0.push(jump);
                } else {
                    self.loops[target].1.push(jump);
                }
            }
            Stmt::Foreach { .. }
            | Stmt::Switch { .. }
            | Stmt::Try { .. }
            | Stmt::Global(_)
            | Stmt::Static { .. }
            | Stmt::Unset(_) => {
                // Reuse canonical iterator/unwind/scope machinery while the
                // surrounding bytecode loops retain their own jump targets.
                let at = self.emit(Op::CanonicalStmt {
                    stmt: Box::new(s.clone()),
                    decl_site: None,
                    depth: self.loops.len() as u32,
                    normal: usize::MAX,
                    breaks: Vec::new(),
                    continues: Vec::new(),
                });
                let mut breaks = Vec::new();
                let mut continues = Vec::new();
                for index in (0..self.loops.len()).rev() {
                    let jump = self.emit(Op::Jump(usize::MAX));
                    self.loops[index].0.push(jump);
                    breaks.push(jump);
                    let jump = self.emit(Op::Jump(usize::MAX));
                    self.loops[index].1.push(jump);
                    continues.push(jump);
                }
                let normal = self.ops.len();
                if let Op::CanonicalStmt {
                    normal: n,
                    breaks: b,
                    continues: c,
                    ..
                } = &mut self.ops[at]
                {
                    *n = normal;
                    *b = breaks;
                    *c = continues;
                }
            }
            Stmt::Function(_)
            | Stmt::Class(_)
            | Stmt::Namespace(_)
            | Stmt::Use(_)
            | Stmt::Declare { .. }
            | Stmt::ConstDecl(_)
            | Stmt::Diag { .. }
            | Stmt::Inline(_)
                if self.top_level =>
            {
                let next = self.ops.len() + 1;
                self.emit(Op::CanonicalStmt {
                    stmt: Box::new(s.clone()),
                    decl_site: match s {
                        Stmt::Function(d) => Some(std::ptr::from_ref(d) as usize),
                        _ => None,
                    },
                    depth: self.loops.len() as u32,
                    normal: next,
                    breaks: Vec::new(),
                    continues: Vec::new(),
                });
            }
            _ => return None,
        }
        Some(())
    }

    fn expr(&mut self, e: &Expr) -> Bail {
        let e = Interp::unmark_rhs(e);
        self.fallback = match e {
            Expr::Var(_) | Expr::VarVar(..) => "variable",
            Expr::Assign { .. } => "assignment",
            Expr::Call { .. } => "call",
            Expr::ArrayLit(_) | Expr::Index { .. } => "array",
            Expr::Prop { .. } | Expr::StaticProp { .. } => "property",
            Expr::Closure(_) => "closure",
            Expr::Interp(_) => "interpolation",
            Expr::Match { .. } => "match",
            Expr::Throw(_) => "exception",
            Expr::Binary { .. } | Expr::Unary { .. } => "operator",
            _ => "other",
        };
        match e {
            Expr::Null => {
                self.emit(Op::Const(Value::Null));
            }
            Expr::Bool(b) => {
                self.emit(Op::Const(Value::Bool(*b)));
            }
            Expr::Int(i) => {
                self.emit(Op::Const(Value::Int(*i)));
            }
            Expr::Float(f) => {
                self.emit(Op::Const(Value::Float(*f)));
            }
            Expr::Str(s) => {
                self.emit(Op::Const(Value::str(s.clone())));
            }
            Expr::Interp(parts)
                if parts
                    .iter()
                    .all(|p| matches!(p, crate::lexer::StringPart::Lit(_))) =>
            {
                let bytes: Vec<u8> = parts
                    .iter()
                    .flat_map(|p| match p {
                        crate::lexer::StringPart::Lit(bytes) => bytes.as_slice(),
                        _ => unreachable!(),
                    })
                    .copied()
                    .collect();
                self.emit(Op::Const(Value::bytes(bytes)));
            }
            Expr::Paren(i) => return self.expr(i),
            Expr::Var(n) => {
                if n == "this" || !self.assigned.contains(n.as_str()) {
                    self.emit(Op::Canonical(Box::new(e.clone())));
                    return Some(());
                }
                let sl = self.slot(n);
                self.emit(Op::Load(sl));
            }
            Expr::Assign {
                op: "=&" | "??=", ..
            } => {
                self.emit(Op::Canonical(Box::new(e.clone())));
            }
            Expr::Assign {
                target, op, value, ..
            } => {
                if *op == "=" && matches!(Interp::unmark_rhs(target), Expr::Index { .. }) {
                    if let (
                        Expr::Index {
                            e: base,
                            i: Some(key),
                        },
                        Some(value),
                    ) = (Interp::unmark_rhs(target), const_val(value))
                    {
                        if matches!(value, Value::Null | Value::Bool(_) | Value::Int(_)) {
                            if let Expr::Var(name) = Interp::unmark_rhs(base) {
                                if !Interp::is_superglobal(name) {
                                    let key = match Interp::unmark_rhs(key) {
                                        Expr::Var(n) if !Interp::is_superglobal(n) => {
                                            Some((Some(self.slot(n)), 0))
                                        }
                                        Expr::Int(i) => Some((None, *i)),
                                        _ => None,
                                    };
                                    if let Some((key_slot, key_int)) = key {
                                        let array = self.slot(name);
                                        self.emit(Op::DimCv {
                                            array,
                                            key_slot,
                                            key_int,
                                            write: Some(value),
                                            fallback: Box::new(e.clone()),
                                        });
                                        return Some(());
                                    }
                                }
                            }
                        }
                    }
                }
                let Expr::Var(n) = &**target else {
                    self.emit(Op::Canonical(Box::new(e.clone())));
                    return Some(());
                };
                if n == "this" {
                    return None; // AST path emits the canonical fatal
                }
                let slot = self.slot(n);
                if *op == "=" {
                    self.expr(value)?;
                } else {
                    let bop = op.strip_suffix('=')?;
                    if bop == "?" || bop.is_empty() {
                        return None; // ??= and bare op stays on AST path
                    }
                    if !matches!(
                        bop,
                        "+" | "-" | "*" | "/" | "%" | "**" | "." | "&" | "|" | "^" | "<<" | ">>"
                    ) {
                        return None;
                    }
                    // zend binds the lhs CV when the op runs — rhs
                    // first, then BinaryCv reads the slot's CURRENT
                    // value (`$a .= ($a = 'B')` → 'BB').
                    if bop == "." {
                        self.expr(value)?;
                        self.emit(Op::AppendCv(slot));
                    } else if let Some(rhs) = const_val(value) {
                        self.emit(Op::BinaryCvConst(bop, slot, rhs));
                    } else {
                        self.expr(value)?;
                        self.emit(Op::BinaryCv(bop, slot));
                    }
                }
                self.emit(Op::Store(slot));
            }
            Expr::PreInc(t) | Expr::PreDec(t) | Expr::PostInc(t) | Expr::PostDec(t) => {
                let Expr::Var(n) = &**t else {
                    return None;
                };
                if n == "this" {
                    return None;
                }
                let delta = if matches!(e, Expr::PreInc(_) | Expr::PostInc(_)) {
                    1
                } else {
                    -1
                };
                let post = matches!(e, Expr::PostInc(_) | Expr::PostDec(_));
                let sl = self.slot(n);
                self.emit(Op::IncDec {
                    slot: sl,
                    delta,
                    post,
                });
            }
            Expr::Unary { op: "!", e } => {
                self.expr(e)?;
                self.emit(Op::Not);
            }
            Expr::Unary { op: "-", e } => match const_val(e)? {
                Value::Int(i) => {
                    self.emit(Op::Const(Value::Int(i.checked_neg()?)));
                }
                Value::Float(f) => {
                    self.emit(Op::Const(Value::Float(-f)));
                }
                _ => return None,
            },
            Expr::Unary { op: "+", e } => match const_val(e) {
                Some(v @ (Value::Int(_) | Value::Float(_))) => {
                    self.emit(Op::Const(v));
                }
                _ => return None,
            },
            Expr::Ternary { c, t: Some(t), f } => {
                self.expr(c)?;
                let jf = self.emit(Op::JumpIfFalse(usize::MAX));
                self.expr(t)?;
                let j = self.emit(Op::Jump(usize::MAX));
                let f_at = self.ops.len();
                self.expr(f)?;
                let end = self.ops.len();
                self.patch(jf, f_at);
                self.patch(j, end);
            }
            Expr::Binary { op, l, r } => match *op {
                "&&" => {
                    // `a && b`: JMPZ pops the lhs — short path pushes
                    // false, long path pushes bool(b), so both leave
                    // exactly one value (PHP &&/|| always yield bool).
                    self.expr(l)?;
                    let jf = self.emit(Op::JumpIfFalse(usize::MAX));
                    self.expr(r)?;
                    self.emit(Op::Boolify);
                    let j = self.emit(Op::Jump(usize::MAX));
                    let f_at = self.ops.len();
                    self.emit(Op::Const(Value::Bool(false)));
                    let end = self.ops.len();
                    self.patch(jf, f_at);
                    self.patch(j, end);
                }
                "||" => {
                    self.expr(l)?;
                    let jt = self.emit(Op::JumpIfTrue(usize::MAX));
                    self.expr(r)?;
                    self.emit(Op::Boolify);
                    let j = self.emit(Op::Jump(usize::MAX));
                    let t_at = self.ops.len();
                    self.emit(Op::Const(Value::Bool(true)));
                    let end = self.ops.len();
                    self.patch(jt, t_at);
                    self.patch(j, end);
                }
                "+" | "-" | "*" | "/" | "%" | "**" | "." | "&" | "|" | "^" | "<<" | ">>" | "=="
                | "!=" | "===" | "!==" | "<" | "<=" | ">" | ">=" | "<=>" => {
                    // A plain-CV lhs binds at op-exec like zend —
                    // the rhs may have just re-assigned it.
                    if let Expr::Var(n) = Interp::unmark_rhs(l) {
                        if n != "this" && self.assigned.contains(n.as_str()) {
                            let sl = self.slot(n);
                            if let Some(rhs) = const_val(r) {
                                self.emit(Op::BinaryCvConst(op, sl, rhs));
                            } else {
                                self.expr(r)?;
                                self.emit(Op::BinaryCv(op, sl));
                            }
                            return Some(());
                        }
                    }
                    self.expr(l)?;
                    self.expr(r)?;
                    self.emit(Op::Binary(op));
                }
                _ => return None,
            },
            Expr::Call {
                name,
                args,
                site,
                callee,
            } => {
                // Only compile-time literal unqualified names: dynamic
                // names, method/static calls and named/unpack args keep
                // the AST call machinery.
                let Expr::Str(raw) = &**name else {
                    self.emit(Op::Canonical(Box::new(e.clone())));
                    return Some(());
                };
                if !raw.starts_with('\u{1}')
                    || raw.contains('\\')
                    || raw.contains("::")
                    || args.iter().any(|a| {
                        matches!(
                            Interp::unmark_arg(a),
                            Expr::Unpack(_) | Expr::Binary { op: "named", .. }
                        )
                    })
                {
                    self.emit(Op::Canonical(Box::new(e.clone())));
                    return Some(());
                }
                // Reads the caller's var table — VM frames keep vars
                // in slots, not `f.vars` (compact_one's lookup_var
                // would see an empty scope).
                if raw
                    .trim_start_matches('\u{1}')
                    .rsplit('\\')
                    .next()
                    .is_some_and(|seg| {
                        ["compact", "extract", "get_defined_vars"]
                            .iter()
                            .any(|name| seg.eq_ignore_ascii_case(name))
                    })
                {
                    return None;
                }
                let init = self.emit(Op::InitCall {
                    call: usize::MAX,
                    args: args.clone().into_boxed_slice(),
                });
                for a in args {
                    match Interp::unmark_arg(a) {
                        Expr::Unpack(_) => return None,
                        Expr::Binary { op: "named", .. } => return None,
                        _ => {}
                    }
                    self.expr(Interp::unmark_arg(a))?;
                }
                let lname: Rc<str> = Rc::from(raw.trim_start_matches('\u{1}').to_lowercase());
                let call = self.emit(Op::Call {
                    lname,
                    raw: Rc::from(raw.trim_start_matches('\u{1}')),
                    argc: args.len() as u16,
                    site: *site,
                    callee: *callee,
                    cache: std::cell::RefCell::new(None),
                });
                if let Op::InitCall { call: at, .. } = &mut self.ops[init] {
                    *at = call;
                }
            }
            Expr::MethodCall {
                obj,
                name: crate::ast::PropName::Name(mname),
                args,
                nullsafe: false,
                site,
            } if matches!(Interp::unmark_rhs(obj), Expr::Var(n) if !Interp::is_superglobal(n))
                && args.iter().all(|a| {
                    !matches!(
                        Interp::unmark_arg(a),
                        Expr::Unpack(_) | Expr::Binary { op: "named", .. }
                    )
                }) =>
            {
                let Expr::Var(vname) = Interp::unmark_rhs(obj) else {
                    unreachable!()
                };
                let obj = self.slot(vname);
                let init = self.emit(Op::InitMethod {
                    call: usize::MAX,
                    obj,
                });
                for a in args {
                    self.expr(Interp::unmark_arg(a))?;
                }
                let call = self.emit(Op::MethodCall {
                    name: Rc::from(mname.as_str()),
                    argc: args.len() as u16,
                    site: *site,
                    cache: std::cell::RefCell::new(None),
                    fallback: Box::new(e.clone()),
                });
                if let Op::InitMethod { call: at, .. } = &mut self.ops[init] {
                    *at = call;
                }
            }
            Expr::New { class, args, site }
                if matches!(Interp::unmark_rhs(class), Expr::Const(n)
                    if !(n.eq_ignore_ascii_case("self")
                        || n.eq_ignore_ascii_case("parent")
                        || n.eq_ignore_ascii_case("static")))
                    && args.iter().all(|a| {
                        !matches!(
                            Interp::unmark_arg(a),
                            Expr::Unpack(_) | Expr::Binary { op: "named", .. }
                        )
                    }) =>
            {
                let Expr::Const(cname) = Interp::unmark_rhs(class) else {
                    unreachable!()
                };
                let init = self.emit(Op::InitNew {
                    call: usize::MAX,
                    name: Rc::from(cname.as_str()),
                    fallback: Box::new(e.clone()),
                });
                for a in args {
                    self.expr(Interp::unmark_arg(a))?;
                }
                let call = self.emit(Op::NewCall {
                    lname: Rc::from(cname.to_lowercase().as_str()),
                    argc: args.len() as u16,
                    site: *site,
                    cache: std::cell::RefCell::new(None),
                });
                if let Op::InitNew { call: at, .. } = &mut self.ops[init] {
                    *at = call;
                }
            }
            Expr::Prop {
                obj,
                name: crate::ast::PropName::Name(name),
                nullsafe: false,
                site,
            } if matches!(Interp::unmark_rhs(obj), Expr::Var(n) if n == "this") => {
                self.emit(Op::ThisProp {
                    name: Rc::from(name.as_str()),
                    site: *site,
                    cache: std::cell::RefCell::new(None),
                    fallback: Box::new(e.clone()),
                });
            }
            Expr::Index {
                e: base,
                i: Some(key),
            } => {
                if let Expr::Var(array) = Interp::unmark_rhs(base) {
                    if !Interp::is_superglobal(array) {
                        let key = match Interp::unmark_rhs(key) {
                            Expr::Var(n) if !Interp::is_superglobal(n) => {
                                Some((Some(self.slot(n)), 0))
                            }
                            Expr::Int(i) => Some((None, *i)),
                            _ => None,
                        };
                        if let Some((key_slot, key_int)) = key {
                            let array = self.slot(array);
                            self.emit(Op::DimCv {
                                array,
                                key_slot,
                                key_int,
                                write: None,
                                fallback: Box::new(e.clone()),
                            });
                            return Some(());
                        }
                    }
                }
                self.emit(Op::Canonical(Box::new(e.clone())));
            }
            Expr::ArrayLit(_)
            | Expr::Index { .. }
            | Expr::Prop { .. }
            | Expr::StaticProp { .. }
            | Expr::MethodCall { .. }
            | Expr::StaticCall { .. }
            | Expr::StaticCallDyn { .. }
            | Expr::ClassConst { .. }
            | Expr::ClassConstDyn { .. }
            | Expr::Closure(_)
            | Expr::Interp(_)
            | Expr::Cast { .. }
            | Expr::Const(_)
            | Expr::MagicConst(_)
            | Expr::Isset(_)
            | Expr::Empty(_)
            | Expr::Fcc(_)
            | Expr::Clone(_)
            | Expr::Instanceof { .. }
            | Expr::Match { .. }
            | Expr::Throw(_)
            | Expr::Include { .. } => {
                self.emit(Op::Canonical(Box::new(e.clone())));
            }
            _ => return None,
        };
        Some(())
    }
}

impl<'a> Interp<'a> {
    pub(in crate::interp) fn vm_top_exec(&mut self, stmts: &[Stmt]) -> Option<Flow> {
        fn has_loop(stmts: &[Stmt]) -> bool {
            stmts.iter().any(|s| match s {
                Stmt::For { .. } | Stmt::While { .. } | Stmt::DoWhile { .. } => true,
                Stmt::Block(b) => has_loop(b),
                Stmt::If { then, else_, .. } => has_loop(then) || has_loop(else_),
                _ => false,
            })
        }
        // Avoid compilation overhead for short CLI entry files with no loops.
        if !has_loop(stmts) {
            return None;
        }
        let decl = FunctionDecl {
            name: Rc::from("{main}"),
            params: Vec::new(),
            ret: None,
            body: Vec::new(),
            attrs: Vec::new(),
            by_ref: false,
            line: 0,
            end_line: 0,
            file: self.cur_file.clone(),
            ns: String::new(),
            decl_in: None,
        };
        let comp = Compiled::compile(&decl, stmts, true).ok()?;
        Some(match self.vm_bound_exec(&comp) {
            Ok(flow) => flow,
            Err(err) => self.err_flow(err),
        })
    }

    /// CLI-only diagnostic report; profiling changes runtime overhead.
    pub fn dump_vm_coverage() {
        if coverage_on() {
            for (name, counter) in COVERAGE_NAMES.iter().zip(&COVERAGE) {
                eprintln!(
                    "vm-coverage: {name}={}",
                    counter.load(std::sync::atomic::Ordering::Relaxed)
                );
            }
        }
    }

    /// Compile-cache: keyed by the decl's Rc pointer, with the Rc kept
    /// alive by the entry itself so a dropped decl can never collide.
    /// `None` entries memoize "doesn't compile" so uncompiled bodies
    /// don't pay the compile walk per call.
    pub(in crate::interp) fn vm_compiled(
        &mut self,
        decl: &Rc<FunctionDecl>,
    ) -> Option<Rc<Compiled>> {
        use std::collections::hash_map::Entry;
        let entry = match self.compiled_fns.entry(Rc::as_ptr(decl) as usize) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let (compiled, reason) = match Compiled::compile(decl, &decl.body, false) {
                    Ok(c) => {
                        let reason = if c.hybrid {
                            "hybrid-lookup"
                        } else {
                            "compiled-lookup"
                        };
                        (Some(c), reason)
                    }
                    Err(reason) => {
                        if coverage_on() {
                            eprintln!(
                                "vm-fallback: {}:{} {} reason={reason}",
                                decl.file, decl.line, decl.name
                            );
                        }
                        (None, reason)
                    }
                };
                let entry = (decl.clone(), compiled, reason);
                #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
                let entry = (entry.0, entry.1, entry.2, native::State::Cold(0));
                e.insert(entry)
            }
        };
        coverage_hit(entry.2);
        entry.1.clone()
    }

    /// The bind_and_run shell for a compiled body: identical
    /// save/trace/teardown order, with slots in place of vars-map
    /// binds. Non-bind_free decls run the zend param/return checks
    /// inline (delegating failures to invoke_fn) — typed, variadic
    /// and declared-return fns still execute on the slot frame.
    pub(in crate::interp) fn vm_run(
        &mut self,
        decl: &Rc<FunctionDecl>,
        comp: &Compiled,
        mut args: super::CallArgs,
    ) -> Result<Value, PhpError> {
        // Scalar slots may share this call's arg cells, never the caller's
        // by-value cells (array_walk, unpack and call_user_func_array share them).
        // Do this before tracing/coercion so both see the isolated argument.
        for (c, p) in args.cells.iter_mut().zip(&decl.params) {
            if !p.by_ref && (Rc::strong_count(c) > 1 || Rc::weak_count(c) > 0) {
                let value = c.borrow().clone();
                *c = cell(value);
            }
        }
        let __p = pnow!();
        if __p.is_some() {
            PROF[6].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let saved = VmSaved {
            line: self.cur_line,
            prop_ov: self.last_prop_ov.take(),
            dim_by_ref: std::mem::replace(&mut self.dim_by_ref, false),
        };
        // Only defer when every provided param is check-free or proven to
        // pass. Rebinding/coercion errors retain eager send-time arguments.
        let value_abi = !self.cur().value_args.is_empty();
        let defer_args = value_abi
            || !decl.params.iter().any(|p| p.promoted)
                && args.named.is_empty()
                && args.cells.len() >= decl.params.len()
                && decl.params.iter().enumerate().all(|(i, p)| {
                    p.ty.is_none()
                        || comp.param_fast[i].is_some_and(|gate| gate(&args.cells[i].borrow()))
                });
        let fr = self.call_site_frame(decl, &args, defer_args);
        self.call_trace.push(fr);
        padd!(2, __p);
        let __p = pnow!();
        self.last_call_by_ref = decl.by_ref;
        // Param slots SHARE the arg cells: func_get_arg(i) reads them,
        // and a CV overwrite is the arg write like Zend's shared slot.
        // Positional extras beyond the params ride frame.args too.
        // fa is the frame's arg list — positional args already in
        // order, so the caller's cells vec moves wholesale (one clone
        // per slot, zero per-arg vec copy). Missing params never join
        // fa: func_num_args counts provided args only (zend's CV fill
        // stops at the highest bound slot).
        let mut fa: Vec<Cell> = std::mem::take(&mut args.cells);
        let mut slots: Vec<Slot> = self
            .vm_slot_pool
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(comp.nslots));
        if value_abi {
            // Params bind inline to the frame's arg vector. An arg that
            // was never sent binds its const default as an owned value —
            // introspection must not see it (zend's CV fill stops at the
            // highest bound slot); a variadic packs the tail args into a
            // fresh array the callee owns.
            let n_args = self.cur().value_args.len();
            for (i, p) in decl.params.iter().enumerate() {
                if p.variadic {
                    let mut arr = PhpArray::new();
                    for v in &self.cur().value_args[i.min(n_args)..] {
                        arr.push(v.clone());
                    }
                    slots.push(Slot::V(Value::Array(Rc::new(std::cell::RefCell::new(arr)))));
                    break;
                }
                if i < n_args {
                    slots.push(Slot::Arg(i as u16));
                    continue;
                }
                let mut dv = comp.defaults[i].clone().unwrap_or(Value::Null);
                if let Some(ty) = &p.ty {
                    // `float $f = 0` widens at bind like the typed prelude.
                    if ty.iter().any(|m| m.eq_ignore_ascii_case("float"))
                        && !ty.iter().any(|m| m.eq_ignore_ascii_case("int"))
                    {
                        if let Value::Int(n) = dv {
                            dv = Value::Float(n as f64);
                        }
                    }
                }
                slots.push(Slot::V(dv));
            }
        } else if comp.bind_free {
            for (i, _) in decl.params.iter().enumerate() {
                let c = match fa.get(i) {
                    Some(c) => c.clone(),
                    None => cell(comp.defaults[i].clone().unwrap_or(Value::Null)),
                };
                slots.push(Slot::C(c));
            }
        } else {
            // Typed/variadic prelude — zend's arg-check semantics from
            // bind_and_run_inner in miniature. Any failure rolls this
            // call's pushes back and delegates to invoke_fn so the
            // canonical TypeError (arg trace, "and defined" display)
            // exists in exactly one place.
            for (i, p) in decl.params.iter().enumerate() {
                if p.variadic {
                    // Extras pack into a fresh arData the callee owns.
                    let mut arr = PhpArray::new();
                    for j in i..fa.len() {
                        let v = fa[j].borrow().clone();
                        if let Some(ty) = &p.ty {
                            if let Some(e) = self.vm_param_gate(decl, p, ty, &v) {
                                return self.vm_rebind(e, decl, args, &mut fa, saved);
                            }
                        }
                        arr.push(v);
                    }
                    slots.push(Slot::C(cell(Value::Array(Rc::new(
                        std::cell::RefCell::new(arr),
                    )))));
                    break;
                }
                let c = match fa.get(i) {
                    Some(c) => c.clone(),
                    None => {
                        if p.default.is_some() {
                            match &comp.defaults[i] {
                                Some(dv) => {
                                    // `float $f = 0` widens at bind —
                                    // the int default coerces even
                                    // under strict_types.
                                    let mut dv = dv.clone();
                                    if let Some(ty) = &p.ty {
                                        if ty.iter().any(|m| m.eq_ignore_ascii_case("float"))
                                            && !ty.iter().any(|m| m.eq_ignore_ascii_case("int"))
                                        {
                                            if let Value::Int(n) = dv {
                                                dv = Value::Float(n as f64);
                                            }
                                        }
                                    }
                                    cell(dv)
                                }
                                // Non-literal default — bind evals it.
                                None => {
                                    return self.vm_rebind(None, decl, args, &mut fa, saved);
                                }
                            }
                        } else {
                            cell(Value::Null) // arity-guarded unreachable
                        }
                    }
                };
                if let Some(ty) = &p.ty {
                    if comp.param_fast[i].is_some_and(|g| g(&c.borrow())) {
                        slots.push(Slot::C(c));
                        continue;
                    }
                    // Probe errors belong to THIS param's check only.
                    self.callable_probe_err = None;
                    let v = c.borrow().clone();
                    if let Some(e) = self.vm_param_gate(decl, p, ty, &v) {
                        return self.vm_rebind(e, decl, args, &mut fa, saved);
                    }
                    let strict = self.caller_file_strict();
                    if !strict && !self.ty_weak_exact(ty, &v) {
                        if let Some(cv) = self.coerce_scalar(ty, &v) {
                            // Arg-coercion deprecations attribute to
                            // the callee's decl line (scalar_basic).
                            let pl = self.cur_line;
                            self.cur_line = decl.line;
                            self.deprecate_lossy_int(ty, &v, &cv);
                            self.cur_line = pl;
                            *c.borrow_mut() = cv;
                        }
                    }
                    // Strict mode still allows int->float widening
                    // stored back for callee visibility.
                    if strict
                        && ty.iter().any(|m| m.eq_ignore_ascii_case("float"))
                        && !self.ty_weak_exact(ty, &v)
                    {
                        if let Value::Int(n) = v {
                            *c.borrow_mut() = Value::Float(n as f64);
                        }
                    }
                }
                slots.push(Slot::C(c));
            }
        }
        slots.resize_with(comp.nslots, || Slot::Uninit);
        if let Some(f) = self.stack.last_mut() {
            f.vm_sites.append(&mut args.vm_sites);
            f.args = fa;
        }
        let temps_base = self.expr_temps.len();
        let saved_depth = std::mem::replace(&mut self.loop_depth, 0);
        padd!(3, __p);
        let __p = pnow!();
        let r = (|| {
            if decl.params.iter().any(|p| p.promoted) {
                let obj = self.cur().this_obj.clone();
                // A set hook can inspect this constructor's live
                // args/locals — only a hooked promote pays materialize.
                let hooked = obj.as_ref().is_some_and(|o| {
                    decl.params
                        .iter()
                        .filter(|p| p.promoted)
                        .any(|p| p.hooks.is_some() || self.hooked_prop(o, &p.name).is_some())
                });
                if hooked {
                    self.vm_materialize(comp, &mut slots);
                    if let Some(obj) = &obj {
                        for p in decl.params.iter().filter(|p| p.promoted) {
                            let value = self.cur().vars[&p.name].borrow().clone();
                            self.store_prop(Value::Object(obj.clone()), &p.name, value)?;
                        }
                    }
                    self.vm_refresh(comp, &mut slots);
                } else if let Some(obj) = &obj {
                    // No observer — publish straight from the bound
                    // slots through the promoted fast path.
                    let dc = self.cur().decl_class.clone();
                    for (i, p) in decl.params.iter().enumerate() {
                        if !p.promoted {
                            continue;
                        }
                        let value = match &slots[i] {
                            Slot::C(c) => c.borrow().clone(),
                            Slot::V(v) => v.clone(),
                            Slot::Arg(a) => self.cur().value_args[*a as usize].clone(),
                            Slot::Uninit => Value::Null,
                        };
                        self.promoted_prop_write(obj, dc.as_ref(), &p.name, value)?;
                    }
                }
            }
            #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
            if let Some(value) = if self.cur().this_obj.is_none() && self.cur().closure_rc.is_none()
            {
                // Tier one accepts globals only; do not pay an outlined native
                // entry on every method/callback that must keep the VM.
                native::execute(self, decl, &slots)
            } else {
                None
            } {
                return Ok(Flow::Return(value));
            }
            self.vm_exec(comp, &mut slots, temps_base)
        })();
        self.loop_depth = saved_depth;
        padd!(4, __p);
        let __p = pnow!();
        // The return-type tail runs while the callee's frame, class
        // context and trace frame are still live — `static` resolves
        // against the callee and the TypeError's backtrace must list
        // it (zend checks between exec and the teardown).
        let r = match r {
            Ok(fl) => match (&fl, comp.ret_fast) {
                (Flow::Return(v), Some(chk)) if chk(v) => Ok(v.clone()),
                _ => self.vm_ret_apply(decl, comp, fl),
            },
            Err(e) => Err(e),
        };
        let popped = self.stack_pop();
        self.last_popped_frame = popped;
        self.last_call_by_ref = decl.by_ref;
        self.trace_pop();
        self.cur_line = saved.line;
        self.send_line = Some(saved.line);
        let keep = match &r {
            Ok(Value::Object(o)) => Some(Rc::as_ptr(o) as usize),
            _ => None,
        };
        let sweep_err = self.sweep_expr_temps_ret(temps_base, keep).err();
        let out = if let Some(mut f) = self.last_popped_frame.take() {
            // Slot-held objects die with the frame too — a call-returned
            // object stored in a local decrefs here like a CV decref.
            let mut dying: Vec<Cell> = Vec::new();
            for sl in &mut slots {
                match sl {
                    Slot::V(v) if matches!(v, Value::Object(_)) => {
                        dying.push(cell(std::mem::replace(v, Value::Null)));
                    }
                    Slot::C(c) if matches!(&*c.borrow(), Value::Object(_)) => {
                        // The shared arg cell's object: if this cell is
                        // the last owner, decref — destruct_cells runs
                        // the shared-cell counts itself.
                        dying.push(c.clone());
                    }
                    _ => {}
                }
            }
            self.last_ret_cell = None;
            let dtor_err = sweep_err
                .map_or_else(|| self.destruct_frame_objs(&f), Err)
                .and_then(|_| self.destruct_cells(&dying))
                .err();
            // The frame's arg vec is dead now (dtor passes ran) —
            // clear+pool it like the slots vec so the next call's arg
            // materialization costs no malloc.
            let mut fa = std::mem::take(&mut f.args);
            f.value_args.clear();
            slots.clear();
            // Pooled frames retain capacity, not PHP owners. Methods and
            // closures also use vm_run, so release their receiver/captures
            // after canonical destructor passes and before recycling cells.
            f.vars.clear();
            f.this_obj = None;
            f.closure_rc = None;
            f.statics_unit = None;
            f.scope_class = None;
            f.called_class = None;
            f.decl_class = None;
            self.vm_recycle_scalar_cells(fa.drain(..));
            if self.vm_cell_pool.len() < 64 {
                self.vm_cell_pool.push(fa);
            }
            if self.vm_slot_pool.len() < 64 {
                self.vm_slot_pool.push(std::mem::take(&mut slots));
            }
            // stack_pop already repaid these spans. Reuse only unaliased
            // site tokens after destructor callbacks and argument cleanup.
            for site in f.vm_sites.drain(..) {
                if self.vm_site_pool.len() < 256 && Rc::strong_count(&site) == 1 {
                    self.vm_site_pool.push(site);
                }
            }
            if self.vm_frame_pool.len() < 64 {
                self.vm_frame_pool.push(f);
            }
            match (r, dtor_err) {
                (Ok(_), Some(e)) => Err(e),
                (r, _) => r,
            }
        } else {
            match (r, sweep_err) {
                (Ok(_), Some(e)) => Err(e),
                (r, _) => r,
            }
        };
        padd!(5, __p);
        self.last_prop_ov = saved.prop_ov;
        self.dim_by_ref = saved.dim_by_ref;
        out
    }

    fn vm_exec(
        &mut self,
        comp: &Compiled,
        slots: &mut [Slot],
        tmark: usize,
    ) -> Result<Flow, PhpError> {
        if comp.loop_body {
            coverage_hit("loop-body-entry");
        } else if comp.top_level {
            coverage_hit("top-level-entry");
        } else {
            coverage_hit("executed-body");
        }
        if comp.hybrid && !comp.top_level && !comp.loop_body {
            coverage_hit("hybrid-body");
        }
        // Value-stack/argv vecs come from a per-Interp pool — a call
        // costs no malloc here (cap bounds retention under deep
        // recursion; each live frame holds its own vec anyway).
        let mut vs = self
            .vm_val_pool
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(16));
        let mut targets = self.vm_target_pool.pop().unwrap_or_default();
        let r = self.vm_exec_ops(comp, slots, tmark, &mut vs, &mut targets);
        targets.clear();
        if self.vm_target_pool.len() < 64 {
            self.vm_target_pool.push(targets);
        }
        vs.clear();
        if self.vm_val_pool.len() < 64 {
            self.vm_val_pool.push(vs);
        }
        r
    }

    /// Shared `Op::Binary`/`Op::BinaryCv` eval: Int×Int scalars bypass
    /// the arith/compare machinery — no type dispatch, no cmp-depth or
    /// notices protocol (scalar-scalar can't trigger either).
    /// Overflow-to-float, div/mod-by-zero and the INT_MIN/-1 edges drop
    /// to the general path (`self.arith`) so their zend errors stay
    /// exact.
    fn vm_binary(&mut self, op: &'static str, lv: Value, rv: Value) -> Result<Value, PhpError> {
        // Int×Int scalars bypass the arith/compare
        // machinery — no type dispatch, no cmp-depth or
        // notices protocol (scalar-scalar can't trigger
        // either). Overflow-to-float, div/mod-by-zero and
        // the INT_MIN/-1 edges drop to the general path
        // (`self.arith`) so their zend errors stay exact.
        let fast = if let (Value::Int(a), Value::Int(b)) = (&lv, &rv) {
            let (a, b) = (*a, *b);
            match op {
                "+" => Some(match a.checked_add(b) {
                    Some(i) => Value::Int(i),
                    None => Value::Float(a as f64 + b as f64),
                }),
                "-" => Some(match a.checked_sub(b) {
                    Some(i) => Value::Int(i),
                    None => Value::Float(a as f64 - b as f64),
                }),
                "*" => Some(match a.checked_mul(b) {
                    Some(i) => Value::Int(i),
                    None => Value::Float(a as f64 * b as f64),
                }),
                "/" if b != 0 && !(a == i64::MIN && b == -1) => Some(if a % b == 0 {
                    Value::Int(a / b)
                } else {
                    Value::Float(a as f64 / b as f64)
                }),
                "%" if b != 0 && b != -1 => Some(Value::Int(a % b)),
                "==" | "===" => Some(Value::Bool(a == b)),
                "!=" | "!==" => Some(Value::Bool(a != b)),
                "<=>" => Some(Value::Int(match a.cmp(&b) {
                    Ordering::Less => -1,
                    Ordering::Equal => 0,
                    Ordering::Greater => 1,
                })),
                "<" => Some(Value::Bool(a < b)),
                "<=" => Some(Value::Bool(a <= b)),
                ">" => Some(Value::Bool(a > b)),
                ">=" => Some(Value::Bool(a >= b)),
                _ => None,
            }
        } else {
            None
        };
        Ok(match fast {
            Some(v) => v,
            None => match op {
                "&&" => Value::Bool(rv.is_truthy()),
                "||" => Value::Bool(rv.is_truthy()),
                "." => {
                    let grow = matches!(&lv, Value::Str(s) if Rc::strong_count(&s.rc) == 1);
                    let nv = self.concat_bytes(&lv, &rv)?;
                    if let Value::Str(s) = &nv {
                        match &lv {
                            Value::Str(os) if grow => {
                                self.mem_grow_str(&os.rc, &s.rc, s.len() as u64 + 25)
                            }
                            _ => self.mem_track(&s.rc, s.len() as u64 + 25),
                        }
                    }
                    nv
                }
                "===" | "!==" | "==" | "!=" | "<=>" | "<" | "<=" | ">" | ">=" => {
                    crate::value::clear_cmp_depth_err();
                    let (a, b) = (&lv, &rv);
                    let v = match op {
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
                        ">" => Value::Bool(compare(b, a) == Ordering::Less),
                        _ => Value::Bool(compare(b, a) != Ordering::Greater),
                    };
                    self.emit_cmp_notices()?;
                    if crate::value::cmp_depth_err() {
                        return self.fail(PhpError::uncaught(
                            "Error",
                            "Nesting level too deep - recursive dependency?",
                            self.cur_line,
                        ));
                    }
                    v
                }
                _ => self.arith(op, lv, rv)?,
            },
        })
    }

    fn vm_cell_value(&mut self, c: &Cell, value: Value) -> Result<Value, PhpError> {
        let ptr = Rc::as_ptr(c) as usize;
        if self.typed_slots.contains_key(&ptr) || self.slot_owners.contains_key(&ptr) {
            self.typed_slot_store_mode(c, value, false)
        } else {
            Ok(value)
        }
    }

    fn vm_publish_global(&mut self, comp: &Compiled, slots: &mut [Slot], index: u16) {
        if comp.top_level {
            if let Slot::V(value) = &mut slots[index as usize] {
                let c = cell(std::mem::replace(value, Value::Null));
                let name = comp.names.iter().find(|(_, i)| **i == index).unwrap().0;
                self.cur().vars.insert(name.clone(), c.clone());
                slots[index as usize] = Slot::C(c);
            }
        }
    }

    fn vm_materialize(&mut self, comp: &Compiled, slots: &mut [Slot]) {
        let frame = self.cur();
        if !frame.value_args.is_empty() {
            frame.args.extend(frame.value_args.drain(..).map(cell));
        }
        for (name, index) in &comp.names {
            let slot = &mut slots[*index as usize];
            // Move the handle out: it must not look like a PHP alias while
            // AST code runs. Existing canonical cells need no reinsertion.
            let c = match std::mem::replace(slot, Slot::Uninit) {
                Slot::Uninit => continue,
                Slot::C(c) => c,
                Slot::Arg(i) => frame.args[i as usize].clone(),
                Slot::V(v) => cell(v),
            };
            if frame
                .vars
                .get(name)
                .is_some_and(|current| Rc::ptr_eq(current, &c))
            {
                continue;
            }
            frame.vars.insert(name.clone(), c);
        }
    }

    fn vm_refresh(&mut self, comp: &Compiled, slots: &mut [Slot]) {
        for (name, index) in &comp.names {
            let c = if comp.top_level {
                // $GLOBALS writes/unsets synchronize lazily on canonical reads.
                self.global_var_cell(name)
            } else {
                self.cur().vars.get(name).cloned()
            };
            slots[*index as usize] = c.map(Slot::C).unwrap_or(Slot::Uninit);
        }
    }

    fn vm_slot_value(
        &mut self,
        comp: &Compiled,
        slots: &[Slot],
        index: u16,
    ) -> Result<Value, PhpError> {
        Ok(match &slots[index as usize] {
            Slot::V(v) => v.clone(),
            Slot::Arg(i) => self.stack.last().unwrap().value_args[*i as usize].clone(),
            Slot::C(c) => c.borrow().clone(),
            Slot::Uninit => {
                let name = comp
                    .names
                    .iter()
                    .find_map(|(name, i)| (*i == index).then_some(name))
                    .unwrap();
                self.warn(&format!("Undefined variable ${name}"))?;
                Value::Null
            }
        })
    }

    fn vm_exec_ops(
        &mut self,
        comp: &Compiled,
        slots: &mut [Slot],
        mut tmark: usize,
        vs: &mut Vec<Value>,
        targets: &mut Vec<CachedFn>,
    ) -> Result<Flow, PhpError> {
        let mut mtargets: Vec<Option<(Rc<std::cell::RefCell<PhpObject>>, MSiteCore)>> = Vec::new();
        let mut ntargets: Vec<NewTarget> = Vec::new();
        let mut pc = 0usize;
        let base_depth = if comp.loop_body { self.loop_depth } else { 0 };
        while pc < comp.ops.len() {
            match &comp.ops[pc] {
                Op::BoolLoop {
                    end, executions, ..
                } => {
                    if *end > pc && vs.is_empty() {
                        if let Some(line) = self.vm_bool_loop(comp, slots, pc + 1, *end)? {
                            executions.set(executions.get().saturating_add(1));
                            self.cur_line = line;
                            self.send_line = None;
                            self.last_ret_cell = None;
                            pc = *end;
                            continue;
                        }
                    }
                }
                Op::Const(v) => vs.push(v.clone()),
                Op::Foreach {
                    arr,
                    key,
                    val,
                    body,
                    depth,
                } => {
                    self.vm_materialize(comp, slots);
                    let previous_depth =
                        std::mem::replace(&mut self.loop_depth, base_depth + depth);
                    let flow = self.exec_foreach_with(arr, key, val, &mut |s| {
                        s.loop_depth += 1;
                        let result = s.vm_scope_exec(body, true);
                        let flow = match result {
                            Ok(flow) => flow,
                            Err(error) => s.err_flow(error),
                        };
                        s.loop_depth -= 1;
                        flow
                    });
                    self.loop_depth = previous_depth;
                    self.vm_refresh(comp, slots);
                    if !matches!(flow, Flow::Normal) {
                        return Ok(flow);
                    }
                }
                Op::ThisProp {
                    name,
                    site,
                    cache,
                    fallback,
                } => {
                    self.cur_line = *site;
                    self.send_line = Some(*site);
                    let receiver = self.stack.last().and_then(|f| f.this_obj.clone());
                    let plain = receiver.and_then(|o| {
                        let frame = self.stack.last().unwrap();
                        let scope = frame
                            .decl_class
                            .as_ref()
                            .or(frame.scope_class.as_ref())
                            .cloned();
                        let can_cache = frame.hook_prop.is_none();
                        if can_cache {
                            let ob = o.borrow();
                            if let Some(hit) = cache.borrow().as_ref() {
                                let same_scope = match (&hit.scope, &scope) {
                                    (None, None) => true,
                                    (Some(a), Some(b)) => Rc::ptr_eq(a, b),
                                    _ => false,
                                };
                                // A public fallback key cannot shadow a private
                                // scope slot that exists in a different instance.
                                let same_resolution =
                                    hit.private_candidate.as_ref().is_none_or(|private| {
                                        private == &hit.key || !ob.prop_has(private)
                                    });
                                if Rc::ptr_eq(&hit.class, &ob.class)
                                    && same_scope
                                    && same_resolution
                                {
                                    let c = match hit.slot {
                                        Some(i) => ob.slots.get(i).and_then(|s| s.clone()),
                                        None => ob.prop_get(&hit.key),
                                    };
                                    if let Some(c) = c {
                                        return Some(c.borrow().clone());
                                    }
                                }
                            }
                        }
                        let (value, key) = self.prop_read_plain_key(&o, name)?;
                        if can_cache {
                            let private_candidate =
                                scope.as_ref().map(|c| format!("\0{}\0{}", c.name(), name));
                            let slot = o.borrow().decl_slot(&key);
                            *cache.borrow_mut() = Some(CachedProp {
                                class: o.borrow().class.clone(),
                                scope,
                                key,
                                slot,
                                private_candidate,
                            });
                        }
                        Some(value)
                    });
                    if let Some(value) = plain {
                        vs.push(value);
                    } else {
                        self.vm_materialize(comp, slots);
                        let result = self.eval(fallback);
                        self.vm_refresh(comp, slots);
                        vs.push(result?);
                    }
                }
                Op::Canonical(expr) => {
                    self.vm_materialize(comp, slots);
                    let result = self.eval(expr);
                    self.vm_refresh(comp, slots);
                    vs.push(result?);
                }
                Op::DimCv {
                    array,
                    key_slot,
                    key_int,
                    write,
                    fallback,
                } => {
                    // ponytail: fresh current-table lookup, no offset cache. Global
                    // views, non-scalars and missing/uninitialized keys stay AST;
                    // generation-guarded offsets need a measured storage change.
                    let protected = |c: &Cell| {
                        let ptr = Rc::as_ptr(c) as usize;
                        self.typed_slots.contains_key(&ptr)
                            || self.slot_owners.contains_key(&ptr)
                            || self.slot_merged.contains_key(&ptr)
                            || self.readonly_cells.contains_key(&ptr)
                    };
                    let plain = if (comp.top_level || write.is_some()) && self.globals_arr.is_some()
                        || write.is_some()
                            && (self.in_handler
                                || self.dim_throw.is_some()
                                || matches!(&slots[*array as usize], Slot::C(c) if protected(c)))
                    {
                        None
                    } else {
                        let peek = |index: u16| match &slots[index as usize] {
                            Slot::V(v) => Some(v.clone()),
                            Slot::C(c) => Some(c.borrow().clone()),
                            _ => None,
                        };
                        let key = match key_slot {
                            Some(index) => match peek(*index) {
                                Some(Value::Int(i)) => Some(i),
                                _ => None,
                            },
                            None => Some(*key_int),
                        };
                        match (peek(*array), key) {
                            (Some(Value::Array(a)), Some(k)) => {
                                if let Some(value) = write {
                                    // Only the CV and this peek own the table. Shared tables
                                    // keep canonical CoW; existing buckets do not change
                                    // array layout, iterator positions or memory charges.
                                    if Rc::strong_count(&a) == 2
                                        && a.borrow_mut().entries.store_packed_scalar(k, value)
                                    {
                                        vs.push(value.clone());
                                        pc += 1;
                                        continue;
                                    }
                                    let c = if Rc::strong_count(&a) == 2 {
                                        a.borrow().get_cell(&ArrKey::Int(k))
                                    } else {
                                        None
                                    };
                                    if let Some(c) = c.filter(|c| {
                                        !protected(c)
                                            && matches!(
                                                &*c.borrow(),
                                                Value::Null
                                                    | Value::Bool(_)
                                                    | Value::Int(_)
                                                    | Value::Float(_)
                                                    | Value::Str(_)
                                            )
                                    }) {
                                        self.cell_store(&c, value.clone())?;
                                        Some(value.clone())
                                    } else {
                                        None
                                    }
                                } else {
                                    a.borrow().get(&ArrKey::Int(k)).filter(|v| {
                                        matches!(
                                            v,
                                            Value::Null
                                                | Value::Bool(_)
                                                | Value::Int(_)
                                                | Value::Float(_)
                                                | Value::Str(_)
                                        )
                                    })
                                }
                            }
                            _ => None,
                        }
                    };
                    if let Some(value) = plain {
                        vs.push(value);
                    } else {
                        self.vm_materialize(comp, slots);
                        let result = self.eval(fallback);
                        self.vm_refresh(comp, slots);
                        vs.push(result?);
                    }
                }
                Op::CanonicalStmt {
                    stmt,
                    decl_site,
                    depth,
                    normal,
                    breaks,
                    continues,
                } => {
                    self.vm_materialize(comp, slots);
                    let previous_depth = std::mem::replace(&mut self.loop_depth, *depth);
                    let flow = self.exec_at(stmt, *decl_site);
                    self.loop_depth = previous_depth;
                    self.vm_refresh(comp, slots);
                    pc = match flow {
                        Flow::Normal => *normal,
                        Flow::Break(level) => match breaks.get(level.saturating_sub(1) as usize) {
                            Some(target) => *target,
                            None => return Ok(Flow::Break(level)),
                        },
                        Flow::Continue(level) => {
                            match continues.get(level.saturating_sub(1) as usize) {
                                Some(target) => *target,
                                None => return Ok(Flow::Continue(level)),
                            }
                        }
                        other => return Ok(other),
                    };
                    continue;
                }
                Op::Load(s) => vs.push(self.vm_slot_value(comp, slots, *s)?),
                Op::Store(s) => {
                    match &mut slots[*s as usize] {
                        // CV write-through: zend's arg slot IS the param,
                        // so the write lands in the frame's arg vector and
                        // func_get_args/debug_backtrace report it.
                        Slot::Arg(i) => {
                            let old = std::mem::replace(
                                &mut self.cur().value_args[*i as usize],
                                vs.last().unwrap().clone(),
                            );
                            self.destruct_dying_value(&old)?;
                        }
                        Slot::Uninit => slots[*s as usize] = Slot::V(vs.last().unwrap().clone()),
                        Slot::V(v) => {
                            let old = std::mem::replace(v, vs.last().unwrap().clone());
                            self.destruct_dying_value(&old)?;
                        }
                        Slot::C(c) => {
                            // CV write: new zval lands, displaced decrefs —
                            // same ordering as cell_store.
                            let c = c.clone();
                            let value = self.vm_cell_value(&c, vs.last().unwrap().clone())?;
                            *vs.last_mut().unwrap() = value.clone();
                            self.cell_store(&c, value)?;
                        }
                    }
                    self.vm_publish_global(comp, slots, *s);
                }
                Op::Pop => {
                    // Statement boundary like exec's Stmt::Expr entry:
                    // the ret-cell pin from the just-run call is stale
                    // here — drop it or its object skips the teardown
                    // decref (hold2's ED ordering).
                    self.last_ret_cell = None;
                    if let Some(v) = vs.pop() {
                        if matches!(v, Value::Object(_) | Value::Callable(_)) {
                            // Discarded object temp: same decref pass
                            // exec's statement end runs.
                            self.destruct_cells(&[cell(v)])?;
                        }
                    }
                }
                Op::Binary(op) => {
                    let rv = vs.pop().unwrap();
                    let lv = vs.pop().unwrap();
                    let v = self.vm_binary(op, lv, rv)?;
                    vs.push(v);
                }
                Op::BinaryCv(op, sl) => {
                    let rv = vs.pop().unwrap();
                    // The left CV binds here — zend fetches it at the
                    // binary op, after the rhs ran.
                    let lv = self.vm_slot_value(comp, slots, *sl)?;
                    let v = self.vm_binary(op, lv, rv)?;
                    vs.push(v);
                }
                Op::AppendCv(sl) => {
                    let rhs = vs.pop().unwrap();
                    let fast = match (&rhs, &mut slots[*sl as usize]) {
                        (Value::Str(rhs), Slot::V(Value::Str(s)))
                            if Rc::strong_count(&s.rc) == 1 =>
                        {
                            Some(self.append_unique_string(s, rhs))
                        }
                        (_, Slot::C(c)) => self.append_string_cell(c, &rhs),
                        _ => None,
                    };
                    let value = match fast {
                        Some(v) => v,
                        None => {
                            let lv = self.vm_slot_value(comp, slots, *sl)?;
                            self.vm_binary(".", lv, rhs)?
                        }
                    };
                    vs.push(value);
                }
                Op::BinaryCvConst(op, sl, rhs) => {
                    let lv = self.vm_slot_value(comp, slots, *sl)?;
                    vs.push(self.vm_binary(op, lv, rhs.clone())?);
                }
                Op::Boolify => {
                    let v = vs.pop().unwrap();
                    vs.push(Value::Bool(v.is_truthy()));
                }
                Op::Not => {
                    let v = vs.pop().unwrap();
                    vs.push(Value::Bool(!v.is_truthy()));
                }
                Op::Jump(t) => {
                    self.vm_backedge(*t, pc)?;
                    pc = *t;
                    continue;
                }
                Op::JumpIfFalse(t) => {
                    if !vs.pop().unwrap().is_truthy() {
                        self.vm_backedge(*t, pc)?;
                        pc = *t;
                        continue;
                    }
                }
                Op::JumpIfTrue(t) => {
                    if vs.pop().unwrap().is_truthy() {
                        self.vm_backedge(*t, pc)?;
                        pc = *t;
                        continue;
                    }
                }
                Op::Sweep => {
                    self.sweep_expr_temps(tmark)?;
                    tmark = self.expr_temps.len();
                }
                Op::IncDec { slot, delta, post } => {
                    let old = self.vm_slot_value(comp, slots, *slot)?;
                    let new = self.incdec_value(&old, *delta)?;
                    match &mut slots[*slot as usize] {
                        Slot::Arg(i) => {
                            self.cur().value_args[*i as usize] = new;
                        }
                        Slot::Uninit => slots[*slot as usize] = Slot::V(new),
                        Slot::V(v) => *v = new,
                        Slot::C(c) => {
                            let c = c.clone();
                            let value = self.vm_cell_value(&c, new)?;
                            self.cell_store(&c, value)?;
                        }
                    }
                    self.vm_publish_global(comp, slots, *slot);
                    vs.push(if *post {
                        old
                    } else {
                        self.vm_slot_value(comp, slots, *slot)?
                    });
                }
                Op::Echo => {
                    let v = vs.pop().unwrap();
                    let s = self.conv_bytes(&v)?;
                    self.emit_bytes(&s);
                }
                Op::Line(l) => {
                    if comp.hybrid || comp.top_level {
                        if let Some(flow) = self.statement_boundary() {
                            return Ok(flow);
                        }
                    }
                    self.cur_line = *l;
                    // Statement boundary like exec's Stmt::Line — the
                    // caller's pending send_line dies with the stmt.
                    self.send_line = None;
                }
                Op::Return => return Ok(Flow::Return(vs.pop().unwrap_or(Value::Null))),
                Op::InitCall { call: at, args } => {
                    let Op::Call {
                        lname,
                        raw,
                        callee,
                        site,
                        cache,
                        ..
                    } = &comp.ops[*at]
                    else {
                        unreachable!("InitCall always points to its Call");
                    };
                    let target = self.vm_resolve(lname, raw, *callee, cache)?;
                    let needs_refs = match &target {
                        CachedFn::Decl(d) => d.params.iter().any(|p| p.by_ref),
                        CachedFn::Builtin => {
                            builtin_byref(lname).is_some_and(|flags| flags.iter().any(|flag| *flag))
                        }
                        CachedFn::Direct(..) => false,
                    };
                    if needs_refs {
                        // Cold reference calls use the canonical SEND machinery.
                        // Cells shared with slots let argument expressions update
                        // caller locals and keep escaping references alive.
                        self.vm_materialize(comp, slots);
                        let result = self.vm_ref_call(&target, lname, raw, args, *site);
                        self.vm_refresh(comp, slots);
                        let value = result?;
                        vs.push(value);
                        pc = *at + 1; // skip compiled argument ops and Call
                        continue;
                    }
                    targets.push(target);
                }
                Op::InitMethod { call: at, obj } => {
                    let Op::MethodCall {
                        name,
                        site,
                        cache,
                        fallback,
                        ..
                    } = &comp.ops[*at]
                    else {
                        unreachable!("InitMethod always points to its MethodCall");
                    };
                    let mval = match &slots[*obj as usize] {
                        Slot::V(v) => Some(v.clone()),
                        Slot::C(c) => Some(c.borrow().clone()),
                        Slot::Arg(i) => self
                            .stack
                            .last()
                            .and_then(|f| f.value_args.get(*i as usize).cloned()),
                        Slot::Uninit => None,
                    };
                    let hit = match mval {
                        Some(Value::Object(o)) => self.vm_method_target(o, name, cache, *site),
                        _ => None,
                    };
                    if let Some(hit) = hit {
                        mtargets.push(Some(hit));
                    } else {
                        // Receiver or target outside the direct subset —
                        // canonical eval keeps the full semantics (undef
                        // CV warning, member-on-scalar error, __call).
                        self.vm_materialize(comp, slots);
                        let r = self.eval(fallback);
                        self.vm_refresh(comp, slots);
                        vs.push(r?);
                        pc = *at + 1;
                        continue;
                    }
                }
                Op::InitNew {
                    call: at,
                    name,
                    fallback,
                } => {
                    let target = crate::interp::util::lkey_get(&self.classes, name)
                        .cloned()
                        .and_then(|cls| {
                            let ctor = self.find_method_in(&cls, "__construct");
                            if ctor
                                .as_ref()
                                .is_some_and(|(m, _)| m.decl.params.iter().any(|p| p.by_ref))
                            {
                                return None;
                            }
                            Some((cls, ctor))
                        });
                    match target {
                        Some(target) => ntargets.push(target),
                        None => {
                            // Autoload, missing classes and by-ref ctor arguments
                            // retain the canonical binder and caller Cells.
                            self.vm_materialize(comp, slots);
                            let r = self.eval(fallback);
                            self.vm_refresh(comp, slots);
                            vs.push(r?);
                            pc = *at + 1;
                            continue;
                        }
                    }
                }
                Op::NewCall {
                    lname,
                    argc,
                    site,
                    cache,
                } => {
                    let n = *argc as usize;
                    self.send_line = Some(*site);
                    let (cls, ctor) = ntargets.pop().expect("InitNew resolved this new");
                    let mut argv = self.vm_val_pool.pop().unwrap_or_default();
                    argv.extend(vs.drain(vs.len() - n..));
                    if comp.top_level {
                        self.vm_materialize(comp, slots);
                    }
                    let result = self.vm_new_run(&cls, lname, ctor, &mut argv, cache, *site);
                    if comp.top_level {
                        self.vm_refresh(comp, slots);
                    }
                    let v = result?;
                    argv.clear();
                    if self.vm_val_pool.len() < 64 {
                        self.vm_val_pool.push(argv);
                    }
                    vs.push(v);
                }
                Op::MethodCall {
                    name, argc, site, ..
                } => {
                    let n = *argc as usize;
                    let hit = mtargets.pop().expect("InitMethod resolved this call");
                    let Some((obj, core)) = hit else {
                        unreachable!("canonical targets jump over MethodCall");
                    };
                    let mut argv = self.vm_val_pool.pop().unwrap_or_default();
                    argv.extend(vs.drain(vs.len() - n..));
                    if comp.top_level {
                        if let Some(value) = self.vm_float_method(&core, &argv, &obj) {
                            argv.clear();
                            if self.vm_val_pool.len() < 64 {
                                self.vm_val_pool.push(argv);
                            }
                            vs.push(value);
                            pc += 1;
                            continue;
                        }
                    }
                    // A direct callee can rebind globals or release their
                    // last PHP owner. Drop stale caller slot handles before
                    // dispatch and reload the canonical bindings afterward.
                    if comp.top_level {
                        self.vm_materialize(comp, slots);
                    }
                    let result = self.vm_run_method_direct(&core, &mut argv, obj, name, *site);
                    if comp.top_level {
                        self.vm_refresh(comp, slots);
                    }
                    let v = result?;
                    argv.clear();
                    if self.vm_val_pool.len() < 64 {
                        self.vm_val_pool.push(argv);
                    }
                    vs.push(v);
                }
                Op::Call {
                    lname,
                    raw,
                    argc,
                    site,
                    ..
                } => {
                    let n = *argc as usize;
                    let start = vs.len() - n;
                    // Direct hits bypass invoke_fn — arg values become
                    // the frame's cells inside vm_run_direct. send_line
                    // still pins this call site: call_site() reads it
                    // for the callee's trace-frame line (review
                    // finding: skipped it attributed callee traces to
                    // the previous call's site).
                    let target = targets.pop().expect("InitCall resolved this call");
                    // A proven scalar kernel cannot rebind globals or call PHP.
                    // ponytail: exposed $GLOBALS views retain the canonical
                    // bridge; finer binding epochs need separate evidence.
                    if comp.top_level
                        && self.globals_arr.is_none()
                        && matches!(target, CachedFn::Builtin)
                    {
                        self.send_line = Some(*site);
                        if let Some(value) = self.vm_scalar_builtin(lname, &vs[start..]) {
                            vs.truncate(start);
                            vs.push(value);
                            pc += 1;
                            continue;
                        }
                    }
                    let mut argv = self.vm_val_pool.pop().unwrap_or_default();
                    argv.extend(vs.drain(start..));
                    if comp.top_level {
                        // Callees may unset/rebind globals or run GC. Slot
                        // handles must not pin old bindings during the call.
                        self.vm_materialize(comp, slots);
                    }
                    let result = if let CachedFn::Direct(d, c) = &target {
                        self.send_line = Some(*site);
                        self.vm_run_direct(d, c, &mut argv, None)
                    } else {
                        self.vm_call(lname, raw, &mut argv, *site, &target)
                    };
                    if comp.top_level {
                        self.vm_refresh(comp, slots);
                    }
                    let v = result?;
                    argv.clear();
                    if self.vm_val_pool.len() < 64 {
                        self.vm_val_pool.push(argv);
                    }
                    vs.push(v);
                }
            }
            pc += 1;
        }
        Ok(Flow::Normal)
    }

    /// Body exec under bind_and_run's shell: binds already sit in the
    /// frame's vars — slots alias those cells by name (by-ref params
    /// write through them like a zend CV), non-param locals stay
    /// Slot::V. Caller converts Err through err_flow, exactly like
    /// exec_block's stmt-level failures.
    pub(in crate::interp) fn vm_bound_exec(&mut self, comp: &Compiled) -> Result<Flow, PhpError> {
        self.vm_scope_exec(comp, comp.top_level)
    }

    fn vm_scope_exec(&mut self, comp: &Compiled, keep_vars: bool) -> Result<Flow, PhpError> {
        let mut slots = self
            .vm_slot_pool
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(comp.nslots));
        slots.resize_with(comp.nslots, || Slot::Uninit);
        for (name, idx) in &comp.names {
            if let Some(c) = self.cur().vars.get(name) {
                slots[*idx as usize] = Slot::C(c.clone());
            }
        }
        let r = self.vm_exec(comp, &mut slots, self.expr_temps.len());
        // Slot-V objects die with the frame like a CV decref (Slot::C
        // cells are f.vars — bind's own frame teardown owns those).
        let derr = if keep_vars {
            self.vm_materialize(comp, &mut slots);
            None // Globals live until canonical request shutdown.
        } else {
            let mut dying = Vec::new();
            for sl in &mut slots {
                if let Slot::V(v) = sl {
                    if matches!(v, Value::Object(_)) {
                        dying.push(cell(std::mem::replace(v, Value::Null)));
                    }
                }
            }
            self.destruct_cells(&dying).err()
        };
        slots.clear();
        if self.vm_slot_pool.len() < 64 {
            self.vm_slot_pool.push(slots);
        }
        match (r, derr) {
            (Ok(_), Some(e)) => Err(e),
            (r, _) => r,
        }
    }

    /// zend param-type gate (ok-path only): implicit_null, strict
    /// ty_exact, weak param_type_match. On a miss the canonical
    /// TypeError lives in bind_and_run — this returns `Some(err)`
    /// only when a `callable` probe raised (autoloader threw: zend
    /// propagates THAT exception, not a TypeError); the caller then
    /// fails with it instead of rebinding. `Some(None)` = rebind.
    fn vm_param_gate(
        &mut self,
        _decl: &FunctionDecl,
        p: &Param,
        ty: &[String],
        v: &Value,
    ) -> Option<Option<PhpError>> {
        let implicit_null = !ty.iter().any(|m| m.eq_ignore_ascii_case("null"))
            && match &p.default {
                Some(Expr::Null) => true,
                Some(Expr::Const(c)) => c.eq_ignore_ascii_case("null"),
                _ => false,
            };
        let ok = (implicit_null && matches!(v, Value::Null))
            || if self.caller_file_strict() {
                self.ty_exact(ty, v)
            } else {
                ty.iter().any(|m| self.param_type_match(m, v))
            };
        if ok {
            return None;
        }
        if ty.iter().any(|m| m.eq_ignore_ascii_case("callable")) {
            if let Some(e) = self.take_callable_probe_err() {
                return Some(Some(e));
            }
        }
        Some(None)
    }

    /// Cold-path rollback: undo this call's trace push and hand the
    /// arg cells to bind_and_run so the canonical TypeError (arg
    /// trace, "and defined" display) builds in exactly one place.
    /// The callee frame STAYS pushed — bind_and_run_inner expects it
    /// at stack top. Only reachable before any body op ran — side
    /// effects can't double. `Some(e)` fails directly (a thrown
    /// autoloader probe propagates, it doesn't rebind).
    fn vm_rebind(
        &mut self,
        probe: Option<PhpError>,
        decl: &Rc<FunctionDecl>,
        mut args: super::CallArgs,
        fa: &mut Vec<Cell>,
        saved: VmSaved,
    ) -> Result<Value, PhpError> {
        self.trace_pop();
        self.cur_line = saved.line;
        self.send_line = Some(saved.line);
        self.last_prop_ov = saved.prop_ov;
        self.dim_by_ref = saved.dim_by_ref;
        if let Some(e) = probe {
            self.stack_discard();
            return self.fail(e);
        }
        args.cells = std::mem::take(fa);
        self.bind_and_run(decl, args, Vec::new())
    }

    /// Flow→value for a compiled body — bind_free maps directly; the
    /// bound kinds run bind_and_run_inner's Flow::Return/Normal
    /// return-type arms verbatim (weak coerce + deprecate, strict
    /// ty_exact, `must be of type`/`none returned` TypeErrors).
    /// ponytail: the `__tostring` implicit-contract arm isn't copied —
    /// a free function can't carry the magic-method name into a
    /// literal call site.
    fn vm_ret_apply(
        &mut self,
        decl: &Rc<FunctionDecl>,
        comp: &Compiled,
        flow: Flow,
    ) -> Result<Value, PhpError> {
        if comp.bind_free {
            return Ok(match flow {
                Flow::Return(v) => v,
                _ => Value::Null,
            });
        }
        let ret_fname = self.decl_fname(decl);
        let resolved_ret = decl.ret.as_ref().map(|ty| self.resolve_static(ty));
        match flow {
            Flow::Return(v) => {
                if let Some(ty) = &resolved_ret {
                    let ret_strict = self.strict_files.contains(decl.file.as_ref());
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
                    Ok(v)
                }
            }
            Flow::Normal => {
                // Falling off a typed fn still checks: `none returned`
                // TypeError for real types, `must not implicitly
                // return` for `never` (typed_return*_without_value).
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
                        let msg = format!(
                            "{}(): Return value must be of type {}, none returned",
                            ret_fname,
                            disp_v.join("|")
                        );
                        let mut e = PhpError::uncaught("TypeError", msg, self.cur_line);
                        e.thrown_line = Some(decl.end_line);
                        return self.fail(e);
                    }
                }
                Ok(Value::Null)
            }
            _ => Ok(Value::Null),
        }
    }

    /// Deadline check on a loop back-edge — the AST checks per
    /// statement; a compiled `while`/`for`/`do` loop only re-enters
    /// exec ops via jumps, so backward edges are the polling point
    /// (045's `while (true)` shutdown fn).
    fn vm_backedge(&mut self, target: usize, pc: usize) -> Result<(), PhpError> {
        if target < pc {
            if let Some(d) = self.deadline {
                if std::time::Instant::now() > d {
                    let secs = self.deadline_secs;
                    let fl = self.err_flow(PhpError::fatal(
                        format!(
                            "Maximum execution time of {} second{} exceeded",
                            secs,
                            if secs == 1 { "" } else { "s" }
                        ),
                        self.cur_line,
                    ));
                    if let super::Flow::Exit(c) = fl {
                        return Err(PhpError {
                            trace: None,
                            thrown_line: None,
                            display_msg: None,
                            kind: crate::error::ErrorKind::Fatal,
                            message: format!("\u{1}exit:{}", c),
                            line: 0,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// ponytail: dev-only phase profiler — PHPUN_CALLPROF=1 accumulates
    /// ns per call phase; printed at process exit (registered once).
    /// Clock reads skew timings; exec includes nested calls. Report totals,
    /// then normalize by calls; benchmark speed with profiling disabled.
    pub(super) fn callprof_on() -> bool {
        use std::sync::atomic::{AtomicBool, Ordering};
        static ON: AtomicBool = AtomicBool::new(false);
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            ON.store(std::env::var_os("PHPUN_CALLPROF").is_some(), Ordering::Relaxed);
            if !ON.load(Ordering::Relaxed) {
                return;
            }
            extern "C" fn dump() {
                use std::sync::atomic::Ordering;
                eprintln!(
                    "callprof: pre={}ns cells={}ns site={}ns bind={}ns exec={}ns post={}ns calls={}; unit=total_ns exec=inclusive",
                    PROF[0].load(Ordering::Relaxed),
                    PROF[1].load(Ordering::Relaxed),
                    PROF[2].load(Ordering::Relaxed),
                    PROF[3].load(Ordering::Relaxed),
                    PROF[4].load(Ordering::Relaxed),
                    PROF[5].load(Ordering::Relaxed),
                    PROF[6].load(Ordering::Relaxed),
                );
            }
            unsafe { libc::atexit(dump) };
        });
        ON.load(Ordering::Relaxed)
    }

    /// Compiled-callee call: skips `invoke_fn`'s preamble (the yield
    /// body-walk, SPL stubs, decl-class pendings) — a compiled decl
    /// can contain no `yield`, `Stmt`/`Expr` coverage is fixed, and a
    /// literal `f()` or proven context-free closure carries no class context.
    /// The frame still
    /// materializes exactly like invoke_fn_run's fills (args cells for
    /// func_get_args + the dtor pass, vm_sites arena charge, trace).
    /// Arity failures delegate to invoke_fn for the zend error path.
    fn vm_run_direct(
        &mut self,
        decl: &Rc<FunctionDecl>,
        comp: &Compiled,
        argv: &mut Vec<Value>,
        closure: Option<Rc<PhpCallable>>,
    ) -> Result<Value, PhpError> {
        let __p = pnow!();
        let required = decl
            .params
            .iter()
            .rposition(|p| p.default.is_none() && !p.variadic)
            .map(|i| i + 1)
            .unwrap_or(0);
        if argv.len() < required {
            let mut args = super::CallArgs::empty();
            args.cells = argv.drain(..).map(cell).collect();
            return self.invoke_fn(decl, args, None, None);
        }
        let decl_site = self.pending_decl_site.take();
        let _ = self.pending_decl_class.take();
        let _ = self.pending_called_class.take();
        // Pooled frame: every field vm_run/exec touches is reset here
        // — vars/args/vm_sites come back cleared, class/closure state
        // is absent for the proven direct-call/closure subset. A closure keeps
        // its own callable metadata until the normal frame teardown.
        let mut frame = self
            .vm_frame_pool
            .pop()
            .unwrap_or_else(|| Box::new(super::Frame::new(String::new())));
        frame.fn_name = decl.name.clone();
        frame.decl_site = decl_site.unwrap_or(Rc::as_ptr(decl) as usize);
        frame.fn_line = decl.line;
        frame.file = if decl.file.is_empty() {
            self.cur_file.clone()
        } else {
            decl.file.clone()
        };
        frame.ns = decl.ns.clone();
        frame.trait_origin = decl.decl_in.clone();
        frame.hook_prop = self.pending_hook_prop.take();
        frame.gen_body = self.pending_gen_body;
        self.pending_gen_body = false;
        frame.this_obj = None;
        frame.scope_class = None;
        frame.called_class = None;
        frame.decl_class = None;
        frame.ret_by_ref = false;
        frame.closure_rc = closure;
        frame.call_alias = None;
        frame.statics_unit = None;
        frame.vars.clear();
        frame.args.clear();
        frame.value_args.clear();
        frame.vm_sites.clear();
        frame.vm_span = None;
        let pending_caps = std::mem::take(&mut self.pending_gen_captures);
        for (n, c, by_ref) in pending_caps {
            let c2 = if by_ref { c } else { cell(c.borrow().clone()) };
            frame.vars.insert(n, c2);
        }
        self.stack.push(frame);
        padd!(0, __p);
        let __p = pnow!();
        let mut args = super::CallArgs::empty();
        // Inline (value) binding: params stay owned Values in the frame's
        // arg vector until a canonical path promotes them. Bound params
        // need either a provided arg or a const default; a typed variadic
        // and any failed check keep the canonical binder's machinery
        // (per-extra gates, TypeErrors).
        let mut abi = comp.value_abi
            && !argv.is_empty()
            && argv.len() <= u16::MAX as usize
            && decl
                .params
                .iter()
                .enumerate()
                .all(|(i, p)| p.variadic || i < argv.len() || comp.defaults[i].is_some());
        if abi {
            'abi: for (i, p) in decl.params.iter().enumerate() {
                if p.variadic {
                    abi = p.ty.is_none();
                    break;
                }
                let (Some(ty), Some(v)) = (&p.ty, argv.get(i)) else {
                    continue;
                };
                if comp.param_fast[i].is_some_and(|g| g(v)) {
                    continue;
                }
                let v = v.clone();
                match self.vm_abi_gate_slow(decl, p, ty, &v, argv, i) {
                    Ok(()) => {}
                    Err(Some(e)) => {
                        self.stack_pop();
                        return self.fail(e);
                    }
                    Err(None) => {
                        abi = false;
                        break 'abi;
                    }
                }
            }
        }
        if abi {
            let n = argv.len() as u64;
            std::mem::swap(&mut self.cur().value_args, argv);
            self.vm_direct_reserve(&mut args, n);
            padd!(1, __p);
            return self.vm_run(decl, comp, args);
        }
        args.cells = self.vm_arg_cells(argv);
        let n = args.cells.len() as u64;
        self.vm_direct_reserve(&mut args, n);
        padd!(1, __p);
        self.vm_run(decl, comp, args)
    }

    /// Fresh positional callback values only: original/reference argument cells
    /// must use call_value. Pool the value vector before invoking the shared ABI.
    pub(crate) fn call_values<const N: usize>(
        &mut self,
        callable: &Value,
        values: [Value; N],
        nonref: bool,
    ) -> Result<Value, PhpError> {
        let Some((decl, comp, closure)) = self.vm_callback_values_target(callable, &values) else {
            // Fallbacks go straight to their original fresh cells. Do not add a
            // value-vector transfer for bound/object/string/captured callbacks.
            let mut args = super::CallArgs::positional(values.into_iter().map(cell).collect());
            if nonref {
                args.nonref_cells.extend(0..N);
            }
            return self.call_value(callable, args);
        };
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        if let Some(value) = native::callback(self, decl, &values) {
            return Ok(value);
        }
        let mut argv = self.vm_val_pool.pop().unwrap_or_default();
        argv.extend(values);
        let result = self.vm_run_direct(decl, &comp, &mut argv, Some(closure.clone()));
        argv.clear();
        if self.vm_val_pool.len() < 64 {
            self.vm_val_pool.push(argv);
        }
        result
    }

    fn vm_callback_values_target<'cb>(
        &mut self,
        callable: &'cb Value,
        values: &[Value],
    ) -> Option<(&'cb Rc<FunctionDecl>, Rc<Compiled>, &'cb Rc<PhpCallable>)> {
        let Value::Callable(c) = callable else {
            return None;
        };
        let CallableKind::Closure(decl) = &c.kind else {
            return None;
        };
        // ponytail: context-free fixed exact primitive closure calls only.
        // Captures/receivers/class scope, coercion/defaults and refs retain the
        // canonical callback binder; no callable resolution is cached here.
        if !c.captures.is_empty()
            || c.this_obj.is_some()
            || c.scope_class.is_some()
            || c.called_class.is_some()
            || decl.by_ref
            || values.len() != decl.params.len()
            || decl
                .params
                .iter()
                .any(|p| p.by_ref || p.variadic || p.promoted)
            || !values.iter().all(|v| {
                matches!(
                    v,
                    Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)
                )
            })
            || self.pending_call_alias.is_some()
            || self.pending_decl_class.is_some()
            || self.pending_called_class.is_some()
            || self.pending_decl_site.is_some()
            || self.pending_hook_prop.is_some()
            || self.pending_gen_body
            || !self.pending_gen_captures.is_empty()
        {
            return None;
        }
        let comp = self.vm_compiled(decl)?;
        if !comp.value_abi
            || !decl.params.iter().enumerate().all(|(i, p)| {
                p.ty.is_none() || comp.param_fast[i].is_some_and(|gate| gate(&values[i]))
            })
        {
            return None;
        }
        Some((decl, comp, c))
    }

    /// The typed check a fast `param_fast` gate could not settle for a
    /// value-ABI candidate: probe-ok params get the prelude's widening/
    /// coercion written into argv (so func_get_args reports the bound
    /// value); anything else falls back to the canonical binder for its
    /// TypeError machinery. Err(None) = rebind, Err(Some) = propagate.
    #[cold]
    fn vm_abi_gate_slow(
        &mut self,
        decl: &FunctionDecl,
        p: &Param,
        ty: &[String],
        v: &Value,
        argv: &mut [Value],
        i: usize,
    ) -> Result<(), Option<PhpError>> {
        self.callable_probe_err = None;
        match self.vm_param_gate(decl, p, ty, v) {
            // A swallowed probe's exception propagates verbatim.
            Some(Some(e)) => return Err(Some(e)),
            // Any other miss needs the canonical TypeError.
            Some(None) => return Err(None),
            None => {}
        }
        let v = v.clone();
        let strict = self.caller_file_strict();
        if !strict && !self.ty_weak_exact(ty, &v) {
            if let Some(cv) = self.coerce_scalar(ty, &v) {
                // Arg-coercion deprecations attribute to the callee's
                // decl line (scalar_basic).
                let pl = self.cur_line;
                self.cur_line = decl.line;
                self.deprecate_lossy_int(ty, &v, &cv);
                self.cur_line = pl;
                argv[i] = cv;
            }
        }
        if strict
            && ty.iter().any(|m| m.eq_ignore_ascii_case("float"))
            && !self.ty_weak_exact(ty, &v)
        {
            if let Value::Int(n) = v {
                argv[i] = Value::Float(n as f64);
            }
        }
        Ok(())
    }

    /// Cells for args taking the canonical bind path — scalar cells come
    /// from the recycle pool like the value-ABI teardown fills it.
    pub(in crate::interp) fn vm_scalar_arg(&mut self, value: Value) -> Cell {
        if matches!(
            value,
            Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)
        ) {
            if let Some(c) = self.vm_scalar_cell_pool.pop() {
                *c.borrow_mut() = value;
                return c;
            }
        }
        cell(value)
    }

    // ponytail: retain at most256 scalar cells; raise this existing pool cap
    // only if allocation profiles show larger live argument sets churn it.
    pub(in crate::interp) fn vm_recycle_scalar_cells(&mut self, cells: impl Iterator<Item = Cell>) {
        for c in cells {
            // Same existing VM pool: owners, references, weak anchors and
            // trace/capture snapshots keep their identities outside it.
            if self.vm_scalar_cell_pool.len() < 256
                && Rc::strong_count(&c) == 1
                && Rc::weak_count(&c) == 0
                && matches!(
                    &*c.borrow(),
                    Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)
                )
            {
                *c.borrow_mut() = Value::Null;
                self.vm_scalar_cell_pool.push(c);
            }
        }
    }

    /// Cells for args taking the canonical bind path, using the same scalar pool.
    fn vm_arg_cells(&mut self, argv: &mut Vec<Value>) -> Vec<Cell> {
        let mut cells = self.vm_cell_pool.pop().unwrap_or_default();
        cells.extend(argv.drain(..).map(|value| self.vm_scalar_arg(value)));
        cells
    }

    /// Resolve and cache at INIT, before args can declare overrides.
    fn vm_resolve(
        &mut self,
        lname: &str,
        raw: &str,
        callee: usize,
        cache: &std::cell::RefCell<Option<CachedFn>>,
    ) -> Result<CachedFn, PhpError> {
        self.send_line = Some(callee);
        if lname == "__halt_compiler" {
            return Err(PhpError {
                trace: None,
                thrown_line: None,
                display_msg: None,
                kind: crate::error::ErrorKind::Fatal,
                message: "\u{1}exit:0".to_string(),
                line: 0,
            });
        }
        if let Some(hit) = cache.borrow().clone() {
            return Ok(hit);
        }
        let (decl, _) = self.resolve_user_fn(lname, true);
        let miss_name = if decl.is_none() && !self.caller_ns().is_empty() {
            Some(format!("{}\\{}", self.caller_ns(), raw))
        } else {
            None
        };
        // INIT_FCALL fails before argument evaluation can have effects.
        if decl.is_none()
            && !builtins::is_builtin(lname)
            && builtins::builtin_params(lname).is_none()
            && builtin_byref(lname).is_none()
        {
            self.send_line = Some(callee);
            return self.fail(PhpError::uncaught(
                "Error",
                format!(
                    "Call to undefined function {}()",
                    miss_name.as_deref().unwrap_or(raw)
                ),
                0,
            ));
        }
        // Zend's literal call-site cache retains the first successful
        // resolution, including a global/builtin namespace fallback.
        // An override declared by an argument affects other unresolved
        // sites, but not this call or later calls from the same site.
        let target = match decl {
            Some(d) => match self.vm_compiled(&d) {
                Some(c) if !c.needs_bind => CachedFn::Direct(d, c),
                _ => CachedFn::Decl(d),
            },
            None => CachedFn::Builtin,
        };
        *cache.borrow_mut() = Some(target.clone());
        Ok(target)
    }

    /// Reuse AST argument binding for the cold reference path; resolving
    /// before this helper keeps the same pinned literal-call target.
    fn vm_ref_call(
        &mut self,
        target: &CachedFn,
        lname: &str,
        raw: &str,
        exprs: &[Expr],
        site: usize,
    ) -> Result<Value, PhpError> {
        let decl = match target {
            CachedFn::Decl(d) | CachedFn::Direct(d, _) => Some(d),
            CachedFn::Builtin => None,
        };
        let builtin_params = if decl.is_none() {
            super::calls::builtin_ref_params(lname)
        } else {
            Vec::new()
        };
        self.send_line = Some(site);
        let args = self.arg_cells(
            exprs,
            decl.map(|d| d.params.as_slice()).unwrap_or(&builtin_params),
            raw,
            decl.is_none(),
            Some(site),
            false,
            decl.is_some()
                || !builtin_params.is_empty()
                || crate::builtins::builtin_sig(lname).is_some_and(|sg| !sg.is_empty())
                || crate::builtins::builtin_params(lname).is_some(),
        )?;
        if let Some(d) = decl {
            return self.invoke_fn(d, args, None, None);
        }
        if let Some(value) = self.call_builtin(lname, &args, true)? {
            return Ok(value);
        }
        self.fail(PhpError::uncaught(
            "Error",
            format!("Call to undefined function {}()", raw),
            0,
        ))
    }

    /// Execute the target chosen before argument evaluation.
    /// `new X(...)` with the class entry and ctor already resolved at
    /// InitNew — runs new_instance's post-resolution sequence verbatim,
    /// then routes the ctor through the same direct/canonical split a
    /// `->m()` call takes (ctorless classes just drop the args).
    fn vm_new_run(
        &mut self,
        cls: &Rc<PhpClass>,
        lname: &Rc<str>,
        ctor: Option<(Rc<MethodDecl>, Rc<PhpClass>)>,
        argv: &mut Vec<Value>,
        cache: &std::cell::RefCell<Option<CachedMethod>>,
        site: usize,
    ) -> Result<Value, PhpError> {
        if cls.name().eq_ignore_ascii_case("closure") {
            return self.fail(PhpError::uncaught(
                "Error",
                "Instantiation of class Closure is not allowed",
                0,
            ));
        }
        if cls.name().eq_ignore_ascii_case("weakreference") {
            return self.fail(PhpError::uncaught(
                "Error",
                "Direct instantiation of WeakReference is not allowed, use WeakReference::create instead",
                0,
            ));
        }
        let kerr = |what: &str| {
            PhpError::uncaught(
                "Error",
                format!("Cannot instantiate {} {}", what, cls.name()),
                0,
            )
        };
        match cls.decl.kind {
            crate::ast::ClassKind::Trait => return self.fail(kerr("trait")),
            crate::ast::ClassKind::Interface => return self.fail(kerr("interface")),
            crate::ast::ClassKind::Enum => return self.fail(kerr("enum")),
            _ => {}
        }
        if cls.decl.is_abstract {
            return self.fail(kerr("abstract class"));
        }
        self.link_const_inits(cls)?;
        let obj = self.instantiate(lname, &[])?;
        if let Value::Object(o) = &obj {
            let r = if let Some((m, dc)) = ctor {
                // Same guards a `$o->m()` hit runs — private-ctor
                // visibility and __call routing keep canonical errors.
                match self.vm_method_target(o.clone(), "__construct", cache, site) {
                    Some((o2, core)) => {
                        self.vm_run_method_direct(&core, argv, o2, "__construct", site)
                    }
                    None => {
                        let mut args = super::CallArgs::empty();
                        args.cells = argv.drain(..).map(cell).collect();
                        self.method_invoke_vis(o.clone(), "__construct", args, Some((m, dc)))
                    }
                }
            } else {
                // Ctorless class silently ignores the arg list.
                Ok(Value::Null)
            };
            if let Err(e) = r {
                // A ctor that throws leaves a half-built object;
                // zend never runs its __destruct (bug29368_1/_3).
                self.mark_destructed(o);
                return Err(e);
            }
        }
        if let Value::Object(o) = &obj {
            self.expr_temps.push(o.clone());
        }
        Ok(obj)
    }

    /// Resolve a `$cv->m` site to its direct target: receiver must be
    /// an object whose resolved method is a userland, compiled,
    /// value-callable decl. Per-call context guards (scope-private
    /// precedence, visibility, pending hook) run on every hit; the
    /// class-keyed pieces live in the site cache.
    fn vm_method_target(
        &mut self,
        o: Rc<std::cell::RefCell<PhpObject>>,
        name: &str,
        cache: &std::cell::RefCell<Option<CachedMethod>>,
        _site: usize,
    ) -> Option<(Rc<std::cell::RefCell<PhpObject>>, MSiteCore)> {
        if self.pending_hook_prop.is_some() {
            return None;
        }
        let cls = o.borrow().class.clone();
        let cached = {
            let b = cache.borrow();
            b.as_ref()
                .filter(|h| Rc::ptr_eq(&h.class, &cls))
                .map(|h| h.core.clone())
        };
        let core = match cached {
            Some(core) => core,
            None => {
                let core = self.find_method_in(&cls, name).and_then(|(m, dc)| {
                    if m.is_static
                        || m.is_abstract
                        || m.decl.line == 0
                        || m.decl.by_ref
                        || m.decl.params.iter().any(|p| p.by_ref)
                        || Self::decl_contains_yield(&m.decl.body)
                    {
                        return None;
                    }
                    let fdecl = self.method_function(&m);
                    let comp = self.vm_compiled(&fdecl)?;
                    (!comp.needs_bind).then(|| MSiteCore { m, fdecl, comp, dc })
                });
                *cache.borrow_mut() = Some(CachedMethod {
                    class: cls.clone(),
                    core: core.clone(),
                });
                core
            }
        }?;
        // Scope-private methods shadow the object's table only inside
        // their own scope; visibility gates __call/vis_error — both
        // keep canonical semantics.
        if self.scope_private_method(name).is_some() || !self.method_access_ok(&core.m, &core.dc) {
            return None;
        }
        Some((o, core))
    }

    /// Direct method run — `vm_run_direct` with the frame's method
    /// context (`this` var, this_obj/scope/decl/called class) instead
    /// of the function-call shape. Arg-count misses still route the
    /// evaluated args through method_invoke_vis so zend's
    /// ArgumentCountError trace keeps its exact shape.
    fn vm_run_method_direct(
        &mut self,
        core: &MSiteCore,
        argv: &mut Vec<Value>,
        this_obj: Rc<std::cell::RefCell<PhpObject>>,
        name: &str,
        site: usize,
    ) -> Result<Value, PhpError> {
        let __p = pnow!();
        let m = &core.m;
        let decl = &core.fdecl;
        let comp = &core.comp;
        let scope = core.dc.clone();
        let required = decl
            .params
            .iter()
            .rposition(|p| p.default.is_none() && !p.variadic)
            .map(|i| i + 1)
            .unwrap_or(0);
        if argv.len() < required {
            let mut args = super::CallArgs::empty();
            args.cells = argv.drain(..).map(cell).collect();
            return self.method_invoke_vis(this_obj, name, args, Some((m.clone(), scope.clone())));
        }
        let called = this_obj.borrow().class.clone();
        let mut frame = self
            .vm_frame_pool
            .pop()
            .unwrap_or_else(|| Box::new(super::Frame::new(String::new())));
        frame.fn_name = decl.name.clone();
        frame.decl_site = Rc::as_ptr(m) as usize;
        frame.fn_line = decl.line;
        frame.file = if decl.file.is_empty() {
            self.cur_file.clone()
        } else {
            decl.file.clone()
        };
        frame.ns = decl.ns.clone();
        frame.trait_origin = decl.decl_in.clone();
        frame.hook_prop = self.pending_hook_prop.take();
        frame.gen_body = self.pending_gen_body;
        self.pending_gen_body = false;
        frame.this_obj = Some(this_obj.clone());
        frame.scope_class = Some(scope.clone());
        frame.called_class = Some(called);
        frame.decl_class = Some(scope);
        frame.ret_by_ref = false;
        frame.closure_rc = None;
        frame.call_alias = None;
        frame.statics_unit = None;
        frame.vars.clear();
        frame
            .vars
            .insert("this".to_string(), cell(Value::Object(this_obj)));
        frame.args.clear();
        frame.value_args.clear();
        frame.vm_sites.clear();
        frame.vm_span = None;
        let pending_caps = std::mem::take(&mut self.pending_gen_captures);
        for (n, c, by_ref) in pending_caps {
            let c2 = if by_ref { c } else { cell(c.borrow().clone()) };
            frame.vars.insert(n, c2);
        }
        self.send_line = Some(site);
        self.stack.push(frame);
        padd!(0, __p);
        let __p = pnow!();
        let mut args = super::CallArgs::empty();
        let mut abi = comp.value_abi
            && !argv.is_empty()
            && argv.len() <= u16::MAX as usize
            && decl
                .params
                .iter()
                .enumerate()
                .all(|(i, p)| p.variadic || i < argv.len() || comp.defaults[i].is_some());
        if abi {
            'abi: for (i, p) in decl.params.iter().enumerate() {
                if p.variadic {
                    abi = p.ty.is_none();
                    break;
                }
                let (Some(ty), Some(v)) = (&p.ty, argv.get(i)) else {
                    continue;
                };
                if comp.param_fast[i].is_some_and(|g| g(v)) {
                    continue;
                }
                let v = v.clone();
                match self.vm_abi_gate_slow(decl, p, ty, &v, argv, i) {
                    Ok(()) => {}
                    Err(Some(e)) => {
                        self.stack_pop();
                        return self.fail(e);
                    }
                    Err(None) => {
                        abi = false;
                        break 'abi;
                    }
                }
            }
        }
        if abi {
            let n = argv.len() as u64;
            std::mem::swap(&mut self.cur().value_args, argv);
            self.vm_direct_reserve(&mut args, n);
            padd!(1, __p);
            return self.vm_run(decl, comp, args);
        }
        args.cells = self.vm_arg_cells(argv);
        let n = args.cells.len() as u64;
        self.vm_direct_reserve(&mut args, n);
        padd!(1, __p);
        self.vm_run(decl, comp, args)
    }

    /// Pure warmed leaves have no observable frame or global access. Any
    /// failed proof returns before side effects to the full root bridge.
    fn vm_float_method(
        &mut self,
        core: &MSiteCore,
        argv: &[Value],
        obj: &Rc<std::cell::RefCell<PhpObject>>,
    ) -> Option<Value> {
        let comp = &core.comp;
        if !comp.float_leaf
            || argv.len() != core.fdecl.params.len()
            || argv
                .iter()
                .any(|v| !matches!(v, Value::Float(_) | Value::Int(_)))
            || Rc::strong_count(obj) < 3
            || self.globals_arr.is_some()
            || self.pending_hook_prop.is_some()
            || self.pending_gen_body
            || !self.pending_gen_captures.is_empty()
            || !self.expr_temps.is_empty()
            || self.last_ret_cell.is_some()
            || self.last_prop_ov.is_some()
            || Self::callprof_on()
            || coverage_on()
            || self.deadline.is_some()
            || self.cast_err.is_some()
            || self.callable_probe_err.is_some()
            || crate::value::cmp_notices_pending()
            || self.mem_exceeded
            || self.oom_at.is_some()
            || !self.vm_stack.last().is_some_and(|top| {
                top.size.saturating_sub(top.used)
                    >= (2 * super::VM_FRAME_SLOTS + argv.len() as u64 + 1) * 16
            })
        {
            return None;
        }
        let mut values = [0.0f64; 64];
        let mut used = 0usize;
        for op in &comp.ops {
            let value = match op {
                Op::Const(Value::Float(v)) => Some(*v),
                Op::Load(index) => Some(match argv.get(*index as usize)? {
                    Value::Float(v) => *v,
                    Value::Int(v) => *v as f64,
                    _ => return None,
                }),
                Op::ThisProp { cache, .. } => {
                    let hit = cache.borrow();
                    let hit = hit.as_ref()?;
                    let ob = obj.borrow();
                    if !Rc::ptr_eq(&hit.class, &ob.class)
                        || !hit
                            .scope
                            .as_ref()
                            .is_some_and(|scope| Rc::ptr_eq(scope, &core.dc))
                        || !hit
                            .private_candidate
                            .as_ref()
                            .is_none_or(|key| key == &hit.key || !ob.prop_has(key))
                    {
                        return None;
                    }
                    let cell = match hit.slot {
                        Some(i) => ob.slots.get(i).and_then(|s| s.clone()),
                        None => ob.prop_get(&hit.key),
                    }?;
                    let value = match &*cell.borrow() {
                        Value::Float(v) => *v,
                        _ => return None,
                    };
                    Some(value)
                }
                Op::Binary(operator) => {
                    used = used.checked_sub(1)?;
                    let right = values[used];
                    let left = values.get_mut(used.checked_sub(1)?)?;
                    *left = match *operator {
                        "+" => *left + right,
                        "-" => *left - right,
                        "*" => *left * right,
                        _ => return None,
                    };
                    None
                }
                Op::Call { cache, .. } => {
                    if !matches!(&*cache.borrow(), Some(CachedFn::Builtin)) {
                        return None;
                    }
                    let value = values.get_mut(used.checked_sub(1)?)?;
                    *value = value.sqrt();
                    None
                }
                Op::Return => {
                    if used != 1 {
                        return None;
                    }
                    // Both leaf and builtin spans fit the current page.
                    // No frame/token is needed; repay through the shared arena.
                    let want = super::VM_FRAME_SLOTS + argv.len() as u64;
                    let top = self.vm_stack.last_mut()?;
                    let key = Rc::as_ptr(&top.tok) as usize;
                    top.used += want * 16;
                    Self::vm_stack_apply(&mut self.vm_stack, key, want, 0);
                    self.last_call_by_ref = false;
                    self.send_line = Some(self.cur_line);
                    return Some(Value::Float(values[0]));
                }
                Op::Line(_) | Op::Sweep | Op::InitCall { .. } => None,
                _ => return None,
            };
            if let Some(value) = value {
                *values.get_mut(used)? = value;
                used += 1;
            }
        }
        None
    }

    fn vm_scalar_builtin(&mut self, lname: &str, argv: &[Value]) -> Option<Value> {
        let span = (super::VM_FRAME_SLOTS + argv.len() as u64) * 16;
        if !Self::callprof_on()
            && !coverage_on()
            && self.cast_err.is_none()
            && self.callable_probe_err.is_none()
            && !crate::value::cmp_notices_pending()
            && !self.mem_exceeded
            && self.oom_at.is_none()
            && self
                .vm_stack
                .last()
                .is_some_and(|top| top.size.saturating_sub(top.used) >= span)
        {
            if let Some(value) = builtins::call_scalar(self, lname, argv) {
                // Exact typed kernels cannot observe this already-proven fit.
                // Reuse float-leaf repayment; result string charges stay canonical.
                let want = super::VM_FRAME_SLOTS + argv.len() as u64;
                let top = self.vm_stack.last_mut()?;
                let key = Rc::as_ptr(&top.tok) as usize;
                top.used += span;
                Self::vm_stack_apply(&mut self.vm_stack, key, want, 0);
                self.scalar_builtin_oom(lname, argv, true);
                return Some(value);
            }
        }
        None
    }

    fn vm_call(
        &mut self,
        lname: &str,
        raw: &str,
        argv: &mut Vec<Value>,
        site: usize,
        target: &CachedFn,
    ) -> Result<Value, PhpError> {
        self.send_line = Some(site);
        if matches!(target, CachedFn::Builtin) {
            if let Some(value) = self.vm_scalar_builtin(lname, argv) {
                return Ok(value);
            }
        }
        let decl = match target {
            CachedFn::Direct(d, _) | CachedFn::Decl(d) => Some(d.clone()),
            CachedFn::Builtin => None,
        };
        let mut args = super::CallArgs::empty();
        args.cells = self.vm_arg_cells(argv);
        let n = args.cells.len() as u64;
        self.vm_call_reserve(&mut args, n);
        if decl.is_none() {
            if let Some(v) = self.call_builtin(lname, &args, true)? {
                return Ok(v);
            }
            return self.fail(PhpError::uncaught(
                "Error",
                format!("Call to undefined function {}()", raw),
                0,
            ));
        }
        self.invoke_fn(&decl.unwrap(), args, None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::Interp;
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    use std::rc::Rc;

    #[test]
    fn scalar_dimension_slots_preserve_mutation_refs_views_and_fallbacks() {
        let mut it = Interp::new("array-read.php");
        let result = it.run_source(r#"<?php
function read_at($a, $k) { return $a[$k]; }
function read_zero($a) { return $a[0]; }
$a = [3, 1, 2]; $copy = $a; echo read_at($a, 1), ' ', read_zero($a), "\n";
$r =& $a[1]; $r = 9; echo read_at($a, 1), ' ', read_at($copy, 1), "\n";
unset($a[0]); $a[0] = 8; echo read_zero($a), "\n";
sort($a); echo read_zero($a), "\n";
array_unshift($a, 7); echo read_zero($a), "\n";
array_splice($a, 0, 1, [4, 5]); echo read_zero($a), ' ', read_at($a, 1), "\n";
foreach ($a as &$v) $v += 1; unset($v); echo read_zero($a), "\n";
foreach ([null, false, 1, 1.5, "x\0z"] as $v) { $x=read_zero([$v]); echo gettype($x), ':', is_string($x) ? bin2hex($x) : json_encode($x), "\n"; }
echo read_at(['k'=>'str', 2=>'num'], 'k'), ' ', read_at([2=>'num'], '2'), "\n";
$o = new stdClass; $o->x=6; echo read_at([$o], 0)->x, ' ', read_at([[2]], 0)[0], "\n";
class ReadAccess implements ArrayAccess {
 function offsetExists(mixed $k): bool { return true; }
 function offsetGet(mixed $k): mixed { echo "get:$k\n"; return 12; }
 function offsetSet(mixed $k, mixed $v): void {}
 function offsetUnset(mixed $k): void {}
}
echo read_at(new ReadAccess, 3), "\n";
set_error_handler(function($errno, $message) { echo "warning:$message\n"; });
var_dump(read_at([], 7));
function missing_key($a) { return $a[$unknown]; }
var_dump(missing_key([0=>2]));
restore_error_handler();
echo read_at(k: 0, a: [11]), "\n";
function clear_at(&$a, $k) { return $a[$k] = false; }
$a=[true,true]; var_dump(clear_at($a,0)); $copy=$a; clear_at($a,1);
echo json_encode($a), ' ', json_encode($copy), "\n";
$r =& $a[1]; $r=true; clear_at($a,1); var_dump($r);
clear_at($a,7); clear_at($a,'key'); echo json_encode($a), "\n";
class WriteTyped { public int $v=1; public readonly array $a; function __construct() { $this->a=[true]; } }
$o=new WriteTyped; $a=[0]; $a[0] =& $o->v;
var_dump(clear_at($a,0)); var_dump($o->v);
$a=$o->a; clear_at($a,0); echo json_encode($a), ' ', json_encode($o->a), "\n";
try { $o->a[0]=false; } catch (Error $e) { echo $e->getMessage(), "\n"; }
class WriteAccess implements ArrayAccess {
 function offsetExists(mixed $k):bool { return true; }
 function offsetGet(mixed $k):mixed { return true; }
 function offsetSet(mixed $k,mixed $v):void { echo "set:$k:", json_encode($v), "\n"; }
 function offsetUnset(mixed $k):void {}
}
$a=new WriteAccess; clear_at($a,3);
$g=[1]; for ($i=0; $i<2; $i++) { echo $g[0], "\n"; $GLOBALS['g']=[5]; }
echo $GLOBALS['g'][0], "\n";
"#);
        assert_eq!(
            String::from_utf8_lossy(&it.out),
            r#"1 3
9 1
8
2
7
4 5
5
NULL:null
boolean:false
integer:1
double:1.5
string:78007a
str num
6 2
get:3
12
warning:Undefined array key 7
NULL
warning:Undefined variable $unknown
warning:Using null as an array offset is deprecated, use an empty string instead
warning:Undefined array key ""
NULL
11
bool(false)
[false,false] [false,true]
bool(false)
{"0":false,"1":false,"7":false,"key":false}
int(0)
int(0)
[false] [true]
Cannot indirectly modify readonly property WriteTyped::$a
set:3:false
1
5
5
"#
        );
        assert_eq!(it.err_buf, "");
        assert_eq!(result.exit_code, 0);
        for name in ["read_at", "read_zero", "clear_at"] {
            let decl = it.functions[name].clone();
            let compiled = it.vm_compiled(&decl).unwrap();
            assert!(compiled.needs_bind && !compiled.value_abi);
            assert!(compiled
                .ops
                .iter()
                .any(|op| matches!(op, super::Op::DimCv { .. })));
            if name == "clear_at" {
                assert!(compiled.ops.iter().any(|op| matches!(
                    op,
                    super::Op::DimCv {
                        write: Some(super::Value::Bool(false)),
                        ..
                    }
                )));
            }
        }
        let mut packed = Interp::new("packed.php");
        let stmts = crate::parser::parse_source(
            r#"<?php
$a = array_fill(0, 4, true); $a[0] = $a[1] = false;
$b = $a; $b[2] = false;
$c = array_fill(0, 4, true); $ref =& $c[0]; $ref = false;
$count = 0; for ($i = 0; $i < 4; $i++) { if ($a[$i]) $count++; }
var_dump($count, $b[2], $c[0]);
"#,
            false,
        )
        .unwrap();
        let flow = packed.exec_block(&stmts);
        assert!(
            matches!(flow, super::super::Flow::Normal),
            "{}",
            packed.err_buf
        );
        assert_eq!(packed.out, b"int(2)\nbool(false)\nbool(false)\n");
        for (name, inline) in [("a", true), ("b", true), ("c", false)] {
            let super::Value::Array(a) = packed.globals.vars[name].borrow().clone() else {
                panic!("array")
            };
            assert_eq!(a.borrow().entries.is_scalar_packed(), inline, "{name}");
        }
        let mut regions = Interp::new("bool-regions.php");
        let result = regions.run_source(
            r#"<?php
function typed_region() {
 $a=array_fill(0, 12, true);
 for ($i=2; $i<12; $i++) { for ($j=$i*2; $j<12; $j+=$i) $a[$j]=false; }
 $count=0; for ($i=2; $i<12; $i++) if ($a[$i]) $count++;
 return $count;
}
function shared_region() {
 $a=array_fill(0, 4, true); $copy=$a;
 for ($i=0; $i<4; $i++) $a[$i]=false;
 return json_encode([$a,$copy]);
}
function overflow_region() {
 $a=array_fill(0,1,true); $v=PHP_INT_MAX;
 for ($i=0; $i<1; $i++) { $a[$i]=false; $v++; }
 return json_encode([$a,is_float($v)]);
}
function bounded_region() {
 $a=array_fill(0,1048577,true); $count=0;
 for($i=0;$i<1;$i++) if($a[$i]) $count++;
 return $count;
}
function bounds_region() {
 $a=array_fill(0,1,true);
 for($i=0;$i<2;$i++) $a[$i]=false;
 return json_encode($a);
}
function infinite_region() { $a=array_fill(0,1,true); for(;;) $a[0]=false; }
echo typed_region(),"\n",shared_region(),"\n",overflow_region(),"\n",bounded_region(),"\n",bounds_region(),"\n";
"#,
        );
        assert_eq!(result.exit_code, 0, "{}", regions.err_buf);
        assert_eq!(regions.err_buf, "");
        assert_eq!(
            regions.out,
            b"5\n[[false,false,false,false],[true,true,true,true]]\n[[false],true]\n1\n[false,false]\n"
        );
        let decl = regions.functions["typed_region"].clone();
        let compiled = regions.vm_compiled(&decl).unwrap();
        assert!(compiled.ops.iter().any(|op| matches!(op,
            super::Op::BoolLoop { executions, .. } if executions.get() > 0)));
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        assert!(compiled.ops.iter().any(|op| matches!(op,
            super::Op::BoolLoop { native, .. }
                if native.get().and_then(Option::as_ref).is_some_and(|p| p.executions.get() > 0))));
        for name in [
            "shared_region",
            "overflow_region",
            "bounded_region",
            "bounds_region",
        ] {
            let decl = regions.functions[name].clone();
            let compiled = regions.vm_compiled(&decl).unwrap();
            assert!(
                compiled.ops.iter().all(|op| !matches!(op,
                super::Op::BoolLoop { executions, .. } if executions.get() > 0)),
                "{name}"
            );
        }
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        {
            for name in ["overflow_region", "bounds_region"] {
                let decl = regions.functions[name].clone();
                let compiled = regions.vm_compiled(&decl).unwrap();
                assert!(compiled.ops.iter().any(|op| matches!(op,
                    super::Op::BoolLoop { native, .. } if native.get().and_then(Option::as_ref).is_some())), "{name}");
            }
            let decl = regions.functions["infinite_region"].clone();
            let compiled = regions.vm_compiled(&decl).unwrap();
            let (marker, end) = compiled
                .ops
                .iter()
                .enumerate()
                .find_map(|(pc, op)| match op {
                    super::Op::BoolLoop { end, .. } if *end > pc => Some((pc, *end)),
                    _ => None,
                })
                .unwrap();
            let program =
                super::native::Program::region(&regions, &compiled, marker + 1, end).unwrap();
            let mut raw = vec![[0i64; 2]; compiled.nslots];
            let mut private = vec![true];
            let mut line = 0;
            assert!(!program.run_region(&mut raw, &mut private, &mut line));
            assert_eq!(private, [false]);
            drop(program);
            drop(compiled);
            assert!(regions.native_regions_bytes.get() > 0);
            regions.compiled_fns.clear();
            // The earlier typed_region handle deliberately still owns one map.
            assert!(regions.native_regions_bytes.get() > 0);
        }
        drop(compiled);
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        assert_eq!(regions.native_regions_bytes.get(), 0);
    }

    #[test]
    fn callback_values_preserve_live_args_fallbacks_sort_errors_and_owners() {
        let mut it = Interp::new("callbacks.php");
        let mut root = Interp::new("scalar-root.php");
        let initial: Vec<_> = root.vm_stack.iter().map(|s| (s.size, s.used)).collect();
        let result = root.run_source(
            r#"<?php
$s = "abcd"; $a = substr($s, 1, 2); echo $a, "\n";
$GLOBALS['s'] = "WXYZ"; $a = substr($s, 1, 2); echo $a, "\n";
function change_scalar_root() { unset($GLOBALS['s']); $GLOBALS['s'] = "1234"; return 4; }
$n = sqrt(change_scalar_root()); echo $n, " ", substr($s, 1, 2), "\n";
"#,
        );
        assert_eq!(result.exit_code, 0, "{}", root.err_buf);
        assert_eq!(root.out, b"bc\nXY\n2 23\n");
        assert_eq!(root.err_buf, "");
        assert_eq!(
            root.vm_stack
                .iter()
                .map(|s| (s.size, s.used))
                .collect::<Vec<_>>(),
            initial
        );

        // Builtin binding borrows complete positional Cells; incomplete or
        // named packs still run the canonical owned/default/error binder.
        let exact = super::super::CallArgs::positional(vec![
            super::cell(super::Value::str("abcd")),
            super::cell(super::Value::Int(1)),
            super::cell(super::Value::Int(2)),
        ]);
        let params = crate::builtins::builtin_params("substr").unwrap();
        let bound = it.resolve_named_builtin("substr", params, &exact).unwrap();
        assert!(matches!(bound, std::borrow::Cow::Borrowed(_)));
        assert!(std::rc::Rc::ptr_eq(&bound[0], &exact.cells[0]));
        let partial = super::super::CallArgs::positional(exact.cells[..2].to_vec());
        assert!(matches!(
            it.resolve_named_builtin("substr", params, &partial)
                .unwrap(),
            std::borrow::Cow::Owned(_)
        ));
        let mut named = super::super::CallArgs::positional(exact.cells[..1].to_vec());
        named
            .named
            .push(("offset".into(), exact.cells[1].clone(), false, false));
        assert!(matches!(
            it.resolve_named_builtin("substr", params, &named).unwrap(),
            std::borrow::Cow::Owned(_)
        ));
        let excessive = super::super::CallArgs::positional(vec![exact.cells[0].clone(); 4]);
        assert!(it
            .resolve_named_builtin("substr", params, &excessive)
            .is_err());
        let variadic = super::super::CallArgs::positional(vec![
            super::cell(super::Value::Int(3)),
            super::cell(super::Value::Int(1)),
        ]);
        assert!(matches!(
            it.resolve_named_builtin(
                "min",
                crate::builtins::builtin_params("min").unwrap(),
                &variadic
            )
            .unwrap(),
            std::borrow::Cow::Borrowed(_)
        ));
        let missing = super::super::CallArgs::empty();
        assert!(it
            .resolve_named_builtin("substr", params, &missing)
            .is_err());
        let result = it.run_source(r#"<?php
function liveCallbackArg() { return debug_backtrace(0, 2)[1]['args'][0]; }
var_dump(array_map(function(int $x): int { $x++; echo json_encode(func_get_args()), "\n"; return liveCallbackArg(); }, ['a'=>1,'b'=>2]));
echo json_encode(array_map(function($x) { $x = 1.5; return func_get_args(); }, [null, true])), "\n";
echo json_encode(array_filter([1,2,3], fn(int $x): bool => $x % 2 === 0)), "\n";
$a=[3,1,2]; usort($a, fn(int $x,int $y):int => $x <=> $y); echo json_encode($a), "\n";
$a=[1=>3,2=>1,3=>2]; uasort($a, fn(int $x,int $y):int => $x <=> $y); echo json_encode($a), "\n";
uksort($a, fn(int $x,int $y):int => $y <=> $x); echo json_encode($a), "\n";
$seen=0; echo json_encode(array_map(function($x) use (&$seen) { $seen += $x; return $seen; }, [1,2])), "\n";
echo json_encode(array_map(function($x,$y=10) { return $x+$y; }, [1,2])), "\n";
echo json_encode(array_map(fn()=>func_num_args(), [1,2])), "\n";
echo json_encode(array_map(fn(int $x)=>$x+1, ['2','3'])), "\n";
class CallbackScope { private int $k=4; function run() { return array_map(fn($x)=>$this->k+$x, [1,2]); } }
echo json_encode((new CallbackScope)->run()), "\n";
$warnings=0; set_error_handler(function($code,$message) use (&$warnings) { $warnings++; return true; });
$a=[2,1]; usort($a, function(&$x,$y) { return $x <=> $y; }); echo json_encode($a),' ', $warnings>0?'warning':'missing', "\n";
$a=[2,1,2]; usort($a, fn(int $x,int $y):bool=>$x>$y); echo json_encode($a), "\n";
restore_error_handler();
try { $a=[1,1]; usort($a, fn(int $x,int $y):int=>intdiv(1,$x-$y)); } catch (Throwable $e) { echo get_class($e), ':', $e->getMessage(), "\n"; }
echo json_encode(array_map(fn(int $x):int=>$x*2,[3,4])), "\n";
function liveCallbackArgs() { return debug_backtrace(0, 2)[1]['args']; }
$a=[2,1]; usort($a, function(int $x,int $y):int { $x++; echo json_encode(func_get_args()), ' ', json_encode(liveCallbackArgs()), "\n"; return ($x-1)<=>$y; });
echo json_encode($a), "\n";
"#);
        assert_eq!(result.exit_code, 0, "{}", it.err_buf);
        assert_eq!(it.err_buf, "");
        assert_eq!(
            it.out,
            br#"[2]
[3]
array(2) {
  ["a"]=>
  int(2)
  ["b"]=>
  int(3)
}
[[1.5],[1.5]]
{"1":2}
[1,2,3]
{"2":1,"3":2,"1":3}
{"3":2,"2":1,"1":3}
[1,3]
[11,12]
[1,1]
[3,4]
[5,6]
[1,2] warning
[1,2,2]
DivisionByZeroError:Division by zero
[6,8]
[3,1] [3,1]
[1,2]
"#
        );

        it.trace_pool.clear();
        let used = it.vm_stack.last().unwrap().used;
        for (name, argv, expected) in [
            (
                "str_replace",
                vec![
                    super::Value::str("\0"),
                    super::Value::bytes(vec![255]),
                    super::Value::str("a\0b\0"),
                ],
                vec![97, 255, 98, 255],
            ),
            (
                "str_ireplace",
                vec![
                    super::Value::str("AB"),
                    super::Value::str("x"),
                    super::Value::str("abABa"),
                ],
                b"xxa".to_vec(),
            ),
            (
                "strtoupper",
                vec![super::Value::bytes(vec![97, 0, 255, 98])],
                vec![65, 0, 255, 66],
            ),
            (
                "strtolower",
                vec![super::Value::bytes(vec![65, 0, 255, 66])],
                vec![97, 0, 255, 98],
            ),
        ] {
            let mut argv = argv;
            let super::Value::Str(bytes) = it
                .vm_call(name, name, &mut argv, 1, &super::CachedFn::Builtin)
                .unwrap()
            else {
                panic!("string")
            };
            assert_eq!(bytes.as_ref(), expected.as_slice());
        }
        let subject = super::Value::str("unchanged");
        let mut argv = vec![
            super::Value::str(""),
            super::Value::str("x"),
            subject.clone(),
        ];
        let value = it
            .vm_call(
                "str_replace",
                "str_replace",
                &mut argv,
                1,
                &super::CachedFn::Builtin,
            )
            .unwrap();
        let (super::Value::Str(before), super::Value::Str(after)) = (&subject, &value) else {
            panic!("string")
        };
        assert!(std::rc::Rc::ptr_eq(&before.rc, &after.rc));
        let mut args = super::super::CallArgs::empty();
        args.cells = ["fox", "wolf", "fox\0fox"]
            .into_iter()
            .map(|s| std::rc::Rc::new(std::cell::RefCell::new(super::Value::str(s))))
            .collect();
        let super::Value::Str(bytes) = it
            .call_builtin("str_replace", &args, true)
            .unwrap()
            .unwrap()
        else {
            panic!("string")
        };
        assert_eq!(bytes.as_ref(), b"wolf\0wolf");
        let mut values = vec![super::Value::Float(4.0)];
        assert!(
            matches!(it.vm_call("sqrt", "sqrt", &mut values, 1, &super::CachedFn::Builtin).unwrap(), super::Value::Float(n) if n == 2.0)
        );
        let mut values = vec![
            super::Value::str("a\0b"),
            super::Value::Int(1),
            super::Value::Int(2),
        ];
        let super::Value::Str(bytes) = it
            .vm_call(
                "substr",
                "substr",
                &mut values,
                1,
                &super::CachedFn::Builtin,
            )
            .unwrap()
        else {
            panic!("string")
        };
        assert_eq!(bytes.as_ref(), b"\0b");
        let mut values = vec![
            super::Value::bytes(vec![255, 0, 97]),
            super::Value::Int(0),
            super::Value::Int(2),
        ];
        let super::Value::Str(bytes) = it
            .vm_call(
                "substr",
                "substr",
                &mut values,
                1,
                &super::CachedFn::Builtin,
            )
            .unwrap()
        else {
            panic!("string")
        };
        assert_eq!(bytes.as_ref(), &[255, 0]);
        let mut values = vec![super::Value::str("abcd"), super::Value::Int(-2)];
        let super::Value::Str(bytes) = it
            .vm_call(
                "substr",
                "substr",
                &mut values,
                1,
                &super::CachedFn::Builtin,
            )
            .unwrap()
        else {
            panic!("string")
        };
        assert_eq!(bytes.as_ref(), b"cd");
        assert!(
            it.trace_pool.is_empty(),
            "scalar builtin created a transient trace frame"
        );
        assert_eq!(it.vm_stack.last().unwrap().used, used);
        let mut values = vec![
            super::Value::Array(std::rc::Rc::new(std::cell::RefCell::new(
                super::PhpArray::new(),
            ))),
            super::Value::Int(0),
            super::Value::Int(1),
        ];
        assert!(it
            .vm_call(
                "substr",
                "substr",
                &mut values,
                1,
                &super::CachedFn::Builtin
            )
            .is_err());
        assert!(
            !it.trace_pool.is_empty(),
            "bad type skipped canonical builtin trace"
        );
        assert_eq!(it.vm_stack.last().unwrap().used, used);
        it.trace_pool.clear();
        it.vm_stack.last_mut().unwrap().used = it.vm_stack.last().unwrap().size;
        let full = it.vm_stack.last().unwrap().used;
        let mut values = vec![super::Value::Float(4.0)];
        assert!(
            matches!(it.vm_call("sqrt", "sqrt", &mut values, 1, &super::CachedFn::Builtin).unwrap(), super::Value::Float(n) if n == 2.0)
        );
        assert_eq!(it.vm_stack.last().unwrap().used, full);
        assert!(
            !it.trace_pool.is_empty(),
            "page growth skipped canonical path"
        );
        it.vm_stack.last_mut().unwrap().used = used;

        // Weak Rust ownership also covers the callable metadata retained by the
        // shared direct frame. The compile cache may own the decl, never this Rc.
        let mut it = Interp::new("callback-owner.php");
        it.run_source("<?php function callbackOwned(int $x, int $y): int { return $x + $y; }");
        let callable = it
            .eval(&crate::ast::Expr::Closure(crate::ast::ClosureExpr {
                decl: (*it.functions["callbackowned"]).clone(),
                uses: Vec::new(),
                arrow: false,
                is_static: false,
            }))
            .unwrap();
        let crate::value::Value::Callable(c) = &callable else {
            panic!("closure")
        };
        let weak = std::rc::Rc::downgrade(c);
        let result = it
            .call_values(
                &callable,
                [crate::value::Value::Int(2), crate::value::Value::Int(1)],
                false,
            )
            .unwrap();
        assert!(matches!(result, crate::value::Value::Int(3)));
        assert!(
            it.vm_frame_pool.iter().any(|f| f.value_args.capacity() > 0),
            "primitive callback did not reach the value ABI"
        );
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        {
            for _ in 0..70 {
                assert!(matches!(
                    it.call_values(
                        &callable,
                        [super::Value::Int(2), super::Value::Int(1)],
                        false
                    )
                    .unwrap(),
                    super::Value::Int(3)
                ));
            }
            let super::CallableKind::Closure(decl) = &c.kind else {
                panic!("closure");
            };
            match &it.compiled_fns[&(Rc::as_ptr(decl) as usize)].3 {
                super::native::State::Ready(p) => assert!(
                    p.executions.get() > 0,
                    "callback did not enter native before VM frame"
                ),
                _ => panic!("callback graph was not native"),
            }
            let error = it
                .call_values(
                    &callable,
                    [super::Value::Int(i64::MAX), super::Value::Int(1)],
                    false,
                )
                .unwrap_err();
            assert!(
                error.message.contains("Return value must be of type int"),
                "{}",
                error.message
            );
            assert!(it.call_trace.is_empty());
            assert_eq!(it.vm_stack.len(), 1);
            assert_eq!(it.vm_stack[0].used, 32 + super::super::VM_FRAME_SLOTS * 16);
        }
        drop(callable);
        assert!(
            weak.upgrade().is_none(),
            "pooled callback frame retained callable owner"
        );

        // The same proof must reach native code for one fresh value, without
        // retaining its callable owner or skipping typed overflow replay.
        let mut it = Interp::new("single-callback.php");
        it.run_source("<?php function singleOwned(int $x): int { return $x * 3 + 1; }");
        let callable = it
            .eval(&crate::ast::Expr::Closure(crate::ast::ClosureExpr {
                decl: (*it.functions["singleowned"]).clone(),
                uses: Vec::new(),
                arrow: false,
                is_static: false,
            }))
            .unwrap();
        let super::Value::Callable(c) = &callable else {
            panic!("closure")
        };
        let weak = std::rc::Rc::downgrade(c);
        for _ in 0..70 {
            assert!(matches!(
                it.call_values(&callable, [super::Value::Int(2)], false)
                    .unwrap(),
                super::Value::Int(7)
            ));
        }
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        {
            let super::CallableKind::Closure(decl) = &c.kind else {
                panic!("closure")
            };
            match &it.compiled_fns[&(Rc::as_ptr(decl) as usize)].3 {
                super::native::State::Ready(p) => assert!(p.executions.get() > 0),
                _ => panic!("single callback did not enter native"),
            }
        }
        let error = it
            .call_values(&callable, [super::Value::Int(i64::MAX)], false)
            .unwrap_err();
        assert!(
            error.message.contains("Return value must be of type int"),
            "{}",
            error.message
        );
        assert!(it.call_trace.is_empty());
        assert_eq!(it.vm_stack.len(), 1);
        assert_eq!(it.vm_stack[0].used, 32 + super::super::VM_FRAME_SLOTS * 16);
        drop(callable);
        assert!(weak.upgrade().is_none(), "single callback owner retained");

        let mut it = Interp::new("native-predicates.php");
        it.run_source("<?php function predicateOwned(int $x): bool { return $x % 7 === 0; }");
        let callable = it
            .eval(&crate::ast::Expr::Closure(crate::ast::ClosureExpr {
                decl: (*it.functions["predicateowned"]).clone(),
                uses: Vec::new(),
                arrow: false,
                is_static: false,
            }))
            .unwrap();
        for _ in 0..70 {
            assert!(matches!(
                it.call_values(&callable, [super::Value::Int(7)], false)
                    .unwrap(),
                super::Value::Bool(true)
            ));
        }
        assert!(matches!(
            it.call_values(&callable, [super::Value::Int(8)], false)
                .unwrap(),
            super::Value::Bool(false)
        ));
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        {
            let super::Value::Callable(c) = &callable else {
                panic!("closure")
            };
            let super::CallableKind::Closure(decl) = &c.kind else {
                panic!("closure")
            };
            match &it.compiled_fns[&(Rc::as_ptr(decl) as usize)].3 {
                super::native::State::Ready(p) => assert!(p.executions.get() > 0),
                _ => panic!("predicate did not enter native"),
            }
        }
        let mut it = Interp::new("native-remainder.php");
        let result = it.run_source(
            r#"<?php declare(strict_types=1);
function rem_edge(int $n,int $d): int { return $n % $d; }
function rem_zero(int $n,int $d): int { return $n % $d; }
function bool_child(int $n): bool { return $n === 0; }
function integer_parent(int $n): int { return bool_child($n) === 0 ? 7 : 9; }
function bad_bool_return(int $n): bool { return $n; }
for ($i=0;$i<70;$i++) {
    rem_edge(9,7); rem_zero(9,7);
    if (integer_parent(0) !== 9) throw new Exception('bool entered integer stack');
    try { bad_bool_return(1); } catch (TypeError $e) { continue; }
    throw new Exception('bool return check bypassed');
}
echo rem_edge(PHP_INT_MIN,7), ' ', rem_edge(PHP_INT_MIN,-3), ' ', rem_edge(PHP_INT_MIN,-1), "\n";
try { rem_zero(9,0); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
"#,
        );
        assert_eq!(result.exit_code, 0, "{}", it.err_buf);
        assert_eq!(it.err_buf, "");
        assert_eq!(it.out, b"-1 -2 0\nModulo by zero\n");
        assert!(it.call_trace.is_empty());
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        for name in ["rem_edge", "rem_zero"] {
            assert!(matches!(
                it.compiled_fns[&(Rc::as_ptr(&it.functions[name]) as usize)].3,
                super::native::State::Disabled
            ));
        }
    }

    #[test]
    fn canonical_frames_reuse_boxes_and_release_php_owners() {
        let mut ledger = Interp::new("weak-ledger.php");
        let owner = super::Rc::new(7u64);
        let key = super::Rc::as_ptr(&owner) as usize;
        ledger.mem_track(&owner, 112);
        assert_eq!(super::Rc::strong_count(&owner), 1);
        assert_eq!(ledger.mem_tracked[&key].probe.strong_count(), 1);
        assert!(ledger.mem_tracked[&key]
            .probe
            .upgrade()
            .unwrap()
            .is::<u64>());
        let alias = owner.clone();
        assert_eq!(ledger.mem_tracked[&key].probe.strong_count(), 2);
        drop(alias);
        ledger.mem_realloc(&owner, 512);
        assert_eq!(ledger.mem_tracked[&key].table_req, 512);
        let charged = ledger.mem_tracked[&key].inner;
        ledger.mem_credit(&owner, 112);
        assert!(ledger.mem_tracked[&key].inner < charged);
        drop(owner);
        assert_eq!(ledger.mem_tracked[&key].probe.strong_count(), 0);
        ledger.mem_sweep();
        assert!(!ledger.mem_tracked.contains_key(&key));
        let owner = super::Rc::new(std::cell::RefCell::new(super::Value::Int(3)));
        let key = super::Rc::as_ptr(&owner) as usize;
        ledger.mem_track(&owner, 64);
        assert_eq!(super::Rc::strong_count(&owner), 1);
        ledger.mem_retire(&owner);
        assert!(!ledger.mem_tracked.contains_key(&key));
        let mut it = Interp::new("pool.php");
        let result = it.run_source(
            r#"<?php
            class Receiver { function run($n = 1) { return $n; } }
            function named($n, $m = 2) { return $n + $m; }
            for ($i = 0; $i < 200; $i++) { named(m: 3, n: $i); }
            $o = new Receiver; $w = WeakReference::create($o);
            $o->run(n: 4); unset($o); var_dump($w->get());
        "#,
        );
        assert_eq!(result.exit_code, 0, "{}", it.err_buf);
        assert_eq!(it.out, b"NULL\n");
        assert!(!it.vm_frame_pool.is_empty());
        assert!(it.vm_frame_pool.iter().all(|f| f.vars.is_empty()
            && f.args.is_empty()
            && f.value_args.is_empty()
            && f.this_obj.is_none()
            && f.closure_rc.is_none()));
        let frame = it.frame_new("first");
        let ptr = &*frame as *const super::super::Frame;
        it.frame_recycle(frame);
        let frame = it.frame_new("second");
        assert_eq!(&*frame as *const super::super::Frame, ptr);

        let mut scan = super::super::GcScan::default();
        let mut out = Vec::new();
        let mut visited = crate::value::FxSet::default();
        for value in [
            super::Value::Null,
            super::Value::Bool(true),
            super::Value::Int(7),
            super::Value::Float(1.5),
            super::Value::str("leaf"),
        ] {
            Interp::gc_scan_cell(
                &super::cell(value),
                &mut scan,
                &mut out,
                16,
                &mut visited,
                false,
            );
        }
        assert!(scan.cells.is_empty() && scan.cell_slots.is_empty() && out.is_empty());
        let array = std::rc::Rc::new(std::cell::RefCell::new(super::PhpArray::new()));
        let c = super::cell(super::Value::Array(array));
        Interp::gc_scan_cell(&c, &mut scan, &mut out, 16, &mut visited, false);
        Interp::gc_scan_cell(&c, &mut scan, &mut out, 16, &mut visited, false);
        let cp = std::rc::Rc::as_ptr(&c) as usize;
        assert_eq!(scan.cell_slots[&cp], 2);
        assert_eq!(out.len(), 2);
        assert!(std::rc::Rc::ptr_eq(&scan.cells[&cp], &c));
    }

    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    #[test]
    fn native_integer_graph_executes_and_replays_guard_failures() {
        // VM replay at depth100 needs the CLI-sized stack in a debug test.
        std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                let mut it = Interp::new("native.php");
                let result = it.run_source(
                    r#"<?php
function nt_fib(int $n): int { return $n < 2 ? $n : nt_fib($n-1) + nt_fib($n-2); }
function nt_add($n) { return $n + 1; }
function nt_depth(int $n): int { return $n < 1 ? 0 : nt_depth($n-1) + 1; }
function nt_bad(int $n): int { return $n + 1; }
function nt_local($n) { if ($n) { $x=9; } return $x; }
function nt_effect($n) { echo "effect\n"; return $n + 1; }
function nt_loop(int $n): int { $s=0; for ($i=0;$i<$n;$i++) { for ($j=0;$j<3;$j++) { if ($j===1) continue; $s+=$i; } if ($i===4) break; } return $s; }
function nt_loop_bad(int $n): int { for ($i=0;$i<2;$i++) $n++; return $n; }
function nt_loop_local($n) { while ($n<0) { $x=1; $n++; } return $x; }
echo nt_fib(16), "\n";
for ($i=0;$i<70;$i++) { nt_add(1); nt_depth(1); nt_bad(1); nt_local(1); nt_loop(10); nt_loop_bad(1); nt_loop_local(-1); }
echo nt_loop(10), "\n";
try { nt_loop_bad(PHP_INT_MAX-1); } catch (TypeError $e) { echo get_class($e), "\n"; }
var_dump(nt_add(PHP_INT_MAX));
echo nt_depth(100), "\n";
try { nt_bad(PHP_INT_MAX); } catch (TypeError $e) { echo get_class($e), "\n"; }
set_error_handler(function () { echo "uninit\n"; return true; });
var_dump(nt_local(0));
var_dump(nt_loop_local(0));
restore_error_handler();
echo nt_effect(2), "\n";
"#,
                );
                assert_eq!(result.exit_code, 0, "{}", it.err_buf);
                assert_eq!(it.err_buf, "");
                assert_eq!(
                    it.out,
                    b"987\n20\nTypeError\nfloat(9.223372036854776E+18)\n100\nTypeError\nuninit\nNULL\nuninit\nNULL\neffect\n3\n"
                );
                let loop_decl = it.functions["nt_loop"].clone();
                match &it.compiled_fns[&(Rc::as_ptr(&loop_decl) as usize)].3 {
                    super::native::State::Ready(p) => assert!(p.executions.get() > 0, "loop stayed in VM"),
                    _ => panic!("pure nested/backward graph did not execute native code"),
                }
                it.deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(5));
                assert!(super::native::execute(&mut it, &loop_decl, &[super::Slot::V(super::Value::Int(10))]).is_none());
                it.deadline = None;
                let decl = it.functions["nt_fib"].clone();
                match &it.compiled_fns[&(Rc::as_ptr(&decl) as usize)].3 {
                    super::native::State::Ready(p) => assert!(p.executions.get() > 0),
                    _ => panic!("pure recursive graph did not execute native code"),
                }
                // No page capacity: native code must repay all attempted spans and
                // request shared VM replay, without allocating an uncharged page.
                let original = it.vm_stack[0].used;
                it.vm_stack[0].used = it.vm_stack[0].size;
                let charges = it.mem_tracked.len();
                assert!(super::native::execute(
                    &mut it,
                    &decl,
                    &[super::Slot::V(super::Value::Int(6))]
                )
                .is_none());
                assert_eq!(it.vm_stack[0].used, it.vm_stack[0].size);
                assert_eq!(it.mem_tracked.len(), charges);
                it.vm_stack[0].used = original;
                for name in ["nt_add", "nt_depth", "nt_bad", "nt_local", "nt_loop_bad", "nt_loop_local"] {
                    let decl = it.functions[name].clone();
                    assert!(
                        matches!(
                            &it.compiled_fns[&(Rc::as_ptr(&decl) as usize)].3,
                            super::native::State::Disabled
                        ),
                        "guard did not replay {name}"
                    );
                }
                assert_eq!(it.vm_stack.len(), 1);
                assert_eq!(it.vm_stack[0].used, 32 + super::super::VM_FRAME_SLOTS * 16);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn borrowed_receiver_teardown_keeps_argument_and_exception_owners() {
        let mut constructors = Interp::new("promoted-values.php");
        let result = constructors.run_source(
            r#"<?php
class ValueCtor {
 function __construct(private float $x=1.5, public int $n=2) {}
 function values() { return [$this->x, $this->n]; }
}
class ValueCtorChild extends ValueCtor {}
class ValueCtorArgs {
 function __construct(public float $x, public string $s="d") { echo json_encode(func_get_args()), "\n"; }
}
class ValueCtorRef { function __construct(&$x) { $x++; } }
$a = new ValueCtor(3, 4); $weak = WeakReference::create($a);
echo json_encode($a->values()), "\n";
$b = new ValueCtorChild; echo json_encode($b->values()), "\n";
new ValueCtorArgs(4); new ValueCtorArgs(5, "ok");
$x=1; new ValueCtorRef($x); echo $x, "\n";
try { new ValueCtor("bad", 2); } catch (TypeError $e) { echo get_class($e), "\n"; }
unset($a); var_dump($weak->get());
class ReceiverDtor { function __destruct() { echo "D\n"; } }
$a = new ReceiverDtor; $b = $a; unset($a); echo "S\n"; unset($b);
"#,
        );
        assert_eq!(result.exit_code, 0, "{}", constructors.err_buf);
        assert_eq!(constructors.err_buf, "");
        assert_eq!(
            constructors.out,
            b"[3,4]\n[1.5,2]\n[4]\n[5,\"ok\"]\n2\nTypeError\nNULL\nS\nD\n"
        );
        let super::Value::Object(object) = constructors.instantiate("valuector", &[]).unwrap()
        else {
            panic!("object")
        };
        let super::Value::Object(other) = constructors.instantiate("valuector", &[]).unwrap()
        else {
            panic!("object");
        };
        assert_eq!(
            std::mem::size_of_val(&object.borrow().slots[0]),
            std::mem::size_of::<usize>(),
            "ordinary declared-property slots stay one pointer"
        );
        assert!(super::Rc::ptr_eq(
            &object.borrow().prop_order,
            &other.borrow().prop_order
        ));
        {
            let mut first = object.borrow_mut();
            super::Rc::make_mut(&mut first.prop_order).push("extra".into());
            first.prop_insert("extra", super::cell(super::Value::Int(1)));
        }
        assert!(!super::Rc::ptr_eq(
            &object.borrow().prop_order,
            &other.borrow().prop_order
        ));
        assert!(object.borrow().prop_has("extra"));
        assert!(!other.borrow().prop_order.iter().any(|key| key == "extra"));
        assert!(!other.borrow().prop_has("extra"));
        let target = constructors
            .vm_method_target(object, "__construct", &std::cell::RefCell::new(None), 1)
            .expect("empty promoted constructor retains direct target");
        assert!(target.1.comp.value_abi);
        let mut fresh = Interp::new("fresh-float.php");
        let stmts = crate::parser::parse_source(
            "<?php class FreshFloat { function __construct(private float $x, public float $y) {} function values() { return [$this->x,$this->y]; } }",
            false,
        ).unwrap();
        assert!(matches!(
            fresh.exec_block(&stmts),
            super::super::Flow::Normal
        ));
        assert!(fresh.vm_frame_pool.is_empty());
        let mut args = super::super::CallArgs::positional(vec![
            super::cell(super::Value::Int(3)),
            super::cell(super::Value::Float(4.0)),
        ]);
        let argument_ids: Vec<_> = args.cells.iter().map(super::Rc::as_ptr).collect();
        fresh.vm_call_reserve(&mut args, 2);
        let before: Vec<_> = fresh.vm_stack.iter().map(|s| (s.size, s.used)).collect();
        let object = fresh.new_instance("FreshFloat", args).unwrap();
        assert_eq!(fresh.vm_scalar_cell_pool.len(), 2);
        assert!(fresh
            .vm_scalar_cell_pool
            .iter()
            .all(|c| argument_ids.contains(&super::Rc::as_ptr(c))));
        let reused = fresh.vm_scalar_arg(super::Value::Int(11));
        assert!(argument_ids.contains(&super::Rc::as_ptr(&reused)));
        assert!(matches!(*reused.borrow(), super::Value::Int(11)));
        let weak = super::Rc::downgrade(&reused);
        fresh.vm_recycle_scalar_cells(std::iter::once(reused));
        assert!(
            weak.upgrade().is_none(),
            "weak-address observers must prevent recycling"
        );
        let shared = fresh.vm_scalar_arg(super::Value::Int(12));
        let retained = shared.clone();
        fresh.vm_recycle_scalar_cells(std::iter::once(shared));
        assert_eq!(super::Rc::strong_count(&retained), 1);
        assert!(matches!(*retained.borrow(), super::Value::Int(12)));

        let super::Value::Object(inline) = &object else {
            panic!("object")
        };
        let index = inline.borrow().decl_slot("y").unwrap();
        assert!(matches!(
            inline.borrow().slots[index]
                .as_ref()
                .unwrap()
                .borrow()
                .clone(),
            super::Value::Float(4.0)
        ));
        let alias = inline.borrow().prop_get("y").unwrap();
        let canonical = inline.borrow().prop_get("y").unwrap();
        assert!(super::Rc::ptr_eq(&alias, &canonical));
        *alias.borrow_mut() = super::Value::Float(8.0);
        assert!(
            matches!(
                inline.borrow().slots[index]
                    .as_ref()
                    .unwrap()
                    .borrow()
                    .clone(),
                super::Value::Float(8.0)
            ),
            "direct read must use the canonical cell after alias mutation"
        );
        let super::Value::Object(copy) = fresh.clone_object(inline).unwrap() else {
            panic!("clone")
        };
        let copied = copy.borrow().prop_get("y").unwrap();
        assert!(!super::Rc::ptr_eq(&copied, &alias));
        *copied.borrow_mut() = super::Value::Float(9.0);
        assert!(matches!(*alias.borrow(), super::Value::Float(8.0)));
        assert!(copy.borrow_mut().prop_take("y").is_some());
        assert!(!copy.borrow().prop_has("y"));

        assert!(
            fresh.vm_frame_pool.is_empty(),
            "fresh float promotions bypass canonical frames"
        );
        assert_eq!(
            fresh
                .vm_stack
                .iter()
                .map(|s| (s.size, s.used))
                .collect::<Vec<_>>(),
            before
                .iter()
                .map(|(size, used)| (*size, used - (super::super::VM_FRAME_SLOTS + 2) * 16))
                .collect::<Vec<_>>()
        );
        let result = fresh
            .method_invoke_vis(
                match object {
                    super::Value::Object(o) => o,
                    _ => panic!("object"),
                },
                "values",
                super::super::CallArgs::empty(),
                None,
            )
            .unwrap();
        let super::Value::Array(values) = result else {
            panic!("values")
        };
        assert!(matches!(
            *values.borrow().entries.first().unwrap().1.borrow(),
            super::Value::Float(3.0)
        ));
        let mut leaf = Interp::new("float-leaf.php");
        let stmts = crate::parser::parse_source(
            r#"<?php
class ArithmeticLeaf {
 private float $x=3.0;
 private float $y=4.0;
 function length(): float { return sqrt($this->x*$this->x+$this->y*$this->y); }
}
$o = new ArithmeticLeaf;
echo $o->length(), "\n";
"#,
            false,
        )
        .unwrap();
        assert!(
            matches!(leaf.exec_block(&stmts), super::super::Flow::Normal),
            "{}",
            leaf.err_buf
        );
        assert_eq!(leaf.out, b"5\n");
        let super::Value::Object(object) = leaf.globals.vars["o"].borrow().clone() else {
            panic!("receiver")
        };
        let (_, core) = leaf
            .vm_method_target(object.clone(), "length", &std::cell::RefCell::new(None), 1)
            .unwrap();
        assert!(core.comp.float_leaf);
        let pin = object.clone();
        assert!(matches!(
            leaf.vm_float_method(&core, &[], &object),
            Some(super::Value::Float(5.0))
        ));
        let x = object.borrow().prop_get("\0ArithmeticLeaf\0x").unwrap();
        *x.borrow_mut() = super::Value::Int(3);
        assert!(leaf.vm_float_method(&core, &[], &object).is_none());
        *x.borrow_mut() = super::Value::Float(3.0);
        leaf.deadline = Some(std::time::Instant::now());
        assert!(leaf.vm_float_method(&core, &[], &object).is_none());
        leaf.deadline = None;
        drop(pin);
        leaf.globals.vars.remove("o");
        assert!(leaf.vm_float_method(&core, &[], &object).is_none());
        let mut it = Interp::new("receiver.php");
        let result = it.run_source(
            r#"<?php
class TeardownItem { function __destruct() { echo "item\n"; } }
class TeardownHost {
 public int $x = 4;
 function run(int $n): int { return $this->x + $n; }
 function arg(TeardownItem $item): int { return $this->x; }
 function __destruct() { echo "host\n"; }
}
function invoke(TeardownHost $o, int $n): int { return $o->run($n); }
$h = new TeardownHost; $wh = WeakReference::create($h);
echo invoke($h, 2), "\n";
var_dump($wh->get() !== null);
$i = new TeardownItem; $wi = WeakReference::create($i);
echo $h->arg($i), "\n";
unset($i); var_dump($wi->get());
try { invoke($h, 'bad'); } catch (TypeError $e) { echo get_class($e), "\n"; }
echo $h->run(n: 3), "\n";
unset($e); unset($h); var_dump($wh->get());
"#,
        );
        assert_eq!(result.exit_code, 0, "{}", it.err_buf);
        assert_eq!(it.err_buf, "");
        assert_eq!(
            it.out,
            b"6\nbool(true)\n4\nitem\nNULL\nTypeError\n7\nhost\nNULL\n"
        );
        assert!(it
            .vm_frame_pool
            .iter()
            .all(|frame| frame.this_obj.is_none() && frame.vars.is_empty()));
    }

    #[test]
    fn arena_spans_are_repaid_after_return_exception_and_page_extension() {
        assert_eq!(
            std::mem::size_of::<super::Value>(),
            16,
            "keep every VM value compact"
        );
        assert_eq!(
            std::mem::size_of::<crate::value::PhpStr>(),
            std::mem::size_of::<usize>()
        );
        assert_eq!(std::mem::size_of::<crate::value::PhpBytes>(), 32, "bound shared string payload cost (old Vec24)");
        let mut small = crate::value::PhpStr::from_slice(b"a\0b");
        assert!(matches!(&*small.rc, crate::value::PhpBytes::Inline { .. }));
        let alias = small.clone();
        let weak = super::Rc::downgrade(&small.rc);
        drop(small);
        assert!(weak.upgrade().is_some());
        small = alias;
        small.append_unique(&[b'x'; 20]);
        assert!(matches!(
            &*small.rc,
            crate::value::PhpBytes::Inline { len: 23, .. }
        ));
        assert!(
            weak.upgrade().is_none(),
            "unique append detaches weak byte identity"
        );
        small.append_unique(b"y");
        assert!(matches!(&*small.rc, crate::value::PhpBytes::Heap(_)));
        assert_eq!(&small[..3], b"a\0b");
        assert_eq!(small.len(), 24);
        assert_eq!(&small[23..], b"y");
        assert!(crate::value::PhpStr::from_slice(b"").is_empty());
        assert!(matches!(
            &*crate::value::PhpStr::from_slice(&[0; 24]).rc,
            crate::value::PhpBytes::Heap(_)
        ));
        let mut stacked = Interp::new("stacked-scalar.php");
        let stmts = crate::parser::parse_source(
            r#"<?php $out=0;for($i=0;$i<4;$i++) {$out+=sqrt(sqrt(81));$text=substr(strtolower("ABCD"),1,2);}echo $out,":",$text;"#,
            false,
        ).unwrap();
        assert!(matches!(
            stacked.vm_top_exec(&stmts),
            Some(super::super::Flow::Normal)
        ));
        assert_eq!(stacked.out, b"12:bc");
        assert_eq!(stacked.err_buf, "");
        assert_eq!(
            stacked.vm_val_pool.len(),
            1,
            "only the value-stack vector, no scalar argument-vector lease"
        );
        assert!(stacked.vm_val_pool[0].is_empty());
        {
            let mut limits = Interp::new("limit.php");
            limits.ini.insert("memory_limit".into(), "128M".into());
            assert!(limits.statement_boundary().is_none());
            limits.ini.insert("memory_limit".into(), "1K".into());
            assert!(limits.statement_boundary().is_some());
            assert!(limits.mem_exceeded);
        }
        let mut it = Interp::new("arena.php");
        it.mem_sweep();
        let initial: Vec<_> = it.vm_stack.iter().map(|s| (s.size, s.used)).collect();
        let scalar_charges = it.mem_tracked.len();
        let source = super::Value::str("abcd");
        for args in [
            vec![source.clone(), super::Value::Int(0)],
            vec![source.clone(), super::Value::Int(-4)],
            vec![
                source.clone(),
                super::Value::Int(-8),
                super::Value::Int(100),
            ],
        ] {
            let value = it.vm_scalar_builtin("substr", &args).unwrap();
            let (super::Value::Str(original), super::Value::Str(result)) = (&source, value) else {
                panic!("strings")
            };
            assert!(
                super::Rc::ptr_eq(&original.rc, &result.rc),
                "whole range retains source identity"
            );
            assert_eq!(
                it.mem_tracked.len(),
                scalar_charges,
                "no new full-range charge"
            );
        }
        for _ in 0..20 {
            assert!(matches!(
                it.vm_scalar_builtin("sqrt", &[super::Value::Int(9)]),
                Some(super::Value::Float(3.0))
            ));
            let value = it
                .vm_scalar_builtin(
                    "substr",
                    &[source.clone(), super::Value::Int(1), super::Value::Int(2)],
                )
                .unwrap();
            assert!(matches!(value, super::Value::Str(ref s) if &s[..] == b"bc"));
            let super::Value::Str(ref result) = value else {
                unreachable!()
            };
            assert!(
                it.mem_tracked
                    .contains_key(&(super::Rc::as_ptr(&result.rc) as usize)),
                "substring result must be charged while still live"
            );
            assert_eq!(
                it.mem_tracked.len(),
                scalar_charges + 1,
                "one result allocation, no temporary call site"
            );
            drop(value);
            it.mem_sweep();
        }
        assert_eq!(
            it.mem_tracked.len(),
            scalar_charges,
            "fit scalar calls need no temporary site charge"
        );
        assert_eq!(
            it.vm_stack
                .iter()
                .map(|s| (s.size, s.used))
                .collect::<Vec<_>>(),
            initial
        );
        assert!(it
            .vm_scalar_builtin("sqrt", &[super::Value::str("9")])
            .is_none());
        let last = it.vm_stack.last_mut().unwrap();
        let old_used = last.used;
        last.used = last.size;
        assert!(it
            .vm_scalar_builtin("sqrt", &[super::Value::Int(9)])
            .is_none());
        assert_eq!(
            it.vm_stack.last().unwrap().used,
            it.vm_stack.last().unwrap().size
        );
        it.vm_stack.last_mut().unwrap().used = old_used;

        let frame = it.frame_new("direct");
        it.stack.push(frame);
        let charges = it.mem_tracked.len();
        let mut args = super::super::CallArgs::empty();
        it.vm_direct_reserve(&mut args, 2);
        assert!(args.vm_sites.is_empty());
        assert_eq!(it.mem_tracked.len(), charges);
        assert!(it.stack.last().unwrap().vm_span.is_some());
        let frame = it.stack_pop().unwrap();
        assert!(frame.vm_span.is_none());
        it.frame_recycle(frame);
        let result = it.run_source(
            r#"<?php
            function scalar($n) { return $n + 1; }
            function explode_call($n) { return intdiv($n, 0); }
            for ($i = 0; $i < 300; $i++) { scalar($i); }
            try { explode_call(5); } catch (Throwable $e) {}
            scalar(...array_fill(0, 20000, 1));
            for ($i = 0; $i < 300; $i++) { scalar($i); }
            echo 'ok';
        "#,
        );
        assert_eq!(result.exit_code, 0, "{}", it.err_buf);
        assert_eq!(it.out, b"ok");
        assert_eq!(
            it.vm_stack
                .iter()
                .map(|s| (s.size, s.used))
                .collect::<Vec<_>>(),
            initial
        );
        assert!(it.mem_tracked.values().all(|charge| charge.vm.is_none()));
        assert!(it.vm_frame_pool.iter().all(|frame| frame.vm_span.is_none()));
        let mut overlap = Interp::new("substr-overlap.php");
        let result = overlap.run_source(r#"<?php ini_set('memory_limit','4M'); $s=str_repeat('a',1500000); for($i=0;$i<3;$i++) {$t=substr($s,0,1300000);echo strlen($t),"\n";}"#);
        assert_eq!(result.exit_code, 255, "{}", overlap.err_buf);
        let output = String::from_utf8_lossy(&overlap.out);
        assert_eq!(output.lines().next(), Some("1300000"));
        assert!(
            output.contains("tried to allocate 1302528 bytes"),
            "{output}"
        );
        assert!(
            output.contains(": substr("),
            "missing builtin OOM frame: {output}"
        );
    }
}
