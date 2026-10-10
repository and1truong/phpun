//! Bytecode control flow and scalar operations with canonical AST bridges.
//! Unsupported bodies retain the AST path; hybrid functions retain binding
//! and frame teardown. Top-level loops reuse globals without a synthetic frame.
//! Oracle probes and regression gates are required for each extension.

use std::cmp::Ordering;
use std::rc::Rc;

use super::calls::builtin_byref;
use super::util::cell;
use crate::ast::{Expr, FunctionDecl, Param, Stmt};
use crate::builtins;
use crate::error::PhpError;
use crate::value::{compare, identical, Cell, FxMap, PhpArray, Value};

use super::{Flow, Interp};

/// A local slot: plain Value for body-assigned locals; params share
/// the call's arg Cell so `$a = v` writes through to what
/// func_get_arg()/func_get_args() report — Zend's CV *is* the arg
/// slot, not a copy.
pub(in crate::interp) enum Slot {
    Uninit,
    V(Value),
    C(Cell),
    /// Read-only parameter in the live frame value vector.
    Arg(u16),
}

pub(in crate::interp) type CompileCacheEntry =
    (Rc<FunctionDecl>, Option<Rc<Compiled>>, &'static str);

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
    /// ctor-promoted params (`$this` writes). Those decls still
    /// compile — they flow through bind_and_run, which runs the body
    /// via vm_bound_exec.
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

fn coverage_on() -> bool {
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

enum Op {
    Const(Value),
    /// Shared AST expression implementation; slots and frame vars alias.
    Canonical(Box<Expr>),
    ThisProp {
        name: Rc<str>,
        site: usize,
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
    /// (`?T`/union-null handled; `float` excluded — an Int arg widens,
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
        let needs_bind = decl.params.iter().any(|p| p.by_ref || p.promoted);
        let mut bind_free = !needs_bind
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
                Op::Canonical(_) | Op::CanonicalStmt { .. } | Op::Foreach { .. }
            )
        });
        let hybrid = canonical_binding || c.ops.iter().any(|op| matches!(op, Op::ThisProp { .. }));
        // ponytail: scalar, read-only params only. Writable/by-ref/hybrid
        // bodies keep cells until lazy promotion supports their full lifetime.
        let value_abi = !hybrid
            && !needs_bind
            && !decl.params.iter().any(|p| p.variadic)
            && !c.ops.iter().any(|op| match op {
                Op::Store(i) | Op::IncDec { slot: i, .. } => (*i as usize) < decl.params.len(),
                _ => false,
            });
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
                    self.expr(value)?;
                    self.emit(Op::BinaryCv(bop, slot));
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
                            self.expr(r)?;
                            let sl = self.slot(n);
                            self.emit(Op::BinaryCv(op, sl));
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
            Expr::Prop {
                obj,
                name: crate::ast::PropName::Name(name),
                nullsafe: false,
                site,
            } if matches!(Interp::unmark_rhs(obj), Expr::Var(n) if n == "this") => {
                self.emit(Op::ThisProp {
                    name: Rc::from(name.as_str()),
                    site: *site,
                    fallback: Box::new(e.clone()),
                });
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
            | Expr::New { .. }
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
                e.insert((decl.clone(), compiled, reason))
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
            || args.named.is_empty()
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
            slots.extend((0..decl.params.len()).map(|i| Slot::Arg(i as u16)));
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
        let r = self.vm_exec(comp, &mut slots, temps_base);
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
        let sweep_err = self.sweep_expr_temps(temps_base).err();
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
            for c in fa.drain(..) {
                // Only uniquely owned, untracked scalar cells can be reused.
                // Trace snapshots, references and captures keep their cells.
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
                    let mut ls = self.conv_bytes(&lv)?;
                    let rs = self.conv_bytes(&rv)?;
                    ls.extend_from_slice(&rs);
                    let nv = Value::bytes(ls);
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
        let mut pc = 0usize;
        let base_depth = if comp.loop_body { self.loop_depth } else { 0 };
        while pc < comp.ops.len() {
            match &comp.ops[pc] {
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
                    fallback,
                } => {
                    self.cur_line = *site;
                    self.send_line = Some(*site);
                    let receiver = self.stack.last().and_then(|f| f.this_obj.clone());
                    let plain = receiver.and_then(|o| self.prop_read_plain(&o, name));
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
                        Slot::Arg(_) => unreachable!("value ABI parameters are read-only"),
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
                        Slot::Arg(_) => unreachable!("value ABI parameters are read-only"),
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
                Op::Call {
                    lname,
                    raw,
                    argc,
                    site,
                    ..
                } => {
                    let n = *argc as usize;
                    let mut argv = self.vm_val_pool.pop().unwrap_or_default();
                    argv.extend(vs.drain(vs.len() - n..));
                    // Direct hits bypass invoke_fn — arg values become
                    // the frame's cells inside vm_run_direct. send_line
                    // still pins this call site: call_site() reads it
                    // for the callee's trace-frame line (review
                    // finding: skipped it attributed callee traces to
                    // the previous call's site).
                    let target = targets.pop().expect("InitCall resolved this call");
                    if comp.top_level {
                        // Callees may unset/rebind globals or run GC. Slot
                        // handles must not pin old bindings during the call.
                        self.vm_materialize(comp, slots);
                    }
                    let result = if let CachedFn::Direct(d, c) = &target {
                        self.send_line = Some(*site);
                        self.vm_run_direct(d, c, &mut argv)
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
            self.stack_pop();
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
    fn callprof_on() -> bool {
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
    /// literal `f()` call carries no class context. The frame still
    /// materializes exactly like invoke_fn_run's fills (args cells for
    /// func_get_args + the dtor pass, vm_sites arena charge, trace).
    /// Arity failures delegate to invoke_fn for the zend error path.
    fn vm_run_direct(
        &mut self,
        decl: &Rc<FunctionDecl>,
        comp: &Compiled,
        argv: &mut Vec<Value>,
    ) -> Result<Value, PhpError> {
        let __p = pnow!();
        let required = decl
            .params
            .iter()
            .rposition(|p| p.default.is_none())
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
        // is unreachable from a literal f() call so None is correct.
        let mut frame = self
            .vm_frame_pool
            .pop()
            .unwrap_or_else(|| super::Frame::new(String::new()));
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
        frame.closure_rc = None;
        frame.call_alias = None;
        frame.statics_unit = None;
        frame.vars.clear();
        frame.args.clear();
        frame.value_args.clear();
        frame.vm_sites.clear();
        let pending_caps = std::mem::take(&mut self.pending_gen_captures);
        for (n, c, by_ref) in pending_caps {
            let c2 = if by_ref { c } else { cell(c.borrow().clone()) };
            frame.vars.insert(n, c2);
        }
        self.stack.push(frame);
        padd!(0, __p);
        let __p = pnow!();
        let mut args = super::CallArgs::empty();
        let value_abi = comp.value_abi
            && !argv.is_empty()
            && argv.len() == decl.params.len()
            && argv.iter().all(|v| {
                matches!(
                    v,
                    Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_)
                )
            })
            && decl.params.iter().enumerate().all(|(i, p)| {
                p.ty.is_none() || comp.param_fast[i].is_some_and(|gate| gate(&argv[i]))
            });
        if value_abi {
            let n = argv.len() as u64;
            std::mem::swap(&mut self.cur().value_args, argv);
            self.vm_call_reserve(&mut args, n);
            padd!(1, __p);
            return self.vm_run(decl, comp, args);
        }
        let mut cells = self.vm_cell_pool.pop().unwrap_or_default();
        cells.extend(argv.drain(..).map(|value| {
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
        }));
        args.cells = cells;
        let n = args.cells.len() as u64;
        self.vm_call_reserve(&mut args, n);
        padd!(1, __p);
        self.vm_run(decl, comp, args)
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
    fn vm_call(
        &mut self,
        lname: &str,
        raw: &str,
        argv: &mut Vec<Value>,
        site: usize,
        target: &CachedFn,
    ) -> Result<Value, PhpError> {
        self.send_line = Some(site);
        let decl = match target {
            CachedFn::Direct(d, _) | CachedFn::Decl(d) => Some(d.clone()),
            CachedFn::Builtin => None,
        };
        let mut cells = self.vm_cell_pool.pop().unwrap_or_default();
        cells.extend(argv.drain(..).map(cell));
        let mut args = super::CallArgs::empty();
        args.cells = cells;
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

    #[test]
    fn arena_spans_are_repaid_after_return_exception_and_page_extension() {
        let mut it = Interp::new("arena.php");
        let initial: Vec<_> = it.vm_stack.iter().map(|s| (s.size, s.used)).collect();
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
    }
}
