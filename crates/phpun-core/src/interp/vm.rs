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
use crate::ast::{Expr, FunctionDecl, Stmt};
use crate::builtins;
use crate::error::PhpError;
use crate::value::{compare, identical, Cell, FxMap, Value};

use super::Interp;

/// A local slot: plain Value for body-assigned locals; params share
/// the call's arg Cell so `$a = v` writes through to what
/// func_get_arg()/func_get_args() report — Zend's CV *is* the arg
/// slot, not a copy.
enum Slot {
    V(Value),
    C(Cell),
}

/// One compiled function body — op vector + slot layout.
pub(crate) struct Compiled {
    ops: Vec<Op>,
    /// Slot count — params occupy the first `decl.params.len()` slots.
    nslots: usize,
    /// Const-folded param defaults for args the caller omitted
    /// (`None` entry = the param is required — unreachable past the
    /// arity check `invoke_fn` already ran).
    defaults: Vec<Option<Value>>,
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
    /// diagnostics and `ns\name` fallback candidates.
    Call {
        lname: Rc<str>,
        raw: Rc<str>,
        argc: u16,
        site: usize,
        callee: usize,
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
        // Return-type checks, by-ref return, by-ref/variadic/promoted/
        // typed params and non-literal defaults all keep the AST path —
        // they need machinery the slot frame doesn't carry.
        if decl.ret.is_some()
            || decl.by_ref
            || decl
                .params
                .iter()
                .any(|p| p.by_ref || p.variadic || p.promoted || p.ty.is_some())
        {
            return None;
        }
        let mut defaults: Vec<Option<Value>> = Vec::with_capacity(decl.params.len());
        for p in &decl.params {
            defaults.push(match &p.default {
                Some(e) => Some(const_val(e)?),
                None => None,
            });
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
        c.ops.push(Op::Const(Value::Null));
        c.ops.push(Op::Return);
        Some(Rc::new(Compiled {
            nslots: c.slots.len(),
            ops: c.ops,
            defaults,
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
    /// binds and no return-type tail (compile gates `decl.ret`).
    pub(in crate::interp) fn vm_run(
        &mut self,
        decl: &FunctionDecl,
        comp: &Compiled,
        mut args: super::CallArgs,
    ) -> Result<Value, PhpError> {
        let saved_line = self.cur_line;
        let saved_prop_ov = self.last_prop_ov.take();
        let saved_dim_by_ref = std::mem::replace(&mut self.dim_by_ref, false);
        let fr = self.call_site_frame(decl, &args);
        self.call_trace.push(fr);
        self.last_call_by_ref = decl.by_ref;
        // Param slots SHARE the arg cells: func_get_arg(i) reads them,
        // and a CV overwrite is the arg write like Zend's shared slot.
        // Positional extras beyond the params ride frame.args too.
        let mut fa: Vec<Cell> = Vec::with_capacity(args.cells.len());
        let mut slots: Vec<Slot> = Vec::with_capacity(comp.nslots);
        for (i, _) in decl.params.iter().enumerate() {
            let c = match args.cells.get(i) {
                Some(c) => c.clone(),
                None => cell(comp.defaults[i].clone().unwrap_or(Value::Null)),
            };
            fa.push(c.clone());
            slots.push(Slot::C(c));
        }
        fa.extend(
            args.cells[decl.params.len().min(args.cells.len())..]
                .iter()
                .cloned(),
        );
        slots.resize_with(comp.nslots, || Slot::V(Value::Null));
        if let Some(f) = self.stack.last_mut() {
            f.vm_sites.append(&mut args.vm_sites);
            f.args = fa;
            args.cells.clear();
        }
        let temps_base = self.expr_temps.len();
        let saved_depth = std::mem::replace(&mut self.loop_depth, 0);
        let r = self.vm_exec(comp, &mut slots, temps_base);
        self.loop_depth = saved_depth;
        let popped = self.stack_pop();
        self.last_popped_frame = popped;
        self.last_call_by_ref = decl.by_ref;
        self.call_trace.pop();
        self.cur_line = saved_line;
        self.send_line = Some(saved_line);
        let sweep_err = self.sweep_expr_temps(temps_base).err();
        let out = if let Some(f) = self.last_popped_frame.take() {
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
        self.last_prop_ov = saved_prop_ov;
        self.dim_by_ref = saved_dim_by_ref;
        out
    }

    fn vm_exec(
        &mut self,
        comp: &Compiled,
        slots: &mut [Slot],
        mut tmark: usize,
    ) -> Result<Value, PhpError> {
        let mut vs: Vec<Value> = Vec::with_capacity(16);
        let mut pc = 0usize;
        loop {
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
                    let v = match *op {
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
                Op::Line(l) => self.cur_line = *l,
                Op::Return => return Ok(vs.pop().unwrap_or(Value::Null)),
                Op::Call {
                    lname,
                    raw,
                    argc,
                    site,
                    callee,
                } => {
                    let n = *argc as usize;
                    let argv: Vec<Value> = vs.split_off(vs.len() - n);
                    let v = self.vm_call(lname, raw, argv, *site, *callee)?;
                    vs.push(v);
                }
            }
            pc += 1;
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

    /// VM-path callee dispatch: userland decls re-enter `invoke_fn`
    /// (which lands back on `vm_run` when the callee compiles),
    /// builtins take `call_builtin`, anything else is the same
    /// undefined-function fatal `call_named` raises. Arg cells are
    /// still materialized — the callee frame's dtor pass needs them.
    fn vm_call(
        &mut self,
        lname: &str,
        raw: &str,
        argv: Vec<Value>,
        site: usize,
        callee: usize,
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
        let mut decl = self.functions.get(lname).cloned();
        let mut miss_name: Option<String> = None;
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
        let cells: Vec<Cell> = argv.into_iter().map(cell).collect();
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
