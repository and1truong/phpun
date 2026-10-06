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
    /// Innermost enclosing `finally` node id (gotos must not cross it).
    fin: Option<usize>,
    /// `ctxs` depth where the innermost enclosing `finally` began — a
    /// break/continue resolving to a context pushed before it jumps
    /// out of the finally (Zend zend_check_finally_break).
    fin_depth: Option<usize>,
    /// label name → (enclosing loop ids outermost→innermost, fin id)
    labels: HashMap<String, (Vec<usize>, Option<usize>)>,
    gotos: Vec<GotoSite>,
    /// static var name → first decl line (dup-decl detection).
    statics: HashMap<String, usize>,
    /// Current source line from the last `Stmt::Line` marker — kept on
    /// the scope so it carries across the top-level per-stmt scans.
    line: usize,
    warns: Vec<(String, usize)>,
}

struct GotoSite {
    name: String,
    line: usize,
    /// Loop/switch node ids enclosing the goto (outermost→innermost).
    loops: Vec<usize>,
    fin: Option<usize>,
}

impl<'a> Interp<'a> {
    /// Run the compile-time flow checks on one freshly parsed unit
    /// (main program, included file, eval'd code). Warnings collected
    /// along the way emit in scan order, even when the scan itself
    /// fails — Zend reports them while compiling.
    pub(in crate::interp) fn flow_gate(&mut self, stmts: &[Stmt]) -> Result<(), PhpError> {
        let mut sc = ScanScope::default();
        let r = self
            .flow_unit(stmts, &mut sc)
            .and_then(|_| Self::flow_resolve_gotos(&sc));
        for (msg, line) in sc.warns {
            self.cur_line = line;
            let _ = self.warn(&msg);
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
                Some((loops, fin)) => {
                    if *fin != g.fin {
                        if fin.is_some() {
                            return Err(PhpError::compile_fatal(
                                "jump into a finally block is disallowed",
                                g.line,
                            ));
                        }
                        return Err(PhpError::compile_fatal(
                            "jump out of a finally block is disallowed",
                            g.line,
                        ));
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
    fn flow_fn(d: &FunctionDecl, warns_out: &mut Vec<(String, usize)>) -> Result<(), PhpError> {
        let mut sc = ScanScope::default();
        let r = Self::flow_scope(&d.body, &mut sc);
        warns_out.append(&mut sc.warns);
        r
    }

    fn flow_class(d: &ClassDecl, warns_out: &mut Vec<(String, usize)>) -> Result<(), PhpError> {
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
                        warns_out.append(&mut sc.warns);
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
                Stmt::Function(d) => {
                    Self::flow_fn(d, &mut sc.warns)?;
                }
                Stmt::Class(d) => {
                    Self::flow_class(d, &mut sc.warns)?;
                }
                Stmt::Break(op) => Self::flow_operand(op.as_ref(), sc.line, true, sc)?,
                Stmt::Continue(op) => Self::flow_operand(op.as_ref(), sc.line, false, sc)?,
                Stmt::Goto(n) => sc.gotos.push(GotoSite {
                    name: n.clone(),
                    line: sc.line,
                    loops: sc.ctxs.iter().map(|c| c.id()).collect(),
                    fin: sc.fin,
                }),
                Stmt::Label(n) => {
                    let site = (sc.ctxs.iter().map(|c| c.id()).collect(), sc.fin);
                    if sc.labels.insert(n.clone(), site).is_some() {
                        return Err(PhpError::compile_fatal(
                            format!("Label '{}' already defined", n),
                            sc.line,
                        ));
                    }
                }
                Stmt::Static { vars, line: sl } => {
                    for (name, _) in vars {
                        if sc.statics.insert(name.clone(), *sl).is_some() {
                            return Err(PhpError::compile_fatal(
                                format!("Duplicate declaration of static variable ${}", name),
                                *sl,
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
                    Self::flow_scan(body, sc)?;
                    for c in catches {
                        Self::flow_scan(&c.body, sc)?;
                    }
                    if let Some(f) = finally {
                        // A finally region is closed to gotos in either
                        // direction — the innermost id marks the boundary.
                        let saved = sc.fin.replace(std::ptr::from_ref(s) as usize);
                        // …and to break/continue operands resolving to a
                        // context outside it — the ctx depth marks that line.
                        let saved_depth = sc.fin_depth.replace(sc.ctxs.len());
                        Self::flow_scan(f, sc)?;
                        sc.fin = saved;
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
    /// target lands on a switch warns (the "continue 2" hint only when
    /// a loop encloses the switch).
    fn flow_operand(
        op: Option<&Expr>,
        line: usize,
        is_break: bool,
        sc: &mut ScanScope,
    ) -> Result<(), PhpError> {
        let kw = if is_break { "break" } else { "continue" };
        // `break (2)`/`break (expr)` — parens and the arg's line
        // marker wrap the literal; peel both for the operand check.
        let mut op = op;
        while let Some(
            Expr::Paren(inner)
            | Expr::Binary {
                op: "argline",
                r: inner,
                ..
            },
        ) = op
        {
            op = Some(inner);
        }
        let n = match op {
            None => 1usize,
            Some(Expr::Int(i)) => {
                if *i <= 0 {
                    return Err(PhpError::compile_fatal(
                        format!("'{}' operator accepts only positive integers", kw),
                        line,
                    ));
                }
                *i as usize
            }
            // Other literals are still "positive integers" diagnostics;
            // expressions are the removed-operand diagnostic.
            Some(Expr::Float(_)) | Some(Expr::Str(_)) | Some(Expr::Bool(_)) => {
                return Err(PhpError::compile_fatal(
                    format!("'{}' operator accepts only positive integers", kw),
                    line,
                ));
            }
            Some(_) => {
                return Err(PhpError::compile_fatal(
                    format!(
                        "'{}' operator with non-integer operand is no longer supported",
                        kw
                    ),
                    line,
                ));
            }
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
        if !is_break {
            if let Ctx::Switch(_) = sc.ctxs[len - n] {
                // The operand lands on a switch — equivalent to break,
                // with Zend's continue-N hint when a loop encloses it.
                let hint = sc.ctxs[..len - n].iter().any(|c| matches!(c, Ctx::Loop(_)));
                let mut msg =
                    "\"continue\" targeting switch is equivalent to \"break\"".to_string();
                if hint {
                    msg.push_str(&format!(". Did you mean to use \"continue {}\"?", n + 1));
                }
                sc.warns.push((msg, line));
            }
        }
        Ok(())
    }

    /// Nested function bodies hide inside expressions (closures,
    /// anonymous classes) — each is its own label/static scope.
    fn flow_expr(e: &Expr, sc: &mut ScanScope) -> Result<(), PhpError> {
        match e {
            Expr::Closure(c) => Self::flow_fn(&c.decl, &mut sc.warns),
            Expr::AnonClass(d) => Self::flow_class(d, &mut sc.warns),
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
            | Expr::VarVar(e, _)
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
            Expr::Call { name, args, .. } => {
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
            Expr::New { class, args, .. } => {
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
            Expr::StaticCallDyn {
                class, name, args, ..
            } => {
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
