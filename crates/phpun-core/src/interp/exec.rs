//! Statement execution: `exec_block`/`exec` plus the loop and
//! foreach drivers — the first seam a bytecode pipeline replaces.

use super::util::*;
use super::*;

impl<'a> Interp<'a> {
    pub fn exec_block(&mut self, stmts: &[Stmt]) -> Flow {
        self.exec_block_from(stmts, 0)
    }

    /// `exec_block` starting partway down the list — used when a
    /// `goto` lands on a label inside it (the label stmt itself is a
    /// no-op; control resumes at the stmt after it).
    fn exec_block_from(&mut self, stmts: &[Stmt], i: usize) -> Flow {
        // The dim/prop write scratch is shared `Interp` state: a nested
        // statement list — a callee, error-handler, eval/include, or
        // destructor body — sees the enclosing op's caches, and its
        // per-statement clears wipe them mid-op. The suspended outer
        // write then re-reads live CVs (bug79793's handler flipping
        // `$key` before the pending `[$key]++` write) and re-emits
        // conversions the op already diagnosed. Park the outer scratch
        // for this list's run; the in-list clears still drop each
        // statement's own leftovers (gc_006).
        let saved_key_conv = std::mem::take(&mut self.dim_key_conv);
        let saved_cv_bound = std::mem::take(&mut self.dim_cv_bound);
        let saved_undef = std::mem::take(&mut self.dim_undef_cells);
        let saved_prop_ov = self.last_prop_ov.take();
        let saved_dyn = std::mem::take(&mut self.fresh_dyn_props);
        let out = self.exec_block_run(stmts, i);
        self.dim_key_conv = saved_key_conv;
        self.dim_cv_bound = saved_cv_bound;
        self.dim_undef_cells = saved_undef;
        self.last_prop_ov = saved_prop_ov;
        self.fresh_dyn_props = saved_dyn;
        out
    }

    fn exec_block_run(&mut self, stmts: &[Stmt], mut i: usize) -> Flow {
        // goto labels bind at the statement-list scope they appear in —
        // a goto bubbling up from nested control flow lands here.
        let mut labels: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for (i, s) in stmts.iter().enumerate() {
            if let Stmt::Label(n) = s {
                labels.entry(n.as_str()).or_insert(i);
            }
        }
        while i < stmts.len() {
            let s = &stmts[i];
            i += 1;
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
            // Per-op scratch tied to a statement's last write op: the
            // prop-cell receiver stash and the dim operand caches hold
            // Rc clones — left over they keep a container externally
            // strong and gc_collect_cycles reads the dead cycle as
            // rooted (gc_006's `$a->a[0] =& $a` then `unset($a)`).
            self.last_prop_ov = None;
            self.dim_key_conv.clear();
            self.dim_cv_bound.clear();
            self.dim_undef_cells.clear();
            match self.exec(s) {
                Flow::Normal => {
                    // Generators that died at this statement (unset(),
                    // overwrite, foreach abandon) replay their
                    // suspended finally chains here — Zend destroys
                    // them at last-ref drop. A destruction-time raise
                    // (force-closed finally yield, parked throwable,
                    // finally-region death) becomes this statement's
                    // error.
                    if !self.live_gens.is_empty() {
                        if let Err(e) = self.gen_gc_sweep(false) {
                            return self.err_flow(e);
                        }
                    }
                }
                Flow::Goto(mut l) => loop {
                    if let Some(&t) = labels.get(l.as_str()) {
                        i = t + 1;
                        break;
                    }
                    match self.exec_label_in(stmts, &l) {
                        // The landing ran to this list's end, or ended
                        // in another goto — the latter loops so it
                        // resolves against this scope's labels too.
                        Some(Flow::Goto(l2)) => l = l2,
                        Some(out) => return out,
                        None => return Flow::Goto(l),
                    }
                },
                f => return f,
            }
        }
        Flow::Normal
    }

    /// A `goto` landing inside this list's nested structure — Zend
    /// binds labels to the whole function op-array, so a label under a
    /// bare block, an `if`/`else` arm, or a try/catch/finally body is
    /// a legal target: control enters that list at the label and runs
    /// to its end, then resumes after the enclosing statement. The
    /// flow gate already rejected landings into loops and switches,
    /// so those bodies are not searched.
    fn exec_label_in(&mut self, stmts: &[Stmt], name: &str) -> Option<Flow> {
        for (idx, s) in stmts.iter().enumerate() {
            if let Stmt::Label(n) = s {
                if n == name {
                    return Some(self.exec_block_from(stmts, idx + 1));
                }
                continue;
            }
            let inner = match s {
                Stmt::Block(b) => self.exec_label_in(b, name),
                Stmt::If { then, else_, .. } => self
                    .exec_label_in(then, name)
                    .or_else(|| self.exec_label_in(else_, name)),
                Stmt::Try {
                    body,
                    catches,
                    finally,
                } => {
                    if let Some(flow) = self.exec_label_in(body, name) {
                        // A landing inside the try arms its catches —
                        // Zend's catch op-ranges cover the target too.
                        let out = self.try_dispatch(flow, catches);
                        Some(self.try_finally(out, finally))
                    } else {
                        let mut hit = None;
                        for c in catches {
                            if let f @ Some(_) = self.exec_label_in(&c.body, name) {
                                hit = f;
                                break;
                            }
                        }
                        if hit.is_some() {
                            // A catch body runs unguarded; the finally
                            // region still runs after it.
                            hit.map(|out| self.try_finally(out, finally))
                        } else if let Some(fb) = finally {
                            // Landing inside the finally region itself:
                            // its tail only — the region does not re-run.
                            self.exec_label_in(fb, name)
                        } else {
                            None
                        }
                    }
                }
                _ => None,
            };
            if let Some(f) = inner {
                // The nested landing consumed control through the
                // ancestor's end; resume this list after it.
                return match f {
                    Flow::Normal => Some(self.exec_block_from(stmts, idx + 1)),
                    other => Some(other),
                };
            }
        }
        None
    }

    /// A loop/switch body: one enclosing context for `break`/`continue`
    /// level counting (zend's loop_var_stack depth).
    fn exec_loop_body(&mut self, stmts: &[Stmt]) -> Flow {
        self.loop_depth += 1;
        let f = self.exec_block(stmts);
        self.loop_depth -= 1;
        f
    }

    fn exec(&mut self, s: &Stmt) -> Flow {
        match s {
            Stmt::Line(l) => {
                self.cur_line = *l;
                Flow::Normal
            }
            // Compile-time diagnostics park before their source stmt;
            // the flow gate already emitted each at its position (they
            // are properties of the compiled unit, not the executed
            // path — `if(0){ echo "${a}"; }` still deprecates).
            Stmt::Diag { line, .. } => {
                self.cur_line = *line;
                Flow::Normal
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
                            // statement value + its expr_temps slot —
                            // and the temps slot must actually pin it,
                            // else a `=&`/`=` result aliased to a live
                            // cell would falsely count 2 (gh10168).
                            if let Value::Object(o) = &v {
                                if Rc::strong_count(o) == 2
                                    && self.expr_temps.iter().any(|t| Rc::ptr_eq(t, o))
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
                                    self.mark_obj_died(o);
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
                    match self.exec_loop_body(body) {
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
                                        // ZEND_CASE (TMP|VAR subjects) is
                                        // noncommutative — subject stays
                                        // left; CONST|CV subjects emit
                                        // IS_EQUAL which pass_two
                                        // commutative-swaps when the case
                                        // operand ranks higher.
                                        let r = compare_operand_rank(cond);
                                        let (x, y) = if (r & 6) == 0 && r < compare_operand_rank(ce)
                                        {
                                            (&v, &cv)
                                        } else {
                                            (&cv, &v)
                                        };
                                        crate::value::clear_cmp_depth_err();
                                        if compare(x, y) == Ordering::Equal {
                                            start = Some(i);
                                        }
                                        if let Err(e) = self.emit_cmp_notices() {
                                            return self.err_flow(e);
                                        }
                                        if crate::value::cmp_depth_err() {
                                            if let Err(e) = self.fail::<()>(PhpError::uncaught(
                                                "Error",
                                                "Nesting level too deep - recursive dependency?",
                                                self.cur_line,
                                            )) {
                                                return self.err_flow(e);
                                            }
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
                        match self.exec_loop_body(body) {
                            Flow::Break(0) | Flow::Break(1) => return Flow::Normal,
                            Flow::Break(n) => return Flow::Break(n - 1),
                            // A `continue` aimed at the switch itself acts
                            // as `break` (Zend warns at compile time, which
                            // our unit gate mirrors); a deeper `continue N`
                            // escapes toward the enclosing loop.
                            Flow::Continue(0) | Flow::Continue(1) => {
                                return Flow::Normal;
                            }
                            Flow::Continue(n) => return Flow::Continue(n - 1),
                            Flow::Normal => {}
                            f => return f,
                        }
                    }
                }
                Flow::Normal
            }
            Stmt::Function(d) => {
                if let Err(e) = self.decl_type_checks(&d.name, d, None) {
                    let e = self.decl_fatal_ctx(e);
                    return self.err_flow(e);
                }
                let key = d.name.to_lowercase();
                let site = std::ptr::from_ref(d) as usize;
                // The decl site early-bound at compile no-ops on
                // execution; a DIFFERENT decl (a conditional decl in an
                // if/loop, or a decl in another unit) claiming the
                // occupied name is the 'Cannot redeclare' fatal — a
                // line+file match is not enough, two decls can share
                // a line.
                let self_decl = self.early_bound_funcs.get(&key) == Some(&(self.cur_unit_id, site));
                if !self_decl {
                    if let Some(prev) = self.functions.get(&key) {
                        let e = self.decl_fatal_ctx(PhpError::fatal(
                            format!(
                                "Cannot redeclare function {}() (previously declared in {}:{})",
                                d.name, prev.file, prev.line
                            ),
                            self.cur_line,
                        ));
                        return self.err_flow(e);
                    }
                }
                let mut d = d.clone();
                d.file = self.diag_file();
                self.functions.insert(key, Rc::new(d));
                Flow::Normal
            }
            Stmt::Class(d) => {
                // Method-decl diagnostics run even for early-bound
                // classes (implicit-nullable deprecations, default-
                // value fatals) — they are decl checks, not
                // registration side effects.
                for m in &d.methods {
                    let fname = format!("{}::{}", d.name, m.decl.name);
                    if let Err(e) =
                        self.decl_type_checks(&fname, &m.decl, Some((&d.name, d.parent.clone())))
                    {
                        let e = self.decl_fatal_ctx(e);
                        return self.err_flow(e);
                    }
                }
                let key = d.name.to_lowercase();
                // The same decl site early-bound for THIS unit is a
                // no-op; a DIFFERENT decl claiming an occupied name is
                // the 'Cannot redeclare' fatal. The unit key guards a
                // freed AST allocation recycled by a later unit's decl.
                if self.early_bound_classes.get(&key)
                    == Some(&(self.cur_unit_id, Rc::as_ptr(d) as usize))
                {
                    return Flow::Normal;
                }
                if let Some((kind, file, line)) = self.existing_class_site(&key) {
                    // The message names the EXISTING decl's kind
                    // (`interface I {} class I {}` → 'Cannot redeclare
                    // interface I').
                    let e = self.decl_fatal_ctx(PhpError::fatal(
                        Self::redeclare_class_msg(kind, &d.name, &file, line),
                        self.cur_line,
                    ));
                    return self.err_flow(e);
                }
                let mut d = (**d).clone();
                for m in &mut d.methods {
                    let mut mm = (**m).clone();
                    mm.decl.file = self.cur_file.clone();
                    *m = Rc::new(mm);
                }
                if let Err(e) = self.register_class(Rc::new(d)) {
                    return self.err_flow(e);
                }
                Flow::Normal
            }
            Stmt::Static { vars, .. } => {
                let mut key = self.fn_statics_key();
                // Static storage keys on the op_array the decl was
                // compiled into: a function body's own table (bare key),
                // eval/include unit code executing inside a frame
                // (`key\0u{unit}` — a fresh table per unit, re-initialized
                // on every call like Zend's fresh op_array), or top-level
                // code where the executing unit itself is the owner.
                let unit = if self.stack.is_empty() {
                    Some(self.cur_unit_id)
                } else {
                    self.stack.last().and_then(|f| f.statics_unit)
                };
                if let Some(u) = unit {
                    key = format!("{}\u{0}u{}", key, u);
                }
                // Site identity: (compile unit, stmt node). A `static $a`
                // redeclared at a different statement in the same scope
                // and unit is a compile fatal — even on the same line
                // (static_basic_002) — while re-executing the same
                // statement (loops) or redeclaring in a different unit
                // — a separate include/eval/run, which Zend compiles to
                // a fresh op_array — is not. The serial (not the file
                // string) keys the unit: a re-parsed unit may recycle
                // the freed Vec's stmt ptr and must still count as new.
                let site = (self.cur_unit_id, vars.as_ptr() as usize);
                for (name, default, var_line) in vars {
                    // Every site is kept: a decl in a different unit is
                    // legal AND must not erase the same-unit record a
                    // later duplicate checks against.
                    let sites = self
                        .static_decls
                        .entry(key.clone())
                        .or_default()
                        .entry(name.clone())
                        .or_default();
                    let dup = sites.iter().any(|(u, l)| u == &site.0 && *l != site.1);
                    sites.insert(site);
                    if dup {
                        // A compile fatal in Zend — carry the compile-
                        // context backtrace (include chain minus context).
                        let mut e = PhpError::compile_fatal(
                            format!("Duplicate declaration of static variable ${}", name),
                            *var_line,
                        );
                        e.trace = Some(self.compile_err_frames());
                        return self.err_flow(e);
                    }
                    // Statics live per-function-decl: inside a function
                    // they never fall back to the top-level table
                    // (static_variation_001).
                    let exists = self.statics.get(&key).and_then(|t| t.get(name).cloned());
                    let cellv = match exists {
                        Some(c) => c,
                        None => {
                            let v = match default {
                                // Runtime init: an unresolved const is a
                                // catchable Error, not silent NULL
                                // (bug79778). `static` initializers are
                                // runtime expressions in Zend — calls,
                                // `new`, and non-static closures all
                                // work (static_initalizer).
                                Some(d) => match self.eval(d) {
                                    Ok(v) => v,
                                    Err(e) => return self.err_flow(e),
                                },
                                None => Value::Null,
                            };
                            let c = cell(v);
                            self.statics
                                .entry(key.clone())
                                .or_default()
                                .insert(name.clone(), c.clone());
                            c
                        }
                    };
                    // The bound cell belongs to the function's statics
                    // table — a suspended gen frame's release must not
                    // null it out from under sibling calls/instances
                    // (bug64979).
                    self.mark_shared(&cellv);
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
                            Expr::Call { .. }
                                | Expr::MethodCall { .. }
                                | Expr::StaticCall { .. }
                                | Expr::StaticCallDyn { .. }
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
            Stmt::Break(e) => self.exec_break_continue(e, true),
            Stmt::Continue(e) => self.exec_break_continue(e, false),
            Stmt::Goto(l) => Flow::Goto(l.clone()),
            Stmt::Label(_) => Flow::Normal,
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
                    // Materialize through the $GLOBALS table first — a
                    // `global $x` after `$GLOBALS['x']=v` sees the
                    // arr-written cell.
                    let gcell = match self.global_var_cell(&name) {
                        Some(c) => c,
                        None => {
                            let c = cell(Value::Null);
                            self.globals.vars.insert(name.clone(), c.clone());
                            self.globals_order.push(name.clone());
                            c
                        }
                    };
                    self.mark_ref(&gcell);
                    self.cur().vars.insert(name, gcell);
                }
                Flow::Normal
            }
            Stmt::Unset(xs) => {
                for x in xs {
                    match x {
                        Expr::Var(n) => {
                            // Global scope: unset($x) ==
                            // unset($GLOBALS['x']) — tombstone the
                            // table entry too.
                            if self.stack.is_empty() {
                                if let Some(arr) = self.globals_arr.clone() {
                                    // The borrow must end before the
                                    // evicted payload's dtors run.
                                    let ak = ArrKey::Str(Rc::from(n.as_str()));
                                    let evicted = arr.borrow_mut().unset(&ak);
                                    if let Some(v) = evicted {
                                        if let Err(e) = self.destruct_dying_value(&v) {
                                            return self.err_flow(e);
                                        }
                                    }
                                }
                                self.globals_synced.remove(n);
                            }
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
                            // zend's unset Errors (non-array offset,
                            // string offsets, object-as-array) are
                            // catchable — they must propagate.
                            if let Err(e) = self.unset_index(e, i.as_deref()) {
                                return self.err_flow(e);
                            }
                        }
                        Expr::Prop { .. } => {
                            if let Err(e) = self.unset_prop(x) {
                                return self.err_flow(e);
                            }
                        }
                        Expr::StaticProp { class, name } => {
                            // zend refuses with a catchable Error —
                            // declared, undeclared and dynamic static
                            // props all throw `Attempt to unset static
                            // property K::$x` instead of deleting.
                            let cls = match self.class_of(class) {
                                Ok(c) => c,
                                Err(e) => {
                                    let te = self.fail::<()>(e).unwrap_err();
                                    return self.err_flow(te);
                                }
                            };
                            let pn = match self.prop_name(name) {
                                Ok(pn) => pn,
                                Err(e) => {
                                    let te = self.fail::<()>(e).unwrap_err();
                                    return self.err_flow(te);
                                }
                            };
                            let e = PhpError::uncaught(
                                "Error",
                                format!("Attempt to unset static property {}::${}", cls.name(), pn),
                                self.cur_line,
                            );
                            if let Err(e) = self.fail::<()>(e) {
                                return self.err_flow(e);
                            }
                        }
                        _ => {}
                    }
                }
                // A released generator replays its suspended
                // finally chains now — unset() is its GC moment.
                match self.gen_gc_sweep(false) {
                    Err(e) => self.err_flow(e),
                    Ok(()) => Flow::Normal,
                }
            }
            Stmt::Try {
                body,
                catches,
                finally,
            } => {
                let flow = self.exec_block(body);
                let out = self.try_dispatch(flow, catches);
                self.try_finally(out, finally)
            }
            Stmt::Namespace(n) => {
                // Top-level scope follows `namespace` declarations —
                // unqualified calls/consts resolve relative to it.
                // Inside an include running in a function frame the
                // file's ns goes on its own include slot instead.
                if let Some((depth, slot)) = self.include_ns.last_mut() {
                    if *depth == self.stack.len() {
                        *slot = n.clone();
                        return Flow::Normal;
                    }
                }
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
                        let mut e = PhpError::compile_fatal(
                            format!("Cannot redeclare constant '{}'", short),
                            self.cur_line,
                        );
                        e.trace = Some(self.compile_err_frames());
                        return self.err_flow(e);
                    }
                    // A top-level `const X = <expr>` decl evaluates its
                    // initializer eagerly in {main} — zend only enters
                    // the lazy const-expr context (its `[constant
                    // expression]()` pseudo-frame) for class-init
                    // exprs (prop/static/class-const), not here.
                    let v = self.eval_const(e);
                    match v {
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

    /// The Throw half of `Stmt::Try` exec — dispatch to the first
    /// matching catch and run its body. Shared with the `goto`
    /// landing path: control jumping into the middle of a try body is
    /// still covered by its catch op-ranges.
    fn try_dispatch(&mut self, flow: Flow, catches: &[Catch]) -> Flow {
        match flow {
            Flow::Throw(v) => {
                let mut result = Flow::Throw(v.clone());
                for c in catches {
                    if self.catch_matches(&v, &c.types) {
                        // The throwable's raise-site stamp is consumed
                        // here — a later engine error must not inherit
                        // its file (a caught include-time throwable
                        // would otherwise poison attribution).
                        self.last_err_file.clear();
                        if let Some(var) = &c.var {
                            // Binding the catch var is a normal
                            // assign — a `&`-bound typed ref gates it
                            // and the TypeError propagates out of the
                            // try (typed_properties_108).
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
        }
    }

    /// The finally half of `Stmt::Try` exec — the region runs after
    /// the body/catches flow and supersedes it unless that flow was
    /// Normal.
    fn try_finally(&mut self, out: Flow, finally: &Option<Vec<Stmt>>) -> Flow {
        match finally {
            Some(fb) => {
                // Output of a finally region inside a generator body
                // is death-time output (Zend replays it when the
                // suspended gen is destroyed) — tag it for fin_q.
                let fin = self.gen_run_state.is_some();
                if fin {
                    self.gen_fin_depth += 1;
                }
                let fr = self.exec_block(fb);
                if fin {
                    self.gen_fin_depth -= 1;
                    // A body error raised inside the region is the
                    // force-close terminal error — destruction
                    // replays it (fin_err) rather than the deferred
                    // resume. (Return/Break inside finally are not
                    // deaths.)
                    if matches!(fr, Flow::Throw(_) | Flow::Exit(_)) && self.gen_fin_depth == 0 {
                        self.gen_fin_err = true;
                    }
                }
                match fr {
                    Flow::Normal => out,
                    f => f,
                }
            }
            None => out,
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

    /// `break`/`continue` — Zend checks the operand (a literal positive
    /// int) and the enclosing loop/switch depth at compile time, so the
    /// operand errors are fatals, not runtime values; escaping the last
    /// context surfaces later as `not in the 'loop' or 'switch' context`
    /// at the unit boundary.
    fn exec_break_continue(&mut self, e: &Option<Expr>, is_break: bool) -> Flow {
        let kw = if is_break { "break" } else { "continue" };
        let operand_fatal = |interp: &mut Self, msg: String| -> Flow {
            let mut e = PhpError::compile_fatal(msg, interp.cur_line);
            e.trace = Some(interp.compile_err_frames());
            interp.err_flow(e)
        };
        let n = match e {
            Some(e) => {
                // `break (2)` is a parenthesized literal — still valid;
                // variables/arithmetic are not supported operands.
                let mut inner = e;
                while let Expr::Paren(p) = inner {
                    inner = p;
                }
                match inner {
                    Expr::Int(i) if *i > 0 => *i as u32,
                    // Any scalar literal that isn't a positive int:
                    // `'break' operator accepts only positive integers`
                    // (zend checks the literal zval's type at compile).
                    Expr::Int(_) | Expr::Float(_) | Expr::Str(_) => {
                        return operand_fatal(
                            self,
                            format!("'{}' operator accepts only positive integers", kw),
                        );
                    }
                    Expr::Interp(parts)
                        if parts
                            .iter()
                            .all(|p| matches!(p, crate::lexer::StringPart::Lit(_))) =>
                    {
                        return operand_fatal(
                            self,
                            format!("'{}' operator accepts only positive integers", kw),
                        );
                    }
                    _ => {
                        return operand_fatal(
                            self,
                            format!(
                                "'{}' operator with non-integer operand is no longer supported",
                                kw
                            ),
                        );
                    }
                }
            }
            None => 1,
        };
        if self.loop_depth > 0 && n > self.loop_depth {
            return operand_fatal(self, format!("Cannot '{}' {} levels", kw, n));
        }
        if is_break {
            Flow::Break(n)
        } else {
            Flow::Continue(n)
        }
    }

    fn exec_while(&mut self, cond: &Expr, body: &[Stmt], do_first: bool) -> Flow {
        if do_first {
            match self.exec_loop_body(body) {
                Flow::Break(0) | Flow::Break(1) => return Flow::Normal,
                Flow::Break(n) => return Flow::Break(n - 1),
                Flow::Normal | Flow::Continue(0) | Flow::Continue(1) => {}
                Flow::Continue(n) => return Flow::Continue(n - 1),
                f => return f,
            }
        }
        loop {
            match self.eval(cond) {
                Ok(c) if !c.is_truthy() => break,
                Err(e) => return self.err_flow(e),
                _ => {}
            }
            match self.exec_loop_body(body) {
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
            // A compile fatal in Zend (`foreach as &$k => $v` dies at
            // compile time with a `{main}`-or-chain backtrace).
            let mut e = PhpError::compile_fatal("Key element cannot be a reference", self.cur_line);
            e.trace = Some(self.compile_err_frames());
            return self.err_flow(e);
        }
        // Diagnostics raised while destructuring an element attribute
        // to the foreach statement itself — capture its line before
        // `arr` evaluation drifts cur_line into arg positions.
        let stmt_line = self.cur_line;
        // A `&` target fetches the source as a writable cell — zend's
        // 'Cannot indirectly modify readonly property' surfaces here
        // (`foreach ($r->a as &$v)`, finding 8), and prop/index
        // sources iterate their real cells.
        let mut arr_e = arr;
        while let Expr::Paren(inner) = arr_e {
            arr_e = inner.as_ref();
        }
        let cell_src = Self::foreach_target_by_ref(val)
            && matches!(
                arr_e,
                Expr::Prop { .. } | Expr::StaticProp { .. } | Expr::Index { .. } | Expr::VarVar(_)
            );
        let src = if cell_src {
            // A `&` source is a by-ref bind — uninitialized typed props
            // report the 'by reference'/'undeclared' catchable.
            let was = std::mem::replace(&mut self.foreach_by_ref, true);
            let c = self.eval_cell(arr);
            self.foreach_by_ref = was;
            match c {
                Ok(c) => c.borrow().clone(),
                Err(e) => return self.err_flow(e),
            }
        } else {
            match self.eval(arr) {
                Ok(v) => v,
                Err(e) => return self.err_flow(e),
            }
        };
        match src {
            Value::Array(rc) => {
                // `foreach ($arr as [&$p, $q])` iterates by reference
                // too — the `&` inside the list target binds the row's
                // real element cells, so writes reach $arr.
                let by_ref = Self::foreach_target_by_ref(val);
                // `&$v` foreach iterates the live array — appends and
                // removals during the loop are observed (foreachLoop.009).
                let live = by_ref;
                if live {
                    // PHP separates a shared array when the loop takes
                    // references to its elements — ref-marked cells stay
                    // bound, everything else copies, so &-writes don't
                    // leak into non-ref elements of other copies. A
                    // deliberately-shared table ($GLOBALS) iterates
                    // in place.
                    let rc = if Rc::strong_count(&rc) > 1 && !rc.borrow().is_ref {
                        let fresh = self.dup_array(&rc.borrow());
                        let nr = Rc::new(RefCell::new(fresh));
                        if let Ok(c) = self.eval_cell(arr) {
                            *c.borrow_mut() = Value::Array(nr.clone());
                        }
                        nr
                    } else {
                        rc
                    };
                    // The table never CoW-splits while the loop holds
                    // it by reference — restored on exit so later
                    // copies separate normally again. Register the loop's
                    // slot cursor (zend's HashTableIterator): the raw index
                    // of the next element to fetch. Tombstoned unsets keep
                    // indices stable, so `unset($a[$k])` mid-loop leaves the
                    // cursor pointing at the element after the removed one
                    // (foreach_008, gh11244, gh13178).
                    let (pos_i, was_shared) = {
                        let mut a = rc.borrow_mut();
                        let i = match a.foreach_pos.iter().position(|p| p.is_none()) {
                            Some(i) => i,
                            None => {
                                a.foreach_pos.push(None);
                                a.foreach_pos.len() - 1
                            }
                        };
                        a.foreach_pos[i] = Some(0);
                        let ws = a.is_ref;
                        a.is_ref = true;
                        (i, ws)
                    };
                    let flow = loop {
                        let next = {
                            let a = rc.borrow();
                            let from = a.foreach_pos[pos_i].unwrap_or(0).min(a.entries.len());
                            a.entries[from..]
                                .iter()
                                .enumerate()
                                .find(|(_, (k, _))| !matches!(k, ArrKey::Tomb))
                                .map(|(d, (k, c))| (from + d, k.clone(), c.clone()))
                        };
                        let Some((idx, k, c)) = next else {
                            break Flow::Normal;
                        };
                        rc.borrow_mut().foreach_pos[pos_i] = Some(idx + 1);
                        if let Some(ForeachKey::Var(kn)) = key {
                            if let Err(e) = self.var_set(kn, key_value(&k)) {
                                break self.err_flow(e);
                            }
                        }
                        match val {
                            ForeachTarget::Var(n) => {
                                // Hoist the clone: after `as &$v` the var
                                // slot can alias this same cell — an inline
                                // `c.borrow()` would outlive var_set and
                                // panic (probe12b).
                                let v = c.borrow().clone();
                                if let Err(e) = self.var_set(n, v) {
                                    break self.err_flow(e);
                                }
                            }
                            ForeachTarget::ByRef(e) => {
                                if let Some(f) = self.readonly_ref_error(&c) {
                                    break f;
                                }
                                // zend leaves the element IS_REFERENCE
                                // — post-loop copies re-bind it.
                                match self.bind_cell(e, c) {
                                    Ok(()) => {}
                                    Err(e2) => break self.err_flow(e2),
                                }
                            }
                            ForeachTarget::Lvalue(e) => {
                                let v = c.borrow().clone();
                                match self.store(e, v) {
                                    Ok(()) => {}
                                    Err(e2) => break self.err_flow(e2),
                                }
                            }
                            ForeachTarget::List(items) => {
                                match self.foreach_list(items, &c, stmt_line) {
                                    Ok(()) => {}
                                    Err(e2) => break self.err_flow(e2),
                                }
                            }
                        }
                        match self.exec_loop_body(body) {
                            Flow::Break(0) | Flow::Break(1) => break Flow::Normal,
                            Flow::Break(n) => break Flow::Break(n - 1),
                            Flow::Continue(0) | Flow::Continue(1) => continue,
                            Flow::Continue(n) => break Flow::Continue(n - 1),
                            Flow::Normal => {}
                            f => break f,
                        }
                    };
                    {
                        let mut a = rc.borrow_mut();
                        a.foreach_pos[pos_i] = None;
                        a.is_ref = was_shared;
                    }
                    return flow;
                }
                // Snapshot (key, cell) pairs — PHP iterates a copy for
                // value-iteration but shares cells for &-iteration.
                let snapshot: Vec<(ArrKey, Cell)> = if by_ref {
                    rc.borrow().iter().cloned().collect()
                } else {
                    // .iter() skips tombstoned buckets — a value-foreach
                    // never sees shifted/unset elements. IS_REFERENCE
                    // elements stay LIVE: zend reads the real bucket,
                    // so writes through a held alias are observed and a
                    // `&$v`-bound var writes back into the array
                    // (probe12c/p12e vs oracle).
                    rc.borrow()
                        .iter()
                        .map(|(k, c)| {
                            let cc = if self.is_ref_cell(c) {
                                c.clone()
                            } else {
                                cell(c.borrow().clone())
                            };
                            (k.clone(), cc)
                        })
                        .collect()
                };
                for (idx, (k, c)) in snapshot.into_iter().enumerate() {
                    self.cur_line = idx;
                    if let Some(ForeachKey::Var(kn)) = key {
                        if let Err(e) = self.var_set(kn, key_value(&k)) {
                            return self.err_flow(e);
                        }
                    }
                    match val {
                        ForeachTarget::Var(n) => {
                            // Hoist the clone: after `as &$v` the var
                            // slot can alias this same cell — an inline
                            // `c.borrow()` would outlive var_set and
                            // panic (probe12b).
                            let v = c.borrow().clone();
                            if let Err(e) = self.var_set(n, v) {
                                return self.err_flow(e);
                            }
                        }
                        ForeachTarget::ByRef(e) => {
                            if let Some(f) = self.readonly_ref_error(&c) {
                                return f;
                            }
                            match self.bind_cell(e, c) {
                                Ok(()) => {}
                                Err(e2) => return self.err_flow(e2),
                            }
                        }
                        ForeachTarget::Lvalue(e) => {
                            let v = c.borrow().clone();
                            match self.store(e, v) {
                                Ok(()) => {}
                                Err(e2) => return self.err_flow(e2),
                            }
                        }
                        ForeachTarget::List(items) => {
                            match self.foreach_list(items, &c, stmt_line) {
                                Ok(()) => {}
                                Err(e2) => return self.err_flow(e2),
                            }
                        }
                    }
                    match self.exec_loop_body(body) {
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
                                return self.exec_foreach_iter(io, key, val, body, stmt_line);
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
                    return self.exec_foreach_iter(o.clone(), key, val, body, stmt_line);
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
                    // are runtime slots checked against the caller —
                    // int-keyed buckets (SPL `[]=` appends) are public
                    // dynamics that bypass name visibility.
                    if !resolved_decl
                        && crate::value::int_prop_index(&dname).is_none()
                        && !self.prop_visible(&cls, &dname)
                    {
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
                                self.mark_ref(&c);
                            }
                        }
                    }
                    if let Some(ForeachKey::Var(kn)) = key {
                        let kv = match crate::value::int_prop_index(&n) {
                            Some(i) => Value::Int(i),
                            None => Value::str(n.clone()),
                        };
                        if let Err(e) = self.var_set(kn, kv) {
                            return self.err_flow(e);
                        }
                    }
                    match val {
                        ForeachTarget::Var(n) => {
                            // Hoist the clone: after `as &$v` the var
                            // slot can alias this same cell — an inline
                            // `c.borrow()` would outlive var_set and
                            // panic (probe12b).
                            let v = c.borrow().clone();
                            if let Err(e) = self.var_set(n, v) {
                                return self.err_flow(e);
                            }
                        }
                        ForeachTarget::ByRef(e) => {
                            // `foreach($o as &$v)` can't hand out a
                            // reference to a readonly property —
                            // 'Cannot acquire reference to readonly
                            // property C::$p' (probe12a vs oracle).
                            if let Some((pd, dcls)) = self.decl_prop(&o, &dname) {
                                if pd.readonly {
                                    let v = self.exception(
                                        "Error",
                                        &format!(
                                            "Cannot acquire reference to readonly property {}::${}",
                                            dcls.name(),
                                            dname
                                        ),
                                    );
                                    let e = self.throw(v);
                                    return self.err_flow(e);
                                }
                            }
                            match self.bind_cell(e, c.clone()) {
                                Ok(()) => {}
                                Err(e2) => return self.err_flow(e2),
                            }
                        }
                        ForeachTarget::Lvalue(e) => {
                            let v = c.borrow().clone();
                            match self.store(e, v) {
                                Ok(()) => {}
                                Err(e2) => return self.err_flow(e2),
                            }
                        }
                        ForeachTarget::List(items) => {
                            if let Err(e2) = self.foreach_list(items, &c, stmt_line) {
                                return self.err_flow(e2);
                            }
                        }
                    }
                    match self.exec_loop_body(body) {
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
        stmt_line: usize,
    ) -> Flow {
        let f = self.exec_foreach_iter_loop(it.clone(), key, val, body, stmt_line);
        // The iterator's temp dies with the foreach — a `new` captured
        // only by the iteration frees here, not at statement end
        // (typed_properties_115: its prop cells must unalias before a
        // later var_dump counts holders).
        if Rc::strong_count(&it) == 2 && self.expr_temps.iter().any(|o| Rc::ptr_eq(o, &it)) {
            self.expr_temps.retain(|o| !Rc::ptr_eq(o, &it));
            let key = Rc::as_ptr(&it) as usize;
            if !self.was_destructed(key)
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

    /// An iterator-method call driven by foreach itself — marked so
    /// diagnostics raised inside attribute the gen body's original
    /// call frame, not a userland `Generator->next()` resume stack.
    fn iter_call(&mut self, it: &Rc<RefCell<PhpObject>>, name: &str) -> Result<Value, PhpError> {
        self.iter_calls += 1;
        let r = self.method_invoke(it.clone(), name, CallArgs::empty());
        self.iter_calls -= 1;
        r
    }

    fn exec_foreach_iter_loop(
        &mut self,
        it: Rc<RefCell<PhpObject>>,
        key: &Option<ForeachKey>,
        val: &ForeachTarget,
        body: &[Stmt],
        stmt_line: usize,
    ) -> Flow {
        if let Err(e) = self.iter_call(&it, "rewind") {
            return self.err_flow(e);
        }
        loop {
            let ok = match self.iter_call(&it, "valid") {
                Ok(v) => v.is_truthy(),
                Err(e) => return self.err_flow(e),
            };
            if !ok {
                break;
            }
            // PHP calls current() before key() on each iteration.
            let v = match self.iter_call(&it, "current") {
                Ok(v) => v,
                Err(e) => return self.err_flow(e),
            };
            if let Some(ForeachKey::Var(kn)) = key {
                let k = match self.iter_call(&it, "key") {
                    Ok(k) => k,
                    Err(e) => return self.err_flow(e),
                };
                if let Err(e) = self.var_set(kn, k) {
                    return self.err_flow(e);
                }
            }
            match val {
                ForeachTarget::Var(n) => {
                    if let Err(e) = self.var_set(n, v) {
                        return self.err_flow(e);
                    }
                }
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
                        Some(ObjectInternal::ArrayIter { store, pos, .. }) => store
                            .borrow()
                            .arr
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
                    match self.bind_cell(n, c) {
                        Ok(()) => {}
                        Err(e2) => return self.err_flow(e2),
                    }
                }
                ForeachTarget::Lvalue(e) => {
                    if let Err(e2) = self.store(e, v) {
                        return self.err_flow(e2);
                    }
                }
                ForeachTarget::List(items) => {
                    if let Err(e2) = self.foreach_list(items, &cell(v), stmt_line) {
                        return self.err_flow(e2);
                    }
                }
            }
            match self.exec_loop_body(body) {
                Flow::Break(0) | Flow::Break(1) => break,
                Flow::Break(n) => return Flow::Break(n - 1),
                Flow::Continue(0) | Flow::Continue(1) => {}
                Flow::Continue(n) => return Flow::Continue(n - 1),
                Flow::Normal => {}
                f => return f,
            }
            if let Err(e) = self.iter_call(&it, "next") {
                return self.err_flow(e);
            }
        }
        Flow::Normal
    }

    /// Whether a foreach value target binds by reference anywhere —
    /// `&$v` itself or any `&` inside a list-destructure
    /// (`foreach ($arr as [&$p, $q])` iterates the array by-ref).
    fn foreach_target_by_ref(t: &ForeachTarget) -> bool {
        match t {
            ForeachTarget::ByRef(_) => true,
            ForeachTarget::List(items) => items
                .iter()
                .flatten()
                .any(|(_, t)| Self::foreach_target_by_ref(t)),
            _ => false,
        }
    }

    /// `Undefined array key` warn for a missing row element —
    /// zend quotes string keys.
    fn foreach_missing_key(&mut self, key: &ArrKey) -> Result<(), PhpError> {
        match key {
            ArrKey::Str(s) => self.warn(&format!("Undefined array key \"{}\"", s)),
            ArrKey::Int(i) => self.warn(&format!("Undefined array key {}", i)),
            ArrKey::Tomb => Ok(()),
        }
    }

    /// Positional/keyed destructuring of a foreach row. `&` elements
    /// bind the row's real element cell — a missing key auto-creates a
    /// null reference silently — while plain elements read the value
    /// and warn `Undefined array key N` on a miss (zend list-in-foreach
    /// semantics). Non-array rows follow zend's matrix: objects (and
    /// scalars/strings under a `&` element) raise catchable Errors, a
    /// plain list on a scalar warns `Cannot use T as array`, and null
    /// is silent — a `&` element auto-vivifies the null row to an
    /// array first.
    fn foreach_list(
        &mut self,
        items: &[Option<(Option<Expr>, ForeachTarget)>],
        c: &Cell,
        line: usize,
    ) -> Result<(), PhpError> {
        self.foreach_list_q(items, c, line, false)
    }

    /// `quiet` suppresses 'Undefined array key' — set for a nested
    /// list's freshly auto-vivified intermediate (`[&$x]` on a
    /// missing slot creates `[]` and inner plain reads stay silent,
    /// matching zend's write-reference fetch).
    fn foreach_list_q(
        &mut self,
        items: &[Option<(Option<Expr>, ForeachTarget)>],
        c: &Cell,
        line: usize,
        quiet: bool,
    ) -> Result<(), PhpError> {
        // Zend attributes destructure diagnostics to the foreach stmt.
        self.cur_line = line;
        let needs_ref = items
            .iter()
            .flatten()
            .any(|(_, t)| Self::foreach_target_by_ref(t));
        enum Row {
            Array(Rc<RefCell<PhpArray>>),
            Skip,
            Warn(String),
            ScalarErr,
            StrOffsetErr,
            ObjectErr(String),
            ArrayAccess(Rc<RefCell<PhpObject>>, String),
        }
        let row = {
            let mut b = c.borrow_mut();
            match &*b {
                Value::Array(rc) => Row::Array(rc.clone()),
                Value::Null if needs_ref => {
                    *b = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
                    match &*b {
                        Value::Array(rc) => Row::Array(rc.clone()),
                        _ => unreachable!(),
                    }
                }
                Value::Null => Row::Skip,
                Value::Str(_) if needs_ref => Row::StrOffsetErr,
                Value::Str(_) => Row::Warn("string".to_string()),
                Value::Object(o) if self.obj_is_a(o, "ArrayAccess") => {
                    let n = o.borrow().class.name().to_string();
                    Row::ArrayAccess(o.clone(), n)
                }
                Value::Object(o) => Row::ObjectErr(o.borrow().class.name().to_string()),
                Value::Callable(_) => Row::ObjectErr("Closure".to_string()),
                _ if needs_ref => Row::ScalarErr,
                other => Row::Warn(other.type_name().to_string()),
            }
        };
        match row {
            Row::Skip => {}
            Row::Warn(t) => self.warn(&format!("Cannot use {} as array", t))?,
            Row::ScalarErr => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    "Cannot use a scalar value as an array",
                    self.cur_line,
                ));
            }
            Row::StrOffsetErr => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    "Cannot create references to/from string offsets",
                    self.cur_line,
                ));
            }
            Row::ObjectErr(name) => {
                return self.fail(PhpError::uncaught(
                    "Error",
                    format!("Cannot use object of type {} as array", name),
                    self.cur_line,
                ));
            }
            Row::ArrayAccess(o, name) => {
                // Destructures via offsetGet; a `&` element binds the
                // returned temp after zend's 'Indirect modification of
                // overloaded element' notice.
                for (i, elem) in items.iter().enumerate() {
                    let Some((ke, t)) = elem else { continue };
                    let keyv = match ke {
                        Some(ke) => self.eval(ke)?,
                        None => Value::Int(i as i64),
                    };
                    let iv = self
                        .method_invoke(
                            o.clone(),
                            "offsetGet",
                            CallArgs::positional(vec![cell(keyv)]),
                        )
                        .unwrap_or(Value::Null);
                    match t {
                        ForeachTarget::Var(n) => self.var_set(n, iv)?,
                        ForeachTarget::Lvalue(e) => {
                            self.store(e, iv)?;
                        }
                        ForeachTarget::ByRef(e) => {
                            self.notice(&format!(
                                "Indirect modification of overloaded element of {} has no effect",
                                name
                            ))?;
                            self.bind_cell(e, cell(iv))?;
                        }

                        ForeachTarget::List(sub) => {
                            self.foreach_list(sub, &cell(iv), line)?;
                        }
                    }
                }
            }
            Row::Array(a) => {
                for (i, elem) in items.iter().enumerate() {
                    let Some((ke, t)) = elem else { continue };
                    let key = match ke {
                        Some(ke) => {
                            let kv = self.eval(ke)?;
                            self.arr_key(&kv)?
                        }
                        None => ArrKey::Int(i as i64),
                    };
                    match t {
                        ForeachTarget::Var(n) => match a.borrow().get(&key) {
                            Some(iv) => self.var_set(n, iv)?,
                            None => {
                                if !quiet {
                                    self.foreach_missing_key(&key)?;
                                }
                                self.var_set(n, Value::Null)?;
                            }
                        },
                        ForeachTarget::Lvalue(e) => match a.borrow().get(&key) {
                            Some(iv) => {
                                self.store(e, iv)?;
                            }
                            None => {
                                if !quiet {
                                    self.foreach_missing_key(&key)?;
                                }
                                self.store(e, Value::Null)?;
                            }
                        },
                        ForeachTarget::ByRef(e) => {
                            let ec = {
                                let mut arr = a.borrow_mut();
                                match arr.get_cell(&key) {
                                    Some(c) => c,
                                    None => {
                                        arr.set_cell(key.clone(), cell(Value::Null));
                                        arr.get_cell(&key).unwrap()
                                    }
                                }
                            };
                            self.bind_cell(e, ec)?;
                        }
                        ForeachTarget::List(sub) => {
                            // A nested list carrying `&` binds the
                            // row's real cell: a missing key
                            // auto-creates a null cell (auto-vivified
                            // to [] inside), and the fresh array's
                            // inner reads stay silent — zend's
                            // write-reference fetch (kd_nested_ref).
                            let sub_ref = sub
                                .iter()
                                .flatten()
                                .any(|(_, t)| Self::foreach_target_by_ref(t));
                            let ec = {
                                let mut arr = a.borrow_mut();
                                match arr.get_cell(&key) {
                                    Some(c) => c,
                                    None if sub_ref => {
                                        let nc = cell(Value::Null);
                                        arr.set_cell(key.clone(), nc.clone());
                                        nc
                                    }
                                    None => {
                                        if !quiet {
                                            self.foreach_missing_key(&key)?;
                                        }
                                        cell(Value::Null)
                                    }
                                }
                            };
                            let fresh = sub_ref && matches!(&*ec.borrow(), Value::Null);
                            self.foreach_list_q(sub, &ec, line, fresh)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
