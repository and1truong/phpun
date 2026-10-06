mod decl;
mod expr;

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
    /// Compile-time warnings (msg, line) — confusable type names
    /// (confusable_type_warning). Drained into `Stmt::Diag`.
    compile_warnings: Vec<(String, usize)>,
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
    /// Enclosing class-like declarations as (has_parent, is_trait) —
    /// `self`/`static`/`parent` type members are compile errors
    /// outside class scope (static_type_outside_class).
    class_ctx: Vec<(bool, bool)>,
    /// Set by `program()` while the FIRST top-level statement is being
    /// parsed; `stmt()` moves it into `strict_slot` so a nested
    /// `declare(strict_types=1)` can't claim the slot
    /// (scalar_strict_declaration_placement_*, strict_nested).
    first_stmt_slot: bool,
    /// True only while the literal first statement is a
    /// `declare` — the only place `strict_types` is legal.
    strict_slot: bool,
    /// Inside a closure decl — `self`/`static`/`parent` type members
    /// resolve lazily at call time (bindTo can supply the scope), so
    /// the no-class-scope compile fatal doesn't apply
    /// (static_type_return's unbound `{closure:...}(): static`).
    in_closure: bool,
}

pub fn parse(src: &str) -> Result<Vec<Stmt>, PhpError> {
    parse_with(src, false)
}

/// `parse` honoring `short_open_tag` (INI `short_open_tag=On`).
pub fn parse_with(src: &str, short_open: bool) -> Result<Vec<Stmt>, PhpError> {
    let toks = lex_with(src, short_open)?;
    parse_toks(toks, 1 + src.bytes().filter(|&b| b == b'\n').count())
}

/// phpun source mode: PHP code from byte 0, no `<?php` required (a
/// leading tag falls back to classic tag mode for legacy sources).
/// If pure-source parsing fails and the source contains a `<?` tag
/// anywhere, the legacy tag-mode parse is tried so HTML-embedded PHP
/// keeps working; when both fail the tag-mode error is preferred (the
/// file was real tag-mode PHP, and its error is the meaningful one).
pub fn parse_source(src: &str, short_open: bool) -> Result<Vec<Stmt>, PhpError> {
    match parse_pure(src, short_open) {
        Ok(stmts) => Ok(stmts),
        Err(e) => {
            if src.contains("<?") {
                // Tag-mode parse of HTML-embedded PHP: prefer ITS error
                // over the pure-mode one (a `hi<?php declare(strict_types)`
                // file's real failure is the strict_types fatal, not the
                // pure lexer's `?` confusion — placement_003).
                return parse_with(src, short_open);
            }
            Err(e)
        }
    }
}

/// Strict pure-source parse — no tag-mode detection or retry. Used for
/// eval()'d code, which in PHP is always tag-free source.
pub fn parse_pure(src: &str, short_open: bool) -> Result<Vec<Stmt>, PhpError> {
    let toks = crate::lexer::lex_php_source(src, short_open)?;
    parse_toks(toks, 1 + src.bytes().filter(|&b| b == b'\n').count())
}

/// `eof_line` is Zend's scanner line at end-of-input (one past the
/// last consumed newline) — where EOF-attributed errors are reported.
fn parse_toks(toks: Vec<Lexed>, eof_line: usize) -> Result<Vec<Stmt>, PhpError> {
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
    let bracket_err = bracket_check(&toks, eof_line).err();
    let mut p = Parser::new(&toks);
    let mut stmts = match p.program() {
        Ok(s) => s,
        Err(pe) => {
            // Zend reports the earliest error. The scanner dies at its
            // own token before that token ever reaches the parser, so
            // a bracket error at index i only loses to a parser error
            // that a re-parse of the token PREFIX before i still
            // produces (and the prefix parse must die on a real token —
            // an end-of-input error there means the parser was simply
            // still waiting for the dead token).
            if let Some((be, bpos)) = bracket_err {
                let mut p2 = Parser::new(&toks[..bpos]);
                match p2.program() {
                    Err(pe2)
                        if !pe2
                            .message
                            .starts_with("syntax error, unexpected end of") =>
                    {
                        return Err(pe2);
                    }
                    _ => return Err(be),
                }
            }
            return Err(pe);
        }
    };
    if let Some((be, _)) = bracket_err {
        return Err(be);
    }
    let mut diags: Vec<(String, &'static str, usize)> = lex_diags;
    diags.extend(
        std::mem::take(&mut p.deprecations)
            .into_iter()
            .map(|(msg, line)| (msg, "Deprecated", line)),
    );
    diags.extend(
        std::mem::take(&mut p.compile_warnings)
            .into_iter()
            .map(|(msg, line)| (msg, "Warning", line)),
    );
    diags.sort_by_key(|(_, _, line)| *line);
    for (i, (msg, level, line)) in diags.into_iter().enumerate() {
        stmts.insert(i, Stmt::Diag { level, msg, line });
    }
    Ok(stmts)
}

impl<'a> Parser<'a> {
    fn new(toks: &'a [Lexed]) -> Self {
        Self {
            toks,
            pos: 0,
            deprecations: Vec::new(),
            compile_warnings: Vec::new(),
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
            class_ctx: Vec::new(),
            first_stmt_slot: false,
            strict_slot: false,
            in_closure: false,
        }
    }
}

/// Zend-style bracket-balance pre-pass: mismatched/unclosed/mismatched
/// closers report as `Unclosed 'X'`, `Unmatched 'Y'`,
/// `Unclosed 'X' does not match 'Y'` (syntax_errors). The error comes
/// back with the token index where the scanner would have died
/// (`toks.len()` for an EOF-unclosed bracket) so the caller can order
/// it against parser errors by position.
fn bracket_check(
    toks: &[crate::lexer::Lexed],
    eof_line: usize,
) -> Result<(), (PhpError, usize)> {
    let mut stack: Vec<(&'static str, usize)> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
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
                        return Err((PhpError::parse(msg, t.line), i));
                    }
                    None => {
                        return Err((PhpError::parse(format!("Unmatched '{}'", op), t.line), i));
                    }
                }
            }
            _ => {}
        }
    }
    if let Some((o, ol)) = stack.pop() {
        // The opener's line is named only when it differs from the
        // error line — so real files (EOF lands a line past the last
        // newline) always get `Unclosed 'X' on line N` while a
        // single-line eval() string reports a bare `Unclosed 'X'`.
        let msg = if ol != eof_line {
            format!("Unclosed '{}' on line {}", o, ol)
        } else {
            format!("Unclosed '{}'", o)
        };
        return Err((PhpError::parse(msg, eof_line), toks.len()));
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
        compile_warnings: Vec::new(),
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
        class_ctx: Vec::new(),
        first_stmt_slot: false,
        strict_slot: false,
        in_closure: false,
    };
    let e = p.expr()?;
    Ok((e, diags))
}

const ASSIGN_OPS: &[&str] = &[
    "=", "+=", "-=", "*=", "/=", ".=", "%=", "&=", "|=", "^=", "<<=", ">>=", "**=", "??=",
];

impl<'a> Parser<'a> {
    pub(in crate::parser) fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos).map(|l| &l.token)
    }

    pub(in crate::parser) fn peek2(&self) -> Option<&Token> {
        self.toks.get(self.pos + 1).map(|l| &l.token)
    }

    pub(in crate::parser) fn line(&self) -> usize {
        self.toks
            .get(self.pos)
            .map(|l| l.line)
            .unwrap_or_else(|| self.toks.last().map(|l| l.line).unwrap_or(1))
    }

    pub(in crate::parser) fn next(&mut self) -> Option<Token> {
        let t = self.peek().cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// Line of the token just consumed — the `}` closing a body
    /// (FunctionDecl::end_line).
    pub(in crate::parser) fn prev_line(&self) -> usize {
        self.pos
            .checked_sub(1)
            .and_then(|i| self.toks.get(i))
            .map(|t| t.line)
            .unwrap_or_else(|| self.line())
    }

    pub(in crate::parser) fn at_op(&self, op: &str) -> bool {
        matches!(self.peek(), Some(Token::Op(o)) if *o == op)
    }

    pub(in crate::parser) fn eat_op(&mut self, op: &str) -> bool {
        if self.at_op(op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub(in crate::parser) fn expect_op(&mut self, op: &str) -> Result<(), PhpError> {
        if self.eat_op(op) {
            Ok(())
        } else {
            // Zend reports a lone unexpected `\` name separator without an
            // "expecting" clause (namespaced_name_whitespace).
            // Zend never appends an "expecting" clause for `;`
            // (mixed_cast_error); other expected tokens keep it
            // (oct_whitespace's `expecting ")"`).
            let msg = if op == ";" {
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

    pub(in crate::parser) fn describe(&self) -> String {
        match self.peek() {
            None => "end of file".to_string(),
            Some(Token::Ident(s)) => format!("identifier \"{}\"", s),
            Some(Token::Variable(s)) => format!("variable \"${}\"", s),
            Some(Token::Int(v)) => format!("integer \"{}\"", v),
            Some(Token::Float(v)) => format!("float {}", v),
            Some(Token::Op(o)) => format!("token \"{}\"", o),
            Some(_) => "token".to_string(),
        }
    }

    pub(in crate::parser) fn ident(&mut self) -> Option<String> {
        if let Some(Token::Ident(s)) = self.peek() {
            let s = s.clone();
            self.pos += 1;
            Some(s)
        } else {
            None
        }
    }

    pub(in crate::parser) fn ident_is(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Ident(s)) if s.eq_ignore_ascii_case(kw))
    }

    pub(in crate::parser) fn eat_ident(&mut self, kw: &str) -> bool {
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
        // `declare(strict_types)` must be the very first statement —
        // any preceding stmt (even another declare) is a fatal
        // (scalar_strict_declaration_placement_*).
        let mut saw_any = false;
        // Everything after `__HALT_COMPILER()` is ignored entirely —
        // the lexer stops there (namespaces/ns_080).
        let mut halted = false;
        while self.peek().is_some() {
            let stmt_line = self.line();
            stmts.push(Stmt::Line(stmt_line));
            self.first_stmt_slot = !saw_any;
            let s = self.stmt()?;
            saw_any = true;
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
    pub(in crate::parser) fn body(&mut self) -> Result<Vec<Stmt>, PhpError> {
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
    pub(in crate::parser) fn body_any(&mut self, end: &str) -> Result<Vec<Stmt>, PhpError> {
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
    pub(in crate::parser) fn body_until(&mut self, stops: &[&str]) -> Result<Vec<Stmt>, PhpError> {
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

    pub(in crate::parser) fn stmt(&mut self) -> Result<Stmt, PhpError> {
        // The first-statement slot is consumed by whichever stmt
        // parses it — a nested `declare` can't reach it (strict_nested).
        self.strict_slot = self.first_stmt_slot;
        self.first_stmt_slot = false;
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
                    if self.at_op(")") {
                        return Err(PhpError::parse(
                            format!("syntax error, unexpected {}", self.describe()),
                            self.line(),
                        ));
                    }
                    let mut xs = Vec::new();
                    while !self.at_op(")") {
                        let first = xs.is_empty();
                        xs.push(self.unset_arg(first)?);
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
                        && {
                            // `final readonly class` / `abstract final`
                            // — skip a second modifier in the lookahead.
                            let mut n = 1;
                            while matches!(
                                self.toks.get(self.pos + n).map(|l| &l.token),
                                Some(Token::Ident(k))
                                    if ["abstract", "final", "readonly"]
                                        .iter()
                                        .any(|m| k.eq_ignore_ascii_case(m))
                            ) {
                                n += 1;
                            }
                            matches!(
                                self.toks.get(self.pos + n).map(|l| &l.token),
                                Some(Token::Ident(k))
                                    if k.eq_ignore_ascii_case("class")
                            )
                        })
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
                } else if self.ident_is("goto") {
                    self.pos += 1;
                    let label = match self.peek() {
                        Some(Token::Ident(s)) => s.clone(),
                        _ => {
                            return Err(PhpError::parse(
                                "syntax error, unexpected token, expecting identifier",
                                self.line(),
                            ))
                        }
                    };
                    self.pos += 1;
                    self.expect_op(";")?;
                    Ok(Stmt::Goto(label))
                } else if matches!(self.peek2(), Some(Token::Op(":"))) {
                    // `name:` — a goto label; can't start any expression.
                    let label = match self.peek() {
                        Some(Token::Ident(s)) => s.clone(),
                        _ => unreachable!(),
                    };
                    self.pos += 2;
                    Ok(Stmt::Label(label))
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
}

pub(in crate::parser) fn desc_t(t: Option<&Token>) -> String {
    match t {
        None => "end of file".to_string(),
        Some(Token::Ident(s)) => format!("identifier \"{}\"", s),
        Some(Token::Variable(s)) => format!("variable \"${}\"", s),
        Some(Token::Op(o)) => format!("token \"{}\"", o),
        Some(Token::Int(v)) => format!("integer \"{}\"", v),
        Some(Token::Float(v)) => format!("float {}", v),
        _ => "token".to_string(),
    }
}
