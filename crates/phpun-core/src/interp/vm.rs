//! Spike bytecode VM (issue #39): a `FunctionDecl` whose body stays
//! inside the supported subset compiles once to a flat `Vec<Op>` and
//! runs on a value stack with locals in slots — no per-arg Cell, no
//! binds vec, no vars-map insert per call. Any construct outside the
//! subset bails the WHOLE body to the AST path, so semantics can't
//! drift: a compiled body by construction never leaves the subset.
//!
//! ponytail: the subset is deliberately narrow (scalars, arith/compare,
//! if/while/for, direct named calls, return). The known ceiling: frame
//! args still materialize cells (dtor coverage via frame.args) and the
//! call arena charge (`vm_sites`) is kept, so the win here is
//! dispatch + locals, not yet arena accounting. Extend coverage by
//! teaching `compile` more constructs — never by loosening runtime
//! semantics.

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
    V(Value),
    C(Cell),
}

/// One compiled function body — op vector + slot layout.
pub(crate) struct Compiled {
    ops: Vec<Op>,
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
}

enum Op {
    Const(Value),
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
    Jump(usize),
    JumpIfFalse(usize),
    JumpIfTrue(usize),
    /// Direct literal name call. `lname` is the lowercase,
    /// `\u{1}`-stripped lookup name; `raw` is the spelling for
    /// diagnostics and `ns\name` fallback candidates. `cache` is the
    /// inline cache: a resolution that can never change is pinned —
    /// a userland decl (functions can't be redeclared) or a builtin
    /// in an empty caller ns (no `ns\name` can appear later to win
    /// the fallback). Late-definable resolutions stay uncached and
    /// re-resolve each call.
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
enum CachedFn {
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
}

impl Compiled {
    pub(crate) fn compile(decl: &FunctionDecl) -> Option<Rc<Compiled>> {
        // A by-ref return needs cell plumbing on Flow::Return — AST
        // keeps it. Everything else still compiles: typed/variadic/
        // promoted params, declared returns and expr defaults get
        // bound + checked by bind_and_run and only the body runs here.
        if decl.by_ref {
            return None;
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
        collect_assigned(&decl.body, &mut assigned);

        let mut c = Compiler {
            ops: Vec::new(),
            slots: FxMap::default(),
            loops: Vec::new(),
            assigned,
        };
        for (i, p) in decl.params.iter().enumerate() {
            c.slots.insert(p.name.clone(), i as u16);
        }
        c.stmts(&decl.body)?;
        // No trailing Const+Return: pc exhausting the stream is
        // Flow::Normal — the bound path's `Flow::Return` arm treats
        // an explicit `return` differently from fall-off-the-end.
        Some(Rc::new(Compiled {
            nslots: c.slots.len(),
            names: c.slots,
            ops: c.ops,
            defaults,
            bind_free,
            needs_bind,
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
            Stmt::Block(b) => collect_assigned(b, out),
            _ => {}
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
                self.emit(Op::Sweep);
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
            Stmt::Break(n) => {
                if !matches!(n, None | Some(Expr::Int(1))) {
                    return None; // `break N` past the innermost loop
                }
                let j = self.emit(Op::Jump(usize::MAX));
                self.loops.last_mut()?.0.push(j);
            }
            Stmt::Continue(n) => {
                if !matches!(n, None | Some(Expr::Int(1))) {
                    return None;
                }
                let j = self.emit(Op::Jump(usize::MAX));
                self.loops.last_mut()?.1.push(j);
            }
            _ => return None,
        }
        Some(())
    }

    fn expr(&mut self, e: &Expr) -> Bail {
        let e = Interp::unmark_rhs(e);
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
            Expr::Paren(i) => return self.expr(i),
            Expr::Var(n) => {
                if n == "this" || !self.assigned.contains(n.as_str()) {
                    return None;
                }
                let sl = self.slot(n);
                self.emit(Op::Load(sl));
            }
            Expr::Assign {
                target, op, value, ..
            } => {
                let Expr::Var(n) = &**target else {
                    return None;
                };
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
                    self.emit(Op::Load(slot));
                    self.expr(value)?;
                    self.emit(Op::Binary(bop));
                }
                self.emit(Op::Store(slot));
            }
            Expr::PreInc(t) | Expr::PreDec(t) | Expr::PostInc(t) | Expr::PostDec(t) => {
                let Expr::Var(n) = &**t else {
                    return None;
                };
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
                    self.expr(l)?;
                    let jf = self.emit(Op::JumpIfFalse(usize::MAX));
                    self.expr(r)?;
                    self.emit(Op::Binary("&&"));
                    let end = self.ops.len();
                    self.patch(jf, end);
                }
                "||" => {
                    self.expr(l)?;
                    let jt = self.emit(Op::JumpIfTrue(usize::MAX));
                    self.expr(r)?;
                    self.emit(Op::Binary("||"));
                    let end = self.ops.len();
                    self.patch(jt, end);
                }
                "+" | "-" | "*" | "/" | "%" | "**" | "." | "&" | "|" | "^" | "<<" | ">>" | "=="
                | "!=" | "===" | "!==" | "<" | "<=" | ">" | ">=" | "<=>" => {
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
                    return None;
                };
                if raw.contains("::") || raw.starts_with('\\') {
                    return None;
                }
                for a in args {
                    match Interp::unmark_arg(a) {
                        Expr::Unpack(_) => return None,
                        Expr::Binary { op: "named", .. } => return None,
                        _ => {}
                    }
                    self.expr(Interp::unmark_arg(a))?;
                }
                let lname: Rc<str> = Rc::from(raw.trim_start_matches('\u{1}').to_lowercase());
                self.emit(Op::Call {
                    lname,
                    raw: Rc::from(raw.trim_start_matches('\u{1}')),
                    argc: args.len() as u16,
                    site: *site,
                    callee: *callee,
                    cache: std::cell::RefCell::new(None),
                });
            }
            _ => return None,
        };
        Some(())
    }
}

impl<'a> Interp<'a> {
    /// Compile-cache: keyed by the decl's Rc pointer, with the Rc kept
    /// alive by the entry itself so a dropped decl can never collide.
    /// `None` entries memoize "doesn't compile" so uncompiled bodies
    /// don't pay the compile walk per call.
    pub(in crate::interp) fn vm_compiled(
        &mut self,
        decl: &Rc<FunctionDecl>,
    ) -> Option<Rc<Compiled>> {
        use std::collections::hash_map::Entry;
        match self.compiled_fns.entry(Rc::as_ptr(decl) as usize) {
            Entry::Occupied(e) => e.get().1.clone(),
            Entry::Vacant(e) => {
                let c = Compiled::compile(decl);
                e.insert((decl.clone(), c.clone()));
                c
            }
        }
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
        let saved = VmSaved {
            line: self.cur_line,
            prop_ov: self.last_prop_ov.take(),
            dim_by_ref: std::mem::replace(&mut self.dim_by_ref, false),
        };
        let fr = self.call_site_frame(decl, &args);
        self.call_trace.push(fr);
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
        if comp.bind_free {
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
        slots.resize_with(comp.nslots, || Slot::V(Value::Null));
        if let Some(f) = self.stack.last_mut() {
            f.vm_sites.append(&mut args.vm_sites);
            f.args = fa;
        }
        let temps_base = self.expr_temps.len();
        let saved_depth = std::mem::replace(&mut self.loop_depth, 0);
        let r = self.vm_exec(comp, &mut slots, temps_base);
        self.loop_depth = saved_depth;
        // The return-type tail runs while the callee's frame, class
        // context and trace frame are still live — `static` resolves
        // against the callee and the TypeError's backtrace must list
        // it (zend checks between exec and the teardown).
        let r = match r {
            Ok(fl) => self.vm_ret_apply(decl, comp, fl),
            Err(e) => Err(e),
        };
        let popped = self.stack_pop();
        self.last_popped_frame = popped;
        self.last_call_by_ref = decl.by_ref;
        self.call_trace.pop();
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
            fa.clear();
            if self.vm_cell_pool.len() < 64 {
                self.vm_cell_pool.push(fa);
            }
            slots.clear();
            if self.vm_slot_pool.len() < 64 {
                self.vm_slot_pool.push(std::mem::take(&mut slots));
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
        // Value-stack/argv vecs come from a per-Interp pool — a call
        // costs no malloc here (cap bounds retention under deep
        // recursion; each live frame holds its own vec anyway).
        let mut vs = self
            .vm_val_pool
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(16));
        let r = self.vm_exec_ops(comp, slots, tmark, &mut vs);
        vs.clear();
        if self.vm_val_pool.len() < 64 {
            self.vm_val_pool.push(vs);
        }
        r
    }

    fn vm_exec_ops(
        &mut self,
        comp: &Compiled,
        slots: &mut [Slot],
        mut tmark: usize,
        vs: &mut Vec<Value>,
    ) -> Result<Flow, PhpError> {
        let mut pc = 0usize;
        while pc < comp.ops.len() {
            match &comp.ops[pc] {
                Op::Const(v) => vs.push(v.clone()),
                Op::Load(s) => vs.push(match &slots[*s as usize] {
                    Slot::V(v) => v.clone(),
                    Slot::C(c) => c.borrow().clone(),
                }),
                Op::Store(s) => match &slots[*s as usize] {
                    Slot::V(_) => slots[*s as usize] = Slot::V(vs.last().unwrap().clone()),
                    Slot::C(c) => {
                        // CV write: new zval lands, displaced decrefs —
                        // same ordering as cell_store.
                        let c = c.clone();
                        let old =
                            std::mem::replace(&mut *c.borrow_mut(), vs.last().unwrap().clone());
                        self.destruct_dying_value(&old)?;
                    }
                },
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
                    // Int×Int scalars bypass the arith/compare
                    // machinery — no type dispatch, no cmp-depth or
                    // notices protocol (scalar-scalar can't trigger
                    // either). Overflow-to-float, div/mod-by-zero and
                    // the INT_MIN/-1 edges drop to the general path
                    // (`self.arith`) so their zend errors stay exact.
                    let fast = if let (Value::Int(a), Value::Int(b)) = (&lv, &rv) {
                        let (a, b) = (*a, *b);
                        match *op {
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
                    let v = match fast {
                        Some(v) => v,
                        None => match *op {
                            "&&" => Value::Bool(rv.is_truthy()),
                            "||" => Value::Bool(rv.is_truthy()),
                            "." => {
                                let grow =
                                    matches!(&lv, Value::Str(s) if Rc::strong_count(&s.rc) == 1);
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
                                let v = match *op {
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
                    };
                    vs.push(v);
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
                    let old = match &slots[*slot as usize] {
                        Slot::V(v) => v.clone(),
                        Slot::C(c) => c.borrow().clone(),
                    };
                    let new = self.incdec_value(&old, *delta)?;
                    match &mut slots[*slot as usize] {
                        Slot::V(v) => *v = new,
                        Slot::C(c) => {
                            let c = c.clone();
                            let prev = std::mem::replace(&mut *c.borrow_mut(), new);
                            self.destruct_dying_value(&prev)?;
                        }
                    }
                    vs.push(if *post {
                        old
                    } else {
                        match &slots[*slot as usize] {
                            Slot::V(v) => v.clone(),
                            Slot::C(c) => c.borrow().clone(),
                        }
                    });
                }
                Op::Echo => {
                    let v = vs.pop().unwrap();
                    let s = self.conv_bytes(&v)?;
                    self.emit_bytes(&s);
                }
                Op::Line(l) => {
                    self.cur_line = *l;
                    // Statement boundary like exec's Stmt::Line — the
                    // caller's pending send_line dies with the stmt.
                    self.send_line = None;
                }
                Op::Return => return Ok(Flow::Return(vs.pop().unwrap_or(Value::Null))),
                Op::Call {
                    lname,
                    raw,
                    argc,
                    site,
                    callee,
                    cache,
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
                    let hit = cache.borrow().clone();
                    let v = if let Some(CachedFn::Direct(d, c)) = &hit {
                        self.send_line = Some(*site);
                        self.vm_run_direct(d, c, &mut argv)?
                    } else {
                        self.vm_call(lname, raw, &mut argv, *site, *callee, cache)?
                    };
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
        let mut slots = self
            .vm_slot_pool
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(comp.nslots));
        slots.resize_with(comp.nslots, || Slot::V(Value::Null));
        if let Some(f) = self.stack.last() {
            for (name, idx) in &comp.names {
                if let Some(c) = f.vars.get(name) {
                    slots[*idx as usize] = Slot::C(c.clone());
                }
            }
        }
        let r = self.vm_exec(comp, &mut slots, self.expr_temps.len());
        // Slot-V objects die with the frame like a CV decref (Slot::C
        // cells are f.vars — bind's own frame teardown owns those).
        let mut dying = Vec::new();
        for sl in &mut slots {
            if let Slot::V(v) = sl {
                if matches!(v, Value::Object(_)) {
                    dying.push(cell(std::mem::replace(v, Value::Null)));
                }
            }
        }
        let derr = self.destruct_cells(&dying).err();
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
        self.call_trace.pop();
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
        frame.vm_sites.clear();
        let pending_caps = std::mem::take(&mut self.pending_gen_captures);
        for (n, c, by_ref) in pending_caps {
            let c2 = if by_ref { c } else { cell(c.borrow().clone()) };
            frame.vars.insert(n, c2);
        }
        self.stack.push(frame);
        let mut args = super::CallArgs::empty();
        let mut cells = self.vm_cell_pool.pop().unwrap_or_default();
        cells.extend(argv.drain(..).map(cell));
        args.cells = cells;
        let n = args.cells.len() as u64;
        self.vm_call_reserve(&mut args, n);
        self.vm_run(decl, comp, args)
    }

    /// VM-path callee dispatch: userland decls re-enter `invoke_fn`
    /// (which lands back on `vm_run` when the callee compiles),
    /// builtins take `call_builtin`, anything else is the same
    /// undefined-function fatal `call_named` raises. Arg cells are
    /// still materialized — the callee frame's dtor pass needs them.
    fn vm_call(
        &mut self,
        lname: &str,
        raw: &str,
        argv: &mut Vec<Value>,
        site: usize,
        callee: usize,
        cache: &std::cell::RefCell<Option<CachedFn>>,
    ) -> Result<Value, PhpError> {
        self.send_line = Some(site);
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
        let hit = cache.borrow().clone();
        let mut decl: Option<Rc<FunctionDecl>> = match &hit {
            Some(CachedFn::Decl(d)) => Some(d.clone()),
            Some(CachedFn::Builtin) => None,
            _ => None,
        };
        let resolved = hit.is_some();
        let mut miss_name: Option<String> = None;
        if !resolved {
            let direct = self.functions.get(lname).cloned();
            decl = direct.clone();
            if decl.is_none() {
                let ns = self.caller_ns();
                if !ns.is_empty() {
                    let cand = format!("{}\\{}", ns.to_lowercase(), lname);
                    decl = self.functions.get(&cand).cloned();
                    if decl.is_none() {
                        miss_name = Some(format!("{}\\{}", ns, raw));
                    }
                }
            }
            // Resolution happens at INIT — an unresolvable name aborts
            // before the (already-evaluated) args would matter.
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
            // Pin only resolutions that can't change: a direct-hit
            // userland decl (no redeclares — and when it also compiles,
            // Direct skips invoke_fn next time), or a builtin reached
            // with an empty caller ns (no ns\name can appear later).
            // ns-fallback decls and namespaced builtin hits re-resolve.
            let stable = if let Some(d) = direct {
                match self.vm_compiled(&d) {
                    Some(c) if !c.needs_bind => Some(CachedFn::Direct(d, c)),
                    _ => Some(CachedFn::Decl(d)),
                }
            } else if decl.is_none() && self.caller_ns().is_empty() {
                Some(CachedFn::Builtin)
            } else {
                None
            };
            if stable.is_some() {
                *cache.borrow_mut() = stable;
            }
        }
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
                format!(
                    "Call to undefined function {}()",
                    miss_name.as_deref().unwrap_or(raw)
                ),
                0,
            ));
        }
        self.invoke_fn(&decl.unwrap(), args, None, None)
    }
}
