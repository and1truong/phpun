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
        let (body, end_line) = if arrow {
            self.expect_op("=>")?;
            let e = self.expr()?;
            let el = self.prev_line();
            // A call inside the arrow expr needs a line marker — the
            // body has no statements to set cur_line (closure_064).
            (vec![Stmt::Line(line), Stmt::Return(Some(e))], el)
        } else {
            let b = self.body()?;
            let el = self.prev_line();
            (b, el)
        };
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
                })),
                ctor_args,
            ));
        }
        // `new self` / `new static` / `new parent`
        if self.ident_is("self") || self.ident_is("static") || self.ident_is("parent") {
            return Ok((Expr::Const(self.ident().unwrap()), Vec::new()));
        }
        match self.peek().cloned() {
            Some(Token::Ident(_)) | Some(Token::Op("\\")) => {
                let n = self.name_path().unwrap_or_default();
                Ok((Expr::Const(self.ns_resolve(&n, NsKind::Class)), Vec::new()))
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
        let e = self.ternary()?;

        if let Some(Token::Op(op)) = self.peek() {
            if ASSIGN_OPS.contains(op) {
                let mut op: &'static str = op;
                self.pos += 1;
                if op == "=" && self.eat_op("&") {
                    op = "=&"; // by-reference assignment
                }
                let rhs = self.assign()?;
                let target = self.list_target(e)?;
                return Ok(Expr::Assign {
                    target: Box::new(target),
                    op,
                    value: Box::new(rhs),
                });
            }
        }
        Ok(e)
    }

    /// `[a, b]` / `list(a, b)` on the left of `=` is destructuring.
    pub(in crate::parser) fn list_target(&mut self, e: Expr) -> Result<Expr, PhpError> {
        match e {
            Expr::ArrayLit(items) => Ok(Expr::List(
                items
                    .into_iter()
                    .map(|(_, v)| match v {
                        Expr::Null => None,
                        other => Some(other),
                    })
                    .collect(),
            )),
            Expr::Call { name, args } => match *name {
                Expr::Str(n) if n.eq_ignore_ascii_case("list") => {
                    Ok(Expr::List(args.into_iter().map(Some).collect()))
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
            let e = self.unary()?;
            if matches!(
                e,
                Expr::Call { .. }
                    | Expr::MethodCall { .. }
                    | Expr::StaticCall { .. }
                    | Expr::StaticCallDyn { .. }
            ) {
                return Err(PhpError::fatal(
                    "Can't use method return value in write context",
                    self.line(),
                ));
            }
            return Ok(Expr::PreInc(Box::new(e)));
        }
        if self.eat_op("--") {
            let e = self.unary()?;
            if matches!(
                e,
                Expr::Call { .. }
                    | Expr::MethodCall { .. }
                    | Expr::StaticCall { .. }
                    | Expr::StaticCallDyn { .. }
            ) {
                return Err(PhpError::fatal(
                    "Can't use method return value in write context",
                    self.line(),
                ));
            }
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
                let mut op: &'static str = op;
                self.pos += 1;
                if op == "=" && self.eat_op("&") {
                    op = "=&";
                }
                let rhs = self.assign()?;
                let target = self.list_target(e)?;
                return Ok(Expr::Assign {
                    target: Box::new(target),
                    op,
                    value: Box::new(rhs),
                });
            }
        }
        Ok(e)
    }

    pub(in crate::parser) fn postfix(&mut self) -> Result<Expr, PhpError> {
        let mut e = self.primary()?;
        loop {
            if self.eat_op("++") {
                if matches!(
                    e,
                    Expr::Call { .. }
                        | Expr::MethodCall { .. }
                        | Expr::StaticCall { .. }
                        | Expr::StaticCallDyn { .. }
                ) {
                    return Err(PhpError::fatal(
                        "Can't use method return value in write context",
                        self.line(),
                    ));
                }
                e = Expr::PostInc(Box::new(e));
            } else if self.eat_op("--") {
                if matches!(
                    e,
                    Expr::Call { .. }
                        | Expr::MethodCall { .. }
                        | Expr::StaticCall { .. }
                        | Expr::StaticCallDyn { .. }
                ) {
                    return Err(PhpError::fatal(
                        "Can't use method return value in write context",
                        self.line(),
                    ));
                }
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
                let args = self.args()?;
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
                if self.at_op("(") {
                    // `expr::(...)` first-class-callable-ish — unsupported
                    return Err(PhpError::parse("syntax error, unexpected (", self.line()));
                }
                match self.next() {
                    Some(Token::Ident(n)) => {
                        if n == "class" {
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
                args.push(self.expr()?);
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
                    if self.at_op("(") {
                        self.pos += 1;
                        ctor_args = self.args()?;
                        self.check_no_fcc_ctor(&ctor_args)?;
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
                        items.push(Some(self.expr()?));
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    self.expect_op(")")?;
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
                    let e = if self.eat_op("(") {
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
            Ok(Expr::ByRef(Box::new(self.expr()?)))
        } else {
            self.expr()
        }
    }
}
