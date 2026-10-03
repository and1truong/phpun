use crate::ast::*;
use crate::error::PhpError;
use crate::lexer::{lex, lex_with, Lexed, Token};
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum NsKind {
    Class,
    Func,
    Const,
}

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
    /// `#[Attr]` groups parsed before a declaration statement
    /// (class or function — whichever consumes them first).
    pending_class_attrs: Vec<crate::ast::AttrDecl>,
    /// Current `namespace` name ("" = global scope).
    cur_ns: String,
    /// `use` import maps for the current namespace block, keyed by
    /// alias — lowercase for classes/functions, exact for constants.
    use_map: std::collections::HashMap<String, String>,
    use_fn_map: std::collections::HashMap<String, String>,
    use_const_map: std::collections::HashMap<String, String>,
    /// Short names (lowercase) of classes/interfaces/traits/enums
    /// declared in this file — a `use` alias colliding with one is a
    /// compile-time fatal (namespaces/ns_030).
    declared_types: std::collections::HashSet<String>,
    /// Inside a `namespace X { ... }` body — nested `namespace`
    /// declarations are a compile error (namespaces/ns_079).
    in_braced_ns: bool,
    /// Braced vs unbraced namespace declarations in this file —
    /// 0 none, 1 unbraced, 2 braced (mixing is a compile fatal,
    /// namespaces/ns_081/ns_084).
    ns_style: u8,
}

pub fn parse(src: &str) -> Result<Vec<Stmt>, PhpError> {
    parse_with(src, false)
}

/// `parse` honoring `short_open_tag` (INI `short_open_tag=On`).
pub fn parse_with(src: &str, short_open: bool) -> Result<Vec<Stmt>, PhpError> {
    let toks = lex_with(src, short_open)?;
    parse_toks(toks)
}

/// phpun source mode: PHP code from byte 0, no `<?php` required (a
/// leading tag falls back to classic tag mode for legacy sources).
/// If pure-source parsing fails and the source contains a `<?` tag
/// anywhere, the legacy tag-mode parse is tried so HTML-embedded PHP
/// keeps working; the pure-mode error is preferred if both fail.
pub fn parse_source(src: &str, short_open: bool) -> Result<Vec<Stmt>, PhpError> {
    parse_pure(src, short_open).or_else(|e| {
        if src.contains("<?") {
            if let Ok(stmts) = parse_with(src, short_open) {
                return Ok(stmts);
            }
        }
        Err(e)
    })
}

/// Strict pure-source parse — no tag-mode detection or retry. Used for
/// eval()'d code, which in PHP is always tag-free source.
pub fn parse_pure(src: &str, short_open: bool) -> Result<Vec<Stmt>, PhpError> {
    let toks = crate::lexer::lex_php_source(src, short_open)?;
    parse_toks(toks)
}

fn parse_toks(toks: Vec<Lexed>) -> Result<Vec<Stmt>, PhpError> {
    // Compile-time diagnostics ride the token stream; drain them and
    // emit before execution (Zend emits compile warnings upfront).
    let mut lex_diags: Vec<(String, &'static str, usize)> = Vec::new();
    let toks: Vec<Lexed> = toks
        .into_iter()
        .filter_map(|t| match t.token {
            Token::Diag(level, msg) => {
                lex_diags.push((msg, level, t.line));
                None
            }
            _ => Some(t),
        })
        .collect();
    bracket_check(&toks)?;
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        deprecations: Vec::new(),
        cur_class: String::new(),
        hook_ctx: None,
        pending_class_attrs: Vec::new(),
        cur_ns: String::new(),
        use_map: std::collections::HashMap::new(),
        use_fn_map: std::collections::HashMap::new(),
        use_const_map: std::collections::HashMap::new(),
        declared_types: std::collections::HashSet::new(),
        in_braced_ns: false,
        ns_style: 0,
    };
    let mut stmts = p.program()?;
    let mut diags: Vec<(String, &'static str, usize)> = lex_diags;
    diags.extend(
        std::mem::take(&mut p.deprecations)
            .into_iter()
            .map(|(msg, line)| (msg, "Deprecated", line)),
    );
    diags.sort_by_key(|(_, _, line)| *line);
    for (i, (msg, level, line)) in diags.into_iter().enumerate() {
        stmts.insert(i, Stmt::Diag { level, msg, line });
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

/// A compile-time diagnostic produced while re-lexing an embedded
/// source: (level, message, line).
pub type SrcDiags = Vec<(&'static str, String, usize)>;

/// Parse a standalone PHP expression source (used for string
/// interpolation). Diagnostics produced while re-lexing the embedded
/// source (e.g. octal overflow inside `${"\400"}`) come back in the
/// second tuple element so the evaluator can print them inline.
pub fn parse_expr_src(src: &str) -> Result<(Expr, SrcDiags), PhpError> {
    let wrapped = format!("<?php {};", src);
    let toks = lex(&wrapped)?;
    let mut diags = Vec::new();
    let toks: Vec<Lexed> = toks
        .into_iter()
        .filter_map(|t| match t.token {
            Token::Diag(level, msg) => {
                diags.push((level, msg, t.line));
                None
            }
            _ => Some(t),
        })
        .collect();
    let mut p = Parser {
        toks: &toks,
        pos: 0,
        deprecations: Vec::new(),
        cur_class: String::new(),
        hook_ctx: None,
        pending_class_attrs: Vec::new(),
        cur_ns: String::new(),
        use_map: std::collections::HashMap::new(),
        use_fn_map: std::collections::HashMap::new(),
        use_const_map: std::collections::HashMap::new(),
        declared_types: std::collections::HashSet::new(),
        in_braced_ns: false,
        ns_style: 0,
    };
    let e = p.expr()?;
    Ok((e, diags))
}

const ASSIGN_OPS: &[&str] = &[
    "=", "+=", "-=", "*=", "/=", ".=", "%=", "&=", "|=", "^=", "<<=", ">>=", "**=", "??=",
];

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
            // Zend reports a lone unexpected `\` name separator without an
            // "expecting" clause (namespaced_name_whitespace).
            let msg = if op == ";" && matches!(self.peek(), Some(Token::Op("\\"))) {
                format!("syntax error, unexpected {}", self.describe())
            } else {
                format!(
                    "syntax error, unexpected {}, expecting \"{}\"",
                    self.describe(),
                    op
                )
            };
            Err(PhpError::parse(msg, self.line()))
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
        let mut saw_code = false;
        let mut saw_ns = false;
        // Everything after `__HALT_COMPILER()` is ignored entirely —
        // the lexer stops there (namespaces/ns_080).
        let mut halted = false;
        while self.peek().is_some() {
            let stmt_line = self.line();
            stmts.push(Stmt::Line(stmt_line));
            let s = self.stmt()?;
            let is_ns = matches!(&s, Stmt::Namespace(_))
                || matches!(&s, Stmt::Block(v) if matches!(v.first(), Some(Stmt::Namespace(_))));
            // The first `namespace` declaration must precede all code
            // (only `declare` may come earlier); later `namespace`
            // declarations may follow code (namespaces/ns_068).
            if halted {
                // unreachable (guarded above); keeps the flow explicit.
            }
            if is_ns && !saw_ns && saw_code {
                return Err(PhpError::fatal(
                    "Namespace declaration statement has to be the very first statement or after any declare call in the script".to_string(),
                    self.line(),
                ));
            }
            // Once a braced `namespace {}` is used, every later stmt
            // must itself be inside a namespace block (ns_087).
            // `__HALT_COMPILER()` is allowed outside even in braced-ns
            // files (namespaces/ns_080).
            let is_halt = matches!(&s,
                Stmt::Expr(Expr::Call { name, .. })
                    if matches!(&**name, Expr::Str(n) if n.trim_start_matches('\u{1}').eq_ignore_ascii_case("__halt_compiler")));
            if halted {
                stmts.push(s);
                continue;
            }
            if is_halt {
                halted = true;
            }
            if !is_ns
                && self.ns_style == 2
                && !is_halt
                && !matches!(&s, Stmt::Declare { .. })
                && !matches!(&s, Stmt::Expr(Expr::Null))
            {
                return Err(PhpError::fatal(
                    "No code may exist outside of namespace {}".to_string(),
                    stmt_line,
                ));
            }
            saw_ns = saw_ns || is_ns;
            // A lone `;` (Stmt::Expr(Expr::Null)) is not "code" either
            // (namespaces/namespace_first_stmt_nop).
            if !is_ns
                && !matches!(&s, Stmt::Declare { .. })
                && !matches!(&s, Stmt::Expr(Expr::Null))
            {
                saw_code = true;
            }
            stmts.push(s);
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
            self.pending_class_attrs = self.parse_attrs()?;
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
                    let name = self
                        .name_path()
                        .unwrap_or_default()
                        .trim_start_matches('\\')
                        .to_string();
                    // `namespace` is reserved as a name: bare
                    // `namespace namespace;` is a fatal, while
                    // `namespace namespace\x` reads as a stray
                    // ns-relative name (namespace_name_namespace*).
                    if name
                        .split('\\')
                        .next()
                        .is_some_and(|seg| seg.eq_ignore_ascii_case("namespace"))
                    {
                        if name.contains('\\') {
                            return Err(PhpError::parse(
                                format!(
                                    "syntax error, unexpected namespace-relative name \"{}\", expecting \"{{\"",
                                    name
                                ),
                                self.line(),
                            ));
                        }
                        return Err(PhpError::fatal(
                            format!("Cannot use '{}' as namespace name", name),
                            self.line(),
                        ));
                    }
                    self.cur_ns = name.clone();
                    self.use_map.clear();
                    self.use_fn_map.clear();
                    self.use_const_map.clear();
                    self.declared_types.clear();
                    let braced = self.at_op("{");
                    if self.in_braced_ns {
                        return Err(PhpError::fatal(
                            if braced {
                                "Namespace declarations cannot be nested".to_string()
                            } else {
                                "Cannot mix bracketed namespace declarations with unbracketed namespace declarations".to_string()
                            },
                            self.line(),
                        ));
                    }
                    let style = if braced { 2 } else { 1 };
                    if self.ns_style != 0 && self.ns_style != style {
                        return Err(PhpError::fatal(
                            "Cannot mix bracketed namespace declarations with unbracketed namespace declarations".to_string(),
                            self.line(),
                        ));
                    }
                    self.ns_style = style;
                    if self.eat_op("{") {
                        // `namespace Foo { ... }` — body parsed inline
                        // while cur_ns is set, then the enclosing scope
                        // is restored.
                        let mut v = Vec::new();
                        self.in_braced_ns = true;
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
                        self.in_braced_ns = false;
                        self.cur_ns.clear();
                        self.use_map.clear();
                        self.use_fn_map.clear();
                        self.use_const_map.clear();
                        self.declared_types.clear();
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
                } else if self.ident_is("const")
                    && !matches!(self.peek2(), Some(Token::Ident(k)) if k.eq_ignore_ascii_case("function"))
                {
                    // `const FOO = v, ...;` — declares namespaced global
                    // constants (namespaces/ns_042).
                    self.pos += 1;
                    let mut defs = Vec::new();
                    loop {
                        let n = self
                            .name_path()
                            .unwrap_or_default()
                            .trim_start_matches('\\')
                            .to_string();
                        let n = self.ns_qualify(&n);
                        self.expect_op("=")?;
                        defs.push((n, self.expr()?));
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    self.expect_op(";")?;
                    Ok(Stmt::ConstDecl(defs))
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

    /// Top-level `use` import: `use A\B, C as D, function f\g, const H\I;`
    /// and group form `use A\{B, C as D}`. Aliases populate the
    /// per-namespace maps `ns_resolve` consults (Zend/tests/namespaces);
    /// the raw paths ride along in `Stmt::Use` so the interpreter can
    /// warn on non-compound imports (`use A;` — ns_033).
    fn use_stmt(&mut self) -> Result<Stmt, PhpError> {
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
    fn insert_use_alias(&mut self, kind: NsKind, alias: &str, fq: &str) -> Result<(), PhpError> {
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
    fn backslash_adj_ok(&self) -> bool {
        matches!(self.toks.get(self.pos), Some(t) if t.ws_adj & 2 == 0)
    }

    /// Mid-name `\`: continues the path only when tight on the left and
    /// directly followed by an identifier (`Foo\Bar`); also serves as a
    /// group-use terminator before `{` (`use A\B\{C}` — ns_093).
    fn eat_mid_name_sep(&mut self) -> bool {
        let ok = match (self.toks.get(self.pos), self.peek2()) {
            // Group-use terminator `A\B\{C}` / `A\B \ { C }` — spacing
            // around the final separator is free (ns_093).
            (Some(_), Some(Token::Op("{"))) => true,
            (Some(t), Some(Token::Ident(_))) => t.ws_adj == 0,
            _ => false,
        };
        ok && self.eat_op("\\")
    }

    fn name_path(&mut self) -> Option<String> {
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
    fn ns_qualify(&self, name: &str) -> String {
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
    fn ns_resolve(&self, raw: &str, kind: NsKind) -> String {
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
    fn skip_attrs(&mut self) -> Result<(), PhpError> {
        self.parse_attrs()?;
        Ok(())
    }

    /// Line of the statement an attribute group attaches to: scans past
    /// the group's closing `]` (and any further `]` from sibling groups
    /// is not needed — the first `]` at depth 0 ends the scan).
    /// Zend reports attribute-arg compile fatals on the attributed
    /// declaration's line (first_class_callable_011,
    /// named_params/attributes_*).
    fn line_after_attr_group(&self) -> usize {
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
    fn parse_attrs(&mut self) -> Result<Vec<crate::ast::AttrDecl>, PhpError> {
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

    fn class_decl(&mut self) -> Result<Stmt, PhpError> {
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
                Some(Token::Ident(_)) | Some(Token::Op("?")) | Some(Token::Op("\\"))
            ) && !matches!(self.peek2(), Some(Token::Op("(")))
            {
                self.take_type()?
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
        let ret = if self.eat_op(":") {
            self.take_type()?
        } else {
            None
        };
        let body = if self.eat_op(";") {
            Vec::new()
        } else {
            self.body()?
        };
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
                Some(Token::Ident(_)) | Some(Token::Op("?")) | Some(Token::Op("\\"))
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
        // Return type declarations (: int).
        let ret = if self.eat_op(":") {
            self.take_type()?
        } else {
            None
        };
        let body = self.body()?;
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
            file: String::new(),
            ns: self.cur_ns.clone(),
            decl_in: None,
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
        // `: ret` after `(` — return types apply to closures too
        // (scalar_strict uses `{closure:...}(): Return value ...` TypeErrors).
        let cret = if self.eat_op(":") {
            self.take_type()?
        } else {
            None
        };
        let body = if arrow {
            self.expect_op("=>")?;
            let e = self.expr()?;
            vec![Stmt::Return(Some(e))]
        } else {
            self.body()?
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
                file: String::new(),
                ns: self.cur_ns.clone(),
                decl_in: None,
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
                let pty = if matches!(self.peek(), Some(Token::Ident(_)) | Some(Token::Op("?")))
                    && !matches!(self.peek2(), Some(Token::Op("(")))
                {
                    self.take_type()?
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
                    attrs: vec![],
                    name: format!("class@anonymous${}", self.line()),
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
        let _ = self.take_type()?;
        Ok(())
    }

    /// Consume a type expression, returning its member names in source
    /// order. `?T`/`T|null` append a "null" member; parentheses flatten.
    fn take_type(&mut self) -> Result<Option<Vec<String>>, PhpError> {
        let mut members: Vec<String> = Vec::new();
        let mut nullable = false;
        let mut depth = 0i32;
        let mut name = String::new();
        loop {
            match self.peek() {
                Some(Token::Ident(n)) => {
                    if !name.is_empty() && !name.ends_with('\\') {
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
                        break;
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
        let members = if members.is_empty() {
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
                    .map(|m| {
                        let (pre, inner, post) = if m.starts_with('(') && m.ends_with(')') {
                            ("(", &m[1..m.len() - 1], ")")
                        } else {
                            ("", m.as_str(), "")
                        };
                        let resolved = inner
                            .split('&')
                            .map(|p| {
                                let p = p.trim_start_matches('\\');
                                if p.is_empty() || BUILTIN_TYS.contains(&p.to_lowercase().as_str())
                                {
                                    p.to_string()
                                } else {
                                    self.ns_resolve(p, NsKind::Class)
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("&");
                        format!("{pre}{resolved}{post}")
                    })
                    .collect(),
            )
        };
        Ok(members)
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
    fn list_target(&mut self, e: Expr) -> Result<Expr, PhpError> {
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
    fn has_nullsafe(e: &Expr) -> bool {
        match e {
            Expr::MethodCall { obj, nullsafe, .. } => *nullsafe || Self::has_nullsafe(obj),
            Expr::Prop { obj, nullsafe, .. } => *nullsafe || Self::has_nullsafe(obj),
            _ => false,
        }
    }

    fn fcc_wrap(node: Expr) -> Result<Expr, PhpError> {
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
    fn check_no_fcc_ctor(&self, args: &[Expr]) -> Result<(), PhpError> {
        if args.len() == 1 && matches!(args[0], Expr::FccMark) {
            return Err(PhpError::fatal(
                "Cannot create Closure for new expression",
                self.line(),
            ));
        }
        Ok(())
    }

    fn args(&mut self) -> Result<Vec<Expr>, PhpError> {
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
            // List-destructuring hole: `[, $b] = ...` / `[$a, , $c]`.
            // Expr::Null marks the skipped slot; list_target maps it to
            // None. (In array-literal position a hole is a superset.)
            if self.eat_op(",") {
                items.push((None, Expr::Null));
                continue;
            }
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
