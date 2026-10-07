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
        self.opt_before_required(&params, &clo_name, line);
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
        self.fn_ctx.push(clo_name);
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
        self.fn_ctx.pop();
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

    pub(in crate::parser) fn match_expr(&mut self) -> Result<Expr, PhpError> {
        self.pos += 1; // match
        self.expect_op("(")?;
        let sl = self.line();
        let subject = self.expr()?;
        self.expect_op(")")?;
        self.expect_op("{")?;
        let mut arms = Vec::new();
        while !self.at_op("}") {
            if self.eat_ident("default") {
                self.expect_op("=>")?;
                let rl = self.line();
                let r = self.expr()?;
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
                arms.push(MatchArm {
                    conds,
                    result: Self::markline(r, rl),
                });
            }
            self.eat_op(",");
        }
        self.expect_op("}")?;
        Ok(Expr::Match {
            subject: Box::new(Self::markline(subject, sl)),
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
            let tl = self.line();
            let e = self.assign()?;
            return Ok(Expr::Throw(Box::new(Self::markline(e, tl))));
        }
        let tl = self.line();
        let e = self.ternary()?;

        if let Some(Token::Op(op)) = self.peek() {
            if ASSIGN_OPS.contains(op) {
                let mut op: &'static str = op;
                self.pos += 1;
                if op == "=" && self.eat_op("&") {
                    op = "=&"; // by-reference assignment
                }
                let rl = self.line();
                let rhs = self.assign()?;
                let target = self.list_target(e)?;
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

    /// `[a, b]` / `list(a, b)` on the left of `=` is destructuring.
    /// Elements keep their `argline` marks — zend sites each
    /// element's own store op at the element's line.
    pub(in crate::parser) fn list_target(&mut self, e: Expr) -> Result<Expr, PhpError> {
        match e {
            Expr::ArrayLit(items) => Ok(Expr::List(
                items
                    .into_iter()
                    .map(|(_, v)| Self::list_elem(v))
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
                        .map(Self::list_elem)
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
                    .map(|v| v.map_or(Ok(None), Self::list_elem))
                    .collect::<Result<_, _>>()?,
            )),
            // Lvalue targets can't carry the arg's line marker —
            // `($x) = 1` must still resolve to a Var target.
            other => Ok(Self::unmark_argline(other)),
        }
    }

    /// One `list()`/`[]` destructure element: `Null` is a skipped
    /// slot; nested `[...]`/`list(...)` destructures recursively;
    /// the element's `argline` mark stays wrapped around the result
    /// (zend sites each element's own store op at the element's line).
    fn list_elem(e: Expr) -> Result<Option<Expr>, PhpError> {
        match e {
            Expr::Binary {
                op: "argline",
                l,
                r,
            } => Ok(Self::list_elem(*r)?.map(|u| Expr::Binary {
                op: "argline",
                l,
                r: Box::new(u),
            })),
            Expr::Null => Ok(None),
            Expr::ArrayLit(items) => Ok(Some(Expr::List(
                items
                    .into_iter()
                    .map(|(_, v)| Self::list_elem(v))
                    .collect::<Result<_, _>>()?,
            ))),
            Expr::Call {
                name,
                args,
                site,
                callee,
            } => match *name {
                Expr::Str(n) if n.eq_ignore_ascii_case("list") => Ok(Some(Expr::List(
                    args.into_iter()
                        .map(Self::list_elem)
                        .collect::<Result<_, _>>()?,
                ))),
                other => Ok(Some(Expr::Call {
                    name: Box::new(other),
                    args,
                    site,
                    callee,
                })),
            },
            e => Ok(Some(e)),
        }
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
            let t = self.ternary()?;
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
        loop {
            if self.eat_op("||") {
                let rline = self.line();
                let r = self.logical_and()?;
                e = Expr::Binary {
                    op: "||",
                    l: Box::new(Self::markline(e, lline)),
                    r: Box::new(Self::markline(r, rline)),
                };
            } else if self.ident_is("or") {
                self.pos += 1;
                let rline = self.line();
                let r = self.logical_and()?;
                e = Expr::Binary {
                    op: "||",
                    l: Box::new(Self::markline(e, lline)),
                    r: Box::new(Self::markline(r, rline)),
                };
            } else if self.ident_is("xor") {
                self.pos += 1;
                let rline = self.line();
                let r = self.logical_and()?;
                e = Expr::Binary {
                    op: "xor",
                    l: Box::new(Self::markline(e, lline)),
                    r: Box::new(Self::markline(r, rline)),
                };
            } else {
                return Ok(e);
            }
        }
    }

    pub(in crate::parser) fn logical_and(&mut self) -> Result<Expr, PhpError> {
        let lline = self.line();
        let mut e = self.equality()?;
        loop {
            if self.eat_op("&&") {
                let rline = self.line();
                let r = self.equality()?;
                e = Expr::Binary {
                    op: "&&",
                    l: Box::new(Self::markline(e, lline)),
                    r: Box::new(Self::markline(r, rline)),
                };
            } else if self.ident_is("and") {
                self.pos += 1;
                let rline = self.line();
                let r = self.equality()?;
                e = Expr::Binary {
                    op: "&&",
                    l: Box::new(Self::markline(e, lline)),
                    r: Box::new(Self::markline(r, rline)),
                };
            } else {
                return Ok(e);
            }
        }
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
            e = Expr::Binary {
                op: ".",
                l: Box::new(Self::markline(e, lline)),
                r: Box::new(Self::markline(r, rline)),
            };
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
            let e = Self::unmark_argline(self.unary()?);
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
            let e = Self::unmark_argline(self.unary()?);
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
                    line: tl,
                });
            }
        }
        Ok(e)
    }

    pub(in crate::parser) fn postfix(&mut self) -> Result<Expr, PhpError> {
        // The expression's first token — zend's callee-node line for a
        // `(...)` dispatch built in this loop (INIT_DYNAMIC_CALL).
        let callee_line = self.line();
        let mut e = self.primary()?;
        loop {
            if self.eat_op("++") {
                let e_u = Self::unmark_argline(e);
                if matches!(
                    e_u,
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
                e = Expr::PostInc(Box::new(e_u));
            } else if self.eat_op("--") {
                let e_u = Self::unmark_argline(e);
                if matches!(
                    e_u,
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
                e = Expr::PostDec(Box::new(e_u));
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
                if self.at_op("(") {
                    // `expr::(...)` first-class-callable-ish — unsupported
                    return Err(PhpError::parse("syntax error, unexpected (", self.line()));
                }
                // Member-name token line — the trace site for `C::m(...)`.
                let site = self.line();
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
                let el = self.line();
                let e = self.expr()?;
                self.expect_op(")")?;
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
                    let e = self.expr()?;
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
                        items.push(Some(Self::markline(self.expr()?, el)));
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
                    // zend grammar: `T_INCLUDE expr` — the operand is a
                    // full expression at include's very low precedence,
                    // so `include ('/f') == 5` includes the bool result
                    // (zend throws "Path cannot be empty"). `eval` is
                    // `T_EVAL '(' expr ')'` — its parens are syntax.
                    let e = if kind == IncludeKind::Eval {
                        self.expect_op("(")?;
                        let el = self.line();
                        let e = self.expr()?;
                        self.expect_op(")")?;
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
                        // Frameless builtins fuse every bare-CV arg's
                        // read into the call op at the name's line;
                        // dedicated ops (array_key_exists, const-fmt
                        // sprintf) fuse them at the last arg's end.
                        if Self::frameless_call(&resolved, &args) {
                            Self::frameless_arglines(&mut args, site);
                        } else if Self::dedicated_call(&resolved, &args) {
                            Self::dedicated_arglines(&mut args, self.arg_end);
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
                    if Self::frameless_call(&name, &args) {
                        Self::frameless_arglines(&mut args, site);
                    } else if Self::dedicated_call(&name, &args) {
                        Self::dedicated_arglines(&mut args, self.arg_end);
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
            let l = self.line();
            Ok(Expr::ByRef(Box::new(Self::markline(self.expr()?, l))))
        } else {
            self.expr()
        }
    }
}
