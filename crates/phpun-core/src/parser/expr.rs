//! Expression grammar: closures and `new class` atoms, the full
//! precedence chain down to `primary`, call args and array literals.

use super::*;
use crate::interp::util::is_compile_const;

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
        // Zend names a closure by its enclosing context in the
        // optional-before-required notice: `{closure:enclosing():L}`
        // nested, `{closure:FILE:L}` at top level (\u{1} is the file,
        // substituted when the diagnostic is emitted).
        let clo_name = match self.fn_ctx.last() {
            // An enclosing closure's own name already embeds its
            // `{closure:FILE:L}` — nest it as-is, no `()` appended.
            Some(parent) if parent.starts_with("{closure:") => {
                format!("{{closure:{}:{}}}", parent, line)
            }
            Some(parent) => format!("{{closure:{}():{}}}", parent, line),
            None => format!("{{closure:{}:{}}}", '\u{1}', line),
        };
        if !arrow && self.ident_is("use") {
            self.pos += 1;
            self.expect_op("(")?;
            while !self.at_op(")") {
                let by_ref = self.eat_op("&");
                let l = self.line();
                match self.next() {
                    Some(Token::Variable(n)) => uses.push((n, by_ref)),
                    t => {
                        return Err(PhpError::parse(
                            format!(
                                "syntax error, unexpected {}, expecting variable",
                                desc_t(t.as_ref())
                            ),
                            l,
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
                    return Err(PhpError::compile_fatal(
                        "Cannot use auto-global as lexical variable",
                        self.prev_line(),
                    ));
                }
                if !seen.insert(n.as_str()) {
                    return Err(PhpError::compile_fatal(
                        format!("Cannot use variable ${} twice", n),
                        self.prev_line(),
                    ));
                }
            }
            for (n, _) in &uses {
                if params.iter().any(|p| p.name == *n) {
                    return Err(PhpError::compile_fatal(
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
        self.fn_ctx.push(clo_name);
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
        self.fn_ctx.pop();
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
                let mut args = self.args()?;
                Self::dyn_arglines(&mut args);
                args
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
            self.class_ctx
                .push((parent.is_some() || !implements.is_empty(), false));
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
                            line: self.line(),
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
                        line: self.line(),
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
                    let pl = self.line();
                    let pname = match self.next() {
                        Some(Token::Variable(n)) => n,
                        t => {
                            return Err(PhpError::parse(
                                format!(
                                    "syntax error, unexpected {}, expecting variable",
                                    desc_t(t.as_ref())
                                ),
                                pl,
                            ))
                        }
                    };
                    let default = if self.eat_op("=") {
                        Some(self.expr()?)
                    } else {
                        None
                    };
                    let dline = default
                        .as_ref()
                        .and_then(crate::ast::start_line)
                        .unwrap_or(pline);
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
                        dline,
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
                        let il = self.line();
                        let i = if self.at_op("]") {
                            None
                        } else {
                            Some(Box::new(Self::markline(self.expr()?, il)))
                        };
                        self.expect_op("]")?;
                        e = Expr::Index { e: Box::new(e), i };
                    } else if self.eat_op("->") {
                        // `new $this->prop` (bug21669); `->m()` stays ctor args.
                        let site = self.line();
                        match self.next() {
                            Some(Token::Ident(pn)) => {
                                e = Expr::Prop {
                                    obj: Box::new(e),
                                    name: PropName::Name(pn),
                                    nullsafe: false,
                                    site,
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
        let sl = self.line();
        let subject = self.expr()?;
        self.expect_group(")")?;
        self.expect_op("{")?;
        let mut arms = Vec::new();
        // The last arm-result's final token line — zend stamps an
        // empty `[]` result's INIT_ARRAY there.
        let mut tail = 0usize;
        while !self.at_op("}") {
            if self.eat_ident("default") {
                self.expect_op("=>")?;
                let rl = self.line();
                let r = self.expr()?;
                tail = self.prev_line();
                arms.push(MatchArm {
                    conds: Vec::new(),
                    result: Self::markline(r, rl),
                });
            } else {
                let mut conds = Vec::new();
                loop {
                    let cl = self.line();
                    conds.push(Self::markline(self.expr()?, cl));
                    if !self.eat_op(",") || self.at_op("=>") {
                        break;
                    }
                }
                self.expect_op("=>")?;
                let rl = self.line();
                let r = self.expr()?;
                tail = self.prev_line();
                arms.push(MatchArm {
                    conds,
                    result: Self::markline(r, rl),
                });
            }
            self.eat_op(",");
        }
        let close = self.line();
        self.expect_op("}")?;
        Ok(Expr::Match {
            subject: Box::new(Self::markline(subject, sl)),
            arms,
            end: if tail == 0 { close } else { tail },
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
            // `throw`'s operand is a full expr in zend — it reaches
            // below even `or` (`throw $e or die` throws the `or`'s
            // bool result, not $e).
            let e = self.logical_or_low()?;
            return Ok(Expr::Throw(Box::new(e)));
        }
        self.logical_or_low()
    }

    /// `or`/`xor`/`and` — zend's three lowest precedence levels, all
    /// BELOW `=` (`$x = $v or die` parses `($x = $v) or die`) and
    /// below print/yield. `or` is lowest, `and` highest of the three.
    /// The word forms compile to the same BOOL ops as `||`/`xor`/`&&`
    /// (but the result never feeds the assign's RHS).
    pub(in crate::parser) fn logical_or_low(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.logical_xor_low()?;
        while self.ident_is("or") {
            self.pos += 1;
            let rline = self.line();
            let r = self.logical_xor_low()?;
            e = Expr::Binary {
                op: "||",
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
        Ok(e)
    }

    fn logical_xor_low(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.logical_and_low()?;
        while self.ident_is("xor") {
            self.pos += 1;
            let rline = self.line();
            let r = self.logical_and_low()?;
            e = Expr::Binary {
                op: "xor",
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
        Ok(e)
    }

    fn logical_and_low(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.assign()?;
        while self.ident_is("and") {
            self.pos += 1;
            let rline = self.line();
            let r = self.assign()?;
            e = Expr::Binary {
                op: "&&",
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
        Ok(e)
    }

    pub(in crate::parser) fn assign(&mut self) -> Result<Expr, PhpError> {
        // PHP 8 throw-expression: legal wherever an expression is —
        // `?? throw`, ternary arms, match arms, arrow-fn bodies.
        if self.ident_is("throw") {
            self.pos += 1;
            let tl = self.line();
            let e = self.logical_or_low()?;
            return Ok(Expr::Throw(Box::new(Self::markline(e, tl))));
        }
        let lhs_start = self.pos;
        let tl = self.line();
        let e = self.ternary()?;

        if let Some(Token::Op(op)) = self.peek() {
            if ASSIGN_OPS.contains(op) {
                let op_pos = self.pos;
                let mut op: &'static str = op;
                self.pos += 1;
                if op == "=" && self.eat_op("&") {
                    op = "=&"; // by-reference assignment
                }
                let rl = self.line();
                let rhs = self.assign()?;
                let target = self.list_target(e)?;
                if let Expr::List(items) = &target {
                    // zend_compile_list_assign's writability verify is
                    // a COMPILE error — it aborts the whole file
                    // before anything runs.
                    if let Some(err) = Self::list_assign_check(items, &rhs, rl) {
                        return Err(err);
                    }
                }
                self.check_list_ref_literal(op, &target, &rhs)?;
                self.assign_target_gate(&target, op, lhs_start, op_pos)?;
                return Ok(Expr::Assign {
                    target: Box::new(target),
                    op,
                    value: Box::new(Self::markline(rhs, rl)),
                    line: tl,
                });
            }
        }
        Ok(e)
    }

    /// `=`/`&=`/`op=` LHS gate — mirrors zend_compile_assign's
    /// writability pass: a whole-
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
        // The LHS itself carries argline marks — strip them before the
        // shape checks (`f()[] = v` reached the catch-all otherwise).
        let e = Self::unmark_lval(e);
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
            self.write_ctx_errs.push((
                format!("Can't use {} return value in write context", k),
                self.line(),
            ));
            return Ok(());
        }
        if Self::has_nullsafe(e) {
            self.write_ctx_errs.push((
                "Can't use nullsafe operator in write context".to_string(),
                self.line(),
            ));
            return Ok(());
        }
        match e {
            Var(_) | VarVar(..) | StaticProp { .. } => Ok(()),
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
                    self.write_ctx_errs.push((
                        "Cannot use temporary expression in write context".to_string(),
                        self.line(),
                    ));
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

    /// `clone (` — does the `(` (at `pos`) hold a top-level `,`?
    /// Only then is it PHP 8.5's clone-with call form; without a
    /// comma the parens group the unary operand (`clone ($o)->m` is
    /// `clone(($o)->m)` in zend).
    fn clone_paren_has_comma(&self) -> bool {
        let mut depth = 0usize;
        for lt in &self.toks[self.pos..] {
            match &lt.token {
                Token::Op(o) if matches!(*o, "(" | "[" | "{") => depth += 1,
                Token::Op(o) if matches!(*o, ")" | "]" | "}") => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return false;
                    }
                }
                Token::Op(",") if depth == 1 => return true,
                _ => {}
            }
        }
        false
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

    /// `[a, b]` / `list(a, b)` on the left of `=` is destructuring.
    /// Elements keep their `argline` marks — zend sites each
    /// element's own store op at the element's line.
    ///
    /// `list_kind` threads the OUTER destructure's syntax (true =
    /// `list()`, false = `[]`): a nested destructure of the other
    /// syntax is zend's compile fatal 'Cannot mix [] and list()' —
    /// `list($c, [$d])`, `[$c, list($d)]`, `[$a, list($b, [$c])]` all
    /// die at compile; `list($a, list($b))` and `[$a, [$b]]` compile.
    pub(in crate::parser) fn list_target(&mut self, e: Expr) -> Result<Expr, PhpError> {
        match e {
            Expr::ArrayLit(items) => Ok(Expr::List(
                items
                    .into_iter()
                    .map(|(k, v)| Self::list_elem_kv(k, v, false).map(|o| o.map(Self::list_keyed)))
                    .collect::<Result<_, _>>()?,
            )),
            Expr::Call {
                name,
                args,
                site,
                callee,
            } => match *name {
                Expr::Str(n) if n.eq_ignore_ascii_case("list") => Ok(Expr::List(
                    args.into_iter()
                        .map(|e| Self::list_elem(e, true).map(|o| o.map(Self::list_keyed)))
                        .collect::<Result<_, _>>()?,
                )),
                other => Ok(Expr::Call {
                    name: Box::new(other),
                    args,
                    site,
                    callee,
                }),
            },
            // The primary `list(...)` parse path reaches here as an
            // already-built List — still normalize its elements.
            Expr::List(items) => Ok(Expr::List(
                items
                    .into_iter()
                    .map(|v| {
                        v.map_or(Ok(None), |(k, e)| {
                            Self::list_elem_keyed(k, e, true).map(Some)
                        })
                    })
                    .collect::<Result<_, _>>()?,
            )),
            // Lvalue targets can't carry the arg's line marker —
            // `($x) = 1` must still resolve to a Var target.
            other => Ok(Self::unmark_argline(other)),
        }
    }

    fn list_mix_err(line: usize) -> PhpError {
        PhpError::compile_fatal("Cannot mix [] and list()", line)
    }

    /// One `list()`/`[]` destructure element: `Null` is a skipped
    /// slot; nested `[...]`/`list(...)` destructures recursively;
    /// the element's `argline` mark stays wrapped around the result
    /// (zend sites each element's own store op at the element's line).
    fn list_elem(e: Expr, list_kind: bool) -> Result<Option<Expr>, PhpError> {
        let mix_line = crate::ast::start_line(&e).unwrap_or(0);
        match e {
            Expr::Binary {
                op: "argline",
                l,
                r,
            } => Ok(Self::list_elem(*r, list_kind)?.map(|u| Expr::Binary {
                op: "argline",
                l,
                r: Box::new(u),
            })),
            Expr::Null => Ok(None),
            Expr::ArrayLit(items) => {
                if list_kind {
                    return Err(Self::list_mix_err(mix_line));
                }
                Ok(Some(Expr::List(
                    items
                        .into_iter()
                        .map(|(k, v)| {
                            Self::list_elem_kv(k, v, false).map(|o| o.map(Self::list_keyed))
                        })
                        .collect::<Result<_, _>>()?,
                )))
            }
            Expr::Call {
                name,
                args,
                site,
                callee,
            } => match *name {
                Expr::Str(n) if n.eq_ignore_ascii_case("list") => {
                    if !list_kind {
                        return Err(Self::list_mix_err(mix_line));
                    }
                    Ok(Some(Expr::List(
                        args.into_iter()
                            .map(|e| Self::list_elem(e, true).map(|o| o.map(Self::list_keyed)))
                            .collect::<Result<_, _>>()?,
                    )))
                }
                other => Ok(Some(Expr::Call {
                    name: Box::new(other),
                    args,
                    site,
                    callee,
                })),
            },
            // A nested List node only ever comes from the `list(...)`
            // primary — inside `[]` that's the other syntax.
            Expr::List(items) => {
                if !list_kind {
                    return Err(Self::list_mix_err(mix_line));
                }
                Ok(Some(Expr::List(
                    items
                        .into_iter()
                        .map(|v| {
                            v.map_or(Ok(None), |(k, e)| {
                                Self::list_elem_keyed(k, e, true).map(Some)
                            })
                        })
                        .collect::<Result<_, _>>()?,
                )))
            }
            e => Ok(Some(e)),
        }
    }

    /// `k => v` in a destructure keeps its declared key wrapped
    /// around the element as a `listkey` marker: zend's keyed
    /// array_pair reads `rhs[k]` — dropping the key reads the
    /// positional index instead (silent wrong values).
    fn list_elem_kv(k: Option<Expr>, v: Expr, list_kind: bool) -> Result<Option<Expr>, PhpError> {
        Ok(match (k, Self::list_elem(v, list_kind)?) {
            (Some(key), Some(e)) => Some(Expr::Binary {
                op: "listkey",
                l: Box::new(key),
                r: Box::new(e),
            }),
            (_, e) => e,
        })
    }

    /// A re-entering List node's elements get normalized through
    /// list_elem then re-keyed (the primary path already carries
    /// key tuples).
    fn list_elem_keyed(
        k: Option<Expr>,
        e: Expr,
        list_kind: bool,
    ) -> Result<(Option<Expr>, Expr), PhpError> {
        match Self::list_elem(e, list_kind)? {
            Some(u) => Ok(Self::list_keyed(match k {
                Some(k) => Expr::Binary {
                    op: "listkey",
                    l: Box::new(k),
                    r: Box::new(u),
                },
                None => u,
            })),
            None => Ok((k, Expr::Null)),
        }
    }

    /// `argline`/`listkey` marks around a destructure element split
    /// into the keyed-tuple form `Expr::List` stores.
    fn list_keyed(e: Expr) -> (Option<Expr>, Expr) {
        match e {
            Expr::Binary {
                op: "listkey",
                l,
                r,
                ..
            } => (Some(*l), *r),
            Expr::Binary {
                op: "argline",
                l,
                r,
                ..
            } => {
                let (k, v) = Self::list_keyed(*r);
                (
                    k,
                    Expr::Binary {
                        op: "argline",
                        l,
                        r: Box::new(v),
                    },
                )
            }
            other => (None, other),
        }
    }

    /// zend's `zend_compile_list_assign` writability walk — a
    /// COMPILE error fired at the first bad element before anything
    /// in the file runs. `cg` mirrors CG(zend_lineno): the compiled
    /// RHS leaves it at its end; each element's key then its own
    /// assign end re-sites it for the next.
    fn list_assign_check(
        items: &[Option<(Option<Expr>, Expr)>],
        rhs: &Expr,
        rl: usize,
    ) -> Option<PhpError> {
        let mut cg = Self::list_rhs_line(items, rhs).unwrap_or(rl);
        Self::list_assign_walk(items, rhs, &mut cg)
    }

    /// CG(zend_lineno) after the destructure's RHS compiled — the
    /// site of an element-0 writability failure. A CV RHS emits its
    /// QM_ASSIGN at the assign node's own line (the list's first
    /// element's); a compile-const array folds without descending
    /// (CG = the array's lineno — its first element's); anything
    /// else ends at its last compiled leaf.
    fn list_rhs_line(items: &[Option<(Option<Expr>, Expr)>], rhs: &Expr) -> Option<usize> {
        match Self::unmark_lval(rhs) {
            Expr::Var(_) | Expr::VarVar(..) => items
                .iter()
                .flatten()
                .next()
                .and_then(|(_, e)| crate::ast::start_line(e)),
            u @ Expr::ArrayLit(elems) if is_compile_const(u) => {
                elems.first().and_then(|(_, v)| crate::ast::start_line(v))
            }
            _ => crate::ast::end_line(rhs),
        }
    }

    /// A `&` element anywhere in this list level — nested lists
    /// count: zend fires the non-referenceable fatal at the
    //  containing level's first element (`[$v, [&$r]] = [[1],[2]]`
    /// sites at `$v`'s line, before the nested list compiles).
    fn list_any_byref(items: &[Option<(Option<Expr>, Expr)>]) -> bool {
        items.iter().flatten().any(|(_, t)| {
            let mut e = t;
            loop {
                e = match e {
                    Expr::Binary {
                        op: "argline", r, ..
                    } => r,
                    Expr::Paren(inner) => inner,
                    _ => break,
                };
            }
            match e {
                Expr::ByRef(_) => true,
                Expr::List(inner) => Self::list_any_byref(inner),
                _ => false,
            }
        })
    }

    /// zend_is_variable_or_call: the destructure RHS is referenceable
    /// when its access chain roots at a variable (CV, varvar, static
    /// prop) or passes through a call — `f()->p`, `$a[0]`, `C::$a`
    /// qualify; literals, `new`, casts and binary results do not.
    fn rhs_var_or_call(rhs: &Expr) -> bool {
        let mut e = rhs;
        loop {
            e = match e {
                Expr::Index { e, .. } => e,
                Expr::Prop { obj, .. } => obj,
                Expr::Binary {
                    op: "argline", r, ..
                } => r,
                Expr::Paren(inner) | Expr::ByRef(inner) => inner,
                Expr::Call { .. }
                | Expr::MethodCall { .. }
                | Expr::StaticCall { .. }
                | Expr::StaticCallDyn { .. }
                | Expr::Var(_)
                | Expr::VarVar(..)
                | Expr::StaticProp { .. } => return true,
                _ => return false,
            };
        }
    }

    fn list_assign_walk(
        items: &[Option<(Option<Expr>, Expr)>],
        rhs: &Expr,
        cg: &mut usize,
    ) -> Option<PhpError> {
        let keyed = items
            .iter()
            .flatten()
            .next()
            .is_some_and(|(k, _)| k.is_some());
        // zend fires 'Cannot assign reference to non referenceable
        // value' when the level carries a `&` element and the RHS
        // can't produce references — checked as the level's first
        // element begins compiling, so it sites at the first
        // element's line and beats sibling write-context fatals
        // (`[f(), &$r] = [1,2]` reports the ref error).
        // A nested level's RHS is `rhs[i]` — the chain root is the
        // same expr, so the check is identical at every depth.
        if Self::list_any_byref(items) && !Self::rhs_var_or_call(rhs) {
            let site = items
                .iter()
                .flatten()
                .next()
                .and_then(|(_, e)| crate::ast::start_line(e))
                .unwrap_or(*cg);
            return Some(PhpError::compile_fatal(
                "Cannot assign reference to non referenceable value",
                site,
            ));
        }
        let mut has_elems = false;
        for slot in items {
            let Some(elem) = slot else {
                if keyed {
                    return Some(PhpError::compile_fatal(
                        "Cannot use empty array entries in keyed array assignment",
                        *cg,
                    ));
                }
                continue;
            };
            has_elems = true;
            let (key, elem) = elem;
            let key = key.as_ref();
            if keyed != key.is_some() {
                return Some(PhpError::compile_fatal(
                    "Cannot mix keyed and unkeyed array entries in assignments",
                    *cg,
                ));
            }
            // zend compiles a keyed entry's key BEFORE verifying its
            // value — the value's failure sites at the key's end.
            if let Some(k) = key {
                if let Some(l) = crate::ast::end_line(k) {
                    *cg = l;
                }
            }
            let var = Self::unmark_lval(elem);
            if let Expr::List(inner) = var {
                // Nested destructure passes the verify; its elements
                // continue with the same CG (the fetch op leaves it
                // untouched).
                if let Some(err) = Self::list_assign_walk(inner, rhs, cg) {
                    return Some(err);
                }
                continue;
            }
            if let Expr::Unpack(_) = var {
                return Some(PhpError::compile_fatal(
                    "Spread operator is not supported in assignments",
                    *cg,
                ));
            }
            // Element-level write-context fatals: zend checks the
            // unmarked element's own kind — a bare call, `clone` or
            // `f(...)` element is a function-result write; a method
            // call a method-result one. Call-family exprs as a
            // dim/prop chain's ROOT stay writable instead: the call
            // result is a temp container zend writes through —
            // `[f()->p]`, `[$o->m()[0]]`, `[C::m()->p]` all compile.
            match var {
                Expr::Call { .. } | Expr::Clone(_) | Expr::Fcc(_) => {
                    return Some(PhpError::compile_fatal(
                        "Can't use function return value in write context",
                        Self::call_err_line(var, elem),
                    ));
                }
                Expr::MethodCall { .. } | Expr::StaticCall { .. } | Expr::StaticCallDyn { .. } => {
                    return Some(PhpError::compile_fatal(
                        "Can't use method return value in write context",
                        Self::call_err_line(var, elem),
                    ));
                }
                _ => {}
            }
            let base = Self::writable_base(var);
            if Self::short_circuited(base) {
                return Some(PhpError::compile_fatal(
                    "Assignments can only happen to writable values",
                    *cg,
                ));
            }
            match base {
                Expr::Var(_)
                | Expr::VarVar(..)
                | Expr::StaticProp { .. }
                | Expr::Call { .. }
                | Expr::MethodCall { .. }
                | Expr::StaticCall { .. }
                | Expr::StaticCallDyn { .. } => {
                    // Writable. A CV target's assign ends at the var's
                    // own line (zend's PAREN node keeps the inner
                    // var's lineno); delayed dim/prop targets leave
                    // CG where it stood.
                    if matches!(var, Expr::Var(_) | Expr::VarVar(..)) {
                        if let Some(l) = Self::elem_cv_line(elem) {
                            *cg = l;
                        }
                    }
                }
                // `clone`/`f(...)` chain roots get zend's distinct
                // built-in-function message, sited at the element's
                // own first-token line.
                Expr::Clone(_) | Expr::Fcc(_) => {
                    return Some(PhpError::compile_fatal(
                        "Cannot use result of built-in function in write context",
                        crate::ast::start_line(elem).unwrap_or(*cg),
                    ));
                }
                _ => {
                    return Some(PhpError::compile_fatal(
                        "Assignments can only happen to writable values",
                        *cg,
                    ));
                }
            }
        }
        if !has_elems {
            return Some(PhpError::compile_fatal("Cannot use empty list", *cg));
        }
        None
    }

    /// Transparent marks around a destructure element's target —
    /// `argline` marks, parens and by-ref wrappers don't change the
    /// lvalue kind zend's writability check sees.
    fn unmark_lval(e: &Expr) -> &Expr {
        let mut e = e;
        loop {
            e = match e {
                Expr::Binary {
                    op: "argline", r, ..
                } => r,
                Expr::Paren(inner) | Expr::ByRef(inner) => inner,
                _ => return e,
            };
        }
    }

    /// zend_can_write_to_variable's DIM/PROP unwrap: walks index and
    /// non-nullsafe prop chains (and marks) to the container that
    /// decides writability.
    fn writable_base(e: &Expr) -> &Expr {
        let mut e = e;
        loop {
            e = match e {
                Expr::Index { e, .. } => e,
                Expr::Prop {
                    obj,
                    nullsafe: false,
                    ..
                } => obj,
                Expr::Binary {
                    op: "argline", r, ..
                } => r,
                Expr::Paren(inner) | Expr::ByRef(inner) => inner,
                _ => return e,
            };
        }
    }

    /// zend_ast_is_short_circuited: a nullsafe hop anywhere in the
    /// access chain makes the whole target non-writable.
    fn short_circuited(e: &Expr) -> bool {
        match e {
            Expr::Prop { obj, nullsafe, .. } | Expr::MethodCall { obj, nullsafe, .. } => {
                *nullsafe || Self::short_circuited(obj)
            }
            Expr::Index { e, .. }
            | Expr::StaticProp { class: e, .. }
            | Expr::StaticCall { class: e, .. }
            | Expr::StaticCallDyn { class: e, .. } => Self::short_circuited(e),
            Expr::Paren(inner) | Expr::ByRef(inner) => Self::short_circuited(inner),
            Expr::Binary {
                op: "argline", r, ..
            } => Self::short_circuited(r),
            _ => false,
        }
    }

    /// A CV destructure element's zend lineno — the var's own line.
    /// `($a)` marks the inner var at its own line inside the
    /// element's outer element-mark, so peel one mark layer and take
    /// the line of what remains.
    fn elem_cv_line(elem: &Expr) -> Option<usize> {
        let inner = match elem {
            Expr::Binary {
                op: "argline", r, ..
            } => r.as_ref(),
            e => e,
        };
        crate::ast::start_line(inner).or_else(|| crate::ast::start_line(elem))
    }

    /// Line for a call-family element's write-context fatal: a bare
    /// call element sites at its own first token (the synthetic
    /// ASSIGN's lineno); one buried under dims/props sites at the
    /// element's compiled end.
    fn call_err_line(var: &Expr, elem: &Expr) -> usize {
        match var {
            Expr::Call { .. }
            | Expr::MethodCall { .. }
            | Expr::StaticCall { .. }
            | Expr::StaticCallDyn { .. }
            | Expr::Clone(_)
            | Expr::Fcc(_) => crate::ast::start_line(elem),
            _ => crate::ast::end_line(var),
        }
        .unwrap_or(0)
    }

    /// Wrap a sub-expression in an `argline` marker: diagnostics
    /// raised while evaluating it report `line` — the sub-expression's
    /// own first-token line, Zend's per-operand op attribution.
    pub(in crate::parser) fn markline(e: Expr, line: usize) -> Expr {
        Expr::Binary {
            op: "argline",
            l: Box::new(Expr::Int(line as i64)),
            r: Box::new(e),
        }
    }

    pub(in crate::parser) fn ternary(&mut self) -> Result<Expr, PhpError> {
        let cline = self.line();
        let c = self.logical_or()?;
        if self.eat_op("?") {
            if self.at_op(":") {
                self.pos += 1;
                let fl = self.line();
                let f = self.assign()?;
                return Ok(Expr::Ternary {
                    c: Box::new(c),
                    t: None,
                    f: Box::new(Self::markline(f, fl)),
                });
            }
            let tl = self.line();
            // The middle arm is zend's full `expr` — `a ? b or c : d`,
            // `a ? b and c : d`, `a ? $x = f() : e` all parse (word ops
            // bind below `=`; the expr stops at the `:` separator).
            let t = self.expr()?;
            self.expect_op(":")?;
            let fl = self.line();
            let f = self.ternary()?;
            return Ok(Expr::Ternary {
                c: Box::new(c),
                t: Some(Box::new(Self::markline(t, tl))),
                f: Box::new(Self::markline(f, fl)),
            });
        }
        if self.eat_op("??") {
            let rline = self.line();
            let r = self.assign()?;
            return Ok(Expr::Binary {
                op: "??",
                l: Box::new(Self::markline(c, cline)),
                r: Box::new(Self::markline(r, rline)),
            });
        }
        Ok(c)
    }

    pub(in crate::parser) fn logical_or(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.logical_and()?;
        while self.eat_op("||") {
            let rline = self.line();
            let r = self.logical_and()?;
            e = Expr::Binary {
                op: "||",
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
        Ok(e)
    }

    pub(in crate::parser) fn logical_and(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.equality()?;
        while self.eat_op("&&") {
            let rline = self.line();
            let r = self.equality()?;
            e = Expr::Binary {
                op: "&&",
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
        Ok(e)
    }

    pub(in crate::parser) fn equality(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
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
            let rline = self.line();
            let r = self.comparison()?;
            e = Expr::Binary {
                op,
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
    }

    pub(in crate::parser) fn comparison(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
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
            let rline = self.line();
            let r = self.concat()?;
            e = Expr::Binary {
                op,
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
    }

    /// `.` binds tighter than `+`/`-` since PHP 8.0.
    pub(in crate::parser) fn concat(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.bit_or()?;
        while self.eat_op(".") {
            let rline = self.line();
            let r = self.bit_or()?;
            // The reduce lookahead — zend_ast_create_concat_op stamps
            // a parse-time-folded concat's zval at the token following
            // the last operand.
            let tail = self.line();
            e = Expr::Binary {
                op: ".",
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
            if crate::ast::zval_lit(&e) {
                e = Self::markline(e, tail);
            }
        }
        Ok(e)
    }

    pub(in crate::parser) fn bit_or(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.bit_xor()?;
        while self.eat_op("|") {
            let rline = self.line();
            let r = self.bit_xor()?;
            e = Expr::Binary {
                op: "|",
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
        Ok(e)
    }

    pub(in crate::parser) fn bit_xor(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.bit_and()?;
        while self.eat_op("^") {
            let rline = self.line();
            let r = self.bit_and()?;
            e = Expr::Binary {
                op: "^",
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
        Ok(e)
    }

    pub(in crate::parser) fn bit_and(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.shift()?;
        while self.eat_op("&") {
            let rline = self.line();
            let r = self.shift()?;
            e = Expr::Binary {
                op: "&",
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
        Ok(e)
    }

    pub(in crate::parser) fn shift(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.additive()?;
        loop {
            let op = if self.eat_op("<<") {
                "<<"
            } else if self.eat_op(">>") {
                ">>"
            } else {
                return Ok(e);
            };
            let rline = self.line();
            let r = self.additive()?;
            e = Expr::Binary {
                op,
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
    }

    pub(in crate::parser) fn additive(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.term()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op("+")) => "+",
                Some(Token::Op("-")) => "-",
                _ => return Ok(e),
            };
            self.pos += 1;
            let rline = self.line();
            let r = self.term()?;
            e = Expr::Binary {
                op,
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
    }

    pub(in crate::parser) fn term(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.power()?;
        loop {
            let op = match self.peek() {
                Some(Token::Op("*")) => "*",
                Some(Token::Op("/")) => "/",
                Some(Token::Op("%")) => "%",
                _ => return Ok(e),
            };
            self.pos += 1;
            let rline = self.line();
            let r = self.power()?;
            e = Expr::Binary {
                op,
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
        }
    }

    /// `**` is right-associative and binds tighter than unary minus.
    pub(in crate::parser) fn power(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let e = self.unary()?;
        if self.eat_op("**") {
            let rline = self.line();
            let r = self.power()?;
            return Ok(Expr::Binary {
                op: "**",
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            });
        }
        Ok(e)
    }

    pub(in crate::parser) fn unary(&mut self) -> Result<Expr, PhpError> {
        if self.eat_op("!") {
            let el = self.line();
            let e = self.unary()?;
            return Ok(Expr::Unary {
                op: "!",
                e: Box::new(Self::markline(e, el)),
            });
        }
        if self.eat_op("-") {
            let el = self.line();
            let e = self.unary()?;
            return Ok(Expr::Unary {
                op: "-",
                e: Box::new(Self::markline(e, el)),
            });
        }
        if self.eat_op("+") {
            let el = self.line();
            let e = self.unary()?;
            return Ok(Expr::Unary {
                op: "+",
                e: Box::new(Self::markline(e, el)),
            });
        }
        if self.eat_op("~") {
            let el = self.line();
            let e = self.unary()?;
            return Ok(Expr::Unary {
                op: "~",
                e: Box::new(Self::markline(e, el)),
            });
        }
        if self.eat_op("++") {
            // `++` takes a writable variable — zend's grammar reduces
            // it through the same write-context pass as `=` targets
            // (`++f()`, `++($x)`, `++"s"[0]` all fatal at compile).
            let e = self.ref_variable(true)?;
            return Ok(Expr::PreInc(Box::new(e)));
        }
        if self.eat_op("--") {
            let e = self.ref_variable(true)?;
            return Ok(Expr::PreDec(Box::new(e)));
        }
        if self.eat_op("@") {
            // Error suppression — parsed; runtime treats as no-op for now.
            let el = self.line();
            let e = self.unary()?;
            return Ok(Expr::Unary {
                op: "@",
                e: Box::new(Self::markline(e, el)),
            });
        }
        if self.ident_is("clone") {
            self.pos += 1;
            let el = self.line();
            // PHP 8.5 clone-with: `clone($o, [...])` parses as a CALL
            // only when the paren holds a top-level comma; `clone($o)`
            // (and `clone($o)->m`) stays the unary op — zend parses
            // the parens as a plain group around the operand.
            if self.at_op("(") && self.clone_paren_has_comma() {
                self.pos += 1;
                let args = self.args()?;
                return Ok(Expr::Call {
                    name: Box::new(Expr::Str("\u{1}clone".into())),
                    args,
                    site: el,
                    callee: el,
                });
            }
            let e = self.unary()?;
            return Ok(Expr::Clone(Box::new(Self::markline(e, el))));
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
                        let el = self.line();
                        let e = self.unary()?;
                        return Ok(Expr::Cast {
                            kind,
                            e: Box::new(Self::markline(e, el)),
                        });
                    }
                }
            }
        }
        let lhs_start = self.pos;
        let tl = self.line();
        let mut e = self.postfix()?;
        // `instanceof` binds between unary and relational ops.
        while self.ident_is("instanceof") {
            self.pos += 1;
            let mut c = Self::unmark_argline(self.unary()?);
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
                let rl = self.line();
                let rhs = self.assign()?;
                let target = self.list_target(e)?;
                if let Expr::List(items) = &target {
                    // zend_compile_list_assign's writability verify is
                    // a COMPILE error — it aborts the whole file
                    // before anything runs.
                    if let Some(err) = Self::list_assign_check(items, &rhs, rl) {
                        return Err(err);
                    }
                }
                self.check_list_ref_literal(op, &target, &rhs)?;
                self.assign_target_gate(&target, op, lhs_start, op_pos)?;
                return Ok(Expr::Assign {
                    target: Box::new(target),
                    op,
                    value: Box::new(rhs),
                    line: tl,
                });
            }
        }
        Ok(e)
    }

    /// Write-context gate for `++`/`--` operands (postfix form; the
    /// prefix form goes through `ref_variable`). Whole-paren operand
    /// is a parse error; `++$x++` — the operand is already a composite
    /// `expr`, so the trailing operator can't reduce — is zend's yacc
    /// `unexpected token "++"`; nullsafe chains and non-writable chain
    /// roots get the deferred compile fatals; call/method results get
    /// `Can't use ... return value in write context`.
    fn incdec_operand(&mut self, e: Expr, op: &str, start: usize) -> Result<Expr, PhpError> {
        use crate::ast::Expr::*;
        let e = Self::unmark_argline(e);
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
            self.write_ctx_errs.push((
                "Can't use nullsafe operator in write context".to_string(),
                self.line(),
            ));
        }
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
            self.write_ctx_errs.push((
                "Can't use method return value in write context".to_string(),
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
                | VarVar(..)
                | Call { .. }
                | MethodCall { .. }
                | StaticCall { .. }
                | StaticCallDyn { .. }
                | Fcc(_)
                | Paren(_)
                | StaticProp { .. } => {}
                _ => {
                    self.write_ctx_errs.push((
                        "Cannot use temporary expression in write context".to_string(),
                        self.line(),
                    ));
                }
            }
        } else {
            match &e {
                Var(_) | VarVar(..) | StaticProp { .. } => {}
                Call { .. } | Fcc(_) => self.write_ctx_errs.push((
                    "Can't use function return value in write context".to_string(),
                    self.line(),
                )),
                MethodCall { .. } | StaticCall { .. } | StaticCallDyn { .. } => {
                    self.write_ctx_errs.push((
                        "Can't use method return value in write context".to_string(),
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

    pub(in crate::parser) fn postfix(&mut self) -> Result<Expr, PhpError> {
        // The expression's first token — zend's callee-node line for a
        // `(...)` dispatch built in this loop (INIT_DYNAMIC_CALL).
        let start = self.pos;
        let callee_line = self.line();
        let mut e = self.primary()?;
        loop {
            if self.eat_op("++") {
                e = Expr::PostInc(Box::new(self.incdec_operand(e, "++", start)?));
            } else if self.eat_op("--") {
                e = Expr::PostDec(Box::new(self.incdec_operand(e, "--", start)?));
            } else if self.eat_op("[") {
                let il = self.line();
                let i = if self.at_op("]") {
                    None
                } else {
                    Some(Box::new(Self::markline(self.expr()?, il)))
                };
                self.expect_op("]")?;
                e = Expr::Index { e: Box::new(e), i };
            } else if self.at_op("(") {
                // `callable_expr(...)` — Zend sites the frame at the
                // `(` token's line (the DO_FCALL op's lineno).
                let paren_line = self.line();
                self.pos += 1;
                let mut args = self.args()?;
                if !Self::literal_dyn_callee(&e) {
                    Self::dyn_arglines(&mut args);
                } else {
                    // `('max')()` / `"max"()` — a literal-name call via
                    // a dyn callee still gets the frameless fold.
                    let n = match Self::unmark_argline_r(&e) {
                        Expr::Str(s) => Some(s.clone()),
                        _ => Self::interp_lit(&e),
                    };
                    if let Some(n) = n {
                        if Self::frameless_call(&n, &args) {
                            Self::frameless_arglines(&mut args, callee_line);
                        } else if Self::dedicated_call(&n, &args) {
                            Self::dedicated_arglines(&mut args, self.arg_end);
                        }
                    }
                }
                // A folded-varvar callee's CV read sites at its inner
                // name's first-token line (like any other position).
                let callee = match Self::unmark_argline_r(&e) {
                    Expr::VarVar(inner, _)
                        if is_compile_const(inner)
                            && matches!(inner.as_ref(), Expr::Binary { op: "argline", .. }) =>
                    {
                        Self::argline_of(inner).unwrap_or(callee_line)
                    }
                    _ => callee_line,
                };
                e = Self::fcc_wrap(Expr::Call {
                    name: Box::new(e),
                    args,
                    site: paren_line,
                    callee,
                })?;
            } else if self.at_op("->") || self.at_op("?->") {
                let nullsafe = self.at_op("?->");
                self.pos += 1;
                // Zend sites the frame at the member-name token's line
                // (zend_ast_get_lineno(method_ast)).
                let site = self.line();
                let name = self.prop_name()?;
                if self.at_op("(") {
                    self.pos += 1;
                    let mut args = self.args()?;
                    Self::dyn_arglines(&mut args);
                    e = Self::fcc_wrap(Expr::MethodCall {
                        obj: Box::new(e),
                        name,
                        args,
                        nullsafe,
                        site,
                    })?;
                } else {
                    e = Expr::Prop {
                        obj: Box::new(e),
                        name,
                        nullsafe,
                        site,
                    };
                }
            } else if self.eat_op("::") {
                // `parent::` inside a non-trait class with no parent is
                // zend's compile-time fatal — the whole file fails
                // before executing (p15/t/ch vs oracle). Traits defer
                // the check to the using class, and closures defer it
                // to invocation (catchable Error).
                if let Expr::Const(n) = Self::unmark_argline_r(&e) {
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
                // Member-name token line — the trace site for `C::m(...)`.
                let site = self.line();
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
                                if let Expr::Const(cn) = Self::unmark_argline_r(&e) {
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
                                                return Err(PhpError::compile_fatal(
                                                "Cannot use \"parent\" when no class scope is active",
                                                self.line(),
                                            ));
                                            }
                                            match &self.hook_ctx {
                                            None => {
                                                return Err(PhpError::compile_fatal(
                                                    format!(
                                                        "Must not use parent::${}::{}() outside a property hook",
                                                        pn, n
                                                    ),
                                                    self.line(),
                                                ))
                                            }
                                            Some((hp, hg)) => {
                                                if hp != pn {
                                                    return Err(PhpError::compile_fatal(
                                                        format!(
                                                            "Must not use parent::${}::{}() in a different property (${})",
                                                            pn, n, hp
                                                        ),
                                                        self.line(),
                                                    ));
                                                }
                                                if *hg != n.eq_ignore_ascii_case("get") {
                                                    return Err(PhpError::compile_fatal(
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
                            let mut args = self.args()?;
                            if !Self::literal_static_class(&e) {
                                Self::dyn_arglines(&mut args);
                            }
                            e = Self::fcc_wrap(Expr::StaticCall {
                                class: Box::new(e),
                                name: n,
                                args,
                                site,
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
                            let mut args = self.args()?;
                            Self::dyn_arglines(&mut args);
                            e = Self::fcc_wrap(Expr::StaticCallDyn {
                                class: Box::new(e),
                                name: Box::new(Expr::Var(n)),
                                args,
                                site,
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
                        // `Cls::{expr}(...)` — member name = the inner
                        // expr's first-token line.
                        let site = self.line();
                        let inner = self.expr()?;
                        self.expect_op("}")?;
                        if self.at_op("(") {
                            self.pos += 1;
                            // `Cls::{expr}(...)` keeps per-arg send
                            // lines — zend treats the `::{` member
                            // call like a named static call.
                            let args = self.args()?;
                            e = Expr::MethodCall {
                                obj: Box::new(e),
                                name: PropName::Expr(Box::new(inner)),
                                args,
                                nullsafe: false,
                                site,
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
                        let (inner, isite) = if self.at_op("{") {
                            self.pos += 1;
                            let il = self.line();
                            let inner = self.expr()?;
                            self.expect_op("}")?;
                            (inner, il)
                        } else {
                            // `C::$$x` — name read from variable $x.
                            let il = self.line();
                            match self.next() {
                                Some(Token::Variable(n)) => (Expr::Var(n), il),
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
                            let mut args = self.args()?;
                            Self::dyn_arglines(&mut args);
                            e = Expr::MethodCall {
                                obj: Box::new(e),
                                name: PropName::Expr(Box::new(inner)),
                                args,
                                nullsafe: false,
                                site: isite,
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
                // `=` (`($c ? $a : $b) =& $x`) — the gate normalizes
                // '=&' to '=' for the message.
                self.assign_target_gate(&target, "=&", start, eq_pos)?;
                e = Expr::Assign {
                    target: Box::new(target),
                    op: "=&",
                    value: Box::new(rhs),
                    line: callee_line,
                };
            } else {
                return Ok(e);
            }
        }
    }

    /// `expr(...)` — first-class-callable arg lists rewrite their call
    /// node into `Expr::Fcc`; everything else keeps its args.
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
                Index { e: c, .. } | Prop { obj: c, .. } => c.as_ref(),
                Paren(inner) => inner.as_ref(),
                // A call-shaped link is itself a legal root — a dim
                // write binds to its temporary result (`f()['k'] = v`,
                // `(new R)->getValue()[] = v`), so stop here instead
                // of exposing the callee's root (which can be `new`).
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
                Var(_) | VarVar(..) | StaticProp { .. } => {}
                Call { .. }
                | MethodCall { .. }
                | StaticCall { .. }
                | StaticCallDyn { .. }
                | Fcc(_) => {
                    if write_ctx {
                        self.write_ctx_errs.push((
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
                | VarVar(..)
                | Call { .. }
                | MethodCall { .. }
                | StaticCall { .. }
                | StaticCallDyn { .. }
                | Fcc(_)
                | Paren(_)
                | StaticProp { .. } => {}
                _ => {
                    self.write_ctx_errs.push((
                        "Cannot use temporary expression in write context".to_string(),
                        self.line(),
                    ));
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
                    self.write_ctx_errs.push((
                        format!("Can't use {} return value in write context", kind),
                        self.line(),
                    ));
                }
            }
        }
        if Self::has_nullsafe(&e) {
            if write_ctx {
                self.write_ctx_errs.push((
                    "Can't use nullsafe operator in write context".to_string(),
                    self.line(),
                ));
            } else {
                self.write_ctx_errs.push((
                    "Cannot take reference of a nullsafe chain".to_string(),
                    self.line(),
                ));
            }
        }
        Ok(e)
    }

    pub(in crate::parser) fn list_writable(&mut self, e: &Expr) -> Result<(), PhpError> {
        use crate::ast::Expr::*;
        // Elements keep `argline`/`Paren`/`ByRef` marks — zend's
        // writability check sees through them.
        let e = Self::unmark_lval(e);
        match e {
            Var(_) | VarVar(..) | StaticProp { .. } => Ok(()),
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
            Call { .. } | Fcc(_) => {
                self.write_ctx_errs.push((
                    "Can't use function return value in write context".to_string(),
                    self.line(),
                ));
                Ok(())
            }
            MethodCall { .. } | StaticCall { .. } | StaticCallDyn { .. } => {
                self.write_ctx_errs.push((
                    "Can't use method return value in write context".to_string(),
                    self.line(),
                ));
                Ok(())
            }
            Index { .. } | Prop { .. } => {
                if Self::has_nullsafe(e) || !Self::writeable_root(e) {
                    self.write_ctx_errs.push((
                        "Assignments can only happen to writable values".to_string(),
                        self.line(),
                    ));
                }
                Ok(())
            }
            _ => {
                self.write_ctx_errs.push((
                    "Assignments can only happen to writable values".to_string(),
                    self.line(),
                ));
                Ok(())
            }
        }
    }

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
                // argline/markline wraps nest INSIDE the chain nodes —
                // `(new R)->m()[]` roots at Paren{argline: New}.
                Binary { op, r, .. } if matches!(*op, "argline" | "markline" | "listkey") => {
                    r.as_ref()
                }
                _ => break,
            };
        }
        if call_link && matches!(leaf, New { .. }) {
            return true;
        }
        matches!(
            leaf,
            Var(_)
                | VarVar(..)
                | Call { .. }
                | MethodCall { .. }
                | StaticCall { .. }
                | StaticCallDyn { .. }
                | Fcc(_)
                | Paren(_)
                | StaticProp { .. }
        )
    }

    pub(in crate::parser) fn has_nullsafe(e: &Expr) -> bool {
        match e {
            Expr::MethodCall { obj, nullsafe, .. } => *nullsafe || Self::has_nullsafe(obj),
            Expr::Prop { obj, nullsafe, .. } => *nullsafe || Self::has_nullsafe(obj),
            // `$o?->p['k']` — the nullsafe sits under the dim.
            Expr::Index { e, .. } => Self::has_nullsafe(e),
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
            return Err(PhpError::compile_fatal(
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
            return Err(PhpError::compile_fatal(
                "Cannot create Closure for new expression",
                self.line(),
            ));
        }
        Ok(())
    }

    /// Parses `(arg, ...)`: each returned arg is wrapped in an
    /// `argline` marker carrying that arg's own first-token line —
    /// diagnostics raised while evaluating an argument attribute to
    /// the arg's line, like zend's per-op line info (a warning inside
    /// a multi-line call's argument reports the arg's line, not the
    /// call's).
    pub(in crate::parser) fn args(&mut self) -> Result<Vec<Expr>, PhpError> {
        let mut args: Vec<(Expr, usize)> = Vec::new();
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
                let vline = self.line();
                args.push((Expr::Unpack(Box::new(self.expr()?)), vline));
                unpacked = true;
                if !self.eat_op(",") {
                    break;
                }
                continue;
            }
            let named = matches!(self.peek(), Some(Token::Ident(_)))
                && matches!(self.peek2(), Some(Token::Op(":")));
            let vline = self.arg_line();
            if named {
                // named arguments `name:` — name recorded via Str marker
                let n = self.ident().unwrap();
                self.pos += 1; // :
                let vline = self.arg_line();
                let v = self.expr()?;
                args.push((
                    Expr::Binary {
                        op: "named",
                        l: Box::new(Expr::Str(n)),
                        r: Box::new(v),
                    },
                    vline,
                ));
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
                args.push((self.expr()?, vline));
            }
            if !self.eat_op(",") {
                break;
            }
        }
        // zend_lineno after the last arg compiled — the dedicated-op
        // fold sites every CV arg there (the emitted op's lineno).
        self.arg_end = self.toks[self.pos.saturating_sub(1)].line;
        self.expect_op(")")?;
        // Every arg carries its own line — the interpreter sites
        // arg-eval diagnostics there (zend's per-op lines).
        Ok(args
            .into_iter()
            .map(|(e, l)| Expr::Binary {
                op: "argline",
                l: Box::new(Expr::Int(l as i64)),
                r: Box::new(e),
            })
            .collect())
    }

    /// Inverse of the `argline` wrapper — for consumers whose arg
    /// lists are not evaluated by the call machinery (attribute args,
    /// `list()` destructuring targets, lvalue targets). Recursive:
    /// nested parens stack markers.
    pub(in crate::parser) fn unmark_argline(e: Expr) -> Expr {
        match e {
            Expr::Binary {
                op: "argline", r, ..
            } => Self::unmark_argline(*r),
            e => e,
        }
    }

    /// By-ref variant of `unmark_argline`.
    pub(in crate::parser) fn unmark_argline_r(e: &Expr) -> &Expr {
        match e {
            Expr::Binary {
                op: "argline", r, ..
            } => Self::unmark_argline_r(r),
            e => e,
        }
    }

    /// The `argline` marker's line on an arg, if any.
    fn argline_of(e: &Expr) -> Option<usize> {
        match e {
            Expr::Binary {
                op: "argline", l, ..
            } => match l.as_ref() {
                Expr::Int(n) => Some(*n as usize),
                _ => None,
            },
            _ => None,
        }
    }

    /// A call arg whose send op is a bare CV — `f($x)`, `f(n: $x)`, or
    /// a folded varvar `f(${const})` (zend compiles the folded name to
    /// a plain CV read). Any other arg shape emits ops on its own
    /// lines first.
    fn bare_var_arg(e: &Expr) -> bool {
        fn cvish(e: &Expr) -> bool {
            match e {
                Expr::Var(_) => true,
                Expr::VarVar(inner, _) => is_compile_const(inner),
                _ => false,
            }
        }
        match Self::unmark_argline_r(e) {
            e if cvish(e) => true,
            Expr::Binary { op: "named", r, .. } => cvish(Self::unmark_argline_r(r)),
            _ => false,
        }
    }

    /// A folded varvar's CV-equivalent line lives in its inner's
    /// `argline` marker — retarget that too when the enclosing op
    /// binds the name read (dyn-callee first-arg fold, frameless
    /// name-line fold).
    fn retarget_varvar_line(e: &mut Expr, line: usize) {
        match e {
            Expr::Binary { op: "named", r, .. } => Self::retarget_varvar_line(r, line),
            Expr::VarVar(inner, _)
                if matches!(inner.as_ref(), Expr::Binary { op: "argline", .. }) =>
            {
                if let Expr::Binary { l, .. } = inner.as_mut() {
                    *l.as_mut() = Expr::Int(line as i64);
                }
            }
            _ => {}
        }
    }

    /// Rewrite a bare-CV arg's marker (and a folded varvar's inner
    /// marker) to `line`.
    fn fold_var_arg(a: &mut Expr, line: usize) {
        let mut un = Self::unmark_argline(std::mem::replace(a, Expr::Null));
        Self::retarget_varvar_line(&mut un, line);
        *a = Self::markline(un, line);
    }

    /// Non-literal callee (`$f()`, `$o->m()`, `new`, `parent::m()`,
    /// `static::m()`, `self::m()`, `$cls::m()`, `C::$m()`, `Cls::${e}()`,
    /// `expr()`, string callables containing `::`): zend sites each
    /// bare-CV arg's send at the FIRST arg's line — rewrite those
    /// argline markers. Args with their own ops (calls, binaries,
    /// index, unpack, var-var, …) keep their own lines.
    pub(in crate::parser) fn dyn_arglines(args: &mut [Expr]) {
        let Some(first) = args.first().and_then(Self::argline_of) else {
            return;
        };
        for a in args.iter_mut().skip(1) {
            if Self::bare_var_arg(a) {
                Self::fold_var_arg(a, first);
            }
        }
    }

    /// zend's frameless icall builtins (the `@frameless-function`
    /// stubs, arity-gated): the call compiles to a single FRAMELESS
    /// op at the callee's lineno — every bare-CV arg's read fuses
    /// into it at the function NAME's line. (name, min_args, max_args)
    const FRAMELESS: &'static [(&'static str, usize, usize)] = &[
        ("min", 2, 2),
        ("max", 2, 2),
        ("in_array", 2, 3),
        ("trim", 1, 2),
        ("implode", 1, 2),
        ("dirname", 1, 2),
        ("strstr", 2, 3),
        ("strpos", 2, 3),
        ("str_contains", 2, 2),
        ("str_starts_with", 2, 2),
        ("substr", 2, 3),
        ("strtr", 2, 3),
        ("str_replace", 3, 3),
        ("dechex", 1, 1),
        ("is_numeric", 1, 1),
        ("property_exists", 2, 2),
        ("class_exists", 1, 2),
        ("preg_match", 2, 2),
        ("preg_replace", 3, 3),
    ];

    /// Whether `name(args)` compiles to a frameless op: the function is
    /// a known frameless builtin, arg count fits a handler's arity, and
    /// the args are positional (zend_args_contain_unpack_or_named
    /// disqualifies named/unpack).
    fn frameless_call(name: &str, args: &[Expr]) -> bool {
        let bare = name
            .strip_prefix('\u{1}')
            .or_else(|| name.strip_prefix('\\'))
            .unwrap_or(name);
        if bare.contains('\\') {
            return false;
        }
        let bare = bare.to_lowercase();
        let Some((_, lo, hi)) = Self::FRAMELESS.iter().find(|(n, ..)| *n == bare) else {
            return false;
        };
        if !(*lo..=*hi).contains(&args.len()) {
            return false;
        }
        args.iter().all(|a| {
            !matches!(
                Self::unmark_argline_r(a),
                Expr::Unpack(_) | Expr::Binary { op: "named", .. }
            )
        })
    }

    /// Frameless dispatch: every bare-CV arg's send fuses to the
    /// callee's own line.
    fn frameless_arglines(args: &mut [Expr], line: usize) {
        for a in args.iter_mut() {
            if Self::bare_var_arg(a) {
                Self::fold_var_arg(a, line);
            }
        }
    }

    /// zend_try_compile_special_func's multi-operand dedicated ops —
    /// `array_key_exists` and the const-format `sprintf` fast path:
    /// zend compiles every arg first, then emits the single op that
    /// binds them all, so each bare-CV arg's read fuses into the op
    /// at CG(zend_lineno) — the last arg's compiled end line.
    /// (The other special ops — strlen, is_*, count, ord/chr, the
    /// typecast builtins, defined, gettype, func_get_args — take one
    /// arg or bind none of them; in_array's 3-arg shape is in the
    /// FRAMELESS table already.)
    fn dedicated_call(name: &str, args: &[Expr]) -> bool {
        let bare = name
            .strip_prefix('\u{1}')
            .or_else(|| name.strip_prefix('\\'))
            .unwrap_or(name);
        if bare.contains('\\') {
            return false;
        }
        // zend_args_contain_unpack_or_named disqualifies named/unpack.
        if !args.iter().all(|a| {
            !matches!(
                Self::unmark_argline_r(a),
                Expr::Unpack(_) | Expr::Binary { op: "named", .. }
            )
        }) {
            return false;
        }
        match bare.to_lowercase().as_str() {
            // zend_compile_func_array_key_exists: exactly two args.
            "array_key_exists" => args.len() == 2,
            "sprintf" => Self::sprintf_dedicated(args),
            _ => false,
        }
    }

    /// zend_compile_func_sprintf's dedicated path only fires for a
    /// constant format under 256 bytes whose placeholders are all
    /// %s/%d/%% and number exactly the args after it — otherwise the
    /// call is an ordinary per-arg send.
    fn sprintf_dedicated(args: &[Expr]) -> bool {
        let Some(fmt) = args
            .first()
            .and_then(|a| Self::const_str(Self::unmark_argline_r(a)))
        else {
            return false;
        };
        if fmt.len() >= 256 {
            return false;
        }
        let mut n = 0usize;
        let mut i = 0;
        while i < fmt.len() {
            if fmt[i] == b'%' {
                i += 1;
                match fmt.get(i) {
                    Some(b's') | Some(b'd') => n += 1,
                    Some(b'%') => {}
                    _ => return false,
                }
            }
            i += 1;
        }
        n + 1 == args.len()
    }

    /// A compile-constant string node — `zend_eval_const_expr`
    /// reduces a literal-only concat to a zval too.
    fn const_str(e: &Expr) -> Option<Vec<u8>> {
        match e {
            Expr::Str(s) => Some(s.as_bytes().to_vec()),
            Expr::Interp(parts) => {
                let mut out = Vec::new();
                for p in parts {
                    match p {
                        StringPart::Lit(s) => out.extend_from_slice(s),
                        _ => return None,
                    }
                }
                Some(out)
            }
            Expr::Binary { op: ".", l, r } => {
                let mut out = Self::const_str(Self::unmark_argline_r(l))?;
                out.extend_from_slice(&Self::const_str(Self::unmark_argline_r(r))?);
                Some(out)
            }
            _ => None,
        }
    }

    /// The dedicated-op fold: every bare-CV arg's send fuses into the
    /// single op at the last arg's compiled end line.
    fn dedicated_arglines(args: &mut [Expr], line: usize) {
        for a in args.iter_mut() {
            if Self::bare_var_arg(a) {
                Self::fold_var_arg(a, line);
            }
        }
    }

    /// An arg's marker line — the arg's first token, except a
    /// `<<<`/nowdoc token counts from the line UNDER its opener:
    /// zend's arg lineno is the heredoc body's first line (an empty
    /// body still takes the closer's line — also opener+1).
    fn arg_line(&self) -> usize {
        match self.toks.get(self.pos) {
            Some(t)
                if matches!(t.token, Token::InterpString(_) | Token::SimpleString(_))
                    && t.start != usize::MAX
                    && {
                        let s = self.src.as_bytes();
                        s[t.start..].starts_with(b"<<<") || s[t.start..].starts_with(b"b<<<")
                    } =>
            {
                t.line + 1
            }
            _ => self.line(),
        }
    }

    /// Literal text of a non-interpolating `"..."`/heredoc source
    /// string — `None` when any part interpolates.
    fn interp_lit(e: &Expr) -> Option<String> {
        match Self::unmark_argline_r(e) {
            Expr::Interp(parts) => {
                let mut s = Vec::new();
                for p in parts {
                    match p {
                        crate::lexer::StringPart::Lit(t) => s.extend_from_slice(t),
                        _ => return None,
                    }
                }
                Some(String::from_utf8_lossy(&s).into_owned())
            }
            _ => None,
        }
    }

    /// `expr(...)` callee literalness for arg send lines: only a
    /// source string literal naming a plain function (`'g'()`,
    /// `('g')()`, `"g"()`) resolves like `g()` — a `Cls::m` string
    /// callable goes through dynamic resolution like any other
    /// non-literal callee.
    fn literal_dyn_callee(e: &Expr) -> bool {
        match Self::unmark_argline_r(e) {
            Expr::Str(s) => !s.contains("::"),
            Expr::Interp(_) => Self::interp_lit(e).is_some_and(|t| !t.contains("::")),
            _ => false,
        }
    }

    /// `X::m(...)` class literalness for arg send lines: literal class
    /// names (`O::sm`, `\O::sm`, `A\B::m`, `('O')::sm`) keep per-arg
    /// send lines; `self`/`parent`/`static` and dynamic class exprs
    /// (`$cls::m`) take the first-arg line.
    fn literal_static_class(e: &Expr) -> bool {
        match Self::unmark_argline_r(e) {
            Expr::Const(n) => {
                !n.eq_ignore_ascii_case("self")
                    && !n.eq_ignore_ascii_case("parent")
                    && !n.eq_ignore_ascii_case("static")
            }
            Expr::Str(_) => true,
            Expr::Interp(_) => Self::interp_lit(e).is_some(),
            _ => false,
        }
    }

    pub(in crate::parser) fn prop_name(&mut self) -> Result<PropName, PhpError> {
        match self.next() {
            Some(Token::Ident(n)) => Ok(PropName::Name(n)),
            Some(Token::Variable(n)) => Ok(PropName::Var(n)),
            Some(Token::Op("{")) => {
                let el = self.line();
                let e = self.expr()?;
                self.expect_op("}")?;
                Ok(PropName::Expr(Box::new(Self::markline(e, el))))
            }
            // `$obj->${expr}` / `$obj->$$var` — variable-variable: the prop
            // name is the VALUE of the variable named by the expr
            // (engine_assignExecutionOrder_001).
            Some(Token::Op("$")) => {
                if self.at_op("{") {
                    self.pos += 1;
                    let el = self.line();
                    let e = self.expr()?;
                    self.expect_op("}")?;
                    let end = self.prev_line();
                    Ok(PropName::Expr(Box::new(Expr::VarVar(
                        Box::new(Self::markline(e, el)),
                        end,
                    ))))
                } else {
                    // The offending token's own line, not the EOF
                    // sentinel line (eof_line applies to end-of-input
                    // errors only).
                    let l = self.line();
                    match self.next() {
                        Some(Token::Variable(n)) => {
                            let end = self.prev_line();
                            Ok(PropName::Expr(Box::new(Expr::VarVar(
                                Box::new(Expr::Var(n)),
                                end,
                            ))))
                        }
                        t => Err(PhpError::parse(
                            format!(
                                "syntax error, unexpected {}, expecting variable or \"{{\" or \"$\"",
                                desc_t(t.as_ref())
                            ),
                            l,
                        )),
                    }
                }
            }
            t => Err(PhpError::parse(
                format!(
                    "syntax error, unexpected {}, expecting identifier or variable or \"{{\" or \"$\"",
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
                let el = self.line();
                let e = self.expr()?;
                self.expect_group(")")?;
                // Mark parenthesized class-prop refs so `(X::$p)::m()`
                // is not confused with the `X::$p::m()` hook syntax.
                Ok(if matches!(e, Expr::StaticProp { .. }) {
                    Expr::Paren(Box::new(Self::markline(e, el)))
                } else {
                    Self::markline(e, el)
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
                        let il = self.line();
                        self.pos += 1;
                        let end = self.prev_line();
                        Ok(Expr::VarVar(
                            Box::new(Self::markline(Expr::Var(n), il)),
                            end,
                        ))
                    }
                    Some(Token::Op("{")) => {
                        self.pos += 1;
                        let il = self.line();
                        let e = self.expr()?;
                        self.expect_op("}")?;
                        let end = self.prev_line();
                        Ok(Expr::VarVar(Box::new(Self::markline(e, il)), end))
                    }
                    Some(Token::Op("$")) => {
                        // `$$$a` — primary() consumes the nested `$`.
                        let il = self.line();
                        let e = self.primary()?;
                        let end = self.prev_line();
                        Ok(Expr::VarVar(Box::new(Self::markline(e, il)), end))
                    }
                    t => Err(PhpError::parse(
                        format!("syntax error, unexpected {}", desc_t(t.as_ref())),
                        self.line(),
                    )),
                }
            }
            Some(Token::Ident(_)) => {
                // Statement keywords never appear in expression
                // position — Zend lexes them as distinct tokens the
                // expr grammar rejects outright (`$x ??= break`,
                // `fn() => break`, even `break()`/`break::X`).
                if self.ident_is("break") || self.ident_is("continue") || self.ident_is("goto") {
                    return Err(PhpError::parse(
                        format!(
                            "syntax error, unexpected token \"{}\"",
                            self.ident().unwrap()
                        ),
                        self.line(),
                    ));
                }
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
                    // zend_compile_isset: only variable-ish targets
                    // (var, dim, prop — nullsafe included) are legal;
                    // every other shape is a compile fatal.
                    for a in &args {
                        let mut e = a;
                        while let Expr::Paren(inner) = e {
                            e = inner;
                        }
                        if !matches!(
                            e,
                            Expr::Var(_)
                                | Expr::VarVar(..)
                                | Expr::Index { .. }
                                | Expr::Prop { .. }
                                | Expr::StaticProp { .. }
                        ) {
                            return Err(PhpError::compile_fatal(
                                "Cannot use isset() on the result of an expression (you can use \"null !== expression\" instead)",
                                self.prev_line(),
                            ));
                        }
                    }
                    Ok(Expr::Isset(args))
                } else if self.ident_is("empty") {
                    self.pos += 1;
                    self.expect_op("(")?;
                    let e = self.expr()?;
                    self.expect_group(")")?;
                    Ok(Expr::Empty(Box::new(e)))
                } else if self.ident_is("yield") {
                    self.pos += 1;
                    // `yield from <it>` splices another iterable's items.
                    if self.ident_is("from") {
                        self.pos += 1;
                        let el = self.line();
                        let e = self.assign()?;
                        return Ok(Expr::YieldFrom(Box::new(Self::markline(e, el))));
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
                    let kl = self.line();
                    let first = self.assign()?;
                    // `yield k => v`
                    if self.at_op("=>") {
                        self.pos += 1;
                        let vl = self.line();
                        let v = self.assign()?;
                        return Ok(Expr::Yield {
                            key: Some(Box::new(Self::markline(first, kl))),
                            val: Some(Box::new(Self::markline(v, vl))),
                        });
                    }
                    Ok(Expr::Yield {
                        key: None,
                        val: Some(Box::new(Self::markline(first, kl))),
                    })
                } else if self.ident_is("print") {
                    self.pos += 1;
                    let el = self.line();
                    // print's operand binds above `=` but below zend's
                    // `and`/`xor`/`or`: `print $x = 5` prints 5, while
                    // `print $a or die` is `(print $a) or die`.
                    let e = self.assign()?;
                    Ok(Expr::Print(Box::new(Self::markline(e, el))))
                } else if self.ident_is("exit") || self.ident_is("die") {
                    self.pos += 1;
                    let arg = if self.eat_op("(") {
                        let a = if self.at_op(")") {
                            None
                        } else {
                            let al = self.line();
                            Some(Box::new(Self::markline(self.expr()?, al)))
                        };
                        self.expect_op(")")?;
                        a
                    } else if matches!(
                        self.peek(),
                        Some(Token::Op(";")) | Some(Token::Op(")")) | None
                    ) {
                        None
                    } else {
                        let al = self.line();
                        Some(Box::new(Self::markline(self.expr()?, al)))
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
                    // Trace site: the class expression's first-token
                    // line (`new\nC(...)` sites at C, `new class` at
                    // `class`).
                    let site = self.line();
                    let (class, mut ctor_args) = self.new_class_expr()?;
                    if self.at_op("(") {
                        self.pos += 1;
                        ctor_args = self.args()?;
                        Self::dyn_arglines(&mut ctor_args);
                        self.check_no_fcc_ctor(&ctor_args)?;
                    }
                    Ok(Expr::New {
                        class: Box::new(class),
                        args: ctor_args,
                        site,
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
                        let el = self.line();
                        if self.at_op("&") {
                            // `list(&$r)` — zend's array_pair accepts
                            // `&` elements exactly like `[&$r]`.
                            let e = self.array_elem()?;
                            items.push(Some(Self::list_keyed(Self::markline(e, el))));
                        } else {
                            let e = self.expr()?;
                            if self.eat_op("=>") {
                                // Keyed destructure `list('k' => $v)`
                                // / `list('k' => &$v)` — the key is a
                                // plain expr; the target is a writable
                                // variable like any element.
                                let tl = self.line();
                                let t = if self.at_op("&") {
                                    self.array_elem()?
                                } else {
                                    Self::markline(self.expr()?, tl)
                                };
                                items.push(Some(Self::list_keyed(Expr::Binary {
                                    op: "listkey",
                                    l: Box::new(Self::markline(e, el)),
                                    r: Box::new(t),
                                })));
                            } else {
                                items.push(Some(Self::list_keyed(Self::markline(e, el))));
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
                        let el = self.line();
                        let e = self.expr()?;
                        self.expect_group(")")?;
                        Self::markline(e, el)
                    } else {
                        let el = self.line();
                        Self::markline(self.expr()?, el)
                    };
                    Ok(Expr::Include {
                        kind,
                        e: Box::new(e),
                    })
                } else if self.ident_is("__line__") {
                    let line = self.line();
                    self.pos += 1;
                    // `foldlit`: the VALUE is fixed at parse, but zend
                    // keeps it an AST const (folded by the const scan,
                    // not a literal zval) — matters for match/switch
                    // cond siting and literal-only checks (break N,
                    // declare strict_types).
                    Ok(Expr::Binary {
                        op: "foldlit",
                        l: Box::new(Expr::Int(line as i64)),
                        r: Box::new(Expr::Null),
                    })
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
                    // even inside a namespaced caller (ns_069). Kept a
                    // `foldlit` const like __LINE__ for cond scanning.
                    Ok(Expr::Binary {
                        op: "foldlit",
                        l: Box::new(Expr::Str(self.cur_ns.clone())),
                        r: Box::new(Expr::Null),
                    })
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
                    // Zend sites `name(...)` frames at the name's line.
                    let site = self.line();
                    let name = self.name_path().unwrap_or_default();
                    if self.at_op("(") {
                        self.pos += 1;
                        let mut args = self.args()?;
                        let resolved = self.ns_resolve(&name, NsKind::Func);
                        // Unqualified literal names carry a \u{1} marker:
                        // call_named then applies the ns\f -> f fallback.
                        // Qualified/FQ-resolved names are exact already.
                        let resolved = if resolved.contains('\\') {
                            resolved
                        } else {
                            format!("{}{}", '\u{1}', resolved)
                        };
                        // An unqualified name inside a namespace compiles
                        // to a delayed ns-fallback op — zend can't bind
                        // the callee at compile time, so the dedicated
                        // ops never fire and bare-CV args fuse at the
                        // arglist's FIRST-arg line like a dynamic call.
                        // Frameless icalls still specialize (they carry
                        // the ns delay inside the op) at the name line.
                        let delayed_ns = resolved.starts_with('\u{1}') && !self.cur_ns.is_empty();
                        // zend's per-arg sends only emit for a callee
                        // bound at compile time — an internal function
                        // or a userland function already declared
                        // unconditionally. Every unbound call shape
                        // (forward refs, runtime decls, other-file
                        // functions, delayed ns-fallbacks) fuses its
                        // bare-CV sends at the FIRST arg's line, like
                        // a dynamic call.
                        let callee_bound = {
                            let bare = resolved
                                .strip_prefix('\u{1}')
                                .or_else(|| resolved.strip_prefix('\\'))
                                .unwrap_or(&resolved);
                            let bare_l = bare.to_lowercase();
                            (!bare_l.contains('\\') && crate::builtins::is_builtin(&bare_l))
                                || self.declared_funcs.contains(&bare_l)
                        };
                        // Frameless builtins fuse every bare-CV arg's
                        // read into the call op at the name's line;
                        // dedicated ops (array_key_exists, const-fmt
                        // sprintf) fuse them at the last arg's end.
                        if Self::frameless_call(&resolved, &args) {
                            Self::frameless_arglines(&mut args, site);
                        } else if delayed_ns {
                            Self::dyn_arglines(&mut args);
                        } else if Self::dedicated_call(&resolved, &args) {
                            Self::dedicated_arglines(&mut args, self.arg_end);
                        } else if !callee_bound {
                            Self::dyn_arglines(&mut args);
                        }
                        Self::fcc_wrap(Expr::Call {
                            name: Box::new(Expr::Str(resolved)),
                            args,
                            site,
                            callee: site,
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
                let site = self.line();
                let name = self.name_path().unwrap_or_default();
                if name.is_empty() {
                    return Err(PhpError::parse(
                        "syntax error, unexpected token \"\\\"",
                        self.line(),
                    ));
                }
                if self.at_op("(") {
                    self.pos += 1;
                    let mut args = self.args()?;
                    // Same bound-at-compile rule as the plain-name
                    // path: FQ calls have no ns fallback, so the only
                    // bound callees are builtins and already-declared
                    // userland functions — anything else sends its
                    // bare-CV args fused at the first arg's line.
                    let callee_bound = {
                        let bare = name.trim_start_matches('\\');
                        let bare_l = bare.to_lowercase();
                        (!bare_l.contains('\\') && crate::builtins::is_builtin(&bare_l))
                            || self.declared_funcs.contains(&bare_l)
                    };
                    if Self::frameless_call(&name, &args) {
                        Self::frameless_arglines(&mut args, site);
                    } else if Self::dedicated_call(&name, &args) {
                        Self::dedicated_arglines(&mut args, self.arg_end);
                    } else if !callee_bound {
                        Self::dyn_arglines(&mut args);
                    }
                    Self::fcc_wrap(Expr::Call {
                        name: Box::new(Expr::Str(name)),
                        args,
                        site,
                        callee: site,
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
            let kline = self.line();
            let first = self.array_elem()?;
            if self.eat_op("=>") {
                if matches!(first, Expr::Unpack(_)) {
                    return Err(PhpError::parse(
                        "syntax error, unexpected token \"=>\"",
                        self.line(),
                    ));
                }
                let vline = self.line();
                let v = self.array_elem()?;
                items.push((Some(Self::markline(first, kline)), Self::markline(v, vline)));
            } else {
                items.push((None, Self::markline(first, kline)));
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(close)?;
        Ok(items)
    }

    /// One array-literal element — may be `&expr` (bound by reference)
    /// or `...expr` (spread, PHP 7.4+). The inner expr keeps its own
    /// first-token line so multi-line spreads/by-ref entries site their
    /// diagnostics on the operand, not the `...`/`&` token.
    pub(in crate::parser) fn array_elem(&mut self) -> Result<Expr, PhpError> {
        if self.eat_op("...") {
            let l = self.line();
            Ok(Expr::Unpack(Box::new(Self::markline(self.expr()?, l))))
        } else if self.eat_op("&") {
            self.ref_target_elem()
        } else {
            self.expr()
        }
    }

    /// `&` inside a `list()`/`[..]` element (zend's `& variable` pair):
    /// the operand is a variable chain — `$v`, `$$v`, `$a[0]`, `$o->p`,
    /// `f()->p`, `C::$s` — or a dereferencable base (`"s"`, `CONST`,
    /// `(expr)`, `[...]`, `array(...)`, `new`) that MUST take at least
    /// one `->`/`?->`/`[` continuation (`&"s"`/`&FOO`/`&(1)` are
    /// `unexpected token "…", expecting "->" or "?->" or "["`). Number
    /// literals, keywords and operators can't even start the operand
    /// (`&5` → `unexpected integer "5"`, `&const` → `unexpected token
    /// "const"`). A var/call-rooted operand ends at `,`/`]`/`)` —
    /// anything else is the same "expecting continuation" error
    /// (`&$x+1` → `unexpected token "+"`).
    fn ref_target_elem(&mut self) -> Result<Expr, PhpError> {
        let l = self.line();
        let cont_err = |p: &Self| {
            PhpError::parse(
                format!(
                    "syntax error, unexpected {}, expecting \"->\" or \"?->\" or \"[\"",
                    crate::parser::desc_t(p.peek())
                ),
                p.line(),
            )
        };
        match self.peek() {
            Some(Token::Int(_)) | Some(Token::Float(_)) => {
                return Err(PhpError::parse(
                    format!(
                        "syntax error, unexpected {}",
                        crate::parser::desc_t(self.peek())
                    ),
                    l,
                ));
            }
            // `&static` alone: zend consumes `static` as a class
            // reference then expects `::` — the error sites on the
            // token after it ('unexpected token "]", expecting "::"').
            Some(Token::Ident(n))
                if n.eq_ignore_ascii_case("static")
                    && !matches!(self.peek2(), Some(Token::Op("::"))) =>
            {
                self.pos += 1;
                return Err(PhpError::parse(
                    format!(
                        "syntax error, unexpected {}, expecting \"::\"",
                        crate::parser::desc_t(self.peek())
                    ),
                    self.line(),
                ));
            }
            Some(Token::Ident(n))
                if crate::lexer::is_keyword(n)
                    && !matches!(
                        n.to_ascii_lowercase().as_str(),
                        // `new`/`array` start dereferencable bases;
                        // null/true/false parse as constants.
                        "new" | "array" | "null" | "true" | "false"
                    )
                    // `&static::*` is legal zend (by-ref bind to a
                    // static prop).
                    && !(n.eq_ignore_ascii_case("static")
                        && matches!(self.peek2(), Some(Token::Op("::")))) =>
            {
                return Err(PhpError::parse(
                    format!(
                        "syntax error, unexpected token \"{}\"",
                        n.to_ascii_lowercase()
                    ),
                    l,
                ));
            }
            _ => {}
        }
        let e = Self::markline(self.postfix()?, l);
        let rooted = Self::rhs_var_or_call(&e);
        let continued = matches!(
            Self::unmark_argline_r(&e),
            Expr::Index { .. } | Expr::Prop { .. } | Expr::MethodCall { .. }
        );
        if !rooted && !continued {
            // Dereferencable without a continuation — `&"s"`, `&FOO`,
            // `&(1)`, `&[$a]` — zend was still expecting `->`/`?->`/`[`.
            return Err(cont_err(self));
        }
        // Operand complete: only `,`/`]`/`)` may follow (`&$x+1`,
        // `&$x=>…`, `&$x.` all fail here with the continuation error).
        if !matches!(self.peek(), Some(Token::Op("," | "]" | ")"))) {
            return Err(cont_err(self));
        }
        Ok(Expr::ByRef(Box::new(e)))
    }
}
