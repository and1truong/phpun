//! Expression grammar: closures and `new class` atoms, the full
//! precedence chain down to `primary`, call args and array literals.

use super::*;

impl<'a> Parser<'a> {
    /// `function (&$a) use ($x, &$y) { }`, `static function () {}`,
    /// `fn($x) => $x + 1`.
    pub(in crate::parser) fn closure_expr(&mut self) -> Result<Expr, PhpError> {
        let prev_in_closure = self.in_closure;
        self.in_closure = true;
        let r = self.closure_inner();
        self.in_closure = prev_in_closure;
        r
    }

    pub(in crate::parser) fn closure_inner(&mut self) -> Result<Expr, PhpError> {
        let line = self.line();
        let mut arrow = false;
        let mut uses = Vec::new();
        let mut is_static = false;
        if self.ident_is("static") {
            is_static = true;
            self.pos += 1;
        }
        if self.eat_ident("fn") {
            arrow = true;
        } else {
            self.expect_ident("function")?;
        }
        let by_ref = self.eat_op("&");
        let prev_hook = self.hook_ctx.take();
        let params = self.params()?;
        if !arrow && self.ident_is("use") {
            self.pos += 1;
            self.expect_op("(")?;
            while !self.at_op(")") {
                let by_ref = self.eat_op("&");
                match self.next() {
                    Some(Token::Variable(n)) => uses.push((n, by_ref)),
                    t => {
                        return Err(PhpError::parse(
                            format!(
                                "syntax error, unexpected {}, expecting variable",
                                desc_t(t.as_ref())
                            ),
                            self.line(),
                        ))
                    }
                }
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
            // Zend compile checks on the use list (closure_use_*).
            let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for (n, _) in &uses {
                if n == "GLOBALS" {
                    return Err(PhpError::fatal(
                        "Cannot use auto-global as lexical variable",
                        self.prev_line(),
                    ));
                }
                if !seen.insert(n.as_str()) {
                    return Err(PhpError::fatal(
                        format!("Cannot use variable ${} twice", n),
                        self.prev_line(),
                    ));
                }
            }
            for (n, _) in &uses {
                if params.iter().any(|p| p.name == *n) {
                    return Err(PhpError::fatal(
                        format!("Cannot use lexical variable ${} as a parameter name", n),
                        self.prev_line(),
                    ));
                }
            }
        }
        // `: ret` after `(` — return types apply to closures too
        // (scalar_strict uses `{closure:...}(): Return value ...` TypeErrors).
        let cret = if self.eat_op(":") {
            self.take_type()?
        } else {
            None
        };
        let prev_ret_by_ref = self.ret_by_ref;
        self.ret_by_ref = by_ref;
        let (body, end_line) = if arrow {
            self.expect_op("=>")?;
            // An arrow body inside a const slot is validated as part of
            // the enclosing constant expression — scope keywords there
            // are 'Constant expression contains invalid operations'.
            let saved_const = self.const_ctx;
            if self.const_ctx == ConstCtx::Slot {
                self.const_ctx = ConstCtx::ArrowSlot;
            }
            let e = self.expr();
            self.const_ctx = saved_const;
            let e = e?;
            let el = self.prev_line();
            // A call inside the arrow expr needs a line marker — the
            // body has no statements to set cur_line (closure_064).
            (vec![Stmt::Line(line), Stmt::Return(Some(e))], el)
        } else {
            let b = self.runtime_body()?;
            let el = self.prev_line();
            (b, el)
        };
        self.ret_by_ref = prev_ret_by_ref;
        self.hook_ctx = prev_hook;
        Ok(Expr::Closure(ClosureExpr {
            decl: FunctionDecl {
                ret: cret,
                name: String::new(),
                params,
                body,
                attrs: vec![],
                by_ref,
                line,
                end_line,
                file: String::new(),
                ns: self.cur_ns.clone(),
                decl_in: None,
            },
            uses,
            arrow,
            is_static,
        }))
    }

    /// The class operand of `new`: name path, `self`/`static`/`parent`,
    /// `$var`, `{expr}`, or anonymous `class { ... }`. Returns (class expr,
    /// ctor args) — anonymous classes take their ctor args before the body.
    pub(in crate::parser) fn new_class_expr(&mut self) -> Result<(Expr, Vec<Expr>), PhpError> {
        if self.ident_is("class") {
            // Anonymous class — parse body as a class decl with a synthetic name.
            self.pos += 1;
            // optional constructor args before body
            let ctor_args = if self.at_op("(") {
                self.pos += 1;
                self.args()?
            } else {
                Vec::new()
            };
            // delegate: parse `extends`/`implements`/body by simulating
            self.cur_class = "class@anonymous".into();
            self.skip_attrs()?;
            let mut parent = None;
            if self.eat_ident("extends") {
                parent = self.name_path();
            }
            let mut implements = Vec::new();
            if self.eat_ident("implements") {
                while let Some(n) = self.name_path() {
                    implements.push(n);
                    if !self.eat_op(",") {
                        break;
                    }
                }
            }
            self.expect_op("{")?;
            // Only a real `extends` gives the anonymous class a
            // `parent` (see class_decl).
            self.class_ctx.push((parent.is_some(), false));
            let mut methods = Vec::new();
            let mut props = Vec::new();
            let mut consts = Vec::new();
            let mut traits = Vec::new();
            // reuse class body by inlining a tiny loop (mirrors class_decl)
            while !self.at_op("}") {
                if self.peek().is_none() {
                    return Err(PhpError::parse(
                        "syntax error, unexpected end of file",
                        self.line(),
                    ));
                }
                self.skip_attrs()?;
                let mut vis = Visibility::Public;
                let mut is_static = false;
                let mut m_abstract = false;
                let mut m_final = false;
                let mut m_readonly = false;
                let mut m_set_vis = None;
                loop {
                    if self.ident_is("public") {
                        vis = Visibility::Public;
                        self.pos += 1;
                    } else if self.ident_is("protected") {
                        if self.at_asym_set() {
                            m_set_vis = Some(Visibility::Protected);
                        } else {
                            vis = Visibility::Protected;
                            self.pos += 1;
                        }
                    } else if self.ident_is("private") {
                        if self.at_asym_set() {
                            m_set_vis = Some(Visibility::Private);
                        } else {
                            vis = Visibility::Private;
                            self.pos += 1;
                        }
                    } else if self.ident_is("static") {
                        is_static = true;
                        self.pos += 1;
                    } else if self.ident_is("abstract") {
                        m_abstract = true;
                        self.pos += 1;
                    } else if self.ident_is("final") {
                        m_final = true;
                        self.pos += 1;
                    } else if self.ident_is("readonly") {
                        m_readonly = true;
                        self.pos += 1;
                    } else if self.ident_is("var") {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                if self.ident_is("function") {
                    methods.push(Rc::new(
                        self.method_decl(is_static, m_abstract, m_final, vis)?,
                    ));
                    continue;
                }
                if self.ident_is("const") {
                    self.pos += 1;
                    loop {
                        let cname = self.ident().unwrap_or_default();
                        self.expect_op("=")?;
                        consts.push(crate::ast::ConstDecl {
                            name: cname,
                            value: self.expr()?,
                            visibility: Visibility::Public,
                            is_final: false,
                            ty: None,
                            attrs: vec![],
                            decl_in: None,
                            enum_case: false,
                        });
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    self.expect_op(";")?;
                    continue;
                }
                if self.ident_is("use") {
                    self.pos += 1;
                    while let Some(n) = self.name_path() {
                        traits.push(n);
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    if self.eat_op("{") {
                        let mut depth = 1i32;
                        while depth > 0 {
                            match self.next() {
                                Some(Token::Op("{")) => depth += 1,
                                Some(Token::Op("}")) => depth -= 1,
                                Some(_) => {}
                                None => break,
                            }
                        }
                    } else {
                        self.expect_op(";")?;
                    }
                    continue;
                }
                if self.ident_is("case") {
                    self.pos += 1;
                    let cname = self.ident().unwrap_or_default();
                    let cv = if self.eat_op("=") {
                        self.expr()?
                    } else {
                        Expr::Null
                    };
                    consts.push(crate::ast::ConstDecl {
                        name: cname,
                        value: cv,
                        visibility: Visibility::Public,
                        is_final: false,
                        ty: None,
                        attrs: vec![],
                        decl_in: None,
                        enum_case: false,
                    });
                    self.expect_op(";")?;
                    continue;
                }
                let pline = self.line();
                let pty = if matches!(
                    self.peek(),
                    Some(Token::Ident(_))
                        | Some(Token::Op("?"))
                        | Some(Token::Op("\\"))
                        | Some(Token::Op("("))
                ) && !matches!(self.peek2(), Some(Token::Op("(")))
                {
                    self.take_type()?
                } else {
                    None
                };
                self.check_prop_ty(&pty, pline)?;
                loop {
                    let pname = match self.next() {
                        Some(Token::Variable(n)) => n,
                        t => {
                            return Err(PhpError::parse(
                                format!(
                                    "syntax error, unexpected {}, expecting variable",
                                    desc_t(t.as_ref())
                                ),
                                self.line(),
                            ))
                        }
                    };
                    let default = if self.eat_op("=") {
                        Some(self.expr()?)
                    } else {
                        None
                    };
                    props.push(PropDecl {
                        name: pname,
                        default,
                        is_static,
                        visibility: vis,
                        readonly: m_readonly,
                        ty: pty.clone(),
                        is_abstract: m_abstract,
                        is_final: m_final,
                        set_vis: m_set_vis,
                        decl_in: None,
                        hooks: None,
                        attrs: vec![],
                        line: pline,
                    });
                    if !self.eat_op(",") {
                        break;
                    }
                }
                if self.at_op("{") {
                    let hn = props.last().map(|p| p.name.clone()).unwrap_or_default();
                    let hs = self.prop_hooks(&hn)?;
                    if let Some(p) = props.last_mut() {
                        p.hooks = hs;
                    }
                } else {
                    self.expect_op(";")?;
                }
            }
            self.expect_op("}")?;
            self.class_ctx.pop();
            // Zend names anonymous classes after their first base:
            // `{Parent}@anonymous`, else `{FirstInterface}@anonymous`,
            // else `class@anonymous` (typed_properties_065).
            let anon_base = parent
                .as_deref()
                .or(implements.first().map(String::as_str))
                .unwrap_or("class");
            return Ok((
                Expr::AnonClass(Rc::new(ClassDecl {
                    attrs: vec![],
                    name: format!("{}@anonymous${}", anon_base, self.line()),
                    kind: ClassKind::Class,
                    is_abstract: false,
                    is_final: false,
                    readonly: false,
                    parent,
                    implements,
                    traits,
                    adaptations: vec![],
                    methods,
                    props,
                    consts,
                    file: String::new(),
                    line: self.line(),
                })),
                ctor_args,
            ));
        }
        // `new self` / `new static` / `new parent`
        if self.ident_is("self") || self.ident_is("static") || self.ident_is("parent") {
            let ce = Expr::Const(self.ident().unwrap());
            return Ok((self.new_dcolon(ce)?, Vec::new()));
        }
        match self.peek().cloned() {
            Some(Token::Ident(_)) | Some(Token::Op("\\")) => {
                let n = self.name_path().unwrap_or_default();
                let ce = Expr::Const(self.ns_resolve(&n, NsKind::Class));
                Ok((self.new_dcolon(ce)?, Vec::new()))
            }
            Some(Token::Variable(n)) => {
                self.pos += 1;
                // `new $a[i][j]` — dims belong to the class-name expr
                // (engine_assignExecutionOrder_007), not the new object.
                let mut e = Expr::Var(n);
                loop {
                    if self.eat_op("[") {
                        let i = if self.at_op("]") {
                            None
                        } else {
                            Some(Box::new(self.expr()?))
                        };
                        self.expect_op("]")?;
                        e = Expr::Index { e: Box::new(e), i };
                    } else if self.eat_op("->") {
                        // `new $this->prop` (bug21669); `->m()` stays ctor args.
                        match self.next() {
                            Some(Token::Ident(pn)) => {
                                e = Expr::Prop {
                                    obj: Box::new(e),
                                    name: PropName::Name(pn),
                                    nullsafe: false,
                                };
                            }
                            _ => {
                                return Err(PhpError::parse(
                                    "syntax error, unexpected token, expecting property name",
                                    self.line(),
                                ))
                            }
                        }
                    } else {
                        break;
                    }
                }
                Ok((e, Vec::new()))
            }
            Some(Token::Op("{")) => {
                self.pos += 1;
                let e = self.expr()?;
                self.expect_op("}")?;
                Ok((e, Vec::new()))
            }
            Some(Token::Op("(")) => {
                self.pos += 1;
                let e = self.expr()?;
                self.expect_op(")")?;
                Ok((e, Vec::new()))
            }
            t => Err(PhpError::parse(
                format!(
                    "syntax error, unexpected {}, expecting class name",
                    desc_t(t.as_ref())
                ),
                self.line(),
            )),
        }
    }

    /// `new X::...` — zend's new_expr only allows a class-name
    /// variable after `::` (`new C::$v` instantiates the class named
    /// by the static prop); `X::CONST`, `X::class` and `X::method()`
    /// are parse errors `expecting variable or "$"` (p10v/y, probe10b).
    fn new_dcolon(&mut self, ce: Expr) -> Result<Expr, PhpError> {
        if !self.at_op("::") {
            return Ok(ce);
        }
        self.pos += 1;
        match self.next() {
            Some(Token::Variable(n)) => Ok(Expr::StaticProp {
                class: Box::new(ce),
                name: PropName::Name(n),
            }),
            Some(Token::Op("$")) => {
                // `X::$$v` / `X::${e}` — also a class-name variable.
                let inner = if self.at_op("{") {
                    self.pos += 1;
                    let inner = self.expr()?;
                    self.expect_op("}")?;
                    inner
                } else {
                    match self.next() {
                        Some(Token::Variable(n)) => Expr::Var(n),
                        t => {
                            return Err(PhpError::parse(
                                format!(
                                    "syntax error, unexpected {}, expecting variable or \"$\"",
                                    desc_t(t.as_ref())
                                ),
                                self.line(),
                            ))
                        }
                    }
                };
                Ok(Expr::StaticProp {
                    class: Box::new(ce),
                    name: PropName::Expr(Box::new(inner)),
                })
            }
            t => {
                let desc = match &t {
                    Some(Token::Ident(n)) if crate::lexer::is_keyword(n) => {
                        format!("token \"{}\"", n)
                    }
                    Some(Token::Ident(n)) => format!("identifier \"{}\"", n),
                    other => desc_t(other.as_ref()),
                };
                Err(PhpError::parse(
                    format!(
                        "syntax error, unexpected {}, expecting variable or \"$\"",
                        desc
                    ),
                    self.line(),
                ))
            }
        }
    }

    pub(in crate::parser) fn match_expr(&mut self) -> Result<Expr, PhpError> {
        self.pos += 1; // match
        self.expect_op("(")?;
        let subject = self.expr()?;
        self.expect_op(")")?;
        self.expect_op("{")?;
        let mut arms = Vec::new();
        while !self.at_op("}") {
            if self.eat_ident("default") {
                self.expect_op("=>")?;
                let r = self.expr()?;
                arms.push(MatchArm {
                    conds: Vec::new(),
                    result: r,
                });
            } else {
                let mut conds = vec![self.expr()?];
                while self.eat_op(",") {
                    if self.at_op("=>") {
                        break;
                    }
                    conds.push(self.expr()?);
                }
                self.expect_op("=>")?;
                let r = self.expr()?;
                arms.push(MatchArm { conds, result: r });
            }
            self.eat_op(",");
        }
        self.expect_op("}")?;
        Ok(Expr::Match {
            subject: Box::new(subject),
            arms,
        })
    }

    pub(in crate::parser) fn expr_list(&mut self) -> Result<Vec<Expr>, PhpError> {
        let mut v = vec![self.expr()?];
        while self.eat_op(",") {
            v.push(self.expr()?);
        }
        Ok(v)
    }

    pub fn expr(&mut self) -> Result<Expr, PhpError> {
        if self.ident_is("throw") {
            self.pos += 1;
            let e = self.assign()?;
            return Ok(Expr::Throw(Box::new(e)));
        }
        self.assign()
    }

    pub(in crate::parser) fn assign(&mut self) -> Result<Expr, PhpError> {
        // PHP 8 throw-expression: legal wherever an expression is —
        // `?? throw`, ternary arms, match arms, arrow-fn bodies.
        if self.ident_is("throw") {
            self.pos += 1;
            let e = self.assign()?;
            return Ok(Expr::Throw(Box::new(e)));
        }
        let lhs_start = self.pos;
        let e = self.ternary()?;

        if let Some(Token::Op(op)) = self.peek() {
            if ASSIGN_OPS.contains(op) {
                let op_pos = self.pos;
                let mut op: &'static str = op;
                self.pos += 1;
                if op == "=" && self.eat_op("&") {
                    op = "=&"; // by-reference assignment
                }
                let rhs = self.assign()?;
                let target = self.list_target(e)?;
                self.check_list_ref_literal(op, &target, &rhs)?;
                self.assign_target_gate(&target, op, lhs_start, op_pos)?;
                return Ok(Expr::Assign {
                    target: Box::new(target),
                    op,
                    value: Box::new(rhs),
                });
            }
        }
        Ok(e)
    }

    /// `=`/`op=` LHS gate — zend's `variable` write context: a whole
    /// parenthesized target and any other non-variable expression
    /// parse-errors `unexpected token "{op}"`; call/method results are
    /// `Can't use ... return value in write context` compile fatals;
    /// nullsafe chains `Can't use nullsafe operator in write context`;
    /// a deref-linked target whose chain root isn't writable is
    /// `Cannot use temporary expression in write context` (probe13
    /// family vs oracle). List elements check per-item instead.
    fn assign_target_gate(
        &mut self,
        e: &Expr,
        op: &str,
        lhs_start: usize,
        op_pos: usize,
    ) -> Result<(), PhpError> {
        use crate::ast::Expr::*;
        if let List(items) = e {
            for it in items.iter().flatten() {
                self.list_writable(&it.1)?;
            }
            return Ok(());
        }
        let op = if op == "=&" { "=" } else { op };
        if self.paren_wrapped_target(lhs_start, op_pos) || matches!(e, Paren(_)) {
            return Err(PhpError::parse(
                format!("syntax error, unexpected token \"{}\"", op),
                self.line(),
            ));
        }
        let callish = match e {
            Call { .. } | Fcc(_) => Some("function"),
            MethodCall { .. } | StaticCall { .. } | StaticCallDyn { .. } => Some("method"),
            _ => None,
        };
        if let Some(k) = callish {
            return Err(PhpError::compile_fatal(
                format!("Can't use {} return value in write context", k),
                self.line(),
            ));
        }
        if Self::has_nullsafe(e) {
            return Err(PhpError::compile_fatal(
                "Can't use nullsafe operator in write context",
                self.line(),
            ));
        }
        match e {
            Var(_) | VarVar(_) | StaticProp { .. } => Ok(()),
            // A deref-linked target writes iff its chain root is
            // writable grammar — literal/const/`new` roots die as
            // temporaries, not parse errors (nr/ns probes).
            Index { .. } | Prop { .. } => {
                if Self::writeable_root(e) {
                    Ok(())
                } else {
                    // Deferred — zend's compile-time check fires only
                    // after the whole file parses, so a later syntax
                    // error wins over it (probe m8).
                    self.write_ctx_errs.push(self.line());
                    Ok(())
                }
            }
            _ => Err(PhpError::parse(
                format!("syntax error, unexpected token \"{}\"", op),
                self.line(),
            )),
        }
    }

    /// True when token `start` is `(` whose matching `)` sits
    /// immediately before `op_pos` — the whole LHS is a parenthesized
    /// expression (`($x) = 5`, `(f()) += v`), which zend refuses as an
    /// assign target. `Paren` nodes only survive for static-prop refs,
    /// so plain parens are recovered from the token stream.
    fn paren_wrapped_target(&self, start: usize, op_pos: usize) -> bool {
        if !matches!(self.toks.get(start).map(|l| &l.token), Some(Token::Op("("))) {
            return false;
        }
        let mut depth = 0usize;
        for (i, lt) in self.toks.iter().enumerate().skip(start) {
            match &lt.token {
                Token::Op("(") => depth += 1,
                Token::Op(")") => {
                    depth -= 1;
                    if depth == 0 {
                        return i == op_pos - 1;
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// Deepest chain root under `x[..]`/`x->y`/`x->m()` links (parens
    /// transparent) is writable grammar. A `new`-rooted chain is only
    /// writable once a member CALL intervenes — zend's temporary
    /// object: `(new F)->m()->p[]` and `(new F)->p->m()[]` assign,
    //  `(new F)->p[]` alone is a temporary (oracle-probed).
    fn writeable_root(e: &Expr) -> bool {
        use crate::ast::Expr::*;
        let mut leaf = e;
        let mut call_link = false;
        loop {
            leaf = match leaf {
                Index { e: c, .. } | Prop { obj: c, .. } => c.as_ref(),
                MethodCall { obj: c, .. } => {
                    call_link = true;
                    c.as_ref()
                }
                Paren(inner) => inner.as_ref(),
                _ => break,
            };
        }
        if call_link && matches!(leaf, New { .. }) {
            return true;
        }
        matches!(
            leaf,
            Var(_)
                | VarVar(_)
                | Call { .. }
                | MethodCall { .. }
                | StaticCall { .. }
                | StaticCallDyn { .. }
                | Fcc(_)
                | Paren(_)
                | StaticProp { .. }
        )
    }

    /// `clone (` — does the `(` (at `pos+1`) hold a top-level `,`?
    /// Only then is it PHP 8.5's clone-with call form; without a
    /// comma the parens group the unary operand (`clone ($o)->m` is
    /// `clone(($o)->m)` in zend).
    fn clone_paren_has_comma(&self) -> bool {
        let mut depth = 0usize;
        for lt in &self.toks[self.pos + 1..] {
            match &lt.token {
                Token::Op(o) if matches!(*o, "(" | "[" | "{") => depth += 1,
                Token::Op(o) if matches!(*o, ")" | "]" | "}") => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return false;
                    }
                }
                Token::Op(",") if depth == 1 => return true,
                Token::Op(";") => return false,
                _ => {}
            }
        }
        false
    }

    /// A destructuring element must itself be a writable value:
    /// `($x)` unwraps, nested lists recurse, `&` elements recurse,
    /// call/method results die with the return-value fatals, and
    /// everything else (nullsafe chains included) is
    /// `Assignments can only happen to writable values` (p13 l*).
    pub(in crate::parser) fn list_writable(&self, e: &Expr) -> Result<(), PhpError> {
        use crate::ast::Expr::*;
        let writable = || {
            Err(PhpError::compile_fatal(
                "Assignments can only happen to writable values",
                self.line(),
            ))
        };
        match e {
            Var(_) | VarVar(_) | StaticProp { .. } => Ok(()),
            Paren(inner) | ByRef(inner) => self.list_writable(inner),
            List(items) => {
                for it in items.iter().flatten() {
                    self.list_writable(&it.1)?;
                }
                Ok(())
            }
            ArrayLit(items) => {
                for (_, v) in items {
                    if !matches!(v, Null) {
                        self.list_writable(v)?;
                    }
                }
                Ok(())
            }
            Call { .. } | Fcc(_) => Err(PhpError::compile_fatal(
                "Can't use function return value in write context",
                self.line(),
            )),
            MethodCall { .. } | StaticCall { .. } | StaticCallDyn { .. } => {
                Err(PhpError::compile_fatal(
                    "Can't use method return value in write context",
                    self.line(),
                ))
            }
            Index { .. } | Prop { .. } => {
                if Self::has_nullsafe(e) || !Self::writeable_root(e) {
                    return writable();
                }
                Ok(())
            }
            _ => writable(),
        }
    }

    /// `[a, b]` / `list(a, b)` on the left of `=` is destructuring.
    pub(in crate::parser) fn list_target(&mut self, e: Expr) -> Result<Expr, PhpError> {
        match e {
            Expr::ArrayLit(items) => {
                // Nested destructuring: `[[$a, $b], $c] = $src` — inner
                // array literals (and `list()` calls) convert to List
                // too, so runtime store() can recurse (lp4).
                let mut out = Vec::with_capacity(items.len());
                for (k, v) in items {
                    out.push(match (k, v) {
                        // `[,$a]`/`[$a, ,$c]` holes parse as `(None, Null)`.
                        (None, Expr::Null) => None,
                        (k, other) => Some((k, self.list_target(other)?)),
                    });
                }
                self.list_mix_check(&out)?;
                Ok(Expr::List(out))
            }
            Expr::Call { name, args } => match *name {
                Expr::Str(n) if n.eq_ignore_ascii_case("list") => {
                    let mut out = Vec::with_capacity(args.len());
                    for a in args {
                        out.push(match a {
                            Expr::Null => None,
                            // `list('k' => $v)` — args_flags encodes the
                            // keyed pair as a transient `=>` binary.
                            Expr::Binary { op: "=>", l, r } => {
                                Some((Some(*l), self.list_target(*r)?))
                            }
                            other => Some((None, self.list_target(other)?)),
                        });
                    }
                    self.list_mix_check(&out)?;
                    Ok(Expr::List(out))
                }
                other => Ok(Expr::Call {
                    name: Box::new(other),
                    args,
                }),
            },
            other => Ok(other),
        }
    }

    pub(in crate::parser) fn ternary(&mut self) -> Result<Expr, PhpError> {
        let c = self.logical_or()?;
        if self.eat_op("?") {
            if self.at_op(":") {
                self.pos += 1;
                let f = self.assign()?;
                return Ok(Expr::Ternary {
                    c: Box::new(c),
                    t: None,
                    f: Box::new(f),
                });
            }
            let t = self.ternary()?;
            self.expect_op(":")?;
            let f = self.ternary()?;
            return Ok(Expr::Ternary {
                c: Box::new(c),
                t: Some(Box::new(t)),
                f: Box::new(f),
            });
        }
        if self.eat_op("??") {
            let r = self.assign()?;
            return Ok(Expr::Binary {
                op: "??",
                l: Box::new(c),
                r: Box::new(r),
            });
        }
        Ok(c)
    }

    pub(in crate::parser) fn logical_or(&mut self) -> Result<Expr, PhpError> {
        let mut e = self.logical_and()?;
        loop {
            if self.eat_op("||") {
                let r = self.logical_and()?;
                e = Expr::Binary {
                    op: "||",
                    l: Box::new(e),
                    r: Box::new(r),
                };
            } else if self.ident_is("or") {
                self.pos += 1;
                let r = self.logical_and()?;
                e = Expr::Binary {
                    op: "||",
                    l: Box::new(e),
                    r: Box::new(r),
                };
            } else if self.ident_is("xor") {
                self.pos += 1;
                let r = self.logical_and()?;
                e = Expr::Binary {
                    op: "xor",
                    l: Box::new(e),
                    r: Box::new(r),
                };
            } else {
                return Ok(e);
            }
        }
    }

    pub(in crate::parser) fn logical_and(&mut self) -> Result<Expr, PhpError> {
        let mut e = self.equality()?;
        loop {
            if self.eat_op("&&") {
                let r = self.equality()?;
                e = Expr::Binary {
                    op: "&&",
                    l: Box::new(e),
                    r: Box::new(r),
                };
            } else if self.ident_is("and") {
                self.pos += 1;
                let r = self.equality()?;
                e = Expr::Binary {
                    op: "&&",
                    l: Box::new(e),
                    r: Box::new(r),
                };
            } else {
                return Ok(e);
            }
        }
    }

    pub(in crate::parser) fn equality(&mut self) -> Result<Expr, PhpError> {
        let mut e = self.comparison()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op("==")) => "==",
                Some(Token::Op("!=")) => "!=",
                Some(Token::Op("<>")) => "!=",
                Some(Token::Op("===")) => "===",
                Some(Token::Op("!==")) => "!==",
                Some(Token::Op("<=>")) => "<=>",
                _ => return Ok(e),
            };
            self.pos += 1;
            let r = self.comparison()?;
            e = Expr::Binary {
                op,
                l: Box::new(e),
                r: Box::new(r),
            };
        }
    }

    pub(in crate::parser) fn comparison(&mut self) -> Result<Expr, PhpError> {
        let mut e = self.concat()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op("<")) => "<",
                Some(Token::Op("<=")) => "<=",
                Some(Token::Op(">")) => ">",
                Some(Token::Op(">=")) => ">=",
                _ => return Ok(e),
            };
            self.pos += 1;
            let r = self.concat()?;
            e = Expr::Binary {
                op,
                l: Box::new(e),
                r: Box::new(r),
            };
        }
    }

    /// `.` binds tighter than `+`/`-` since PHP 8.0.
    pub(in crate::parser) fn concat(&mut self) -> Result<Expr, PhpError> {
        let mut e = self.bit_or()?;
        while self.eat_op(".") {
            let r = self.bit_or()?;
            e = Expr::Binary {
                op: ".",
                l: Box::new(e),
                r: Box::new(r),
            };
        }
        Ok(e)
    }

    pub(in crate::parser) fn bit_or(&mut self) -> Result<Expr, PhpError> {
        let mut e = self.bit_xor()?;
        while self.eat_op("|") {
            let r = self.bit_xor()?;
            e = Expr::Binary {
                op: "|",
                l: Box::new(e),
                r: Box::new(r),
            };
        }
        Ok(e)
    }

    pub(in crate::parser) fn bit_xor(&mut self) -> Result<Expr, PhpError> {
        let mut e = self.bit_and()?;
        while self.eat_op("^") {
            let r = self.bit_and()?;
            e = Expr::Binary {
                op: "^",
                l: Box::new(e),
                r: Box::new(r),
            };
        }
        Ok(e)
    }

    pub(in crate::parser) fn bit_and(&mut self) -> Result<Expr, PhpError> {
        let mut e = self.shift()?;
        while self.eat_op("&") {
            let r = self.shift()?;
            e = Expr::Binary {
                op: "&",
                l: Box::new(e),
                r: Box::new(r),
            };
        }
        Ok(e)
    }

    pub(in crate::parser) fn shift(&mut self) -> Result<Expr, PhpError> {
        let mut e = self.additive()?;
        loop {
            let op = if self.eat_op("<<") {
                "<<"
            } else if self.eat_op(">>") {
                ">>"
            } else {
                return Ok(e);
            };
            let r = self.additive()?;
            e = Expr::Binary {
                op,
                l: Box::new(e),
                r: Box::new(r),
            };
        }
    }

    pub(in crate::parser) fn additive(&mut self) -> Result<Expr, PhpError> {
        let mut e = self.term()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op("+")) => "+",
                Some(Token::Op("-")) => "-",
                _ => return Ok(e),
            };
            self.pos += 1;
            let r = self.term()?;
            e = Expr::Binary {
                op,
                l: Box::new(e),
                r: Box::new(r),
            };
        }
    }

    pub(in crate::parser) fn term(&mut self) -> Result<Expr, PhpError> {
        let mut e = self.power()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op("*")) => "*",
                Some(Token::Op("/")) => "/",
                Some(Token::Op("%")) => "%",
                _ => return Ok(e),
            };
            self.pos += 1;
            let r = self.power()?;
            e = Expr::Binary {
                op,
                l: Box::new(e),
                r: Box::new(r),
            };
        }
    }

    /// `**` is right-associative and binds tighter than unary minus.
    pub(in crate::parser) fn power(&mut self) -> Result<Expr, PhpError> {
        let e = self.unary()?;
        if self.eat_op("**") {
            let r = self.power()?;
            return Ok(Expr::Binary {
                op: "**",
                l: Box::new(e),
                r: Box::new(r),
            });
        }
        Ok(e)
    }

    pub(in crate::parser) fn unary(&mut self) -> Result<Expr, PhpError> {
        if self.eat_op("!") {
            let e = self.unary()?;
            return Ok(Expr::Unary {
                op: "!",
                e: Box::new(e),
            });
        }
        if self.eat_op("-") {
            let e = self.unary()?;
            return Ok(Expr::Unary {
                op: "-",
                e: Box::new(e),
            });
        }
        if self.eat_op("+") {
            let e = self.unary()?;
            return Ok(Expr::Unary {
                op: "+",
                e: Box::new(e),
            });
        }
        if self.eat_op("~") {
            let e = self.unary()?;
            return Ok(Expr::Unary {
                op: "~",
                e: Box::new(e),
            });
        }
        if self.eat_op("++") {
            // `++`'s operand is zend's `new_variable` — the same write
            // context grammar as foreach `&` (`++1` → `unexpected
            // integer`, `++(x)` → expecting `->`, `++f()`/`++$o->m()`
            // → return-value fatal, `++"s"[0]`/`++new C()->x` →
            // temporary-expression fatal, `++$x?->y` → nullsafe fatal).
            let e = self.ref_variable(true)?;
            return Ok(Expr::PreInc(Box::new(e)));
        }
        if self.eat_op("--") {
            let e = self.ref_variable(true)?;
            return Ok(Expr::PreDec(Box::new(e)));
        }
        if self.eat_op("@") {
            // Error suppression — parsed; runtime treats as no-op for now.
            let e = self.unary()?;
            return Ok(Expr::Unary {
                op: "@",
                e: Box::new(e),
            });
        }
        if self.ident_is("clone") {
            // `clone($o)` / `clone($o, [...])` — PHP 8.5's function
            // form parses as a normal call to the `clone` builtin;
            // bare `clone $o` stays the unary operator (R3 #13).
            if matches!(self.peek2(), Some(Token::Op("("))) {
                // Only a top-level `,` inside makes it the clone-with
                // call form — otherwise the `(` groups the operand and
                // postfix keeps growing into it: `clone (e)->m` is
                // `clone((e)->m)` (oracle TypeError on `int given`),
                // never a call result followed by `->m`.
                if self.clone_paren_has_comma() {
                    self.pos += 2;
                    let args = self.args()?;
                    // `clone($o, [...])->m` is a parse error in zend
                    // ('unexpected token "->"'): the call-form parens
                    // do not take postfix.
                    if self.at_op("->") || self.at_op("?->") {
                        return Err(PhpError::parse(
                            format!(
                                "syntax error, unexpected {}, expecting \")\"",
                                self.describe()
                            ),
                            self.line(),
                        ));
                    }
                    return Ok(Expr::Call {
                        name: Box::new(Expr::Str("\u{1}clone".to_string())),
                        args,
                    });
                }
                self.pos += 1;
                let e = self.unary()?;
                return Ok(Expr::Clone(Box::new(e)));
            }
            self.pos += 1;
            let e = self.unary()?;
            return Ok(Expr::Clone(Box::new(e)));
        }
        // `(type)` cast: (int) (integer) (float) (real) (double) (string)
        // (binary) (bool) (boolean) (array) (object) (unset)
        if self.at_op("(") {
            if let Some(Token::Ident(t)) = self.peek2() {
                let kind = match t.to_lowercase().as_str() {
                    "int" | "integer" => Some(CastKind::Int),
                    "float" | "real" | "double" => Some(CastKind::Float),
                    "string" | "binary" => Some(CastKind::String),
                    "bool" | "boolean" => Some(CastKind::Bool),
                    "array" => Some(CastKind::Array),
                    "object" => Some(CastKind::Object),
                    "unset" => Some(CastKind::Unset),
                    _ => None,
                };
                if let Some(kind) = kind {
                    if matches!(
                        self.toks.get(self.pos + 2).map(|l| &l.token),
                        Some(Token::Op(")"))
                    ) {
                        self.pos += 3; // ( type )
                        let e = self.unary()?;
                        return Ok(Expr::Cast {
                            kind,
                            e: Box::new(e),
                        });
                    }
                }
            }
        }
        let lhs_start = self.pos;
        let mut e = self.postfix()?;
        // `instanceof` binds between unary and relational ops.
        while self.ident_is("instanceof") {
            self.pos += 1;
            let mut c = self.unary()?;
            if let Expr::Const(n) = &c {
                c = Expr::Const(self.ns_resolve(n, NsKind::Class));
            }
            e = Expr::Instanceof {
                obj: Box::new(e),
                class: Box::new(c),
            };
        }
        // `=` binds to the rightmost operand at any precedence — PHP's
        // `expr: variable '=' expr` production makes
        // `false !== $lastPos = strrpos(...)` (Composer's ClassLoader)
        // parse as `!==` applied to an assignment.
        if let Some(Token::Op(op)) = self.peek() {
            let is_assign = ASSIGN_OPS.contains(op)
                // `=&` stays with the statement-level assign() handler.
                && !(op == &"="
                    && matches!(
                        self.toks.get(self.pos + 1).map(|l| &l.token),
                        Some(Token::Op("&"))
                    ));
            if is_assign {
                let op_pos = self.pos;
                let mut op: &'static str = op;
                self.pos += 1;
                if op == "=" && self.eat_op("&") {
                    op = "=&";
                }
                let rhs = self.assign()?;
                let target = self.list_target(e)?;
                self.check_list_ref_literal(op, &target, &rhs)?;
                self.assign_target_gate(&target, op, lhs_start, op_pos)?;
                return Ok(Expr::Assign {
                    target: Box::new(target),
                    op,
                    value: Box::new(rhs),
                });
            }
        }
        Ok(e)
    }

    /// Zend rejects `['k' => $a, $b]`/`[$a, 'k' => $b]` — keyed and
    /// unkeyed destructuring elements can't mix in one list.
    fn list_mix_check(&self, items: &[Option<(Option<Expr>, Expr)>]) -> Result<(), PhpError> {
        let mut keyed = false;
        let mut unkeyed = false;
        for it in items.iter().flatten() {
            if it.0.is_some() {
                keyed = true;
            } else {
                unkeyed = true;
            }
        }
        if keyed && unkeyed {
            return Err(PhpError::compile_fatal(
                "Cannot mix keyed and unkeyed array entries in assignments",
                self.line(),
            ));
        }
        Ok(())
    }

    /// Any `&` element in the list — nested lists count too
    /// (`list(list(&$x))` is a referenceable-value target).
    fn list_has_ref(items: &[Option<(Option<Expr>, Expr)>]) -> bool {
        items.iter().flatten().any(|(_, t)| match t {
            Expr::ByRef(_) => true,
            Expr::List(sub) => Self::list_has_ref(sub),
            _ => false,
        })
    }

    /// `[$a, &$b] = [..]` — a `&` element against a literal array RHS
    /// is a zend compile fatal: temporaries can't be reference sources
    /// (probe5j). `=` reaches the parser at both the statement and the
    /// unary level, so both call sites check.
    fn check_list_ref_literal(&self, op: &str, target: &Expr, rhs: &Expr) -> Result<(), PhpError> {
        if op != "=" {
            return Ok(());
        }
        let Expr::List(items) = target else {
            return Ok(());
        };
        if !Self::list_has_ref(items) {
            return Ok(());
        }
        let mut lit = rhs;
        while let Expr::Paren(inner) = lit {
            lit = inner;
        }
        if matches!(lit, Expr::ArrayLit(_) | Expr::List(_)) {
            return Err(PhpError::compile_fatal(
                "Cannot assign reference to non referenceable value",
                self.line(),
            ));
        }
        Ok(())
    }

    pub(in crate::parser) fn postfix(&mut self) -> Result<Expr, PhpError> {
        let start = self.pos;
        let e = self.primary()?;
        self.postfix_rest(e, start)
    }

    /// The RHS of `=&` / `foreach (.. as &..)` / `[&..]`: Zend's
    /// `new_variable` grammar — a variable/call root followed by any
    /// `->x`/`[x]`/`::x`/`(...)` links. Anything else is a parse error
    /// (`unexpected integer`, `expecting "->"`, ...) and call roots in a
    /// write context (foreach `&`) are a compile fatal.
    pub(in crate::parser) fn ref_variable(&mut self, write_ctx: bool) -> Result<Expr, PhpError> {
        use crate::ast::Expr::*;
        const KW_REJECT: &[&str] = &[
            "clone",
            "function",
            "fn",
            "match",
            "throw",
            "print",
            "echo",
            "foreach",
            "if",
            "else",
            "elseif",
            "while",
            "do",
            "for",
            "switch",
            "return",
            "global",
            "unset",
            "include",
            "include_once",
            "require",
            "require_once",
            "isset",
            "empty",
            "list",
            "array",
            "eval",
            "exit",
            "die",
            "try",
            "catch",
            "finally",
            "class",
            "interface",
            "trait",
            "enum",
            "extends",
            "implements",
            "use",
            "namespace",
            "declare",
            "var",
            "const",
            "public",
            "private",
            "protected",
            "abstract",
            "final",
            "readonly",
            "instanceof",
            "insteadof",
            "or",
            "and",
            "xor",
            "yield",
            "goto",
            "continue",
            "break",
        ];
        let starter_ok = match self.peek() {
            Some(Token::Variable(_))
            | Some(Token::SimpleString(_))
            | Some(Token::InterpString(_)) => true,
            Some(Token::Op(o)) => matches!(*o, "$" | "$${" | "(" | "["),
            Some(Token::Ident(n)) => !KW_REJECT.contains(&n.as_str()),
            _ => false,
        };
        if !starter_ok {
            let desc = match self.peek().cloned() {
                Some(Token::Int(n)) => format!("integer \"{}\"", n),
                Some(Token::Float(n)) => format!("floating-point number \"{}\"", n),
                Some(Token::Ident(n)) => format!("token \"{}\"", n),
                t => desc_t(t.as_ref()),
            };
            return Err(PhpError::parse(
                format!("syntax error, unexpected {}", desc),
                self.line(),
            ));
        }
        // `new_variable` parens must be followed by a deref link —
        // `=& ($x)` / `=& (f())` / `=& ($$v)` are parse errors while
        // `(&$x)[0]` and `=& ($f)()` are legal (probe7 vs oracle).
        if self.at_op("(") {
            let mut depth = 0usize;
            let mut i = self.pos;
            while let Some(t) = self.toks.get(i).map(|l| &l.token) {
                match t {
                    Token::Op("(") => depth += 1,
                    Token::Op(")") => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            let next = self.toks.get(i + 1).map(|l| &l.token);
            if !matches!(
                next,
                Some(Token::Op("["))
                    | Some(Token::Op("->"))
                    | Some(Token::Op("?->"))
                    | Some(Token::Op("("))
            ) {
                return Err(PhpError::parse(
                    format!(
                        "syntax error, unexpected {}, expecting \"->\" or \"?->\" or \"[\"",
                        desc_t(next)
                    ),
                    self.line(),
                ));
            }
        }
        // `& static` is only legal before `::` (`&static::$p` is a
        // class-name reference). Before `function`/`fn` postfix would
        // swallow it into a static closure — zend instead errors
        // `unexpected token "function", expecting "::"` (probe15b).
        if self.ident_is("static") && !matches!(self.peek2(), Some(Token::Op("::"))) {
            let desc = match self.peek2().cloned() {
                Some(Token::Int(n)) => format!("integer \"{}\"", n),
                Some(Token::Float(n)) => format!("floating-point number \"{}\"", n),
                Some(Token::Ident(n)) => format!("token \"{}\"", n),
                t => desc_t(t.as_ref()),
            };
            return Err(PhpError::parse(
                format!("syntax error, unexpected {}, expecting \"::\"", desc),
                self.line(),
            ));
        }
        let e = self.postfix()?;
        // Deepest chain root: container of `x[...]`/`x->y`/`x->m()`.
        // Parens are transparent for the ROOT search; a Call node is
        // itself a legal root (`=& f()`, `=& ($f)()`), so call names
        // are NOT unwrapped. `linked` = the OUTERMOST node is a real
        // deref link — parens at the top level are NOT a
        // `new_variable` (`=& ($x)`, `=& ($a[0])`, `=& (f())` all
        // parse-error expecting "->"/"?->"/"[", while `(&$x)[0]` is
        // legal because the link is outside).
        let linked = matches!(&e, Index { .. } | Prop { .. } | MethodCall { .. });
        let mut leaf: &Expr = &e;
        loop {
            leaf = match leaf {
                Index { e: c, .. } | Prop { obj: c, .. } | MethodCall { obj: c, .. } => c.as_ref(),
                Paren(inner) => inner.as_ref(),
                _ => break,
            };
        }
        let callish = |x: &Expr| -> Option<&'static str> {
            match x {
                MethodCall { .. } | StaticCall { .. } | StaticCallDyn { .. } => Some("method"),
                Call { .. } | Fcc(_) => Some("function"),
                _ => None,
            }
        };
        if !linked {
            // No deref link — zend's new_variable is a bare variable or
            // a call, never a parenthesized expr (`=& ($x)` /
            // `=& (f())` / `=& ($$v)` → parse error).
            match &e {
                Var(_) | VarVar(_) | StaticProp { .. } => {}
                Call { .. }
                | MethodCall { .. }
                | StaticCall { .. }
                | StaticCallDyn { .. }
                | Fcc(_) => {
                    if write_ctx {
                        return Err(PhpError::compile_fatal(
                            format!(
                                "Can't use {} return value in write context",
                                callish(leaf).unwrap_or("function")
                            ),
                            self.line(),
                        ));
                    }
                }
                New { .. } => {
                    // `new C` wants `(`, `new C()` wants a deref link.
                    let had_parens = matches!(
                        self.toks.get(self.pos.wrapping_sub(1)).map(|l| &l.token),
                        Some(Token::Op(")"))
                    );
                    return Err(PhpError::parse(
                        if had_parens {
                            "syntax error, unexpected token \";\", expecting \"->\" or \"?->\" or \"[\""
                        } else {
                            "syntax error, unexpected token \";\", expecting \"(\""
                        },
                        self.line(),
                    ));
                }
                _ => {
                    // Unexpected token names the token following the
                    // parsed root (`&self function` → 'function';
                    // `&1` → ';', p15/n).
                    let desc = match self.peek().cloned() {
                        Some(Token::Int(n)) => format!("integer \"{}\"", n),
                        Some(Token::Float(n)) => format!("floating-point number \"{}\"", n),
                        Some(Token::Ident(n)) => format!("token \"{}\"", n),
                        t => desc_t(t.as_ref()),
                    };
                    return Err(PhpError::parse(
                        format!(
                            "syntax error, unexpected {}, expecting \"->\" or \"?->\" or \"[\"",
                            desc
                        ),
                        self.line(),
                    ));
                }
            }
        } else {
            // A chain consumed at least one link — the ROOT must be a
            // variable/call family; literal, string/interpolated-string
            // and const-expr roots (`"abc"[0]`, `"a$v"[0]`, `true[0]`,
            // `C::CONST[0]`, `new C()->x`) are a compile fatal —
            // string literals are temporaries, not new_variables
            // (probe6/6b vs oracle).
            match leaf {
                Var(_)
                | VarVar(_)
                | Call { .. }
                | MethodCall { .. }
                | StaticCall { .. }
                | StaticCallDyn { .. }
                | Fcc(_)
                | Paren(_)
                | StaticProp { .. } => {}
                _ => {
                    self.write_ctx_errs.push(self.line());
                }
            }
            // In write context (foreach `&`) a call-shaped OUTER node
            // dies too — `&f()`, `&$o->m()`. A callish root under a
            // deref (`&f()->x`) survives the leaf check but still dies.
            let mut kind = callish(&e);
            if kind.is_none() {
                kind = callish(leaf);
            }
            if write_ctx {
                if let Some(kind) = kind {
                    return Err(PhpError::compile_fatal(
                        format!("Can't use {} return value in write context", kind),
                        self.line(),
                    ));
                }
            }
        }
        if Self::has_nullsafe(&e) {
            return Err(PhpError::compile_fatal(
                if write_ctx {
                    "Can't use nullsafe operator in write context"
                } else {
                    "Cannot take reference of a nullsafe chain"
                },
                self.line(),
            ));
        }
        Ok(e)
    }

    /// `e++`/`e--` operand gate — zend's `new_variable` write context:
    /// a whole-parenthesized operand (`($x)++`, `(f())++`) and any
    /// other non-variable expression parse-error `unexpected token
    /// "++"`; call/method results are compile fatals (`Can't use
    /// function/method return value in write context`); string,
    /// literal and `new` chain roots are `Cannot use temporary
    /// expression in write context`; nullsafe chains `Can't use
    /// nullsafe operator in write context`.
    fn incdec_operand(&mut self, e: Expr, op: &str, start: usize) -> Result<Expr, PhpError> {
        use crate::ast::Expr::*;
        // Whole operand wrapped in parens — the start `(` matches the
        // token right before the operator (`self.pos - 1` is `++`/`--`
        // itself at this point).
        if matches!(self.toks.get(start).map(|l| &l.token), Some(Token::Op("("))) {
            let mut depth = 0usize;
            let mut matched = None;
            for (i, t) in self.toks.iter().enumerate().skip(start) {
                match &t.token {
                    Token::Op("(") => depth += 1,
                    Token::Op(")") => {
                        depth -= 1;
                        if depth == 0 {
                            matched = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if matched == Some(self.pos - 2) {
                return Err(PhpError::parse(
                    format!("syntax error, unexpected token \"{}\"", op),
                    self.line(),
                ));
            }
        }
        if Self::has_nullsafe(&e) {
            return Err(PhpError::compile_fatal(
                "Can't use nullsafe operator in write context",
                self.line(),
            ));
        }
        // `++$x++` / `++$o->m()++` — the operand is already a
        // composite `expr`, so the trailing operator can't reduce:
        // zend yacc errors `unexpected token "++"`.
        if start > 0
            && matches!(
                self.toks.get(start - 1).map(|l| &l.token),
                Some(Token::Op("++")) | Some(Token::Op("--"))
            )
        {
            return Err(PhpError::parse(
                format!("syntax error, unexpected token \"{}\"", op),
                self.line(),
            ));
        }
        let linked = matches!(&e, Index { .. } | Prop { .. } | MethodCall { .. });
        // `$o->m()++` — the ++ targets a method result directly:
        // zend's write-context check rejects it.
        if matches!(&e, MethodCall { .. }) {
            return Err(PhpError::compile_fatal(
                "Can't use method return value in write context",
                self.line(),
            ));
        }
        if linked {
            let mut leaf = &e;
            loop {
                leaf = match leaf {
                    Index { e: c, .. } | Prop { obj: c, .. } | MethodCall { obj: c, .. } => {
                        c.as_ref()
                    }
                    Paren(inner) => inner.as_ref(),
                    _ => break,
                };
            }
            match leaf {
                Var(_)
                | VarVar(_)
                | Call { .. }
                | MethodCall { .. }
                | StaticCall { .. }
                | StaticCallDyn { .. }
                | Fcc(_)
                | Paren(_)
                | StaticProp { .. } => {}
                _ => {
                    self.write_ctx_errs.push(self.line());
                }
            }
        } else {
            match &e {
                Var(_) | VarVar(_) | StaticProp { .. } => {}
                Call { .. } | Fcc(_) => {
                    return Err(PhpError::compile_fatal(
                        "Can't use function return value in write context",
                        self.line(),
                    ))
                }
                MethodCall { .. } | StaticCall { .. } | StaticCallDyn { .. } => {
                    return Err(PhpError::compile_fatal(
                        "Can't use method return value in write context",
                        self.line(),
                    ))
                }
                _ => {
                    return Err(PhpError::parse(
                        format!("syntax error, unexpected token \"{}\"", op),
                        self.line(),
                    ))
                }
            }
        }
        Ok(e)
    }

    pub(in crate::parser) fn postfix_rest(
        &mut self,
        mut e: Expr,
        start: usize,
    ) -> Result<Expr, PhpError> {
        loop {
            if self.eat_op("++") {
                e = self.incdec_operand(e, "++", start)?;
                e = Expr::PostInc(Box::new(e));
            } else if self.eat_op("--") {
                e = self.incdec_operand(e, "--", start)?;
                e = Expr::PostDec(Box::new(e));
            } else if self.eat_op("[") {
                let i = if self.at_op("]") {
                    None
                } else {
                    Some(Box::new(self.expr()?))
                };
                self.expect_op("]")?;
                e = Expr::Index { e: Box::new(e), i };
            } else if self.eat_op("(") {
                // `list($a, &$b)` is the one call site zend's grammar
                // allows `&` elements in (destructuring builtin).
                let is_list = matches!(&e, Expr::Str(n) if n.eq_ignore_ascii_case("list"));
                let args = self.args_flags(is_list)?;
                e = Self::fcc_wrap(Expr::Call {
                    name: Box::new(e),
                    args,
                })?;
            } else if self.at_op("->") || self.at_op("?->") {
                let nullsafe = self.at_op("?->");
                self.pos += 1;
                let name = self.prop_name()?;
                if self.at_op("(") {
                    self.pos += 1;
                    let args = self.args()?;
                    e = Self::fcc_wrap(Expr::MethodCall {
                        obj: Box::new(e),
                        name,
                        args,
                        nullsafe,
                    })?;
                } else {
                    e = Expr::Prop {
                        obj: Box::new(e),
                        name,
                        nullsafe,
                    };
                }
            } else if self.eat_op("::") {
                // `parent::` inside a non-trait class with no parent is
                // zend's compile-time fatal — the whole file fails
                // before executing (p15/t/ch vs oracle). Traits defer
                // the check to the using class, and closures defer it
                // to invocation (catchable Error).
                if let Expr::Const(n) = &e {
                    if self.const_ctx == ConstCtx::ArrowSlot
                        && matches!(
                            n.to_ascii_lowercase().as_str(),
                            "self" | "static" | "parent"
                        )
                    {
                        return Err(PhpError::compile_fatal(
                            "Constant expression contains invalid operations",
                            self.line(),
                        ));
                    }
                    if self.const_ctx == ConstCtx::Slot {
                        // Compile-time constants: a static PROP in the
                        // slot is zend's invalid-operations fatal
                        // regardless of the class side (`F::$p`,
                        // `static::$p`, `self::$p` all die the same
                        // way vs oracle). A `::` CALL defers like a
                        // constant fetch — `F::m(...)` FCC inits
                        // lazily (unknown class → 'Class "F" not
                        // found' at eval), and `self::`/`parent::`
                        // defer their scope checks to the slot's
                        // runtime eval (catchable at call/init). Only
                        // `static::` is the compile fatal, and the
                        // wording splits on the member kind:
                        // `static::m(...)`/`static::$p` get
                        // '"static"', bare `static::K` gets
                        // '"static::"'.
                        if matches!(self.peek(), Some(Token::Variable(_)) | Some(Token::Op("$"))) {
                            return Err(PhpError::compile_fatal(
                                "Constant expression contains invalid operations",
                                self.line(),
                            ));
                        }
                        if n.eq_ignore_ascii_case("static")
                            && !matches!(self.peek(), Some(Token::Ident(m)) if m == "class")
                        {
                            // `static::class` falls through to the
                            // class-name-resolution gate below.
                            let msg = if matches!(self.peek(), Some(Token::Ident(_)))
                                && matches!(self.peek2(), Some(Token::Op("(")))
                            {
                                "\"static\" is not allowed in compile-time constants"
                            } else {
                                "\"static::\" is not allowed in compile-time constants"
                            };
                            return Err(PhpError::compile_fatal(msg, self.line()));
                        }
                    } else if self.const_ctx == ConstCtx::Runtime {
                        // Inside a NAMED function there is no class scope at
                        // all — `self::`/`static::`/`parent::` in any member
                        // position is the 'Cannot use "X" when no class
                        // scope is active' compile fatal (at top level the
                        // same stays a runtime catchable Error).
                        if matches!(
                            n.to_ascii_lowercase().as_str(),
                            "self" | "static" | "parent"
                        ) && self.in_named_fn
                            && self.class_ctx.is_empty()
                            && !self.in_closure
                        {
                            return Err(PhpError::compile_fatal(
                                format!("Cannot use \"{}\" when no class scope is active", n),
                                self.line(),
                            ));
                        }
                        // `parent::$p::get()/set()` hook syntax bypasses
                        // the generic no-parent fatal — the hook-ctx
                        // gate after the member decides (a missing
                        // parent then defers to a thrown Error at hook
                        // invocation).
                        if n.eq_ignore_ascii_case("parent")
                            && self.class_ctx.last().map(|c| !c.1 && !c.0).unwrap_or(false)
                            && !self.in_closure
                            && !matches!(
                                self.peek(),
                                Some(Token::Variable(_))
                                    | Some(Token::Op("$"))
                                    | Some(Token::Op("{"))
                            )
                        {
                            return Err(PhpError::compile_fatal(
                                "Cannot use \"parent\" when current class scope has no parent",
                                self.line(),
                            ));
                        }
                    }
                }
                if self.at_op("(") {
                    // `expr::(...)` first-class-callable-ish — unsupported
                    return Err(PhpError::parse("syntax error, unexpected (", self.line()));
                }
                match self.next() {
                    Some(Token::Ident(n)) => {
                        if n == "class" {
                            // `X::class` in a const slot is compile-time
                            // class-name resolution, not a deferrable
                            // constant: classless named-fn defaults die
                            // 'Cannot use "X" when no class scope is
                            // active'; `static::class` anywhere else in
                            // the slot dies 'cannot be used for
                            // compile-time class name resolution'; a
                            // parentless class's `parent::class` is the
                            // no-parent compile fatal (all oracle-probed;
                            // closures and non-fn slots defer to runtime).
                            if self.const_ctx == ConstCtx::Slot {
                                if let Expr::Const(cn) = &e {
                                    let kw = cn.to_ascii_lowercase();
                                    let classless_named = self.in_named_fn
                                        && self.class_ctx.is_empty()
                                        && !self.in_closure;
                                    match kw.as_str() {
                                        "static" => {
                                            return Err(PhpError::compile_fatal(
                                                if classless_named {
                                                    "Cannot use \"static\" when no class scope is active".to_string()
                                                } else {
                                                    "static::class cannot be used for compile-time class name resolution".to_string()
                                                },
                                                self.line(),
                                            ));
                                        }
                                        "self" | "parent" if classless_named => {
                                            return Err(PhpError::compile_fatal(
                                                format!(
                                                    "Cannot use \"{}\" when no class scope is active",
                                                    kw
                                                ),
                                                self.line(),
                                            ));
                                        }
                                        "parent"
                                            if self
                                                .class_ctx
                                                .last()
                                                .map(|c| !c.1 && !c.0)
                                                .unwrap_or(false)
                                                && !self.in_closure =>
                                        {
                                            return Err(PhpError::compile_fatal(
                                                "Cannot use \"parent\" when current class scope has no parent",
                                                self.line(),
                                            ));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            e = Expr::ClassConst {
                                class: Box::new(e),
                                name: "class".into(),
                            };
                        } else if self.at_op("(") {
                            self.pos += 1;
                            // `parent::$p::get()/set()` — PHP checks the
                            // hook context at compile time.
                            if let Expr::StaticProp {
                                class: pc,
                                name: pname_expr,
                            } = &e
                            {
                                // Static prop name: literal name or a
                                // compile-time scalar `${0}`/`{'p'}` —
                                // Zend applies the same hook-context
                                // checks to both (gh17234).
                                let literal_pn = match pname_expr {
                                    PropName::Name(pn) => Some(pn.clone()),
                                    PropName::Expr(inner) => match inner.as_ref() {
                                        Expr::Int(i) => Some(i.to_string()),
                                        Expr::Float(f) => Some(f.to_string()),
                                        Expr::Str(s) => Some(s.clone()),
                                        _ => None,
                                    },
                                    PropName::Var(_) => None,
                                };
                                if let Expr::Const(cn) = pc.as_ref() {
                                    if let Some(pn) = &literal_pn {
                                        if cn.eq_ignore_ascii_case("parent")
                                            && (n.eq_ignore_ascii_case("get")
                                                || n.eq_ignore_ascii_case("set"))
                                        {
                                            if self.cur_class.is_empty() {
                                                return Err(PhpError::fatal(
                                                "Cannot use \"parent\" when no class scope is active",
                                                self.line(),
                                            ));
                                            }
                                            match &self.hook_ctx {
                                            None => {
                                                return Err(PhpError::fatal(
                                                    format!(
                                                        "Must not use parent::${}::{}() outside a property hook",
                                                        pn, n
                                                    ),
                                                    self.line(),
                                                ))
                                            }
                                            Some((hp, hg)) => {
                                                if hp != pn {
                                                    return Err(PhpError::fatal(
                                                        format!(
                                                            "Must not use parent::${}::{}() in a different property (${})",
                                                            pn, n, hp
                                                        ),
                                                        self.line(),
                                                    ));
                                                }
                                                if *hg != n.eq_ignore_ascii_case("get") {
                                                    return Err(PhpError::fatal(
                                                        format!(
                                                            "Must not use parent::${}::{}() in a different property hook ({})",
                                                            pn,
                                                            n,
                                                            if *hg { "get" } else { "set" }
                                                        ),
                                                        self.line(),
                                                    ));
                                                }
                                            }
                                        }
                                        }
                                    }
                                }
                            }
                            let args = self.args()?;
                            e = Self::fcc_wrap(Expr::StaticCall {
                                class: Box::new(e),
                                name: n,
                                args,
                            })?;
                        } else {
                            e = Expr::ClassConst {
                                class: Box::new(e),
                                name: n,
                            };
                        }
                    }
                    Some(Token::Variable(n)) => {
                        if self.at_op("(") {
                            // `C::$method()` — dynamic static call; the
                            // name comes from the variable's value
                            // (tests/lang/044).
                            self.pos += 1;
                            let args = self.args()?;
                            e = Self::fcc_wrap(Expr::StaticCallDyn {
                                class: Box::new(e),
                                name: Box::new(Expr::Var(n)),
                                args,
                            })?;
                        } else {
                            // `C::$name` — a literal static prop name
                            // (unlike `$o->$name`, which reads the var).
                            e = Expr::StaticProp {
                                class: Box::new(e),
                                name: PropName::Name(n),
                            };
                        }
                    }
                    // `Cls::{expr}` / `Cls::${expr}` — dynamic name or call.
                    Some(Token::Op("{")) => {
                        let inner = self.expr()?;
                        self.expect_op("}")?;
                        if self.at_op("(") {
                            self.pos += 1;
                            let args = self.args()?;
                            e = Expr::MethodCall {
                                obj: Box::new(e),
                                name: PropName::Expr(Box::new(inner)),
                                args,
                                nullsafe: false,
                            };
                        } else {
                            e = Expr::StaticProp {
                                class: Box::new(e),
                                name: PropName::Expr(Box::new(inner)),
                            };
                        }
                    }
                    Some(Token::Op("$")) => {
                        // `C::$${x}` / `C::${expr}` — name by expression.
                        let inner = if self.at_op("{") {
                            self.pos += 1;
                            let inner = self.expr()?;
                            self.expect_op("}")?;
                            inner
                        } else {
                            // `C::$$x` — name read from variable $x.
                            match self.next() {
                                Some(Token::Variable(n)) => Expr::Var(n),
                                t => {
                                    return Err(PhpError::parse(
                                        format!("syntax error, unexpected {}", desc_t(t.as_ref())),
                                        self.line(),
                                    ))
                                }
                            }
                        };
                        if self.at_op("(") {
                            self.pos += 1;
                            let args = self.args()?;
                            e = Expr::MethodCall {
                                obj: Box::new(e),
                                name: PropName::Expr(Box::new(inner)),
                                args,
                                nullsafe: false,
                            };
                        } else {
                            e = Expr::StaticProp {
                                class: Box::new(e),
                                name: PropName::Expr(Box::new(inner)),
                            };
                        }
                    }
                    t => {
                        return Err(PhpError::parse(
                            format!("syntax error, unexpected {}", desc_t(t.as_ref())),
                            self.line(),
                        ))
                    }
                }
            } else if self.at_op("=") && matches!(self.peek2(), Some(Token::Op("&"))) {
                // `expr =& variable` — the `&` binds tighter than any
                // op that can follow (`?`, `??`, binary ops keep going
                // on the Assign node). The RHS is Zend's restricted
                // `new_variable` grammar, not a full expression.
                let eq_pos = self.pos;
                self.pos += 2;
                let rhs = self.ref_variable(false)?;
                let target = self.list_target(e)?;
                // Non-lvalue =& targets are zend's parse error at the
                // `=` (`($c ? $a : $b) =& $x`, finding 16) — the gate
                // normalizes '=&' to '=' for the message.
                self.assign_target_gate(&target, "=&", start, eq_pos)?;
                e = Expr::Assign {
                    target: Box::new(target),
                    op: "=&",
                    value: Box::new(rhs),
                };
            } else {
                return Ok(e);
            }
        }
    }

    /// `expr(...)` — first-class-callable arg lists rewrite their call
    /// node into `Expr::Fcc`; everything else keeps its args.
    pub(in crate::parser) fn has_nullsafe(e: &Expr) -> bool {
        match e {
            Expr::MethodCall { obj, nullsafe, .. } => *nullsafe || Self::has_nullsafe(obj),
            Expr::Prop { obj, nullsafe, .. } => *nullsafe || Self::has_nullsafe(obj),
            // Chain positions only — arg lists are independent exprs.
            Expr::Index { e, .. } => Self::has_nullsafe(e),
            Expr::Paren(inner) => Self::has_nullsafe(inner),
            Expr::Call { name, .. } => Self::has_nullsafe(name),
            Expr::StaticCall { class, .. }
            | Expr::StaticCallDyn { class, .. }
            | Expr::StaticProp { class, .. }
            | Expr::ClassConst { class, .. } => Self::has_nullsafe(class),
            Expr::Fcc(inner) => Self::has_nullsafe(inner),
            _ => false,
        }
    }

    pub(in crate::parser) fn fcc_wrap(node: Expr) -> Result<Expr, PhpError> {
        let is_fcc = match &node {
            Expr::Call { args, .. }
            | Expr::MethodCall { args, .. }
            | Expr::StaticCall { args, .. }
            | Expr::StaticCallDyn { args, .. } => {
                args.len() == 1 && matches!(args[0], Expr::FccMark)
            }
            _ => false,
        };
        if !is_fcc {
            return Ok(node);
        }
        // `$o?->m(...)` — and any nullsafe link in the receiver chain
        // (`$o?->p->m(...)`) — is a compile-time fatal
        // (first_class_callable_012/013).
        if Self::has_nullsafe(&node) {
            return Err(PhpError::fatal(
                "Cannot combine nullsafe operator with Closure creation",
                0,
            ));
        }
        Ok(Expr::Fcc(Box::new(node)))
    }

    /// Bare `...` inside ctor args is a compile-time fatal
    /// ("Cannot create Closure for new expression" — zend_compile.c).
    pub(in crate::parser) fn check_no_fcc_ctor(&self, args: &[Expr]) -> Result<(), PhpError> {
        if args.len() == 1 && matches!(args[0], Expr::FccMark) {
            return Err(PhpError::fatal(
                "Cannot create Closure for new expression",
                self.line(),
            ));
        }
        Ok(())
    }

    pub(in crate::parser) fn args(&mut self) -> Result<Vec<Expr>, PhpError> {
        self.args_flags(false)
    }

    /// `list($a, &$b)` — zend's destructuring builtin accepts `&`
    /// elements; every other call site rejects them at parse time.
    pub(in crate::parser) fn args_flags(&mut self, allow_ref: bool) -> Result<Vec<Expr>, PhpError> {
        let mut args = Vec::new();
        let mut unpacked = false;
        let mut seen_named = false;
        while !self.at_op(")") {
            if self.at_op("...") {
                self.pos += 1;
                if args.is_empty() && self.eat_op(")") {
                    // `f(...)` — first-class callable marker.
                    return Ok(vec![Expr::FccMark]);
                }
                if seen_named {
                    return Err(PhpError::compile_fatal(
                        "Cannot use argument unpacking after named arguments",
                        self.line(),
                    ));
                }
                args.push(Expr::Unpack(Box::new(self.expr()?)));
                unpacked = true;
                if !self.eat_op(",") {
                    break;
                }
                continue;
            }
            let named = matches!(self.peek(), Some(Token::Ident(_)))
                && matches!(self.peek2(), Some(Token::Op(":")));
            if named {
                // named arguments `name:` — name recorded via Str marker
                let n = self.ident().unwrap();
                self.pos += 1; // :
                let v = self.expr()?;
                args.push(Expr::Binary {
                    op: "named",
                    l: Box::new(Expr::Str(n)),
                    r: Box::new(v),
                });
                seen_named = true;
            } else {
                if seen_named {
                    return Err(PhpError::compile_fatal(
                        "Cannot use positional argument after named argument",
                        self.line(),
                    ));
                }
                if unpacked {
                    return Err(PhpError::compile_fatal(
                        "Cannot use positional argument after argument unpacking",
                        self.line(),
                    ));
                }
                if allow_ref && self.at_op("&") {
                    self.pos += 1;
                    args.push(Expr::ByRef(Box::new(self.ref_variable(false)?)));
                } else {
                    let e = self.expr()?;
                    // `list('k' => $v)` — keyed elements carry the key
                    // expr on a transient `=>` binary; list_target
                    // converts it to a keyed List element.
                    if allow_ref && self.eat_op("=>") {
                        let t = if self.at_op("&") {
                            self.pos += 1;
                            Expr::ByRef(Box::new(self.ref_variable(false)?))
                        } else {
                            self.expr()?
                        };
                        args.push(Expr::Binary {
                            op: "=>",
                            l: Box::new(e),
                            r: Box::new(t),
                        });
                    } else {
                        args.push(e);
                    }
                }
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(args)
    }

    pub(in crate::parser) fn prop_name(&mut self) -> Result<PropName, PhpError> {
        match self.next() {
            Some(Token::Ident(n)) => Ok(PropName::Name(n)),
            Some(Token::Variable(n)) => Ok(PropName::Var(n)),
            Some(Token::Op("{")) => {
                let e = self.expr()?;
                self.expect_op("}")?;
                Ok(PropName::Expr(Box::new(e)))
            }
            // `$obj->${expr}` / `$obj->$$var` — variable-variable: the prop
            // name is the VALUE of the variable named by the expr
            // (engine_assignExecutionOrder_001).
            Some(Token::Op("$")) => {
                if self.at_op("{") {
                    self.pos += 1;
                    let e = self.expr()?;
                    self.expect_op("}")?;
                    Ok(PropName::Expr(Box::new(Expr::VarVar(Box::new(e)))))
                } else {
                    match self.next() {
                        Some(Token::Variable(n)) => Ok(PropName::Expr(Box::new(Expr::VarVar(
                            Box::new(Expr::Var(n)),
                        )))),
                        t => Err(PhpError::parse(
                            format!(
                                "syntax error, unexpected {}, expecting identifier",
                                desc_t(t.as_ref())
                            ),
                            self.line(),
                        )),
                    }
                }
            }
            t => Err(PhpError::parse(
                format!(
                    "syntax error, unexpected {}, expecting identifier",
                    desc_t(t.as_ref())
                ),
                self.line(),
            )),
        }
    }

    pub(in crate::parser) fn primary(&mut self) -> Result<Expr, PhpError> {
        match self.peek().cloned() {
            Some(Token::Int(v)) => {
                self.pos += 1;
                Ok(Expr::Int(v))
            }
            Some(Token::Float(v)) => {
                self.pos += 1;
                Ok(Expr::Float(v))
            }
            Some(Token::SimpleString(s)) => {
                self.pos += 1;
                Ok(Expr::Str(s))
            }
            Some(Token::InterpString(parts)) => {
                self.pos += 1;
                Ok(Expr::Interp(parts))
            }
            Some(Token::Variable(name)) => {
                self.pos += 1;
                Ok(Expr::Var(name))
            }
            Some(Token::Op("(")) => {
                self.pos += 1;
                let e = self.expr()?;
                self.expect_op(")")?;
                // Mark parenthesized class-prop refs so `(X::$p)::m()`
                // is not confused with the `X::$p::m()` hook syntax.
                Ok(if matches!(e, Expr::StaticProp { .. }) {
                    Expr::Paren(Box::new(e))
                } else {
                    e
                })
            }
            Some(Token::Op("[")) => {
                // Short array literal.
                self.pos += 1;
                let items = self.array_items("]")?;
                Ok(Expr::ArrayLit(items))
            }
            Some(Token::Op("$")) => {
                // Variable variable: `$$name`, `${expr}`, or chained
                // `$$$a` (023).
                self.pos += 1;
                match self.peek().cloned() {
                    Some(Token::Variable(n)) => {
                        self.pos += 1;
                        Ok(Expr::VarVar(Box::new(Expr::Var(n))))
                    }
                    Some(Token::Op("{")) => {
                        self.pos += 1;
                        let e = self.expr()?;
                        self.expect_op("}")?;
                        Ok(Expr::VarVar(Box::new(e)))
                    }
                    Some(Token::Op("$")) => {
                        let e = self.primary()?;
                        Ok(Expr::VarVar(Box::new(e)))
                    }
                    t => Err(PhpError::parse(
                        format!("syntax error, unexpected {}", desc_t(t.as_ref())),
                        self.line(),
                    )),
                }
            }
            Some(Token::Ident(_)) => {
                if self.ident_is("true") {
                    self.pos += 1;
                    Ok(Expr::Bool(true))
                } else if self.ident_is("false") {
                    self.pos += 1;
                    Ok(Expr::Bool(false))
                } else if self.ident_is("null") {
                    self.pos += 1;
                    Ok(Expr::Null)
                } else if self.ident_is("isset") {
                    self.pos += 1;
                    self.expect_op("(")?;
                    let args = self.expr_list()?;
                    self.expect_op(")")?;
                    Ok(Expr::Isset(args))
                } else if self.ident_is("empty") {
                    self.pos += 1;
                    self.expect_op("(")?;
                    let e = self.expr()?;
                    self.expect_op(")")?;
                    Ok(Expr::Empty(Box::new(e)))
                } else if self.ident_is("yield") {
                    self.pos += 1;
                    // `yield from <it>` splices another iterable's items.
                    if self.ident_is("from") {
                        self.pos += 1;
                        let e = self.assign()?;
                        return Ok(Expr::YieldFrom(Box::new(e)));
                    }
                    // Operand is optional: `yield;` / `(yield)` / `f(yield)`
                    // / `yield ,` in list contexts push a null value.
                    let operand_end = matches!(
                        self.peek(),
                        None | Some(Token::Op(";"))
                            | Some(Token::Op(")"))
                            | Some(Token::Op("]"))
                            | Some(Token::Op(","))
                            | Some(Token::Op(":"))
                    );
                    if operand_end {
                        return Ok(Expr::Yield {
                            key: None,
                            val: None,
                        });
                    }
                    let first = self.assign()?;
                    // `yield k => v`
                    if self.at_op("=>") {
                        self.pos += 1;
                        let v = self.assign()?;
                        return Ok(Expr::Yield {
                            key: Some(Box::new(first)),
                            val: Some(Box::new(v)),
                        });
                    }
                    Ok(Expr::Yield {
                        key: None,
                        val: Some(Box::new(first)),
                    })
                } else if self.ident_is("print") {
                    self.pos += 1;
                    let e = self.expr()?;
                    Ok(Expr::Print(Box::new(e)))
                } else if self.ident_is("exit") || self.ident_is("die") {
                    self.pos += 1;
                    let arg = if self.eat_op("(") {
                        let a = if self.at_op(")") {
                            None
                        } else {
                            Some(Box::new(self.expr()?))
                        };
                        self.expect_op(")")?;
                        a
                    } else if matches!(
                        self.peek(),
                        Some(Token::Op(";")) | Some(Token::Op(")")) | None
                    ) {
                        None
                    } else {
                        Some(Box::new(self.expr()?))
                    };
                    Ok(Expr::Exit(arg))
                } else if self.ident_is("array") && matches!(self.peek2(), Some(Token::Op("("))) {
                    self.pos += 1;
                    self.expect_op("(")?;
                    let items = self.array_items(")")?;
                    Ok(Expr::ArrayLit(items))
                } else if self.ident_is("function")
                    || (self.ident_is("static")
                        && matches!(self.peek2(), Some(Token::Ident(f)) if f.eq_ignore_ascii_case("function")
                            || f.eq_ignore_ascii_case("fn")))
                    || (self.ident_is("fn") && !matches!(self.peek2(), Some(Token::Op("\\"))))
                {
                    // `fn` is soft-reserved: `fn\test()` is a namespaced
                    // call, not an arrow fn (ns_name_reserved_keywords).
                    self.closure_expr()
                } else if self.ident_is("new") {
                    self.pos += 1;
                    let (class, mut ctor_args) = self.new_class_expr()?;
                    // `new parent()` in a parentless class — same
                    // compile-time gate as `parent::` (p10new/ch3);
                    // inside a closure it defers to the runtime Error.
                    if let Expr::Const(n) = &class {
                        if self.const_ctx == ConstCtx::ArrowSlot
                            && matches!(
                                n.to_ascii_lowercase().as_str(),
                                "self" | "static" | "parent"
                            )
                        {
                            return Err(PhpError::compile_fatal(
                                "Constant expression contains invalid operations",
                                self.line(),
                            ));
                        }
                        if self.const_ctx == ConstCtx::Slot {
                            // `new static` in a const slot — the
                            // const-expr compile fatal (no `::`); `new
                            // self()`/`new parent()` defer to runtime.
                            if n.eq_ignore_ascii_case("static") {
                                return Err(PhpError::compile_fatal(
                                    "\"static\" is not allowed in compile-time constants",
                                    self.line(),
                                ));
                            }
                        } else if self.const_ctx == ConstCtx::Runtime {
                            // `new self()`/`new static()`/`new parent()`
                            // inside a named function — no class scope:
                            // 'Cannot use "X" when no class scope is active'
                            // (k12). Same gate as the `X::` postfix.
                            if matches!(
                                n.to_ascii_lowercase().as_str(),
                                "self" | "static" | "parent"
                            ) && self.in_named_fn
                                && self.class_ctx.is_empty()
                                && !self.in_closure
                            {
                                return Err(PhpError::compile_fatal(
                                    format!("Cannot use \"{}\" when no class scope is active", n),
                                    self.line(),
                                ));
                            }
                            if n.eq_ignore_ascii_case("parent")
                                && self.class_ctx.last().map(|c| !c.1 && !c.0).unwrap_or(false)
                                && !self.in_closure
                            {
                                return Err(PhpError::compile_fatal(
                                    "Cannot use \"parent\" when current class scope has no parent",
                                    self.line(),
                                ));
                            }
                        }
                    }
                    let mut ctor_parens = false;
                    if self.at_op("(") {
                        self.pos += 1;
                        ctor_args = self.args()?;
                        self.check_no_fcc_ctor(&ctor_args)?;
                        ctor_parens = true;
                    }
                    // A bare `new A` (no ctor parens) can't chain member
                    // access — zend parse-errors on the next op:
                    // 'unexpected token "->"' (parens or `new A()` do
                    // chain: `(new A)->x`, `new A()->x` — m15 vs oracle).
                    // `new class{...}` is a completed value expr —
                    // the anonymous-class body closes it, so
                    // ->/?->/[ postfixes chain freely (probe m2).
                    if !ctor_parens && !matches!(class, Expr::AnonClass(_)) {
                        if let Some(Token::Op(o)) = self.peek() {
                            if matches!(&**o, "->" | "?->" | "[") {
                                return Err(PhpError::parse(
                                    format!("syntax error, unexpected token \"{}\"", o),
                                    self.line(),
                                ));
                            }
                        }
                    }
                    Ok(Expr::New {
                        class: Box::new(class),
                        args: ctor_args,
                    })
                } else if self.ident_is("match") && matches!(self.peek2(), Some(Token::Op("("))) {
                    self.match_expr()
                } else if self.ident_is("list") && matches!(self.peek2(), Some(Token::Op("("))) {
                    self.pos += 1;
                    self.expect_op("(")?;
                    let mut items = Vec::new();
                    while !self.at_op(")") {
                        if self.at_op(",") {
                            items.push(None);
                            self.pos += 1;
                            continue;
                        }
                        // `list($a, &$b)` — zend's destructuring `&`
                        // element (probe5b): binds the source cell.
                        if self.at_op("&") {
                            self.pos += 1;
                            items.push(Some((
                                None,
                                Expr::ByRef(Box::new(self.ref_variable(false)?)),
                            )));
                        } else {
                            let e = self.expr()?;
                            if self.eat_op("=>") {
                                // `list('k' => $v)` — a keyed element.
                                let t = if self.at_op("&") {
                                    self.pos += 1;
                                    Expr::ByRef(Box::new(self.ref_variable(false)?))
                                } else {
                                    self.expr()?
                                };
                                items.push(Some((Some(e), t)));
                            } else {
                                items.push(Some((None, e)));
                            }
                        }
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                    self.list_mix_check(&items)?;
                    Ok(Expr::List(items))
                } else if self.ident_is("include")
                    || self.ident_is("include_once")
                    || self.ident_is("require")
                    || self.ident_is("require_once")
                    || self.ident_is("eval")
                {
                    let kind = match self.ident().unwrap().to_ascii_lowercase().as_str() {
                        "include" => IncludeKind::Include,
                        "include_once" => IncludeKind::IncludeOnce,
                        "require" => IncludeKind::Require,
                        "require_once" => IncludeKind::RequireOnce,
                        _ => IncludeKind::Eval,
                    };
                    // zend grammar: `T_INCLUDE expr` — the operand is a
                    // full expression at include's very low precedence,
                    // so `include ('/f') == 5` includes the bool result
                    // (zend throws "Path cannot be empty"). `eval` is
                    // `T_EVAL '(' expr ')'` — its parens are syntax.
                    let e = if kind == IncludeKind::Eval {
                        self.expect_op("(")?;
                        let e = self.expr()?;
                        self.expect_op(")")?;
                        e
                    } else {
                        self.expr()?
                    };
                    Ok(Expr::Include {
                        kind,
                        e: Box::new(e),
                    })
                } else if self.ident_is("__line__") {
                    let line = self.line();
                    self.pos += 1;
                    Ok(Expr::Int(line as i64))
                } else if self.ident_is("__file__") {
                    self.pos += 1;
                    Ok(Expr::MagicConst(MagicConst::File))
                } else if self.ident_is("__dir__") {
                    self.pos += 1;
                    Ok(Expr::MagicConst(MagicConst::Dir))
                } else if self.ident_is("__function__") {
                    self.pos += 1;
                    Ok(Expr::MagicConst(MagicConst::Function))
                } else if self.ident_is("__method__") {
                    self.pos += 1;
                    Ok(Expr::MagicConst(MagicConst::Method))
                } else if self.ident_is("__class__") {
                    self.pos += 1;
                    Ok(Expr::MagicConst(MagicConst::Class))
                } else if self.ident_is("__trait__") {
                    self.pos += 1;
                    Ok(Expr::MagicConst(MagicConst::Trait))
                } else if self.ident_is("__namespace__") {
                    self.pos += 1;
                    // __NAMESPACE__ is compile-time per the file the
                    // literal sits in — an include's top level is global
                    // even inside a namespaced caller (ns_069).
                    Ok(Expr::Str(self.cur_ns.clone()))
                } else if self.ident_is("__property__") {
                    self.pos += 1;
                    Ok(Expr::MagicConst(MagicConst::Property))
                } else if self.ident_is("static") {
                    // `static::` — static class ref
                    self.pos += 1;
                    Ok(Expr::Const("static".into()))
                } else if self.ident_is("self") {
                    self.pos += 1;
                    Ok(Expr::Const("self".into()))
                } else if self.ident_is("parent") {
                    self.pos += 1;
                    Ok(Expr::Const("parent".into()))
                } else {
                    // Bare identifier: constant or function name target. Qualified
                    // names (A\B) and function call args go through here.
                    let name = self.name_path().unwrap_or_default();
                    if self.at_op("(") {
                        self.pos += 1;
                        let args = self.args()?;
                        let resolved = self.ns_resolve(&name, NsKind::Func);
                        // Unqualified literal names carry a \u{1} marker:
                        // call_named then applies the ns\f -> f fallback.
                        // Qualified/FQ-resolved names are exact already.
                        let resolved = if resolved.contains('\\') {
                            resolved
                        } else {
                            format!("{}{}", '\u{1}', resolved)
                        };
                        Self::fcc_wrap(Expr::Call {
                            name: Box::new(Expr::Str(resolved)),
                            args,
                        })
                    } else if self.at_op("::") {
                        // `X::…` — a class name in every form.
                        Ok(Expr::Const(self.ns_resolve(&name, NsKind::Class)))
                    } else {
                        // Constant read: resolve now so a `use` alias
                        // applies; an unqualified miss keeps the bare
                        // name for the runtime ns\name -> name fallback.
                        if name.starts_with('\\') {
                            Ok(Expr::Const(name))
                        } else {
                            Ok(Expr::Const(self.ns_resolve(&name, NsKind::Const)))
                        }
                    }
                }
            }
            Some(Token::Op("\\")) => {
                // Fully-qualified name: \PHP_EOL, \Foo\Bar::baz, \func().
                // The `\` marker is kept — downstream lookups treat a
                // backslash-prefixed name as exact (no ns fallback).
                let name = self.name_path().unwrap_or_default();
                if name.is_empty() {
                    return Err(PhpError::parse(
                        "syntax error, unexpected token \"\\\"",
                        self.line(),
                    ));
                }
                if self.at_op("(") {
                    self.pos += 1;
                    let args = self.args()?;
                    Self::fcc_wrap(Expr::Call {
                        name: Box::new(Expr::Str(name)),
                        args,
                    })
                } else {
                    // `\true`/`\false`/`\null` are literals, not const
                    // lookups (PHPUnit uses `\true` in strict code).
                    let bare = name.trim_start_matches('\\');
                    match bare.to_lowercase().as_str() {
                        "true" => Ok(Expr::Bool(true)),
                        "false" => Ok(Expr::Bool(false)),
                        "null" => Ok(Expr::Null),
                        _ => Ok(Expr::Const(name)),
                    }
                }
            }
            Some(t) => Err(PhpError::parse(
                format!("syntax error, unexpected {}", desc_t(Some(&t))),
                self.line(),
            )),
            None => Err(PhpError::parse(
                "syntax error, unexpected end of file",
                self.line(),
            )),
        }
    }

    pub(in crate::parser) fn array_items(
        &mut self,
        close: &str,
    ) -> Result<Vec<(Option<Expr>, Expr)>, PhpError> {
        let mut items = Vec::new();
        while !self.at_op(close) {
            // List-destructuring hole: `[, $b] = ...` / `[$a, , $c]`.
            // Expr::Null marks the skipped slot; list_target maps it to
            // None. (In array-literal position a hole is a superset.)
            if self.eat_op(",") {
                items.push((None, Expr::Null));
                continue;
            }
            let first = self.array_elem()?;
            if self.eat_op("=>") {
                if matches!(first, Expr::Unpack(_)) {
                    return Err(PhpError::parse(
                        "syntax error, unexpected token \"=>\"",
                        self.line(),
                    ));
                }
                let v = self.array_elem()?;
                items.push((Some(first), v));
            } else {
                items.push((None, first));
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(close)?;
        Ok(items)
    }

    /// One array-literal element — may be `&expr` (bound by reference)
    /// or `...expr` (spread, PHP 7.4+).
    pub(in crate::parser) fn array_elem(&mut self) -> Result<Expr, PhpError> {
        if self.eat_op("...") {
            Ok(Expr::Unpack(Box::new(self.expr()?)))
        } else if self.eat_op("&") {
            Ok(Expr::ByRef(Box::new(self.ref_variable(false)?)))
        } else {
            self.expr()
        }
    }
}
