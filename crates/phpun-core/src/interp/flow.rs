//! Compile-time lexical checks Zend runs while compiling an op_array:
//! stray `break`/`continue` outside loop/switch contexts and operand
//! limits, `goto` label resolution (undefined label, jump into
//! loop/switch bodies, jumps across `finally` boundaries), duplicate
//! labels, and duplicate `static` declarations — all firing even when
//! the offending code is unreachable or inside a function that is
//! never called, because they are properties of the compiled unit, not
//! the executed path. `continue` whose operand lands on a switch emits
//! Zend's compile-time warning.

use super::*;

/// A breakable context in lexical order (outermost→innermost).
#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Loop(usize),
    Switch(usize),
}

impl Ctx {
    fn id(&self) -> usize {
        match *self {
            Ctx::Loop(i) | Ctx::Switch(i) => i,
        }
    }
}

#[derive(Default)]
struct ScanScope {
    /// Enclosing breakable contexts, outermost→innermost.
    ctxs: Vec<Ctx>,
    /// Try ids of the `finally` regions enclosing the scan position
    /// (outermost→innermost) — Zend's per-element op-range check
    /// (zend_check_finally_breakout) walks all of them.
    fins: Vec<usize>,
    /// Every try-with-finally in this scope, in scan order — Zend's
    /// try_catch_array registration order; the FIRST element whose
    /// range separates goto and label decides the message direction.
    fin_order: Vec<usize>,
    /// `ctxs` depth where the innermost enclosing `finally` began — a
    /// break/continue resolving to a context pushed before it jumps
    /// out of the finally (Zend zend_check_finally_break).
    fin_depth: Option<usize>,
    /// label name → (enclosing loop ids outermost→innermost, fin stack)
    labels: HashMap<String, (Vec<usize>, Vec<usize>)>,
    gotos: Vec<GotoSite>,
    /// static var name → first decl line (dup-decl detection).
    statics: HashMap<String, usize>,
    /// Current source line from the last `Stmt::Line` marker — kept on
    /// the scope so it carries across the top-level per-stmt scans.
    line: usize,
    /// Compile-time warnings and parked `Stmt::Diag` diagnostics
    /// (level, msg, line) collected at their scan position — Zend
    /// emits them while compiling, so an entry already collected
    /// still prints when a later stmt's compile check fails.
    warnings: Vec<(&'static str, String, usize)>,
}

struct GotoSite {
    name: String,
    line: usize,
    /// Loop/switch node ids enclosing the goto (outermost→innermost).
    loops: Vec<usize>,
    fins: Vec<usize>,
}

impl<'a> Interp<'a> {
    /// Run the compile-time flow checks on one freshly parsed unit
    /// (main program, included file, eval'd code). Warnings and diag
    /// diagnostics collected along the way emit in scan order, even
    /// when the scan itself fails — Zend reports them while compiling.
    pub(in crate::interp) fn flow_gate(&mut self, stmts: &[Stmt]) -> Result<(), PhpError> {
        let mut sc = ScanScope::default();
        let r = self
            .flow_unit(stmts, &mut sc)
            .and_then(|_| Self::flow_resolve_gotos(&sc));
        for (level, msg, line) in sc.warnings {
            self.cur_line = line;
            match level {
                "Warning" => self.warn(&msg)?,
                "Notice" => self.notice(&msg)?,
                _ => self.deprecated(&msg)?,
            }
        }
        r
    }

    /// Top level of a unit (or a `namespace {}` body): unconditional
    /// `function` decls early-bind AT THEIR POSITION in Zend's compile
    /// (the function-table insert runs per statement, not after the
    /// unit), so a collision fatal can fire before a later stmt's flow
    /// error — `function a(){} function a(){} break;` reports the
    /// redeclare, `break; function a(){} function a(){}` the break.
    /// Classes are different: their redeclare check stays deferred in
    /// hoist_funcs (`class A{} class A{} break` reports the break).
    fn flow_unit(&mut self, stmts: &[Stmt], sc: &mut ScanScope) -> Result<(), PhpError> {
        for s in stmts {
            if let Stmt::Function(d) = s {
                self.hoist_func(d)?;
            }
            if let Stmt::Block(v) = s {
                if matches!(v.first(), Some(Stmt::Namespace(_))) {
                    // `namespace X { stmts }` — decls inside are still
                    // unconditional top-level for early binding (ns_085).
                    for inner in &v[1..] {
                        if let Stmt::Block(b) = inner {
                            self.flow_unit(b, sc)?;
                        } else {
                            Self::flow_scan(std::slice::from_ref(inner), sc)?;
                        }
                    }
                    continue;
                }
            }
            Self::flow_scan(std::slice::from_ref(s), sc)?;
        }
        Ok(())
    }

    /// Scan one label/goto scope: the unit's top level or one function
    /// body (labels and `static` declarations bind per function).
    fn flow_scope(stmts: &[Stmt], sc: &mut ScanScope) -> Result<(), PhpError> {
        Self::flow_scan(stmts, sc)?;
        Self::flow_resolve_gotos(sc)
    }

    /// Zend resolves a function's gotos when its op_array finishes
    /// compiling — after every label in it was collected.
    fn flow_resolve_gotos(sc: &ScanScope) -> Result<(), PhpError> {
        for g in &sc.gotos {
            match sc.labels.get(&g.name) {
                None => {
                    return Err(PhpError::compile_fatal(
                        format!("'goto' to undefined label '{}'", g.name),
                        g.line,
                    ));
                }
                Some((loops, fins)) => {
                    if *fins != g.fins {
                        // zend_check_finally_breakout: the FIRST try
                        // element (registration order) whose finally
                        // range separates goto and label decides —
                        // 'into' when only the label is inside, 'out
                        // of' when only the goto is.
                        for e in &sc.fin_order {
                            let g_in = g.fins.contains(e);
                            let l_in = fins.contains(e);
                            if !g_in && l_in {
                                return Err(PhpError::compile_fatal(
                                    "jump into a finally block is disallowed",
                                    g.line,
                                ));
                            }
                            if g_in && !l_in {
                                return Err(PhpError::compile_fatal(
                                    "jump out of a finally block is disallowed",
                                    g.line,
                                ));
                            }
                        }
                    }
                    if !loops.iter().all(|id| g.loops.contains(id)) {
                        return Err(PhpError::compile_fatal(
                            "'goto' into loop or switch statement is disallowed",
                            g.line,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// A fresh scope for one function body — its own labels, statics
    /// and a reset loop depth — resolved before returning.
    fn flow_fn(
        d: &FunctionDecl,
        warns_out: &mut Vec<(&'static str, String, usize)>,
    ) -> Result<(), PhpError> {
        let mut sc = ScanScope::default();
        let r = Self::flow_scope(&d.body, &mut sc);
        warns_out.append(&mut sc.warnings);
        r
    }

    fn flow_class(
        d: &ClassDecl,
        warns_out: &mut Vec<(&'static str, String, usize)>,
    ) -> Result<(), PhpError> {
        for m in &d.methods {
            Self::flow_fn(&m.decl, warns_out)?;
        }
        // Property hooks carry a body too (scoped like methods).
        for p in &d.props {
            if let Some(hooks) = &p.hooks {
                for h in hooks {
                    if let Some(body) = &h.body {
                        let mut sc = ScanScope::default();
                        let r = Self::flow_scope(body, &mut sc);
                        warns_out.append(&mut sc.warnings);
                        r?;
                    }
                }
            }
        }
        Ok(())
    }

    fn flow_scan(stmts: &[Stmt], sc: &mut ScanScope) -> Result<(), PhpError> {
        for s in stmts {
            match s {
                Stmt::Line(l) => sc.line = *l,
                // Parked compile-time diagnostic — emitted at this
                // scan position, so a later stmt's compile-fatal
                // still lets it print (exec is a no-op for it).
                Stmt::Diag { level, msg, line } => {
                    sc.warnings.push((*level, msg.clone(), *line));
                }
                Stmt::Function(d) => {
                    Self::flow_fn(d, &mut sc.warnings)?;
                }
                Stmt::Class(d) => {
                    Self::flow_class(d, &mut sc.warnings)?;
                }
                Stmt::Break(op) => Self::flow_operand(op.as_ref(), sc.line, true, sc)?,
                Stmt::Continue(op) => Self::flow_operand(op.as_ref(), sc.line, false, sc)?,
                Stmt::Goto(n) => sc.gotos.push(GotoSite {
                    name: n.clone(),
                    line: sc.line,
                    loops: sc.ctxs.iter().map(|c| c.id()).collect(),
                    fins: sc.fins.clone(),
                }),
                Stmt::Label(n) => {
                    let site = (sc.ctxs.iter().map(|c| c.id()).collect(), sc.fins.clone());
                    if sc.labels.insert(n.clone(), site).is_some() {
                        return Err(PhpError::compile_fatal(
                            format!("Label '{}' already defined", n),
                            sc.line,
                        ));
                    }
                }
                Stmt::Static { vars, .. } => {
                    for (name, _, vl) in vars {
                        // Zend reports the redeclared var's own
                        // declarator line, not the `static` keyword's.
                        if sc.statics.insert(name.clone(), *vl).is_some() {
                            return Err(PhpError::compile_fatal(
                                format!("Duplicate declaration of static variable ${}", name),
                                *vl,
                            ));
                        }
                    }
                }
                Stmt::While { cond, body } => {
                    Self::flow_expr(cond, sc)?;
                    sc.ctxs.push(Ctx::Loop(std::ptr::from_ref(s) as usize));
                    Self::flow_scan(body, sc)?;
                    sc.ctxs.pop();
                }
                Stmt::DoWhile { body, cond } => {
                    sc.ctxs.push(Ctx::Loop(std::ptr::from_ref(s) as usize));
                    Self::flow_scan(body, sc)?;
                    sc.ctxs.pop();
                    Self::flow_expr(cond, sc)?;
                }
                Stmt::For {
                    init,
                    cond,
                    inc,
                    body,
                } => {
                    for e in init.iter().chain(cond.iter()).chain(inc.iter()) {
                        Self::flow_expr(e, sc)?;
                    }
                    sc.ctxs.push(Ctx::Loop(std::ptr::from_ref(s) as usize));
                    Self::flow_scan(body, sc)?;
                    sc.ctxs.pop();
                }
                Stmt::Foreach { arr, val, body, .. } => {
                    Self::flow_expr(arr, sc)?;
                    Self::flow_foreach_target(val, sc)?;
                    sc.ctxs.push(Ctx::Loop(std::ptr::from_ref(s) as usize));
                    Self::flow_scan(body, sc)?;
                    sc.ctxs.pop();
                }
                Stmt::Switch { cond, cases } => {
                    Self::flow_expr(cond, sc)?;
                    sc.ctxs.push(Ctx::Switch(std::ptr::from_ref(s) as usize));
                    for (c, b) in cases {
                        if let Some(c) = c {
                            Self::flow_expr(c, sc)?;
                        }
                        Self::flow_scan(b, sc)?;
                    }
                    sc.ctxs.pop();
                }
                Stmt::Try {
                    body,
                    catches,
                    finally,
                } => {
                    if finally.is_some() {
                        // try_catch_array registers the element when
                        // the try compiles — before its body — so
                        // outer trys are checked before inner ones.
                        sc.fin_order.push(std::ptr::from_ref(s) as usize);
                    }
                    Self::flow_scan(body, sc)?;
                    for c in catches {
                        Self::flow_scan(&c.body, sc)?;
                    }
                    if let Some(f) = finally {
                        // A finally region is closed to gotos in either
                        // direction — the stack marks its boundaries.
                        sc.fins.push(std::ptr::from_ref(s) as usize);
                        // …and to break/continue operands resolving to a
                        // context outside it — the ctx depth marks that line.
                        let saved_depth = sc.fin_depth.replace(sc.ctxs.len());
                        Self::flow_scan(f, sc)?;
                        sc.fins.pop();
                        sc.fin_depth = saved_depth;
                    }
                }
                Stmt::Block(b) => Self::flow_scan(b, sc)?,
                Stmt::If { cond, then, else_ } => {
                    Self::flow_expr(cond, sc)?;
                    Self::flow_scan(then, sc)?;
                    Self::flow_scan(else_, sc)?;
                }
                Stmt::Echo(es) => {
                    for e in es {
                        Self::flow_expr(e, sc)?;
                    }
                }
                Stmt::Expr(e) => Self::flow_expr(e, sc)?,
                Stmt::Return(Some(e)) => Self::flow_expr(e, sc)?,
                Stmt::Global(v) | Stmt::Unset(v) => {
                    for e in v {
                        Self::flow_expr(e, sc)?;
                    }
                }
                Stmt::Declare { value, .. } => Self::flow_expr(value, sc)?,
                Stmt::ConstDecl(v) => {
                    for (_, e) in v {
                        Self::flow_expr(e, sc)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// break/continue operand checks mirroring Zend's compile rules:
    /// non-literal operands are "no longer supported", non-positive
    /// ints "accept only positive integers", n>depth is "Cannot … N
    /// levels", and a plain stray break/continue outside any context
    /// is "not in the 'loop' or 'switch' context". A continue whose
    /// target lands on a switch warns (the "continue N" hint when any
    /// breakable context encloses the switch).
    fn flow_operand(
        op: Option<&Expr>,
        line: usize,
        is_break: bool,
        sc: &mut ScanScope,
    ) -> Result<(), PhpError> {
        let kw = if is_break { "break" } else { "continue" };
        let n = match op {
            None => 1usize,
            Some(e) => match Self::flow_op_lit(e) {
                Some(Some(i)) => {
                    if i <= 0 {
                        return Err(PhpError::compile_fatal(
                            format!("'{}' operator accepts only positive integers", kw),
                            line,
                        ));
                    }
                    i as usize
                }
                // Other literals — including constant strings and
                // floats — still get the "positive integers"
                // diagnostic (`break "2"`, `break 1.5`).
                Some(None) => {
                    return Err(PhpError::compile_fatal(
                        format!("'{}' operator accepts only positive integers", kw),
                        line,
                    ));
                }
                // Non-literals — including `true`/`false` — get the
                // removed-operand diagnostic (`break "$x"`,
                // `break true`).
                None => {
                    return Err(PhpError::compile_fatal(
                        format!(
                            "'{}' operator with non-integer operand is no longer supported",
                            kw
                        ),
                        line,
                    ));
                }
            },
        };
        let len = sc.ctxs.len();
        if n > len {
            if len == 0 {
                return Err(PhpError::compile_fatal(
                    format!("'{}' not in the 'loop' or 'switch' context", kw),
                    line,
                ));
            }
            return Err(PhpError::compile_fatal(
                format!("Cannot '{}' {} levels", kw, n),
                line,
            ));
        }
        // Zend resolves the operand target before applying
        // zend_check_finally_breakout — a continue landing on a switch
        // reports the equivalence warning even when the jump then
        // fatals for leaving a finally (breakout_finally_*).
        if !is_break {
            if let Ctx::Switch(_) = sc.ctxs[len - n] {
                // The operand lands on a switch — equivalent to break.
                // Zend prints the operand digits for n>=2 and offers
                // the continue-N hint whenever ANY breakable context
                // encloses the switch (another switch qualifies).
                let mut msg = if n == 1 {
                    "\"continue\" targeting switch is equivalent to \"break\"".to_string()
                } else {
                    format!(
                        "\"continue {}\" targeting switch is equivalent to \"break {}\"",
                        n, n
                    )
                };
                if len - n > 0 {
                    msg.push_str(&format!(". Did you mean to use \"continue {}\"?", n + 1));
                }
                sc.warnings.push(("Warning", msg, line));
            }
        }
        // A resolved target pushed before the innermost enclosing
        // `finally` means the jump leaves it — Zend compile-fatals any
        // such break/continue (break on an inner switch is fine).
        if let Some(d) = sc.fin_depth {
            if len - n < d {
                return Err(PhpError::compile_fatal(
                    "jump out of a finally block is disallowed",
                    line,
                ));
            }
        }
        Ok(())
    }

    /// Literal-ness of a break/continue operand: an int resolves
    /// (`Some(Some(n))`), other scalars and constant strings are the
    /// positive-integers diagnostic (`Some(None)`), and anything else —
    /// `true`/`false`, interpolated strings with variables, expressions —
    /// is the removed-operand diagnostic (`None`). Parentheses and
    /// pure-literal interpolations count as their contents.
    fn flow_op_lit(e: &Expr) -> Option<Option<i64>> {
        match e {
            Expr::Int(i) => Some(Some(*i)),
            Expr::Float(_) | Expr::Str(_) => Some(None),
            Expr::Interp(ps)
                if ps
                    .iter()
                    .all(|p| matches!(p, crate::lexer::StringPart::Lit(_))) =>
            {
                Some(None)
            }
            Expr::Paren(e) => Self::flow_op_lit(e),
            _ => None,
        }
    }

    /// Nested function bodies hide inside expressions (closures,
    /// anonymous classes) — each is its own label/static scope.
    fn flow_expr(e: &Expr, sc: &mut ScanScope) -> Result<(), PhpError> {
        match e {
            Expr::Closure(c) => Self::flow_fn(&c.decl, &mut sc.warnings),
            Expr::AnonClass(d) => Self::flow_class(d, &mut sc.warnings),
            Expr::Assign { target, value, .. } => {
                Self::flow_expr(target, sc)?;
                Self::flow_expr(value, sc)
            }
            Expr::Binary { l, r, .. } => {
                Self::flow_expr(l, sc)?;
                Self::flow_expr(r, sc)
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
            | Expr::YieldFrom(e)
            | Expr::Include { e, .. } => Self::flow_expr(e, sc),
            Expr::Exit(e) => match e {
                Some(e) => Self::flow_expr(e, sc),
                None => Ok(()),
            },
            Expr::Yield { key, val } => {
                if let Some(k) = key {
                    Self::flow_expr(k, sc)?;
                }
                if let Some(v) = val {
                    Self::flow_expr(v, sc)?;
                }
                Ok(())
            }
            Expr::Ternary { c, t, f } => {
                Self::flow_expr(c, sc)?;
                if let Some(t) = t {
                    Self::flow_expr(t, sc)?;
                }
                Self::flow_expr(f, sc)
            }
            Expr::Instanceof { obj, class } => {
                Self::flow_expr(obj, sc)?;
                Self::flow_expr(class, sc)
            }
            Expr::Call { name, args } => {
                Self::flow_expr(name, sc)?;
                for a in args {
                    Self::flow_expr(a, sc)?;
                }
                Ok(())
            }
            Expr::Index { e, i } => {
                Self::flow_expr(e, sc)?;
                if let Some(i) = i {
                    Self::flow_expr(i, sc)?;
                }
                Ok(())
            }
            Expr::Isset(v) => {
                for e in v {
                    Self::flow_expr(e, sc)?;
                }
                Ok(())
            }
            Expr::List(v) => {
                for e in v.iter().flatten() {
                    Self::flow_expr(e, sc)?;
                }
                Ok(())
            }
            Expr::ArrayLit(v) => {
                for (k, e) in v {
                    if let Some(k) = k {
                        Self::flow_expr(k, sc)?;
                    }
                    Self::flow_expr(e, sc)?;
                }
                Ok(())
            }

            Expr::Match { subject, arms } => {
                Self::flow_expr(subject, sc)?;
                for a in arms {
                    for c in &a.conds {
                        Self::flow_expr(c, sc)?;
                    }
                    Self::flow_expr(&a.result, sc)?;
                }
                Ok(())
            }
            Expr::New { class, args } => {
                Self::flow_expr(class, sc)?;
                for a in args {
                    Self::flow_expr(a, sc)?;
                }
                Ok(())
            }
            Expr::Prop { obj, name, .. } => {
                Self::flow_expr(obj, sc)?;
                if let PropName::Expr(e) = name {
                    Self::flow_expr(e, sc)?;
                }
                Ok(())
            }
            Expr::MethodCall {
                obj, name, args, ..
            } => {
                Self::flow_expr(obj, sc)?;
                if let PropName::Expr(e) = name {
                    Self::flow_expr(e, sc)?;
                }
                for a in args {
                    Self::flow_expr(a, sc)?;
                }
                Ok(())
            }
            Expr::StaticProp { class, name } => {
                Self::flow_expr(class, sc)?;
                if let PropName::Expr(e) = name {
                    Self::flow_expr(e, sc)?;
                }
                Ok(())
            }
            Expr::StaticCall { class, args, .. } => {
                Self::flow_expr(class, sc)?;
                for a in args {
                    Self::flow_expr(a, sc)?;
                }
                Ok(())
            }
            Expr::StaticCallDyn { class, name, args } => {
                Self::flow_expr(class, sc)?;
                Self::flow_expr(name, sc)?;
                for a in args {
                    Self::flow_expr(a, sc)?;
                }
                Ok(())
            }
            Expr::ClassConst { class, .. } => Self::flow_expr(class, sc),
            Expr::Null
            | Expr::Bool(_)
            | Expr::Int(_)
            | Expr::Float(_)
            | Expr::Str(_)
            | Expr::Var(_)
            | Expr::Const(_)
            | Expr::Interp(_)
            | Expr::MagicConst(_)
            | Expr::FccMark => Ok(()),
        }
    }

    fn flow_foreach_target(t: &ForeachTarget, sc: &mut ScanScope) -> Result<(), PhpError> {
        match t {
            ForeachTarget::Lvalue(e) => Self::flow_expr(e, sc),
            ForeachTarget::Var(_) | ForeachTarget::ByRef(_) => Ok(()),
            ForeachTarget::List(ts) => {
                for t in ts.iter().flatten() {
                    Self::flow_foreach_target(t, sc)?;
                }
                Ok(())
            }
        }
    }
}
