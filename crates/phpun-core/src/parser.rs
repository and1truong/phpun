use crate::ast::*;
use crate::error::PhpError;
use crate::lexer::{lex, lex_with, Lexed, Token};
use std::rc::Rc;

pub struct Parser<'a> {
    toks: &'a [Lexed],
    pos: usize,
    /// Compile-time deprecation diagnostics (msg, line) — PHP emits them
    /// before execution; `parse_with` prepends them as `Stmt::Deprecated`.
    deprecations: Vec<(String, usize)>,
    /// Enclosing class name while parsing members (hook error text).
    cur_class: String,
    /// (prop name, is_get) while inside a hook body — gates
    /// `parent::$p::get()/set()` syntax.
    hook_ctx: Option<(String, bool)>,
}

pub fn parse(src: &str) -> Result<Vec<Stmt>, PhpError> {
    parse_with(src, false)
}

/// `parse` honoring `short_open_tag` (INI `short_open_tag=On`).
pub fn parse_with(src: &str, short_open: bool) -> Result<Vec<Stmt>, PhpError> {
    let toks = lex_with(src, short_open)?;
    bracket_check(&toks)?;
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        deprecations: Vec::new(),
        cur_class: String::new(),
        hook_ctx: None,
    };
    let mut stmts = p.program()?;
    for (i, (msg, line)) in std::mem::take(&mut p.deprecations).into_iter().enumerate() {
        stmts.insert(i, Stmt::Deprecated { msg, line });
    }
    Ok(stmts)
}

/// Zend-style bracket-balance pre-pass: mismatched/unclosed/mismatched
/// closers report as `Unclosed 'X'`, `Unmatched 'Y'`,
/// `Unclosed 'X' does not match 'Y'` (syntax_errors).
fn bracket_check(toks: &[crate::lexer::Lexed]) -> Result<(), PhpError> {
    let mut stack: Vec<(&'static str, usize)> = Vec::new();
    let last_line = toks.last().map(|t| t.line).unwrap_or(1);
    for t in toks {
        let Token::Op(op) = &t.token else { continue };
        match *op {
            "(" | "[" | "{" | "#[" => stack.push((if *op == "#[" { "[" } else { op }, t.line)),
            ")" | "]" | "}" => {
                let open = match *op {
                    ")" => "(",
                    "]" => "[",
                    _ => "{",
                };
                match stack.pop() {
                    Some((o, _)) if o == open => {}
                    Some((o, ol)) => {
                        // Multi-line spans name the opener's line.
                        let msg = if ol != t.line {
                            format!("Unclosed '{}' on line {} does not match '{}'", o, ol, op)
                        } else {
                            format!("Unclosed '{}' does not match '{}'", o, op)
                        };
                        return Err(PhpError::parse(msg, t.line));
                    }
                    None => return Err(PhpError::parse(format!("Unmatched '{}'", op), t.line)),
                }
            }
            _ => {}
        }
    }
    if let Some((o, ol)) = stack.pop() {
        let msg = if ol != last_line {
            format!("Unclosed '{}' on line {}", o, ol)
        } else {
            format!("Unclosed '{}'", o)
        };
        return Err(PhpError::parse(msg, last_line));
    }
    Ok(())
}

/// Parse a standalone PHP expression source (used for string interpolation).
pub fn parse_expr_src(src: &str) -> Result<Expr, PhpError> {
    let wrapped = format!("<?php {};", src);
    let toks = lex(&wrapped)?;
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        deprecations: Vec::new(),
        cur_class: String::new(),
        hook_ctx: None,
    };
    let e = p.expr()?;
    Ok(e)
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos).map(|l| &l.token)
    }

    fn peek2(&self) -> Option<&Token> {
        self.toks.get(self.pos + 1).map(|l| &l.token)
    }

    fn line(&self) -> usize {
        self.toks
            .get(self.pos)
            .map(|l| l.line)
            .unwrap_or_else(|| self.toks.last().map(|l| l.line).unwrap_or(1))
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.peek().cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn at_op(&self, op: &str) -> bool {
        matches!(self.peek(), Some(Token::Op(o)) if *o == op)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if self.at_op(op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, op: &str) -> Result<(), PhpError> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(PhpError::parse(
                format!(
                    "syntax error, unexpected {}, expecting \"{}\"",
                    self.describe(),
                    op
                ),
                self.line(),
            ))
        }
    }

    fn describe(&self) -> String {
        match self.peek() {
            None => "end of file".to_string(),
            Some(Token::Ident(s)) => format!("identifier \"{}\"", s),
            Some(Token::Variable(s)) => format!("variable \"${}\"", s),
            Some(Token::Int(v)) => format!("integer {}", v),
            Some(Token::Float(v)) => format!("float {}", v),
            Some(Token::Op(o)) => format!("token \"{}\"", o),
            Some(_) => "token".to_string(),
        }
    }

    fn ident(&mut self) -> Option<String> {
        if let Some(Token::Ident(s)) = self.peek() {
            let s = s.clone();
            self.pos += 1;
            Some(s)
        } else {
            None
        }
    }

    fn ident_is(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    fn eat_ident(&mut self, kw: &str) -> bool {
        if self.ident_is(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub fn program(&mut self) -> Result<Vec<Stmt>, PhpError> {
        let mut stmts = Vec::new();
        while self.peek().is_some() {
            stmts.push(Stmt::Line(self.line()));
            stmts.push(self.stmt()?);
        }
        Ok(stmts)
    }

    /// A `{ ... }` block or a single statement body.
    fn body(&mut self) -> Result<Vec<Stmt>, PhpError> {
        if self.eat_op("{") {
            let mut v = Vec::new();
            while !self.eat_op("}") {
                if self.peek().is_none() {
                    return Err(PhpError::parse(
                        "syntax error, unexpected end of file",
                        self.line(),
                    ));
                }
                v.push(Stmt::Line(self.line()));
                v.push(self.stmt()?);
            }
            Ok(v)
        } else {
            let l = self.line();
            Ok(vec![Stmt::Line(l), self.stmt()?])
        }
    }

    /// Like `body()` but also accepts PHP's `:` alternative syntax:
    /// `: stmts end<kw>;` (tests/lang/008, 028, 033).
    fn body_any(&mut self, end: &str) -> Result<Vec<Stmt>, PhpError> {
        if self.eat_op(":") {
            let v = self.body_until(&[end])?;
            self.pos += 1; // end<kw>
            self.eat_op(";");
            return Ok(v);
        }
        self.body()
    }

    /// Statements up to (not consuming) any terminator keyword — the
    /// body of a `:` alternative-syntax block.
    fn body_until(&mut self, stops: &[&str]) -> Result<Vec<Stmt>, PhpError> {
        let mut v = Vec::new();
        loop {
            if stops.iter().any(|s| self.ident_is(s)) {
                break;
            }
            if self.peek().is_none() {
                return Err(PhpError::parse(
                    "syntax error, unexpected end of file",
                    self.line(),
                ));
            }
            v.push(Stmt::Line(self.line()));
            v.push(self.stmt()?);
        }
        Ok(v)
    }

    fn stmt(&mut self) -> Result<Stmt, PhpError> {
        // `#[Attr]` may precede any declaration statement.
        if self.at_op("#[") {
            self.skip_attrs();
        }
        match self.peek().cloned() {
            Some(Token::Inline(s)) => {
                self.pos += 1;
                Ok(Stmt::Inline(s))
            }
            Some(Token::Echo) => {
                self.pos += 1;
                let args = self.expr_list()?;
                self.eat_op(";");
                Ok(Stmt::Echo(args))
            }
            Some(Token::Ident(_)) => {
                if self.ident_is("echo") {
                    self.pos += 1;
                    let args = self.expr_list()?;
                    self.expect_op(";")?;
                    Ok(Stmt::Echo(args))
                } else if self.ident_is("if") {
                    self.if_stmt()
                } else if self.ident_is("while") {
                    self.pos += 1;
                    self.expect_op("(")?;
                    let cond = self.expr()?;
                    self.expect_op(")")?;
                    let body = self.body_any("endwhile")?;
                    Ok(Stmt::While { cond, body })
                } else if self.ident_is("do") {
                    self.pos += 1;
                    let body = self.body()?;
                    if !self.eat_ident("while") {
                        return Err(PhpError::parse(
                            "syntax error, unexpected end of statement, expecting \"while\"",
                            self.line(),
                        ));
                    }
                    self.expect_op("(")?;
                    let cond = self.expr()?;
                    self.expect_op(")")?;
                    self.expect_op(";")?;
                    Ok(Stmt::DoWhile { body, cond })
                } else if self.ident_is("for") {
                    self.for_stmt()
                } else if self.ident_is("function") {
                    self.function_decl()
                } else if self.ident_is("return") {
                    self.pos += 1;
                    if self.at_op(";") {
                        self.pos += 1;
                        Ok(Stmt::Return(None))
                    } else {
                        let e = self.expr()?;
                        self.expect_op(";")?;
                        Ok(Stmt::Return(Some(e)))
                    }
                } else if self.ident_is("break") || self.ident_is("continue") {
                    let is_break = self.ident_is("break");
                    self.pos += 1;
                    let arg = if self.at_op(";") {
                        None
                    } else {
                        Some(self.expr()?)
                    };
                    self.expect_op(";")?;
                    Ok(if is_break {
                        Stmt::Break(arg)
                    } else {
                        Stmt::Continue(arg)
                    })
                } else if self.ident_is("global") {
                    self.pos += 1;
                    let mut names = Vec::new();
                    loop {
                        match self.peek().cloned() {
                            Some(Token::Variable(_)) | Some(Token::Op("$")) => {
                                names.push(self.expr()?);
                            }
                            _ => {
                                return Err(PhpError::parse(
                                    "syntax error, unexpected token, expecting variable",
                                    self.line(),
                                ))
                            }
                        }
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    self.expect_op(";")?;
                    Ok(Stmt::Global(names))
                } else if self.ident_is("static")
                    && matches!(self.peek2(), Some(Token::Variable(_)))
                {
                    self.static_stmt()
                } else if self.ident_is("switch") {
                    self.switch_stmt()
                } else if self.ident_is("foreach") {
                    self.foreach_stmt()
                } else if self.ident_is("unset") {
                    self.pos += 1;
                    self.expect_op("(")?;
                    let mut xs = Vec::new();
                    while !self.at_op(")") {
                        xs.push(self.expr()?);
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                    self.expect_op(";")?;
                    Ok(Stmt::Unset(xs))
                } else if self.ident_is("try") {
                    self.try_stmt()
                } else if self.ident_is("throw") {
                    self.pos += 1;
                    let e = self.expr()?;
                    self.expect_op(";")?;
                    Ok(Stmt::Expr(Expr::Throw(Box::new(e))))
                } else if self.ident_is("declare") {
                    self.declare_stmt()
                } else if self.ident_is("namespace") {
                    self.pos += 1;
                    let name = self.name_path().unwrap_or_default();
                    if self.eat_op("{") {
                        // `namespace Foo { ... }` — body parsed inline.
                        let mut v = Vec::new();
                        while !self.eat_op("}") {
                            if self.peek().is_none() {
                                return Err(PhpError::parse(
                                    "syntax error, unexpected end of file",
                                    self.line(),
                                ));
                            }
                            v.push(self.stmt()?);
                        }
                        return Ok(Stmt::Block(vec![Stmt::Namespace(name), Stmt::Block(v)]));
                    }
                    self.expect_op(";")?;
                    Ok(Stmt::Namespace(name))
                } else if self.ident_is("class")
                    || self.ident_is("interface")
                    || self.ident_is("trait")
                    || self.ident_is("enum")
                    || ((self.ident_is("abstract")
                        || self.ident_is("final")
                        || self.ident_is("readonly"))
                        && matches!(self.peek2(), Some(Token::Ident(k)) if k.eq_ignore_ascii_case("class")))
                {
                    self.class_decl()
                } else if self.ident_is("use")
                    && matches!(self.peek2(), Some(Token::Ident(_)) | Some(Token::Op("\\")))
                {
                    self.use_stmt()
                } else {
                    self.expr_stmt()
                }
            }
            Some(Token::Op("{")) => {
                self.pos += 1;
                let mut v = Vec::new();
                while !self.eat_op("}") {
                    if self.peek().is_none() {
                        return Err(PhpError::parse(
                            "syntax error, unexpected end of file",
                            self.line(),
                        ));
                    }
                    v.push(self.stmt()?);
                }
                Ok(Stmt::Block(v))
            }
            Some(Token::Op(";")) => {
                self.pos += 1;
                Ok(Stmt::Expr(Expr::Null))
            }
            Some(_) => self.expr_stmt(),
            None => Err(PhpError::parse(
                "syntax error, unexpected end of file",
                self.line(),
            )),
        }
    }

    fn expr_stmt(&mut self) -> Result<Stmt, PhpError> {
        let e = self.expr()?;
        self.expect_op(";")?;
        Ok(Stmt::Expr(e))
    }

    /// `static $a = 1, $b;` — persistent function-local vars.
    fn static_stmt(&mut self) -> Result<Stmt, PhpError> {
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

    fn switch_stmt(&mut self) -> Result<Stmt, PhpError> {
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

    fn foreach_stmt(&mut self) -> Result<Stmt, PhpError> {
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

    fn foreach_target(&mut self) -> Result<ForeachTarget, PhpError> {
        if self.eat_op("&") {
            return match self.next() {
                Some(Token::Variable(n)) => Ok(ForeachTarget::ByRef(n)),
                t => Err(PhpError::parse(
                    format!(
                        "syntax error, unexpected {}, expecting variable",
                        desc_t(t.as_ref())
                    ),
                    self.line(),
                )),
            };
        }
        if self.at_op("[") {
            self.pos += 1;
            let mut items = Vec::new();
            while !self.at_op("]") {
                if self.eat_op(",") {
                    items.push(None);
                    continue;
                }
                items.push(Some(self.foreach_target()?));
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
                items.push(Some(self.foreach_target()?));
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op(")")?;
            return Ok(ForeachTarget::List(items));
        }
        match self.next() {
            Some(Token::Variable(n)) => {
                // Lvalue targets: `$b[0]`, `$o->p`, ...
                let mut e = Expr::Var(n);
                while self.at_op("[") {
                    self.pos += 1;
                    let i = if self.at_op("]") {
                        None
                    } else {
                        Some(Box::new(self.expr()?))
                    };
                    self.expect_op("]")?;
                    e = Expr::Index { e: Box::new(e), i };
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

    fn try_stmt(&mut self) -> Result<Stmt, PhpError> {
        self.pos += 1; // try
        let body = self.body()?;
        let mut catches = Vec::new();
        while self.ident_is("catch") {
            self.pos += 1;
            self.expect_op("(")?;
            let mut types = Vec::new();
            loop {
                if let Some(n) = self.name_path() {
                    types.push(n);
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

    fn declare_stmt(&mut self) -> Result<Stmt, PhpError> {
        self.pos += 1; // declare
        self.expect_op("(")?;
        let name = self.ident().unwrap_or_default();
        self.expect_op("=")?;
        let value = self.expr()?;
        self.expect_op(")")?;
        let decl = Stmt::Declare { name, value };
        if self.eat_op(";") {
            Ok(decl)
        } else {
            // `declare(...) { }` / `declare(...):` block forms.
            let body = self.body_any("enddeclare")?;
            Ok(Stmt::Block(vec![decl, Stmt::Block(body)]))
        }
    }

    /// Top-level `use A\B, C as D;` (namespace import). Names collected but
    /// aliasing is not applied yet (no namespace support).
    fn use_stmt(&mut self) -> Result<Stmt, PhpError> {
        self.pos += 1; // use
        if self.ident_is("function") || self.ident_is("const") {
            self.pos += 1;
        }
        let mut names = Vec::new();
        loop {
            if let Some(n) = self.name_path() {
                names.push(n);
            }
            if self.ident_is("as") {
                self.pos += 1;
                self.ident();
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(";")?;
        Ok(Stmt::Use(names))
    }

    /// `Foo\Bar\Baz` — backslash-joined qualified name.
    fn name_path(&mut self) -> Option<String> {
        let mut parts = Vec::new();
        // leading \ for FQ names
        let lead = self.eat_op("\\");
        while matches!(self.peek(), Some(Token::Ident(_))) {
            parts.push(self.ident().unwrap());
            if !self.eat_op("\\") {
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

    /// Skip `#[Attr(...)]` groups (attributes are parsed but discarded).
    fn skip_attrs(&mut self) {
        while self.eat_op("#[") {
            let mut depth = 1i32;
            while depth > 0 {
                match self.next() {
                    Some(Token::Op("[")) | Some(Token::Op("#[")) => depth += 1,
                    Some(Token::Op("]")) => depth -= 1,
                    Some(_) => {}
                    None => return,
                }
            }
        }
    }

    fn class_decl(&mut self) -> Result<Stmt, PhpError> {
        // `#[Attr]` groups may precede the class modifiers.
        self.skip_attrs();
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
        self.cur_class = name.clone();
        // enum backing type `enum X: int`
        if self.eat_op(":") {
            self.skip_type()?;
        }
        let mut parent = None;
        let mut implements = Vec::new();
        if self.eat_ident("extends") {
            if kind == ClassKind::Interface {
                // `interface Y extends X, Z` — multiple interface parents
                // recorded in `implements` (what instanceof/iface walks use).
                while let Some(n) = self.name_path() {
                    implements.push(n);
                    if !self.eat_op(",") {
                        break;
                    }
                }
            } else {
                parent = self.name_path();
            }
        }
        if self.eat_ident("implements") {
            while let Some(n) = self.name_path() {
                implements.push(n);
                if !self.eat_op(",") {
                    break;
                }
            }
        }
        self.expect_op("{")?;
        let mut methods = Vec::new();
        let mut props = Vec::new();
        let mut consts = Vec::new();
        let mut traits = Vec::new();
        while !self.at_op("}") {
            if self.peek().is_none() {
                return Err(PhpError::parse(
                    "syntax error, unexpected end of file",
                    self.line(),
                ));
            }
            self.skip_attrs();
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
                loop {
                    let cname = self.ident().unwrap_or_default();
                    self.expect_op("=")?;
                    let cv = self.expr()?;
                    consts.push((cname, cv));
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
                    // Trait adaptations (`insteadof`/`as`) — skip the block.
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
                // enum cases
                self.pos += 1;
                let cname = self.ident().unwrap_or_default();
                let cv = if self.eat_op("=") {
                    self.expr()?
                } else {
                    Expr::Null
                };
                consts.push((cname, cv));
                self.expect_op(";")?;
                continue;
            }
            // Typed or untyped property: [type] $name [= default], ...;
            let pline = self.line();
            let pty = if matches!(self.peek(), Some(Token::Ident(_)) | Some(Token::Op("?")))
                && !matches!(self.peek2(), Some(Token::Op("(")))
            {
                self.take_type()
            } else {
                None
            };
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
        Ok(Stmt::Class(Rc::new(ClassDecl {
            name,
            kind,
            is_abstract,
            is_final,
            parent,
            implements,
            traits,
            methods,
            props,
            consts,
        })))
    }

    fn method_decl(
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
        if self.eat_op(":") {
            self.skip_type()?;
        }
        let body = if self.eat_op(";") {
            Vec::new()
        } else {
            self.body()?
        };
        self.hook_ctx = prev_hook;
        Ok(MethodDecl {
            decl: FunctionDecl {
                name,
                params,
                body,
                by_ref,
                line,
                file: String::new(),
            },
            is_static,
            is_abstract,
            is_final,
            visibility: vis,
        })
    }

    /// `{ get => e; set { .. }; set(T $v) { .. }; get; }` — PHP 8.4
    /// property hooks (Zend/tests/property_hooks). Called with the `{`
    /// already detected; consumes through the closing `}`.
    fn prop_hooks(&mut self, pname: &str) -> Result<Option<Vec<PropHook>>, PhpError> {
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
            self.skip_attrs();
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
    fn at_asym_set(&mut self) -> bool {
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

    fn params(&mut self) -> Result<Vec<Param>, PhpError> {
        self.expect_op("(")?;
        let mut params = Vec::new();
        while !self.at_op(")") {
            self.skip_attrs();
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
            // skip type declaration before the variable
            let ty = if matches!(
                self.peek(),
                Some(Token::Ident(_)) | Some(Token::Op("?")) | Some(Token::Op("\\"))
            ) && !matches!(self.peek2(), Some(Token::Op(",")) | Some(Token::Op(")")))
            {
                self.take_type()
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

    fn if_stmt(&mut self) -> Result<Stmt, PhpError> {
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

    fn for_stmt(&mut self) -> Result<Stmt, PhpError> {
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

    fn function_decl(&mut self) -> Result<Stmt, PhpError> {
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
        // Return type declarations (: int) — parse & ignore for now.
        if self.eat_op(":") {
            self.skip_type()?;
        }
        let body = self.body()?;
        self.hook_ctx = prev_hook;
        Ok(Stmt::Function(FunctionDecl {
            name,
            params,
            body,
            by_ref,
            line,
            file: String::new(),
        }))
    }

    /// `function (&$a) use ($x, &$y) { }`, `static function () {}`,
    /// `fn($x) => $x + 1`.
    fn closure_expr(&mut self) -> Result<Expr, PhpError> {
        let line = self.line();
        let mut arrow = false;
        let mut uses = Vec::new();
        if self.ident_is("static") {
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
        }
        let body = if arrow {
            self.expect_op("=>")?;
            let e = self.expr()?;
            vec![Stmt::Return(Some(e))]
        } else {
            if self.eat_op(":") {
                self.skip_type()?;
            }
            self.body()?
        };
        self.hook_ctx = prev_hook;
        Ok(Expr::Closure(ClosureExpr {
            decl: FunctionDecl {
                name: String::new(),
                params,
                body,
                by_ref,
                line,
                file: String::new(),
            },
            uses,
            arrow,
        }))
    }

    /// The class operand of `new`: name path, `self`/`static`/`parent`,
    /// `$var`, `{expr}`, or anonymous `class { ... }`. Returns (class expr,
    /// ctor args) — anonymous classes take their ctor args before the body.
    fn new_class_expr(&mut self) -> Result<(Expr, Vec<Expr>), PhpError> {
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
            self.skip_attrs();
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
                self.skip_attrs();
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
                        consts.push((cname, self.expr()?));
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
                    consts.push((cname, cv));
                    self.expect_op(";")?;
                    continue;
                }
                let pline = self.line();
                let pty = if matches!(self.peek(), Some(Token::Ident(_)) | Some(Token::Op("?")))
                    && !matches!(self.peek2(), Some(Token::Op("(")))
                {
                    self.take_type()
                } else {
                    None
                };
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
            return Ok((
                Expr::AnonClass(Rc::new(ClassDecl {
                    name: format!("class@anonymous${}", self.line()),
                    kind: ClassKind::Class,
                    is_abstract: false,
                    is_final: false,
                    parent,
                    implements,
                    traits,
                    methods,
                    props,
                    consts,
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
                Ok((Expr::Const(n), Vec::new()))
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

    fn match_expr(&mut self) -> Result<Expr, PhpError> {
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

    fn expect_ident(&mut self, kw: &str) -> Result<(), PhpError> {
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
    fn skip_type(&mut self) -> Result<(), PhpError> {
        let _ = self.take_type();
        Ok(())
    }

    /// Consume a type expression, returning its member names in source
    /// order. `?T`/`T|null` append a "null" member; parentheses flatten.
    fn take_type(&mut self) -> Option<Vec<String>> {
        let mut members: Vec<String> = Vec::new();
        let mut nullable = false;
        let mut depth = 0i32;
        let mut name = String::new();
        loop {
            match self.peek() {
                Some(Token::Ident(n)) => {
                    if !name.is_empty() {
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
                Some(Token::Op("|")) | Some(Token::Op("&")) => {
                    if !name.is_empty() {
                        members.push(std::mem::take(&mut name));
                    }
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
        if nullable && !members.iter().any(|m| m.eq_ignore_ascii_case("null")) {
            members.push("null".into());
        }
        if members.is_empty() {
            None
        } else {
            Some(members)
        }
    }

    fn expr_list(&mut self) -> Result<Vec<Expr>, PhpError> {
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

    fn assign(&mut self) -> Result<Expr, PhpError> {
        let e = self.ternary()?;
        const ASSIGN_OPS: &[&str] = &[
            "=", "+=", "-=", "*=", "/=", ".=", "%=", "&=", "|=", "^=", "<<=", ">>=", "**=", "??=",
        ];
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
    fn list_target(&mut self, e: Expr) -> Result<Expr, PhpError> {
        match e {
            Expr::ArrayLit(items) => Ok(Expr::List(
                items.into_iter().map(|(_, v)| Some(v)).collect(),
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

    fn ternary(&mut self) -> Result<Expr, PhpError> {
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

    fn logical_or(&mut self) -> Result<Expr, PhpError> {
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

    fn logical_and(&mut self) -> Result<Expr, PhpError> {
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

    fn equality(&mut self) -> Result<Expr, PhpError> {
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

    fn comparison(&mut self) -> Result<Expr, PhpError> {
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
    fn concat(&mut self) -> Result<Expr, PhpError> {
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

    fn bit_or(&mut self) -> Result<Expr, PhpError> {
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

    fn bit_xor(&mut self) -> Result<Expr, PhpError> {
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

    fn bit_and(&mut self) -> Result<Expr, PhpError> {
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

    fn shift(&mut self) -> Result<Expr, PhpError> {
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

    fn additive(&mut self) -> Result<Expr, PhpError> {
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

    fn term(&mut self) -> Result<Expr, PhpError> {
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
    fn power(&mut self) -> Result<Expr, PhpError> {
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

    fn unary(&mut self) -> Result<Expr, PhpError> {
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
            let c = self.unary()?;
            e = Expr::Instanceof {
                obj: Box::new(e),
                class: Box::new(c),
            };
        }
        Ok(e)
    }

    fn postfix(&mut self) -> Result<Expr, PhpError> {
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
                e = Expr::Call {
                    name: Box::new(e),
                    args,
                };
            } else if self.at_op("->") || self.at_op("?->") {
                let nullsafe = self.at_op("?->");
                self.pos += 1;
                let name = self.prop_name()?;
                if self.at_op("(") {
                    self.pos += 1;
                    let args = self.args()?;
                    e = Expr::MethodCall {
                        obj: Box::new(e),
                        name,
                        args,
                        nullsafe,
                    };
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
                            e = Expr::StaticCall {
                                class: Box::new(e),
                                name: n,
                                args,
                            };
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
                            e = Expr::StaticCallDyn {
                                class: Box::new(e),
                                name: Box::new(Expr::Var(n)),
                                args,
                            };
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

    fn args(&mut self) -> Result<Vec<Expr>, PhpError> {
        let mut args = Vec::new();
        while !self.at_op(")") {
            // named arguments `name:` — name recorded via Str marker
            if matches!(self.peek(), Some(Token::Ident(_)))
                && matches!(self.peek2(), Some(Token::Op(":")))
            {
                let n = self.ident().unwrap();
                self.pos += 1; // :
                let v = self.expr()?;
                args.push(Expr::Binary {
                    op: "named",
                    l: Box::new(Expr::Str(n)),
                    r: Box::new(v),
                });
            } else {
                args.push(self.expr()?);
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        Ok(args)
    }

    fn prop_name(&mut self) -> Result<PropName, PhpError> {
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

    fn primary(&mut self) -> Result<Expr, PhpError> {
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
                    || self.ident_is("fn")
                {
                    self.closure_expr()
                } else if self.ident_is("new") {
                    self.pos += 1;
                    let (class, mut ctor_args) = self.new_class_expr()?;
                    if self.at_op("(") {
                        self.pos += 1;
                        ctor_args = self.args()?;
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
                } else if self.ident_is("__namespace__") {
                    self.pos += 1;
                    Ok(Expr::MagicConst(MagicConst::Namespace))
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
                    // names (\A\B) and function call args go through here.
                    let start = self.pos;
                    let name = if self.at_op("\\") {
                        self.name_path().unwrap_or_default()
                    } else {
                        self.ident().unwrap()
                    };
                    let name = name.trim_start_matches('\\').to_string();
                    if self.at_op("(") {
                        self.pos += 1;
                        let args = self.args()?;
                        Ok(Expr::Call {
                            name: Box::new(Expr::Str(name)),
                            args,
                        })
                    } else if self.at_op("::") {
                        // reset: `X::` handled by postfix on Const
                        self.pos = start;
                        let name = self.ident().unwrap();
                        Ok(Expr::Const(name))
                    } else {
                        // Unqualified constant (e.g. PHP_EOL) or undefined constant.
                        Ok(Expr::Const(name))
                    }
                }
            }
            Some(Token::Op("\\")) => {
                // Fully-qualified name: \PHP_EOL, \Foo\Bar::baz, \func().
                let name = self
                    .name_path()
                    .unwrap_or_default()
                    .trim_start_matches('\\')
                    .to_string();
                if name.is_empty() {
                    return Err(PhpError::parse(
                        "syntax error, unexpected token \"\\\"",
                        self.line(),
                    ));
                }
                if self.at_op("(") {
                    self.pos += 1;
                    let args = self.args()?;
                    Ok(Expr::Call {
                        name: Box::new(Expr::Str(name)),
                        args,
                    })
                } else {
                    Ok(Expr::Const(name))
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

    fn array_items(&mut self, close: &str) -> Result<Vec<(Option<Expr>, Expr)>, PhpError> {
        let mut items = Vec::new();
        while !self.at_op(close) {
            let first = self.array_elem()?;
            if self.eat_op("=>") {
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

    /// One array-literal element — may be `&expr` (bound by reference).
    fn array_elem(&mut self) -> Result<Expr, PhpError> {
        if self.eat_op("&") {
            Ok(Expr::ByRef(Box::new(self.expr()?)))
        } else {
            self.expr()
        }
    }
}

fn desc_t(t: Option<&Token>) -> String {
    match t {
        None => "end of file".to_string(),
        Some(Token::Ident(s)) => format!("identifier \"{}\"", s),
        Some(Token::Variable(s)) => format!("variable \"${}\"", s),
        Some(Token::Op(o)) => format!("token \"{}\"", o),
        Some(Token::Int(v)) => format!("integer {}", v),
        Some(Token::Float(v)) => format!("float {}", v),
        _ => "token".to_string(),
    }
}
