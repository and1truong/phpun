//! Statement and declaration grammar: `use`/namespace resolution,
//! attributes, class/member declarations, control flow and type syntax.

use super::*;

impl<'a> Parser<'a> {
    pub(in crate::parser) fn expr_stmt(&mut self) -> Result<Stmt, PhpError> {
        let e = self.expr()?;
        self.expect_op(";")?;
        Ok(Stmt::Expr(e))
    }

    /// `static $a = 1, $b;` — persistent function-local vars.
    pub(in crate::parser) fn static_stmt(&mut self) -> Result<Stmt, PhpError> {
        let line = self.line();
        self.pos += 1; // static
        let mut vars = Vec::new();
        loop {
            let name = match self.next() {
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
            vars.push((name, default));
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(";")?;
        Ok(Stmt::Static { vars, line })
    }

    pub(in crate::parser) fn switch_stmt(&mut self) -> Result<Stmt, PhpError> {
        self.pos += 1; // switch
        self.expect_op("(")?;
        let cond = self.expr()?;
        self.expect_op(")")?;
        let alt = self.eat_op(":");
        if !alt {
            self.expect_op("{")?;
        }
        let mut cases: Vec<(Option<Expr>, Vec<Stmt>)> = Vec::new();
        let mut cur: Option<Vec<Stmt>> = None;
        loop {
            if alt && self.ident_is("endswitch") {
                self.pos += 1;
                self.eat_op(";");
                break;
            }
            if !alt && self.eat_op("}") {
                break;
            }
            if self.peek().is_none() {
                return Err(PhpError::parse(
                    "syntax error, unexpected end of file",
                    self.line(),
                ));
            }
            if self.ident_is("case") {
                if let Some(b) = cur.take() {
                    cases.last_mut().unwrap().1 = b;
                }
                let cl = self.line();
                self.pos += 1;
                let e = self.expr()?;
                if !self.eat_op(":") {
                    self.expect_op(";")?;
                    self.deprecations.push((
                        "Case statements followed by a semicolon (;) are deprecated, use a colon (:) instead".into(),
                        cl,
                    ));
                }
                cases.push((Some(e), Vec::new()));
                cur = Some(Vec::new());
            } else if self.ident_is("default") {
                if let Some(b) = cur.take() {
                    cases.last_mut().unwrap().1 = b;
                }
                let cl = self.line();
                self.pos += 1;
                if !self.eat_op(":") {
                    self.expect_op(";")?;
                    self.deprecations.push((
                        "Case statements followed by a semicolon (;) are deprecated, use a colon (:) instead".into(),
                        cl,
                    ));
                }
                cases.push((None, Vec::new()));
                cur = Some(Vec::new());
            } else {
                cur.get_or_insert_with(Vec::new)
                    .push(Stmt::Line(self.line()));
                let s = self.stmt()?;
                cur.get_or_insert_with(Vec::new).push(s);
            }
        }
        if let Some(b) = cur.take() {
            cases.last_mut().unwrap().1 = b;
        }
        Ok(Stmt::Switch { cond, cases })
    }

    /// One `unset(...)` argument — zend's `unset_variable` grammar: a
    /// variable/property/dim/call chain (postfix continuations only),
    /// then compile checks on the result. Bare literals are parse
    /// errors naming their token; everything else that isn't a valid
    /// unset target is a whole-file compile fatal.
    pub(in crate::parser) fn unset_arg(&mut self, first: bool) -> Result<Expr, PhpError> {
        let paren = self.at_op("(");
        let e = self.postfix()?;
        // Walk the chain links — `nullsafe` anywhere and `[]` appends
        // anywhere fire zend's write-context fatals (nullsafe first).
        let mut nullsafe = false;
        let mut has_append = false;
        let mut cur = &e;
        let root: &Expr = loop {
            cur = match cur {
                Expr::Index { e: inner, i } => {
                    has_append |= i.is_none();
                    inner
                }
                Expr::Prop {
                    obj, nullsafe: ns, ..
                }
                | Expr::MethodCall {
                    obj, nullsafe: ns, ..
                } => {
                    nullsafe |= *ns;
                    obj
                }
                _ => break cur,
            };
        };
        // `dim`/`prop` args are chains; everything else is a leaf — its
        // own outermost kind decides which write-context check applies.
        let leaf = !matches!(e, Expr::Index { .. } | Expr::Prop { .. });
        let expecting = |p: &mut Parser| -> PhpError {
            PhpError::parse(
                format!(
                    "syntax error, unexpected {}, expecting \"->\" or \"?->\" or \"[\"",
                    p.describe()
                ),
                p.line(),
            )
        };
        if leaf {
            match &e {
                Expr::Int(n) => {
                    let suffix = if first { "" } else { ", expecting \")\"" };
                    return Err(PhpError::parse(
                        format!("syntax error, unexpected integer \"{}\"{}", n, suffix),
                        self.line(),
                    ));
                }
                Expr::Float(f) => {
                    let suffix = if first { "" } else { ", expecting \")\"" };
                    return Err(PhpError::parse(
                        format!(
                            "syntax error, unexpected floating-point number \"{}\"{}",
                            f, suffix
                        ),
                        self.line(),
                    ));
                }
                Expr::PostInc(_) => {
                    return Err(PhpError::parse(
                        "syntax error, unexpected token \"++\", expecting \"->\" or \"?->\" or \"[\"",
                        self.line(),
                    ));
                }
                Expr::PostDec(_) => {
                    return Err(PhpError::parse(
                        "syntax error, unexpected token \"--\", expecting \"->\" or \"?->\" or \"[\"",
                        self.line(),
                    ));
                }
                Expr::Isset(_) => {
                    return Err(PhpError::parse(
                        "syntax error, unexpected token \"isset\"",
                        self.line(),
                    ));
                }
                Expr::Empty(_) => {
                    return Err(PhpError::parse(
                        "syntax error, unexpected token \"empty\"",
                        self.line(),
                    ));
                }
                // literals and other non-chain starters: zend parsed
                // them as a dim-root then dies on the token that must
                // have been a continuation.
                Expr::Str(_)
                | Expr::Interp(_)
                | Expr::Bool(_)
                | Expr::Null
                | Expr::ArrayLit(_)
                | Expr::Const(_)
                | Expr::Paren(_)
                | Expr::New { .. } => return Err(expecting(self)),
                _ => {}
            }
        }
        if paren && leaf {
            // `unset(($a))` — a parenthesized expr is no variable.
            return Err(expecting(self));
        }
        // The chain must be followed by `,` or `)` — any other token is
        // a parse error with zend's continuation list.
        if !self.at_op(",") && !self.at_op(")") {
            return Err(expecting(self));
        }
        if nullsafe {
            return Err(PhpError::compile_fatal(
                "Can't use nullsafe operator in write context",
                self.line(),
            ));
        }
        if !std::ptr::eq(root, &e) {
            // Root of a dim/prop chain must be a writable container —
            // anything else is a "temporary expression".
            let writable = matches!(
                root,
                Expr::Var(_)
                    | Expr::VarVar(_)
                    | Expr::Prop { .. }
                    | Expr::StaticProp { .. }
                    | Expr::Call { .. }
                    | Expr::MethodCall { .. }
                    | Expr::StaticCall { .. }
                    | Expr::StaticCallDyn { .. }
                    | Expr::Fcc(_)
                    | Expr::New { .. }
            );
            if !writable {
                return Err(PhpError::compile_fatal(
                    "Cannot use temporary expression in write context",
                    self.line(),
                ));
            }
        }
        if has_append {
            return Err(PhpError::compile_fatal(
                "Cannot use [] for unsetting",
                self.line(),
            ));
        }
        if leaf {
            match &e {
                Expr::Var(n) if n == "this" => {
                    return Err(PhpError::compile_fatal("Cannot unset $this", self.line()));
                }
                Expr::Call { .. } => {
                    return Err(PhpError::compile_fatal(
                        "Can't use function return value in write context",
                        self.line(),
                    ));
                }
                Expr::MethodCall { .. } | Expr::StaticCall { .. } | Expr::StaticCallDyn { .. } => {
                    return Err(PhpError::compile_fatal(
                        "Can't use method return value in write context",
                        self.line(),
                    ));
                }
                Expr::Fcc(inner) => {
                    // `unset(f(...))` — FCC of a call is still a call
                    // result in write context.
                    let msg = match inner.as_ref() {
                        Expr::Call { .. } => "Can't use function return value in write context",
                        _ => "Can't use method return value in write context",
                    };
                    return Err(PhpError::compile_fatal(msg, self.line()));
                }
                _ => {}
            }
        }
        Ok(e)
    }

    pub(in crate::parser) fn foreach_stmt(&mut self) -> Result<Stmt, PhpError> {
        self.pos += 1; // foreach
        self.expect_op("(")?;
        let arr = self.expr()?;
        if !self.eat_ident("as") {
            return Err(PhpError::parse(
                "syntax error, unexpected token, expecting \"as\"",
                self.line(),
            ));
        }
        let first = self.foreach_target()?;
        let (key, val) = if self.eat_op("=>") {
            let v = self.foreach_target()?;
            (Some(first), v)
        } else {
            (None, first)
        };
        self.expect_op(")")?;
        let body = self.body_any("endforeach")?;
        let key = key.map(|t| match t {
            ForeachTarget::Var(n) => ForeachKey::Var(n),
            ForeachTarget::ByRef(_) => ForeachKey::ByRef,
            _ => ForeachKey::Var(String::new()), // list keys unsupported
        });
        Ok(Stmt::Foreach {
            arr,
            key,
            val,
            body,
        })
    }

    pub(in crate::parser) fn foreach_target(&mut self) -> Result<ForeachTarget, PhpError> {
        self.foreach_target_in(false)
    }

    /// `in_list` marks destructuring elements (`as [$a, $b]` /
    /// `as list($a, $b)`) — a `?->` chain there reports
    /// 'Assignments can only happen to writable values' while the
    /// direct target reports 'Can't use nullsafe operator in write
    /// context' (p13 l6 vs fp2).
    fn foreach_target_in(&mut self, in_list: bool) -> Result<ForeachTarget, PhpError> {
        if self.eat_op("&") {
            // `&$v`, `&$o->p`, `&$a[i]` — a write-context `new_variable`
            // chain (call roots and `?->` are compile fatals).
            return Ok(ForeachTarget::ByRef(Box::new(self.ref_variable(true)?)));
        }
        if self.at_op("[") {
            self.pos += 1;
            let mut items = Vec::new();
            while !self.at_op("]") {
                if self.eat_op(",") {
                    items.push(None);
                    continue;
                }
                items.push(Some(self.foreach_target_in(true)?));
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op("]")?;
            return Ok(ForeachTarget::List(items));
        }
        if self.ident_is("list") && matches!(self.peek2(), Some(Token::Op("("))) {
            self.pos += 1;
            self.expect_op("(")?;
            let mut items = Vec::new();
            while !self.at_op(")") {
                if self.at_op(",") {
                    items.push(None);
                    self.pos += 1;
                    continue;
                }
                items.push(Some(self.foreach_target_in(true)?));
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
            return Ok(ForeachTarget::List(items));
        }
        match self.next() {
            Some(Token::Variable(n)) => {
                // Lvalue targets: `$b[0]`, `$o->p`, `$o?->p` — dim and
                // prop links like zend's `variable` write context
                // (`foreach($a as $o->p)` is legal; `?->` is the
                // write-context compile fatal).
                let mut e = Expr::Var(n);
                loop {
                    if self.at_op("[") {
                        self.pos += 1;
                        let i = if self.at_op("]") {
                            None
                        } else {
                            Some(Box::new(self.expr()?))
                        };
                        self.expect_op("]")?;
                        e = Expr::Index { e: Box::new(e), i };
                    } else if self.at_op("->") || self.at_op("?->") {
                        let nullsafe = self.at_op("?->");
                        self.pos += 1;
                        let name = match self.next() {
                            Some(Token::Ident(m)) => PropName::Name(m),
                            Some(Token::Op("{")) => {
                                let inner = self.expr()?;
                                self.expect_op("}")?;
                                PropName::Expr(Box::new(inner))
                            }
                            Some(Token::Variable(v)) => PropName::Name(v),
                            t => {
                                return Err(PhpError::parse(
                                    format!("syntax error, unexpected {}", desc_t(t.as_ref())),
                                    self.line(),
                                ));
                            }
                        };
                        e = Expr::Prop {
                            obj: Box::new(e),
                            name,
                            nullsafe,
                        };
                    } else {
                        break;
                    }
                }
                if Self::has_nullsafe(&e) {
                    return Err(PhpError::compile_fatal(
                        if in_list {
                            "Assignments can only happen to writable values"
                        } else {
                            "Can't use nullsafe operator in write context"
                        },
                        self.line(),
                    ));
                }
                if let Expr::Var(_) = e {
                    Ok(ForeachTarget::Var(match e {
                        Expr::Var(n) => n,
                        _ => unreachable!(),
                    }))
                } else {
                    Ok(ForeachTarget::Lvalue(Box::new(e)))
                }
            }
            t => Err(PhpError::parse(
                format!(
                    "syntax error, unexpected {}, expecting variable",
                    desc_t(t.as_ref())
                ),
                self.line(),
            )),
        }
    }

    pub(in crate::parser) fn try_stmt(&mut self) -> Result<Stmt, PhpError> {
        self.pos += 1; // try
        let body = self.body()?;
        let mut catches = Vec::new();
        while self.ident_is("catch") {
            self.pos += 1;
            self.expect_op("(")?;
            let mut types = Vec::new();
            loop {
                if let Some(n) = self.name_path() {
                    types.push(self.ns_resolve(&n, NsKind::Class));
                }
                if !self.eat_op("|") {
                    break;
                }
            }
            let var = match self.next() {
                Some(Token::Variable(n)) => Some(n),
                Some(t) => {
                    self.pos -= 1;
                    let _ = t;
                    None
                }
                None => None,
            };
            self.expect_op(")")?;
            let cbody = self.body()?;
            catches.push(Catch {
                types,
                var,
                body: cbody,
            });
        }
        let finally = if self.ident_is("finally") {
            self.pos += 1;
            Some(self.body()?)
        } else {
            None
        };
        Ok(Stmt::Try {
            body,
            catches,
            finally,
        })
    }

    pub(in crate::parser) fn declare_stmt(&mut self) -> Result<Stmt, PhpError> {
        self.pos += 1; // declare
        self.expect_op("(")?;
        let name = self.ident().unwrap_or_default();
        self.expect_op("=")?;
        let value = self.expr()?;
        self.expect_op(")")?;
        let is_strict = name.eq_ignore_ascii_case("strict_types");
        // `declare(strict_types=1)` is legal only as the very first
        // top-level statement — nowhere nested, nothing before it
        // (scalar_strict_declaration_placement_*, strict_nested).
        if is_strict && !self.strict_slot {
            return Err(PhpError::fatal(
                "strict_types declaration must be the very first statement in the script",
                self.line(),
            ));
        }
        let decl = Stmt::Declare { name, value };
        if self.eat_op(";") {
            Ok(decl)
        } else {
            // `declare(...) { }` / `declare(...):` block forms —
            // strict_types forbids block mode entirely (placement_008).
            if is_strict {
                return Err(PhpError::fatal(
                    "strict_types declaration must not use block mode",
                    self.line(),
                ));
            }
            let body = self.body_any("enddeclare")?;
            Ok(Stmt::Block(vec![decl, Stmt::Block(body)]))
        }
    }

    /// Top-level `use` import: `use A\B, C as D, function f\g, const H\I;`
    /// and group form `use A\{B, C as D}`. Aliases populate the
    /// per-namespace maps `ns_resolve` consults (Zend/tests/namespaces);
    /// the raw paths ride along in `Stmt::Use` so the interpreter can
    /// warn on non-compound imports (`use A;` — ns_033).
    pub(in crate::parser) fn use_stmt(&mut self) -> Result<Stmt, PhpError> {
        self.pos += 1; // use
        let mut kind = NsKind::Class;
        if self.ident_is("function") {
            self.pos += 1;
            kind = NsKind::Func;
        } else if self.ident_is("const") {
            self.pos += 1;
            kind = NsKind::Const;
        }
        let mut names = Vec::new();
        while let Some(n) = self.name_path() {
            let path = n.trim_start_matches('\\').to_string();
            if self.eat_op("{") {
                // Group use: `use A\{B, C as D}` — prefix applies to
                // every entry and does not itself warn.
                loop {
                    let mut ekind = kind;
                    if self.ident_is("function") || self.ident_is("const") {
                        // A typed `use const|function` outer forbids
                        // re-typing items inside the braces (ns_094).
                        if kind != NsKind::Class {
                            let kw = if self.ident_is("function") {
                                "function"
                            } else {
                                "const"
                            };
                            return Err(PhpError::parse(
                                format!(
                                    "syntax error, unexpected token \"{}\", expecting \"}}\"",
                                    kw
                                ),
                                self.line(),
                            ));
                        }
                        ekind = if self.ident_is("function") {
                            NsKind::Func
                        } else {
                            NsKind::Const
                        };
                        self.pos += 1;
                    }
                    // `use A\{\B}` — leading separator illegal (ns_096).
                    if self.at_op("\\") {
                        let lead = self.name_path().unwrap_or_default();
                        return Err(PhpError::parse(
                            format!(
                                "syntax error, unexpected fully qualified name \"{}\", expecting identifier or namespaced name or \"function\" or \"const\"",
                                lead
                            ),
                            self.line(),
                        ));
                    }
                    if let Some(sub) = self.name_path() {
                        let sub = sub.trim_start_matches('\\');
                        let fq = format!("{}\\{}", path, sub);
                        let alias = if self.ident_is("as") {
                            self.pos += 1;
                            self.ident().unwrap_or_default()
                        } else {
                            sub.rsplit('\\').next().unwrap_or(sub).to_string()
                        };
                        self.insert_use_alias(ekind, &alias, &fq)?;
                        names.push(fq);
                    }
                    if self.eat_op("}") {
                        break;
                    }
                    // `use A\{B\{C}}` — nested group use is a syntax
                    // error naming `}` (namespaces/ns_088).
                    if self.at_op("{") {
                        return Err(PhpError::parse(
                            "syntax error, unexpected token \"{\", expecting \"}\"",
                            self.line(),
                        ));
                    }
                    self.expect_op(",")?;
                }
            } else {
                let aliased = self.ident_is("as");
                let alias = if aliased {
                    self.pos += 1;
                    self.ident().unwrap_or_default()
                } else {
                    path.rsplit('\\').next().unwrap_or(&path).to_string()
                };
                self.insert_use_alias(kind, &alias, &path)?;
                if !path.contains('\\') && !aliased && self.cur_ns.is_empty() {
                    names.push(path);
                }
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(";")?;
        Ok(Stmt::Use(names))
    }

    /// A plain `use` imports a class alias only — unqualified function
    /// and const names keep their namespace->global runtime fallback
    /// (Zend/tests/namespaces/ns_012); `use function`/`use const` fill
    /// their own tables.
    pub(in crate::parser) fn insert_use_alias(
        &mut self,
        kind: NsKind,
        alias: &str,
        fq: &str,
    ) -> Result<(), PhpError> {
        // Re-importing the same alias to the same target is a no-op
        // (namespaces/ns_078).
        if kind == NsKind::Class && self.use_map.get(&alias.to_lowercase()) == Some(&fq.to_string())
        {
            return Ok(());
        }
        if kind == NsKind::Class && self.declared_types.contains(&alias.to_lowercase()) {
            return Err(PhpError::fatal(
                format!(
                    "Cannot use {} as {} because the name is already in use",
                    fq, alias
                ),
                self.line(),
            ));
        }
        // `use X as int` / `use int` — reserved scalar names can't be
        // imported (scalar_reserved*_use).
        const RESERVED_ALS: &[&str] = &[
            "int", "float", "string", "bool", "void", "iterable", "object", "mixed", "never",
            "null", "false", "true",
        ];
        let short = alias.rsplit('\\').next().unwrap_or(alias).to_lowercase();
        if kind == NsKind::Class && RESERVED_ALS.contains(&short.as_str()) {
            return Err(PhpError::compile_fatal(
                format!(
                    "Cannot use {} as {} because '{}' is a special class name",
                    fq, alias, short
                ),
                self.line(),
            ));
        }
        match kind {
            NsKind::Class => {
                self.use_map.insert(alias.to_lowercase(), fq.to_string());
            }
            NsKind::Func => {
                self.use_fn_map.insert(alias.to_lowercase(), fq.to_string());
            }
            NsKind::Const => {
                self.use_const_map.insert(alias.to_string(), fq.to_string());
            }
        };
        Ok(())
    }

    /// `Foo\Bar\Baz` — backslash-joined qualified name.
    /// `\` inside or before a qualified name must have no surrounding
    /// whitespace in source (namespaced_name_whitespace). A leading `\`
    /// may have whitespace before it (`= \foo()`), mid-name may not.
    pub(in crate::parser) fn backslash_adj_ok(&self) -> bool {
        matches!(self.toks.get(self.pos), Some(t) if t.ws_adj & 2 == 0)
    }

    /// Mid-name `\`: continues the path only when tight on the left and
    /// directly followed by an identifier (`Foo\Bar`); also serves as a
    /// group-use terminator before `{` (`use A\B\{C}` — ns_093).
    pub(in crate::parser) fn eat_mid_name_sep(&mut self) -> bool {
        let ok = match (self.toks.get(self.pos), self.peek2()) {
            // Group-use terminator `A\B\{C}` / `A\B \ { C }` — spacing
            // around the final separator is free (ns_093).
            (Some(_), Some(Token::Op("{"))) => true,
            (Some(t), Some(Token::Ident(_))) => t.ws_adj == 0,
            _ => false,
        };
        ok && self.eat_op("\\")
    }

    pub(in crate::parser) fn name_path(&mut self) -> Option<String> {
        let mut parts = Vec::new();
        // leading \ for FQ names
        let lead = self.at_op("\\") && self.backslash_adj_ok() && self.eat_op("\\");
        while matches!(self.peek(), Some(Token::Ident(_))) {
            parts.push(self.ident().unwrap());
            if !self.eat_mid_name_sep() {
                break;
            }
        }
        if parts.is_empty() {
            return None;
        }
        let mut s = parts.join("\\");
        if lead {
            s = format!("\\{}", s);
        }
        Some(s)
    }

    /// Name of a declared symbol inside the current namespace:
    /// `Foo` in `namespace A` -> `A\Foo` (Zend/tests/namespaces).
    pub(in crate::parser) fn ns_qualify(&self, name: &str) -> String {
        let name = name.trim_start_matches('\\');
        if self.cur_ns.is_empty() {
            name.to_string()
        } else {
            format!("{}\\{}", self.cur_ns, name)
        }
    }

    /// Compile-time name resolution matching Zend's rules:
    /// `\A\B` is used verbatim; `namespace\A` expands to `A\<cur>`;
    /// qualified `a\b` checks `a` against the alias table then prepends
    /// the namespace; an unqualified `a` checks aliases then — for
    /// classes only — prepends the namespace (function/const names fall
    /// back to global at runtime instead).
    pub(in crate::parser) fn ns_resolve(&self, raw: &str, kind: NsKind) -> String {
        if raw.starts_with('\\') {
            return raw.trim_start_matches('\\').to_string();
        }
        let lower = raw.to_lowercase();
        if matches!(lower.as_str(), "self" | "static" | "parent") {
            return raw.to_string();
        }
        let segs: Vec<&str> = raw.split('\\').collect();
        let mut from_ns = false;
        let segs: Vec<&str> = if segs[0].eq_ignore_ascii_case("namespace") {
            from_ns = true;
            segs[1..].to_vec()
        } else {
            segs
        };
        if segs.is_empty() {
            return self.cur_ns.clone();
        }

        // Qualified names resolve their first segment through the CLASS
        // alias table regardless of symbol kind (`use A\C; C\X` ->
        // `A\C\X` even as a const read). Only unqualified names use the
        // kind-specific tables (`use function`, `use const`).
        let map = if segs.len() > 1 {
            &self.use_map
        } else {
            match kind {
                NsKind::Class => &self.use_map,
                NsKind::Func => &self.use_fn_map,
                NsKind::Const => &self.use_const_map,
            }
        };
        let key = if kind == NsKind::Const && segs.len() == 1 {
            // Unqualified const: the const alias table is case-sensitive.
            segs[0].to_string()
        } else {
            // Class-alias lookups (and `use function`) are insensitive.
            segs[0].to_lowercase()
        };
        if !from_ns {
            if let Some(target) = map.get(&key) {
                if segs.len() == 1 {
                    return target.clone();
                }
                return format!("{}\\{}", target, segs[1..].join("\\"));
            }
        }
        if segs.len() == 1 && kind != NsKind::Class && !from_ns {
            // Unqualified function/const: resolved at runtime with a
            // global fallback, so keep the bare name.
            return segs[0].to_string();
        }
        if self.cur_ns.is_empty() {
            segs.join("\\")
        } else {
            format!("{}\\{}", self.cur_ns, segs.join("\\"))
        }
    }

    /// Skip `#[Attr(...)]` groups (attributes are parsed but discarded).
    pub(in crate::parser) fn skip_attrs(&mut self) -> Result<(), PhpError> {
        self.parse_attrs()?;
        Ok(())
    }

    /// Line of the statement an attribute group attaches to: scans past
    /// the group's closing `]` (and any further `]` from sibling groups
    /// is not needed — the first `]` at depth 0 ends the scan).
    /// Zend reports attribute-arg compile fatals on the attributed
    /// declaration's line (first_class_callable_011,
    /// named_params/attributes_*).
    pub(in crate::parser) fn line_after_attr_group(&self) -> usize {
        let mut i = self.pos;
        let mut d = 0i32;
        while i < self.toks.len() {
            match &self.toks[i].token {
                Token::Op("(") | Token::Op("[") | Token::Op("#[") => d += 1,
                Token::Op(")") => {
                    if d > 0 {
                        d -= 1;
                    }
                }
                Token::Op("]") => {
                    if d == 0 {
                        i += 1;
                        break;
                    }
                    d -= 1;
                }
                _ => {}
            }
            i += 1;
        }
        while self.toks.get(i).is_some_and(|t| t.token == Token::Op("]")) {
            i += 1;
        }
        self.toks.get(i).map(|t| t.line).unwrap_or(0)
    }

    /// Parse `#[Attr(...)]` groups, keeping names and arg Exprs.
    pub(in crate::parser) fn parse_attrs(&mut self) -> Result<Vec<crate::ast::AttrDecl>, PhpError> {
        let mut attrs = Vec::new();
        while self.eat_op("#[") {
            loop {
                let line = self.line();
                let name = self.name_path().ok_or_else(|| {
                    PhpError::parse(
                        "syntax error, unexpected token, expecting attribute name",
                        self.line(),
                    )
                })?;
                // Attribute names resolve through the file's use-map at
                // compile time — `#[AsCommand]` under
                // `use X\Y\AsCommand` instantiates X\Y\AsCommand.
                let name = self.ns_resolve(&name, NsKind::Class);
                let mut args = Vec::new();
                if self.at_op("(") {
                    // `(` followed by `...` is FCC syntax — a compile-time
                    // fatal inside attribute args (first_class_callable_011),
                    // reported on the attributed declaration's line.
                    self.pos += 1;
                    if self.at_op("...") {
                        let line = self.line_after_attr_group();
                        return Err(PhpError::compile_fatal(
                            "Cannot create Closure as attribute argument",
                            line,
                        ));
                    }
                    self.pos -= 1;
                    self.expect_op("(")?;
                    match self.args() {
                        Ok(list) => {
                            // Duplicate named args are a compile-time
                            // fatal for attribute args (unlike calls,
                            // which warn at bind time).
                            let mut seen = std::collections::HashSet::new();
                            for a in &list {
                                if let Expr::Binary { op: "named", l, .. } = a {
                                    if let Expr::Str(n) = l.as_ref() {
                                        if !seen.insert(n.clone()) {
                                            let line = self.line_after_attr_group();
                                            return Err(PhpError::compile_fatal(
                                                format!("Duplicate named parameter ${}", n),
                                                line,
                                            ));
                                        }
                                    }
                                }
                            }
                            args = list;
                        }
                        Err(e) if matches!(e.kind, crate::error::ErrorKind::Fatal) => {
                            let line = self.line_after_attr_group();
                            return Err(PhpError::compile_fatal(&e.message, line));
                        }
                        Err(e) => return Err(e),
                    }
                }
                attrs.push(crate::ast::AttrDecl { name, args, line });
                if self.eat_op(",") {
                    continue;
                }
                self.expect_op("]")?;
                break;
            }
        }
        Ok(attrs)
    }

    pub(in crate::parser) fn class_decl(&mut self) -> Result<Stmt, PhpError> {
        let decl_line = self.line();
        // `#[Attr]` groups may precede the class modifiers (or were
        // already consumed at the statement level).
        let attrs = if self.pending_class_attrs.is_empty() {
            self.parse_attrs()?
        } else {
            std::mem::take(&mut self.pending_class_attrs)
        };
        let mut is_readonly = false;
        let mut is_abstract = false;
        let mut is_final = false;
        loop {
            if self.ident_is("abstract") {
                is_abstract = true;
                self.pos += 1;
            } else if self.ident_is("final") {
                is_final = true;
                self.pos += 1;
            } else if self.ident_is("readonly") {
                is_readonly = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        let kind = if self.eat_ident("interface") {
            ClassKind::Interface
        } else if self.eat_ident("trait") {
            ClassKind::Trait
        } else if self.eat_ident("enum") {
            ClassKind::Enum
        } else if self.eat_ident("class") {
            ClassKind::Class
        } else {
            return Err(PhpError::parse(
                "syntax error, expecting class kind",
                self.line(),
            ));
        };
        let name = self
            .ident()
            .unwrap_or_else(|| "class@anonymous".to_string());
        let name = self.ns_qualify(&name);
        // `use A\B as Foo; class Foo {}` — the alias already occupies
        // the short name (namespaces/ns_029).
        if self
            .use_map
            .contains_key(&name.rsplit('\\').next().unwrap_or(&name).to_lowercase())
        {
            return Err(PhpError::fatal(
                format!(
                    "Cannot redeclare class {} (previously declared as local import)",
                    name
                ),
                self.line(),
            ));
        }
        self.declared_types
            .insert(name.rsplit('\\').next().unwrap_or(&name).to_lowercase());
        self.cur_class = name.clone();
        // enum backing type `enum X: int`
        if self.eat_op(":") {
            self.skip_type()?;
        }
        let mut parent = None;
        let mut implements = Vec::new();
        // `trait Foo extends ...` / `trait Foo implements ...` are
        // syntax errors — traits can't inherit (bug55524).
        if kind == ClassKind::Trait && self.ident_is("extends") {
            return Err(PhpError::parse(
                "syntax error, unexpected token \"extends\", expecting \"{\"",
                self.line(),
            ));
        }
        if kind == ClassKind::Trait && self.ident_is("implements") {
            return Err(PhpError::parse(
                "syntax error, unexpected token \"implements\", expecting \"{\"",
                self.line(),
            ));
        }
        if self.eat_ident("extends") {
            if kind == ClassKind::Interface {
                // `interface Y extends X, Z` — multiple interface parents
                // recorded in `implements` (what instanceof/iface walks use).
                while let Some(n) = self.name_path() {
                    implements.push(self.ns_resolve(&n, NsKind::Class));
                    if !self.eat_op(",") {
                        break;
                    }
                }
            } else {
                parent = self.name_path().map(|n| self.ns_resolve(&n, NsKind::Class));
            }
        }
        if self.eat_ident("implements") {
            while let Some(n) = self.name_path() {
                implements.push(self.ns_resolve(&n, NsKind::Class));
                if !self.eat_op(",") {
                    break;
                }
            }
        }
        self.expect_op("{")?;
        // `implements I` alone does NOT give the class a `parent` —
        // `parent::`/`new parent()`/`parent::$p` inside must still hit
        // zend's whole-file compile fatal.
        self.class_ctx
            .push((parent.is_some(), kind == ClassKind::Trait));
        let mut methods = Vec::new();
        let mut props = Vec::new();
        let mut consts = Vec::new();
        let mut traits = Vec::new();
        let mut adaptations = Vec::new();
        while !self.at_op("}") {
            if self.peek().is_none() {
                return Err(PhpError::parse(
                    "syntax error, unexpected end of file",
                    self.line(),
                ));
            }
            // `#[Attr]` groups attach to the member that follows —
            // consts keep them for ReflectionClassConstant (constant_020).
            let member_attrs = self.parse_attrs()?;
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
                let m = self.method_decl(is_static, m_abstract, m_final, vis)?;
                methods.push(Rc::new(m));
                continue;
            }
            if self.ident_is("const") {
                self.pos += 1;
                // Typed class constants (PHP 8.3): `const string C` —
                // a type is present iff the run of type tokens is
                // followed by `=`; the type applies to every declarator
                // in the statement (`const int X = 1, Y = 'y'`).
                let mut decl_ty = {
                    let mut j = self.pos;
                    let mut run = 0usize;
                    loop {
                        match self.toks.get(j).map(|l| &l.token) {
                            Some(Token::Ident(_)) => {
                                j += 1;
                                run += 1;
                            }
                            Some(Token::Op(o))
                                if matches!(*o, "?" | "|" | "&" | "\\" | "(" | ")") =>
                            {
                                j += 1;
                                run += 1;
                            }
                            _ => break,
                        }
                    }
                    if run > 1
                        && matches!(
                            self.toks.get(j).map(|l| &l.token),
                            Some(Token::Op(o)) if *o == "="
                        )
                    {
                        self.take_type()?
                    } else {
                        None
                    }
                };
                loop {
                    let mut cname = self.ident().unwrap_or_default();
                    // take_type greedily merges the const name into the
                    // final type member (`string CONST1` → `string\CONST1`)
                    // — recover it as the last `\`-segment.
                    if cname.is_empty() {
                        if let Some(ty) = decl_ty.as_mut() {
                            for member in ty.iter_mut().rev() {
                                if let Some(p) = member.rfind('\\') {
                                    cname = member[p + 1..].to_string();
                                    member.truncate(p);
                                    break;
                                }
                            }
                        }
                    }
                    self.expect_op("=")?;
                    let cv = self.expr()?;
                    consts.push(crate::ast::ConstDecl {
                        name: cname,
                        value: cv,
                        visibility: vis,
                        is_final: m_final,
                        ty: decl_ty.clone(),
                        attrs: member_attrs.clone(),
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
                    traits.push(self.ns_resolve(&n, NsKind::Class));
                    if !self.eat_op(",") {
                        break;
                    }
                }
                if self.eat_op("{") {
                    // Trait adaptations: `T::m insteadof T2, T3;`
                    // `m as alias;` `m as private;` `T::m as private alias;`
                    while !self.at_op("}") {
                        if self.peek().is_none() {
                            return Err(PhpError::parse(
                                "syntax error, unexpected end of file",
                                self.line(),
                            ));
                        }
                        let first = self.name_path().unwrap_or_default();
                        let (tname, mname) = if self.eat_op("::") {
                            (
                                Some(self.ns_resolve(&first, NsKind::Class)),
                                self.ident().unwrap_or_default(),
                            )
                        } else {
                            (None, first)
                        };
                        if self.ident_is("insteadof") {
                            self.pos += 1;
                            let mut excludes = Vec::new();
                            while let Some(n) = self.name_path() {
                                excludes.push(self.ns_resolve(&n, NsKind::Class));
                                if !self.eat_op(",") {
                                    break;
                                }
                            }
                            adaptations.push(TraitAdaptation::Insteadof {
                                trait_name: tname.unwrap_or_default(),
                                method: mname,
                                excludes,
                            });
                        } else if self.ident_is("as") {
                            self.pos += 1;
                            let mut vis = None;
                            let mut is_final = false;
                            loop {
                                if self.ident_is("public") {
                                    vis = Some(Visibility::Public);
                                } else if self.ident_is("protected") {
                                    vis = Some(Visibility::Protected);
                                } else if self.ident_is("private") {
                                    vis = Some(Visibility::Private);
                                } else if self.ident_is("final") {
                                    is_final = true;
                                } else {
                                    break;
                                }
                                self.pos += 1;
                            }
                            let alias = if matches!(self.peek(), Some(Token::Ident(_))) {
                                Some(self.ident().unwrap_or_default())
                            } else {
                                None
                            };
                            adaptations.push(TraitAdaptation::Alias {
                                trait_name: tname,
                                method: mname,
                                alias,
                                vis,
                                is_final,
                            });
                        } else {
                            return Err(PhpError::parse(
                                format!("syntax error, unexpected identifier \"{}\"", mname),
                                self.line(),
                            ));
                        }
                        self.expect_op(";")?;
                    }
                    self.expect_op("}")?;
                } else {
                    self.expect_op(";")?;
                }
                continue;
            }
            if self.ident_is("case") {
                // enum cases
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
                    enum_case: true,
                });
                self.expect_op(";")?;
                continue;
            }
            // Typed or untyped property: [type] $name [= default], ...;
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
                if m_readonly && default.is_some() {
                    // Only promoted ctor params may default (probe12c).
                    return Err(PhpError::compile_fatal(
                        format!(
                            "Readonly property {}::${} cannot have default value",
                            name, pname
                        ),
                        self.line(),
                    ));
                }
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
                    attrs: member_attrs.clone(),
                    line: pline,
                });
                if !self.eat_op(",") {
                    break;
                }
            }
            // PHP 8.4 property hooks attach to the LAST declarator
            // (`public $p { get => ..; }`) — no trailing `;` after `}`.
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
        Ok(Stmt::Class(Rc::new(ClassDecl {
            name,
            attrs,
            kind,
            is_abstract,
            is_final,
            readonly: is_readonly,
            parent,
            implements,
            traits,
            adaptations,
            methods,
            props,
            consts,
            file: String::new(),
            line: decl_line,
        })))
    }

    pub(in crate::parser) fn method_decl(
        &mut self,
        is_static: bool,
        is_abstract: bool,
        is_final: bool,
        vis: Visibility,
    ) -> Result<MethodDecl, PhpError> {
        let line = self.line();
        self.pos += 1; // function
        let by_ref = self.eat_op("&");
        let name = self.ident().unwrap_or_default();
        let prev_hook = self.hook_ctx.take();
        let params = self.params()?;
        let ret = if self.eat_op(":") {
            self.take_type()?
        } else {
            None
        };
        let prev_ret_by_ref = self.ret_by_ref;
        self.ret_by_ref = by_ref;
        let (body, end_line) = if self.eat_op(";") {
            (Vec::new(), line)
        } else {
            let b = self.body()?;
            let e = self.prev_line();
            (b, e)
        };
        self.ret_by_ref = prev_ret_by_ref;
        self.hook_ctx = prev_hook;
        Ok(MethodDecl {
            decl: FunctionDecl {
                ret,
                name,
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
            is_static,
            is_abstract,
            is_final,
            visibility: vis,
            trait_alias_of: None,
        })
    }

    /// `static` is never a legal property type — Zend reports it as
    /// `unexpected token "static"` (static_type_property).
    pub(in crate::parser) fn check_prop_ty(
        &self,
        ty: &Option<Vec<String>>,
        line: usize,
    ) -> Result<(), PhpError> {
        if let Some(ms) = ty {
            for m in ms {
                if m.trim_matches(|c| c == '(' || c == ')')
                    .split('&')
                    .any(|p| p.trim_start_matches('\\').eq_ignore_ascii_case("static"))
                {
                    return Err(PhpError::parse(
                        "syntax error, unexpected token \"static\"",
                        line,
                    ));
                }
            }
        }
        Ok(())
    }

    /// `{ get => e; set { .. }; set(T $v) { .. }; get; }` — PHP 8.4
    /// property hooks (Zend/tests/property_hooks). Called with the `{`
    /// already detected; consumes through the closing `}`.
    pub(in crate::parser) fn prop_hooks(
        &mut self,
        pname: &str,
    ) -> Result<Option<Vec<PropHook>>, PhpError> {
        self.expect_op("{")?;
        if self.at_op("}") {
            return Err(PhpError::fatal(
                "Property hook list must not be empty",
                self.line(),
            ));
        }
        let mut hs: Vec<PropHook> = Vec::new();
        while !self.at_op("}") {
            if self.peek().is_none() {
                return Err(PhpError::parse(
                    "syntax error, unexpected end of file",
                    self.line(),
                ));
            }
            self.skip_attrs()?;
            let mut hvis = None;
            let mut hfinal = false;
            loop {
                if self.ident_is("public") {
                    hvis = Some(Visibility::Public);
                    self.pos += 1;
                } else if self.ident_is("protected") {
                    hvis = Some(Visibility::Protected);
                    self.pos += 1;
                } else if self.ident_is("private") {
                    hvis = Some(Visibility::Private);
                    self.pos += 1;
                } else if self.ident_is("final") {
                    hfinal = true;
                    self.pos += 1;
                } else if self.ident_is("static") {
                    return Err(PhpError::fatal(
                        "Cannot use the static modifier on a property hook",
                        self.line(),
                    ));
                } else {
                    break;
                }
            }
            let by_ref = self.eat_op("&");
            // Any identifier is consumed here — an unknown one is a
            // compile-fatal naming the class+prop (unknown_hook).
            let hname = match self.next() {
                Some(Token::Ident(n)) => n,
                t => {
                    return Err(PhpError::parse(
                        format!(
                            "syntax error, unexpected {}, expecting \"get\" or \"set\"",
                            desc_t(t.as_ref())
                        ),
                        self.line(),
                    ))
                }
            };
            if hname != "get" && hname != "set" {
                return Err(PhpError::fatal(
                    format!(
                        "Unknown hook \"{}\" for property {}::${}, expected \"get\" or \"set\"",
                        hname, self.cur_class, pname
                    ),
                    self.line(),
                ));
            }
            let is_get = hname == "get";
            if hs.iter().any(|h| h.is_get == is_get) {
                return Err(PhpError::fatal(
                    format!("Cannot redeclare property hook \"{}\"", hname),
                    self.line(),
                ));
            }
            let has_plist = self.at_op("(");
            let params = if has_plist {
                self.params()?
            } else {
                Vec::new()
            };
            let line = self.line();
            let prev_hook = self.hook_ctx.replace((pname.to_string(), is_get));
            let body = if self.eat_op("=>") {
                let e = self.expr()?;
                self.expect_op(";")?;
                if is_get {
                    Some(vec![Stmt::Line(line), Stmt::Return(Some(e))])
                } else {
                    // `set => e` ≡ `set { $this->prop = e; }` (hooks short form).
                    Some(vec![
                        Stmt::Line(line),
                        Stmt::Expr(Expr::Assign {
                            target: Box::new(Expr::Prop {
                                obj: Box::new(Expr::Var("this".into())),
                                name: PropName::Name(pname.to_string()),
                                nullsafe: false,
                            }),
                            op: "=",
                            value: Box::new(e),
                        }),
                    ])
                }
            } else if self.at_op("{") {
                Some(self.body()?)
            } else if self.eat_op(";") {
                None
            } else {
                return Err(PhpError::parse(
                    format!(
                        "syntax error, unexpected {}, expecting \"=>\" or \"{{\" or \";\"",
                        desc_t(self.peek())
                    ),
                    self.line(),
                ));
            };
            self.hook_ctx = prev_hook;
            hs.push(PropHook {
                name: hname,
                is_get,
                params,
                has_plist,
                body,
                by_ref,
                is_final: hfinal,
                visibility: hvis,
            });
        }
        self.expect_op("}")?;
        // `}` ends the prop; a `;` is tolerated but not required.
        self.eat_op(";");
        Ok(Some(hs))
    }

    /// `private(set)` / `protected(set)` asymmetric write visibility —
    /// at `ident (`, consume `( set )` when that's what follows.
    pub(in crate::parser) fn at_asym_set(&mut self) -> bool {
        if matches!(self.peek2(), Some(Token::Op("(")))
            && matches!(self.toks.get(self.pos + 2).map(|l| &l.token), Some(Token::Ident(n)) if n == "set")
            && matches!(
                self.toks.get(self.pos + 3).map(|l| &l.token),
                Some(Token::Op(")"))
            )
        {
            self.pos += 4; // `private` `(` `set` `)`
            true
        } else {
            false
        }
    }

    pub(in crate::parser) fn params(&mut self) -> Result<Vec<Param>, PhpError> {
        self.expect_op("(")?;
        let mut params = Vec::new();
        while !self.at_op(")") {
            self.skip_attrs()?;
            // promoted ctor params: visibility/readonly precede the type
            // (`public int $x`, `public $errno` — error_2_exception_001).
            let mut promoted = false;
            let mut pvis = None;
            let mut preadonly = false;
            let mut pfinal = false;
            let mut psetv = None;
            for _ in 0..4 {
                if self.ident_is("public") {
                    pvis = Some(Visibility::Public);
                    self.pos += 1;
                    promoted = true;
                } else if self.ident_is("private") {
                    if self.at_asym_set() {
                        psetv = Some(Visibility::Private);
                    } else {
                        pvis = Some(Visibility::Private);
                        self.pos += 1;
                    }
                    promoted = true;
                } else if self.ident_is("protected") {
                    if self.at_asym_set() {
                        psetv = Some(Visibility::Protected);
                    } else {
                        pvis = Some(Visibility::Protected);
                        self.pos += 1;
                    }
                    promoted = true;
                } else if self.ident_is("readonly") {
                    preadonly = true;
                    self.pos += 1;
                    promoted = true;
                } else if self.ident_is("final") {
                    pfinal = true;
                    self.pos += 1;
                    promoted = true;
                } else {
                    break;
                }
            }
            // `static` is never a legal param modifier/type
            // (static_type_param).
            if self.ident_is("static") {
                return Err(PhpError::fatal(
                    "Cannot use the static modifier on a parameter",
                    self.line(),
                ));
            }
            // skip type declaration before the variable
            let ty = if matches!(
                self.peek(),
                Some(Token::Ident(_))
                    | Some(Token::Op("?"))
                    | Some(Token::Op("\\"))
                    | Some(Token::Op("("))
            ) && !matches!(self.peek2(), Some(Token::Op(",")) | Some(Token::Op(")")))
            {
                self.take_type()?
            } else {
                None
            };
            let by_ref = self.eat_op("&");
            let variadic = self.eat_op("...");
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
            // Promoted hooked props: `public $p { get {} }` (8.4). Hooks
            // imply promotion even without a visibility modifier
            // (gh15438_1: `__construct($p { set => ... })`).
            let phooks = if self.at_op("{") {
                promoted = true;
                self.prop_hooks(&pname)?
            } else {
                None
            };
            params.push(Param {
                name: pname,
                default,
                by_ref,
                variadic,
                ty,
                promoted,
                vis: pvis,
                readonly: preadonly,
                is_final: pfinal,
                set_vis: psetv,
                hooks: phooks,
            });
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(params)
    }

    pub(in crate::parser) fn if_stmt(&mut self) -> Result<Stmt, PhpError> {
        self.pos += 1; // if
        self.expect_op("(")?;
        let cond = self.expr()?;
        self.expect_op(")")?;
        let alt = self.at_op(":");
        let arm_body = |p: &mut Self, stops: &[&str]| -> Result<Vec<Stmt>, PhpError> {
            if alt {
                p.expect_op(":")?;
                p.body_until(stops)
            } else {
                p.body()
            }
        };
        let then = if alt {
            self.pos += 1;
            self.body_until(&["elseif", "else", "endif"])?
        } else {
            self.body()?
        };
        let mut arms: Vec<(Expr, Vec<Stmt>)> = vec![(cond, then)];
        let mut else_ = Vec::new();
        loop {
            if self.ident_is("elseif") {
                self.pos += 1;
                self.expect_op("(")?;
                let c = self.expr()?;
                self.expect_op(")")?;
                let b = arm_body(self, &["elseif", "else", "endif"])?;
                arms.push((c, b));
                continue;
            }
            if self.ident_is("else") {
                self.pos += 1;
                else_ = arm_body(self, &["endif"])?;
            }
            break;
        }
        if alt {
            if !self.eat_ident("endif") {
                return Err(PhpError::parse(
                    "syntax error, unexpected end of file, expecting \"endif\"",
                    self.line(),
                ));
            }
            self.eat_op(";");
        }
        let (cond, then) = arms.remove(0);
        for (c, b) in arms.into_iter().rev() {
            else_ = vec![Stmt::If {
                cond: c,
                then: b,
                else_,
            }];
        }
        Ok(Stmt::If { cond, then, else_ })
    }

    pub(in crate::parser) fn for_stmt(&mut self) -> Result<Stmt, PhpError> {
        self.pos += 1; // for
        self.expect_op("(")?;
        let mut init = Vec::new();
        if !self.at_op(";") {
            init = self.expr_list()?;
        }
        self.expect_op(";")?;
        let mut cond = Vec::new();
        if !self.at_op(";") {
            cond = self.expr_list()?;
        }
        self.expect_op(";")?;
        let mut inc = Vec::new();
        if !self.at_op(")") {
            inc = self.expr_list()?;
        }
        self.expect_op(")")?;
        // foreach is a different keyword; plain for body here.
        let body = self.body_any("endfor")?;
        Ok(Stmt::For {
            init,
            cond,
            inc,
            body,
        })
    }

    pub(in crate::parser) fn function_decl(&mut self) -> Result<Stmt, PhpError> {
        let line = self.line();
        self.pos += 1; // function
        let by_ref = self.eat_op("&");
        let name = self.ident().ok_or_else(|| {
            PhpError::parse(
                "syntax error, unexpected token, expecting function name",
                self.line(),
            )
        })?;
        let prev_hook = self.hook_ctx.take();
        let params = self.params()?;
        // Return type declarations (: int).
        let ret = if self.eat_op(":") {
            self.take_type()?
        } else {
            None
        };
        let prev_ret_by_ref = self.ret_by_ref;
        self.ret_by_ref = by_ref;
        let body = self.body()?;
        let end_line = self.prev_line();
        self.ret_by_ref = prev_ret_by_ref;
        self.hook_ctx = prev_hook;
        let name = self.ns_qualify(&name);
        Ok(Stmt::Function(FunctionDecl {
            name,
            params,
            ret,
            body,
            attrs: std::mem::take(&mut self.pending_class_attrs),
            by_ref,
            line,
            end_line,
            file: String::new(),
            ns: self.cur_ns.clone(),
            decl_in: None,
        }))
    }

    pub(in crate::parser) fn expect_ident(&mut self, kw: &str) -> Result<(), PhpError> {
        if self.eat_ident(kw) {
            Ok(())
        } else {
            Err(PhpError::parse(
                format!(
                    "syntax error, unexpected {}, expecting \"{}\"",
                    self.describe(),
                    kw
                ),
                self.line(),
            ))
        }
    }

    /// Skip a type declaration (names, |, &, ?, parenthesized DNF).
    pub(in crate::parser) fn skip_type(&mut self) -> Result<(), PhpError> {
        let _ = self.take_type()?;
        Ok(())
    }

    /// Consume a type expression, returning its member names in source
    /// order. `?T`/`T|null` append a "null" member; parentheses flatten.
    pub(in crate::parser) fn take_type(&mut self) -> Result<Option<Vec<String>>, PhpError> {
        let mut members: Vec<String> = Vec::new();
        let mut nullable = false;
        let mut depth = 0i32;
        let mut name = String::new();
        loop {
            match self.peek() {
                Some(Token::Ident(n)) => {
                    // `A&B` continues the intersection conjunct, not a
                    // namespaced path — no `\` separator after `&`.
                    if !name.is_empty() && !name.ends_with('\\') && !name.ends_with('&') {
                        name.push('\\');
                    }
                    name.push_str(n);
                    self.pos += 1;
                }
                Some(Token::Op("\\")) => {
                    name.push('\\');
                    self.pos += 1;
                }
                Some(Token::Op("?")) if depth == 0 => {
                    nullable = true;
                    self.pos += 1;
                }
                Some(Token::Op("|")) => {
                    // `?` may only prefix a single name — `?X|Y` is a
                    // parse error like `?X&Y` (invalid_nullable_type).
                    if nullable && !name.is_empty() && depth == 0 {
                        return Err(PhpError::parse(
                            "syntax error, unexpected token \"|\", expecting \"{\"",
                            self.line(),
                        ));
                    }
                    if !name.is_empty() {
                        members.push(std::mem::take(&mut name));
                    }
                    self.pos += 1;
                }
                Some(Token::Op("&")) => {
                    // `&` is an intersection operator only when a type
                    // name follows; a `&` before `$var` is by-ref
                    // (`int &$p` — typed_properties_010).
                    let next_is_name = matches!(
                        self.toks.get(self.pos + 1).map(|l| &l.token),
                        Some(Token::Ident(_)) | Some(Token::Op("\\")) | Some(Token::Op("("))
                    );
                    if !next_is_name {
                        // `X& #[Attr]` — the attribute token is the
                        // syntax error, not the `&`
                        // (parsing_attribute).
                        if matches!(
                            self.toks.get(self.pos + 1).map(|l| &l.token),
                            Some(Token::Op("#["))
                        ) {
                            return Err(PhpError::parse(
                                "syntax error, unexpected token \"#[\"",
                                self.line(),
                            ));
                        }
                        break;
                    }
                    // `?` may only prefix a single name — `?X&Y`
                    // (invalid_nullable_type).
                    if nullable && !name.is_empty() && depth == 0 {
                        return Err(PhpError::parse(
                            "syntax error, unexpected token \"&\", expecting \"{\"",
                            self.line(),
                        ));
                    }
                    name.push('&');
                    self.pos += 1;
                }
                Some(Token::Op("(")) => {
                    depth += 1;
                    self.pos += 1;
                }
                Some(Token::Op(")")) if depth > 0 => {
                    depth -= 1;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        if !name.is_empty() {
            members.push(name);
        }
        if nullable {
            members.push("null".into());
        }
        let mut members = if members.is_empty() {
            None
        } else {
            // `mixed` already covers every type incl. null: `?mixed` and
            // `mixed|x` are compile errors (mixed_* tests).
            if members.iter().any(|m| m.eq_ignore_ascii_case("mixed")) {
                let line = self.line();
                if nullable {
                    return Err(PhpError::compile_fatal(
                        "Type mixed cannot be marked as nullable since mixed already includes null",
                        line,
                    ));
                }
                if members.len() > 1 {
                    return Err(PhpError::compile_fatal(
                        "Type mixed can only be used as a standalone type",
                        line,
                    ));
                }
            }
            // `void` is likewise standalone-only: `?void`, `void|x`
            // (nullable_void).
            if members.iter().any(|m| m.eq_ignore_ascii_case("void"))
                && (members.len() > 1 || nullable)
            {
                return Err(PhpError::compile_fatal(
                    "Void can only be used as a standalone type",
                    self.line(),
                ));
            }
            // `never` is standalone-only too (never_with_class).
            if members.iter().any(|m| m.eq_ignore_ascii_case("never"))
                && (members.len() > 1 || nullable)
            {
                return Err(PhpError::compile_fatal(
                    "never can only be used as a standalone type",
                    self.line(),
                ));
            }
            // Class-type members resolve against the current namespace /
            // use-aliases at compile time; builtin scalar types do not
            // (namespaces/ns_055). `&`-intersections resolve each part.
            const BUILTIN_TYS: &[&str] = &[
                "int", "float", "string", "bool", "array", "callable", "iterable", "object",
                "mixed", "void", "never", "null", "false", "true", "numeric", "resource", "self",
                "static", "parent",
            ];
            Some(
                members
                    .into_iter()
                    .map(|m| -> Result<String, PhpError> {
                        let (pre, inner, post) = if m.starts_with('(') && m.ends_with(')') {
                            ("(", &m[1..m.len() - 1], ")")
                        } else {
                            ("", m.as_str(), "")
                        };
                        let in_intersection = inner.contains('&');
                        let resolved = inner
                            .split('&')
                            .map(|p| {
                                let t = p.trim_start_matches('\\');
                                let tl = t.to_lowercase();
                                // Confusable builtin-ish class names warn
                                // at compile time — only when the written
                                // name is unqualified AND not imported
                                // (`use integer` suppresses it).
                                // (confusable_type_warning)
                                if !p.contains('\\') && !self.use_map.contains_key(&tl) {
                                    let suppress = if self.cur_ns.is_empty() {
                                        format!("Write \"\\{p}\" to suppress this warning")
                                    } else {
                                        format!(
                                            "Write \"\\{}\\{p}\" or import the class with \"use\" to suppress this warning",
                                            self.cur_ns
                                        )
                                    };
                                    let w = match tl.as_str() {
                                        "integer" => Some(format!("\"{p}\" will be interpreted as a class name. Did you mean \"int\"? {suppress}")),
                                        "double" => Some(format!("\"{p}\" will be interpreted as a class name. Did you mean \"float\"? {suppress}")),
                                        "boolean" => Some(format!("\"{p}\" will be interpreted as a class name. Did you mean \"bool\"? {suppress}")),
                                        "resource" => Some(format!("\"{p}\" is not a supported builtin type and will be interpreted as a class name. {suppress}")),
                                        _ => None,
                                    };
                                    if let Some(m) = w {
                                        self.compile_warnings.push((m, self.line()));
                                    }
                                }
                                // `self`/`static`/`parent` need an
                                // active class scope (self_*/parent_*/
                                // static_*_global_function).
                                match tl.as_str() {
                                    "self" | "static" | "parent"
                                        if self.class_ctx.is_empty() && !self.in_closure =>
                                    {
                                        return Err(PhpError::compile_fatal(
                                            format!(
                                                "Cannot use \"{}\" when no class scope is active",
                                                tl
                                            ),
                                            self.line(),
                                        ));
                                    }
                                    "parent"
                                        if !self.in_closure
                                            && !self.class_ctx.last().map(|c| c.1).unwrap_or(false)
                                            && !self
                                                .class_ctx
                                                .last()
                                                .map(|c| c.0)
                                                .unwrap_or(false) =>
                                    {
                                        return Err(PhpError::compile_fatal(
                                            "Cannot use \"parent\" when current class scope has no parent",
                                            self.line(),
                                        ));
                                    }
                                    _ => {}
                                }
                                if t.is_empty() || BUILTIN_TYS.contains(&tl.as_str()) {
                                    // `self`/`parent` conjuncts must be
                                    // resolvable at compile time — inside
                                    // a trait neither is (relative_*).
                                    if in_intersection
                                        && (tl == "self"
                                            && self
                                                .class_ctx
                                                .last()
                                                .map(|c| c.1)
                                                .unwrap_or(true)
                                            || tl == "parent"
                                                && self
                                                    .class_ctx
                                                    .last()
                                                    .map(|c| c.1 || !c.0)
                                                    .unwrap_or(true))
                                    {
                                        // Zend echoes the written case
                                        // (SELF/PARENT, relative_*2).
                                        return Err(PhpError::compile_fatal(
                                            format!(
                                                "Type {} cannot be part of an intersection type",
                                                t
                                            ),
                                            self.line(),
                                        ));
                                    }
                                    // Intersections accept class types
                                    // only — builtin members error
                                    // (invalid_iterable/static_type).
                                    if in_intersection && tl != "self" && tl != "parent" {
                                        let disp = if tl == "iterable" {
                                            "Traversable|array"
                                        } else {
                                            tl.as_str()
                                        };
                                        return Err(PhpError::compile_fatal(
                                            format!(
                                                "Type {} cannot be part of an intersection type",
                                                disp
                                            ),
                                            self.line(),
                                        ));
                                    }
                                    Ok(t.to_string())
                                } else {
                                    // `const string X` greedy-merge
                                    // artifact: a builtin first segment
                                    // means the `\`-suffix is the const
                                    // name, not a qualified type — keep
                                    // it unresolved for the caller's
                                    // name recovery.
                                    let first_seg = t
                                        .split('\\')
                                        .next()
                                        .unwrap_or("")
                                        .to_lowercase();
                                    const FIRST_T: &[&str] = &[
                                        "int", "float", "string", "bool", "array", "callable",
                                        "iterable", "object", "mixed", "void", "never", "null",
                                        "false", "true", "numeric", "resource",
                                    ];
                                    if t.contains('\\')
                                        && FIRST_T.contains(&first_seg.as_str())
                                    {
                                        return Ok(t.to_string());
                                    }
                                    // Resolve the RAW part — a leading `\`
                                    // marks the name as fully qualified
                                    // (namespaces/ns_055).
                                    let r = self.ns_resolve(p, NsKind::Class);
                                    // `bar\int` — qualified name ending in a
                                    // reserved scalar type (scalar_relative_).
                                    let seg = r
                                        .rsplit('\\')
                                        .next()
                                        .unwrap_or(&r)
                                        .to_lowercase();
                                    const RESERVED_T: &[&str] = &[
                                        "int", "float", "string", "bool", "void", "iterable",
                                        "object", "mixed", "never", "null", "false", "true",
                                    ];
                                    if r.contains('\\') && RESERVED_T.contains(&seg.as_str()) {
                                        return Err(PhpError::compile_fatal(
                                            format!(
                                                "Cannot use \"{}\" as a type name as it is reserved",
                                                r
                                            ),
                                            self.line(),
                                        ));
                                    }
                                    Ok(r)
                                }
                            })
                            .collect::<Result<Vec<_>, _>>()?
                            .join("&");
                        Ok(format!("{pre}{resolved}{post}"))
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            )
        };
        // Zend stores union members canonically: class-like types in
        // declared order first, then builtin scalars in a fixed rank
        // (callable < object < array < string < int < float < bool <
        // false/true < resource < mixed < void < never < null). This
        // order drives both message display and weak coercion
        // preference (union_types/type_checking_*).
        if let Some(ms) = &mut members {
            // `iterable` is the union Traversable|array — Zend expands
            // it in place before sorting (iterable_alias_redundancy_*).
            let expanded: Vec<String> = ms
                .drain(..)
                .flat_map(|m| {
                    if m.eq_ignore_ascii_case("iterable") {
                        vec!["Traversable".to_string(), "array".to_string()]
                    } else {
                        vec![m]
                    }
                })
                .collect();
            *ms = expanded;
            const SCALARS: &[&str] = &[
                "int", "float", "string", "bool", "array", "callable", "iterable", "object",
                "mixed", "void", "never", "null", "false", "true", "numeric", "resource",
            ];
            const RANK: &[&str] = &[
                "callable", "object", "array", "iterable", "string", "int", "float", "bool",
                "false", "true", "resource", "mixed", "void", "never", "null",
            ];
            let rank = |m: &String| -> usize {
                RANK.iter()
                    .position(|r| *r == m.to_lowercase())
                    .unwrap_or(usize::MAX)
            };
            let (mut classes, mut scalars): (Vec<String>, Vec<String>) = ms
                .drain(..)
                .partition(|m| !SCALARS.contains(&m.to_lowercase().as_str()));
            scalars.sort_by_key(rank);
            classes.extend(scalars);
            *ms = classes;
        }
        Ok(members)
    }
}
