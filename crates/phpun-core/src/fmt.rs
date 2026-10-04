//! `phpun fmt` — PSR-12-ish code formatter over our own token stream
//! (#30). The AST drops comments and literal spellings, so formatting
//! works on `lex_php_source` tokens directly: each token's byte span
//! gives its raw text, and the gap between spans yields whitespace and
//! comments. Output is canonical PSR-12-ish whitespace; the token text
//! itself is never rewritten (strings, heredocs, number spellings and
//! inline HTML pass through byte-identical), which keeps the formatter
//! safe by construction and idempotent.
//!
//! Rules:
//! - 4-space indent; class/method/named-function `{` on the next line,
//!   control/closure/anon-class `{` on the same line; `} else`,
//!   `} catch`, `} finally`, `} while` (do-while) join the brace line.
//! - `;` ends the line (inside `for (;;)` it stays `; `); `case`/
//!   `default` sit at switch-body indent and open a +1 body indent;
//!   alt-syntax `:`/`endX` blocks indent like braces.
//! - Spaces around binary ops and `=>`; tight `->`/`?->`/`::`/`\`/unary
//!   prefixes; `if (`-style space after control keywords; calls and
//!   `isset(`/`array(`/`declare(`-style constructs tight; `fn (`/
//!   `function (`/`use (` spaced; casts `(int)` emitted tight.
//! - `//`/`#`/`/* */`/`/** */` comments re-emitted where they were:
//!   trailing comments stay trailing, own-line comments re-indent,
//!   docblock ` * ` continuations re-align.
//! - At most one blank line, none right after `{` or before `}`;
//!   newlines inside parens/brackets (call args, arrays, match arms)
//!   are preserved with a +1 continuation indent.
//! - `<?php`/`<?`/`<?=`/`?>` and inline HTML pass through; `?>` emits
//!   ` ?>` after code and breaks the line after itself.

use crate::error::PhpError;
use crate::lexer::{is_keyword, lex_php_source, Lexed, Token};

/// Format a PHP source string; returns formatted text ending in a
/// single newline. A `#!` shebang line is preserved verbatim.
pub fn format(src: &str) -> Result<String, PhpError> {
    let (body, shebang) = match src.strip_prefix("#!") {
        Some(rest) => match rest.find('\n') {
            // `nl` is relative to `rest` (post-`#!`) — the shebang
            // slice in `src` is 2 bytes wider.
            Some(nl) => (&rest[nl + 1..], &src[..nl + 3]),
            None => ("", src),
        },
        None => (src, ""),
    };
    let toks = lex_php_source(body, false)?;
    let mut f = Fmt {
        src: body,
        out: shebang.to_string(),
        bol: true,
        ..Default::default()
    };
    f.run(&toks);
    let mut out = f.out;
    while out.ends_with(' ') || out.ends_with('\t') {
        out.pop();
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// What a `{` opens: a class/method decl (brace on the next line), a
/// control block (same line), or an inline brace (`{$x}`, `use A\{B}`).
#[derive(Clone, Copy, PartialEq)]
enum BraceKind {
    Decl,
    Block,
    Inline,
}

/// Tag for `(`/`[`/`#[`/inline-`{` nesting. `(` remembers the keyword or
/// name that opened it so `{`/`:`/alt-syntax/`;` rules can classify it.
#[derive(Clone, Copy, PartialEq)]
enum Nest {
    If,
    ElseIf,
    /// `for (` — `;` inside stays on the line.
    For,
    Foreach,
    While,
    Switch,
    Match,
    Catch,
    /// `function (` closure (`named` false) or `function name (` decl.
    Function {
        named: bool,
    },
    /// `fn (`
    Fn,
    /// closure `use (`
    Use,
    /// `new X (` / anonymous `class (`
    New,
    /// `declare (`
    Declare,
    /// `foo (` call or `(...)` group — only spacing differs.
    Call,
    Group,
    /// `[`
    Bracket,
    /// `#[` attribute
    Attr,
    /// `{` inside an expression (var-var `{$x}`, `use A\{B}`, offsets).
    InlineBrace,
}

/// Head construct of a statement block, for `}`-join bookkeeping.
#[derive(Clone, Copy, PartialEq)]
enum Head {
    Decl,
    /// `do {` — enables the `} while` join.
    Do,
    /// `switch (...) {` or `switch (...):` — `case_open` is the +1
    /// indent of an open case body.
    Switch {
        case_open: bool,
    },
    /// Alt-syntax `if (...):`/`else:`/`for (...):` block (non-switch).
    /// `arm_open` is the +1 indent of the current arm's body —
    /// `elseif`/`else:` close and reopen it instead of nesting.
    Alt {
        arm_open: bool,
    },
    /// `match (...) {` — `,` at depth 0 ends the line per arm.
    Match,
    Other,
}

const CAST_TYPES: &[&str] = &[
    "int", "integer", "float", "double", "real", "string", "binary", "array", "object", "bool",
    "boolean", "unset",
];

const END_KWS: &[&str] = &[
    "endif",
    "endfor",
    "endforeach",
    "endwhile",
    "endswitch",
    "enddeclare",
];

/// Keywords that call like functions — `isset(`, `declare(`, `exit(`
/// stay tight; other keywords get `kw (` space (`if (`, `use (`).
const CALL_KWS: &[&str] = &[
    "isset",
    "unset",
    "empty",
    "eval",
    "exit",
    "die",
    "print",
    "include",
    "include_once",
    "require",
    "require_once",
    "list",
    "array",
    "declare",
];

/// Keywords that double as type names — `static`/`self`/`parent`/`null`
/// are keywords but can sit in a union type (`A|static`).
const TYPE_NAMES: &[&str] = &["static", "true", "false", "null", "self", "parent"];

/// Keywords that read as operands when they end an expression — used
/// only for the ternary-`?` vs nullable-`?` split.
const VALUE_KWS: &[&str] = &["true", "false", "null", "static", "self", "parent"];

/// Binary operators — space on both sides. Keyword ops (`instanceof`,
/// `as`, ...) arrive as `Token::Ident` and get the default space.
const BIN_OPS: &[&str] = &[
    "+", "-", "*", "/", "%", "**", ".", "=", "==", "===", "!=", "!==", "<>", "<", ">", "<=", ">=",
    "<=>", "&&", "||", "??", "=>", "+=", "-=", "*=", "/=", ".=", "%=", "&=", "|=", "^=", "<<=",
    ">>=", "**=", "??=", "<<", ">>", "^",
];

/// After these ops the next token stays tight: `($x`, `[$i`, `{$x}`,
/// `Foo\Bar`, `a->b`, `a?->b`, `A::B`, `#[A`.
const TIGHT_AFTER: &[&str] = &["(", "[", "{", "#[", "\\", "->", "?->", "::"];

/// Before a `(`/`[`, these prev-ops keep it tight (`!($x`, `$(`,
/// `$a[0](`, `Foo::bar(`). Keywords handled separately.
const OPEN_OPS: &[&str] = &[
    "(", "[", "{", ")", "]", "!", "~", "@", "$", "\\", "->", "?->", "::", "#[",
];

#[derive(Default)]
struct Fmt<'a> {
    src: &'a str,
    out: String,
    indent: usize,
    /// At start of line (indent not yet written).
    bol: bool,
    /// Open statement blocks (brace or alt-colon).
    blocks: Vec<Head>,
    /// Open parens/brackets/attrs/inline-braces.
    parens: Vec<Nest>,
    /// Pending ternary `?` count, indexed by parens.len().
    qmarks: Vec<u32>,
    /// Structural newline owed before the next non-joiner token.
    hard_nl: bool,
    /// A statement/expression is mid-flight — source newlines get
    /// preserved at +1 continuation indent.
    stmt_open: bool,
    /// The current line is a preserved continuation — indent +1.
    cont: bool,
    /// Inside a `case`/`default` head (until its `:`).
    in_case: bool,
    /// Next token stays tight (nullable `?`, by-ref `&`, unary `-`/`+`).
    tight_next: bool,
    /// A block `{` was just emitted — suppress a leading blank line.
    after_block_open: bool,
    /// Head of the last closed `}` block — `} while`/`} else` joins.
    last_head: Option<Head>,
    /// Nest popped by the last `)` — `{`/`:` after `)` consult it.
    last_paren: Option<Nest>,
    /// Inside a `match {` block: the arm's `=>` was already seen, so
    /// the next depth-0 `,` ends the arm (`1 => x,` newline), while
    /// `,` before `=>` is a key list (`1, 2 =>` stays inline).
    arm_arrow: bool,
}

#[derive(PartialEq)]
enum Sep {
    Tight,
    Space,
}

impl<'a> Fmt<'a> {
    fn run(&mut self, toks: &[Lexed]) {
        let toks: Vec<&Lexed> = toks
            .iter()
            .filter(|t| !matches!(t.token, Token::Diag(_, _)))
            .collect();
        let mut prev_end = 0usize;
        let mut prev: Option<&Lexed> = None;
        let mut i = 0usize;
        while i < toks.len() {
            let t = toks[i];
            if t.start == usize::MAX || t.end == usize::MAX {
                i += 1;
                continue;
            }
            let gap = &self.src[prev_end.min(t.start)..t.start];
            let (atoms, last_nl) = gap_atoms(gap);
            self.emit_atoms(&atoms);
            let eaten = self.emit(i, &toks, t, prev, last_nl);
            if eaten > 0 {
                let last = toks[i + eaten];
                prev_end = last.end;
                prev = Some(last);
                i += eaten + 1;
            } else {
                prev_end = t.end;
                prev = Some(t);
                i += 1;
            }
        }
        let (atoms, _) = gap_atoms(&self.src[prev_end..]);
        self.emit_atoms(&atoms);
    }

    /// Emit one token plus any merged followers (`else if`→`elseif`,
    /// `? :`→`?:`, `(int)` casts). Returns how many tokens beyond `i`
    /// were consumed.
    fn emit(
        &mut self,
        i: usize,
        toks: &[&Lexed],
        t: &Lexed,
        prev: Option<&Lexed>,
        gap_nl: usize,
    ) -> usize {
        // ---- pre-sep: dedent for line-start constructs ----
        let cur_block_close = matches!(t.token, Token::Op("}"))
            && !matches!(self.parens.last(), Some(Nest::InlineBrace));
        if cur_block_close {
            self.close_case();
            if self.indent > 0 {
                self.indent -= 1;
            }
        }
        let end_kw = matches!(&t.token, Token::Ident(w) if END_KWS.contains(&w.to_ascii_lowercase().as_str()));
        if end_kw {
            self.close_case();
            if let Some(Head::Alt { arm_open }) = self.blocks.last_mut() {
                if *arm_open {
                    *arm_open = false;
                    if self.indent > 0 {
                        self.indent -= 1;
                    }
                }
            }
            match self.blocks.last() {
                // Alt pops after its single arm indent; Switch pops
                // with an extra body indent.
                Some(Head::Alt { .. }) => {
                    self.blocks.pop();
                }
                Some(Head::Switch { .. }) => {
                    self.blocks.pop();
                    if self.indent > 0 {
                        self.indent -= 1;
                    }
                }
                _ => {}
            }
        }
        let case_kw = matches!(&t.token, Token::Ident(w) if {
            let low = w.to_ascii_lowercase();
            (low == "case" || low == "default")
                && self.in_switch()
                && !matches!(prev.map(|p| &p.token), Some(Token::Op("::")))
        });
        if case_kw {
            self.close_case();
        }
        // `elseif`/`else` close an open alt-syntax arm: they dedent to
        // their `if`'s level (not after `}`, which continues a brace if).
        let alt_branch = matches!(&t.token, Token::Ident(w) if {
            let low = w.to_ascii_lowercase();
            low == "elseif" || low == "else"
        }) && !matches!(prev.map(|p| &p.token), Some(Token::Op("}")))
            && matches!(self.blocks.last(), Some(Head::Alt { arm_open: true }));
        if alt_branch {
            if let Some(Head::Alt { arm_open }) = self.blocks.last_mut() {
                *arm_open = false;
            }
            if self.indent > 0 {
                self.indent -= 1;
            }
        }
        let brace_kind = if matches!(t.token, Token::Op("{")) {
            Some(self.classify_brace(toks, i))
        } else {
            None
        };
        // ---- leading separator ----
        // `?>` (a `;` token whose raw text is `?>`) always joins the
        // current line — `code; ?>` and `<?= $x ?>` stay inline.
        let close_tag = matches!(t.token, Token::Op(";")) && &self.src[t.start..t.end] == "?>";
        if close_tag {
            self.hard_nl = false;
        }
        // `last_paren` only informs `{`/`:` right after `)`.
        if !matches!(t.token, Token::Op("{") | Token::Op(":")) {
            self.last_paren = None;
        }
        let join = close_tag || self.joins_after_block(t);
        self.last_head = None; // consumed by the join check
        let mut want = 0usize;
        if self.hard_nl && !join {
            want = 1;
        }
        if cur_block_close || end_kw || case_kw || brace_kind == Some(BraceKind::Decl) {
            want = 1;
        }
        if want == 0 && self.stmt_open && gap_nl >= 1 && !join {
            want = 1; // preserved mid-statement newline
        }
        if gap_nl >= 2 && want <= 1 && !self.after_block_open {
            want = 2; // preserved blank line
        }
        // no blank lines before closing constructs
        if cur_block_close || end_kw || case_kw {
            want = want.min(1);
        }
        if self.out.is_empty() || self.after_block_open {
            want = want.min(if self.out.is_empty() { 0 } else { 1 });
        }
        let have = self.bol as usize;
        for _ in have..want.min(2) {
            self.out.push('\n');
            self.bol = true;
        }
        // `)`/`]`/`}` at line start print at the opener's indent —
        // the continuation +1 belongs to the wrapped contents only.
        let closer = matches!(&t.token, Token::Op(o) if matches!(*o, ")" | "]" | "}"));
        self.cont = want == 1
            && self.stmt_open
            && !self.hard_nl
            && !cur_block_close
            && !end_kw
            && !case_kw
            && !closer
            && brace_kind != Some(BraceKind::Decl);
        self.hard_nl = false;
        self.after_block_open = false;
        if want == 0 && !self.bol {
            if self.tight_next {
                self.tight_next = false;
            } else if self.sep(toks, i, t, prev) == Sep::Space {
                self.out.push(' ');
                self.bol = false;
            }
        } else {
            self.tight_next = false;
        }
        // ---- emit ----
        self.emit_tok(i, toks, t, brace_kind)
    }

    /// `}`-closed block just emitted: does `cur` continue it on the
    /// same line (`} else`, `};`, `} while`, `}`+operator)?
    fn joins_after_block(&self, cur: &Lexed) -> bool {
        let Some(head) = self.last_head else {
            return false;
        };
        match &cur.token {
            Token::Ident(w) => {
                let low = w.to_ascii_lowercase();
                match low.as_str() {
                    "else" | "elseif" | "catch" | "finally" => true,
                    "while" => head == Head::Do,
                    _ => false,
                }
            }
            Token::Op(op) => matches!(
                *op,
                ";" | ","
                    | ")"
                    | "]"
                    | "("
                    | "["
                    | "."
                    | "->"
                    | "?->"
                    | "::"
                    | "?"
                    | ":"
                    | "=>"
                    | "|"
                    | "&"
                    | "&&"
                    | "||"
                    | "??"
                    | "=="
                    | "==="
                    | "!="
                    | "!=="
                    | "<>"
                    | "<"
                    | ">"
                    | "<="
                    | ">="
                    | "<=>"
                    | "+"
                    | "-"
                    | "*"
                    | "/"
                    | "%"
                    | "**"
                    | "="
                    | "+="
                    | "-="
                    | "*="
                    | "/="
                    | ".="
                    | "%="
                    | "&="
                    | "|="
                    | "^="
                    | "<<"
                    | ">>"
                    | "<<="
                    | ">>="
                    | "**="
                    | "??="
                    | "^"
            ),
            _ => false,
        }
    }

    /// Same-line spacing between `prev` and `cur` (no newline wanted).
    fn sep(&self, toks: &[&Lexed], i: usize, cur: &Lexed, prev: Option<&Lexed>) -> Sep {
        let Some(p) = prev else {
            return if self.out.is_empty() {
                Sep::Tight
            } else {
                // a leading comment was emitted before the first token
                Sep::Space
            };
        };
        // `declare(...)` stays fully tight: `declare(strict_types=1)`.
        if matches!(self.parens.last(), Some(Nest::Declare)) {
            return Sep::Tight;
        }
        let ck = kind(cur);
        let pk = kind(p);
        // cur-side rules
        match ck {
            // `<?=` tag token — no space inserted before it
            K::Other => return Sep::Tight,
            K::Op(")" | "]" | "," | ";" | "->" | "?->" | "::") => return Sep::Tight,
            K::Op("\\") => {
                // `\` in namespaced names stays tight after
                // identifiers and open/type ops (`A\B`, `?\T`, `A|\T`
                // — a tight `?`/`|`/`&` never reaches sep). It takes a
                // space after keywords and separators: `instanceof \T`,
                // `extends \T`, `, \T`, `: \T`, `= \T`, `? \T` (ternary),
                // `$a | \T` (binary `|`/`&`).
                return match &p.token {
                    Token::Ident(w) if is_keyword(&w.to_ascii_lowercase()) => Sep::Space,
                    Token::Op(o)
                        if BIN_OPS.contains(o)
                            || matches!(*o, "," | ";" | ":" | "?" | "&" | "|") =>
                    {
                        Sep::Space
                    }
                    _ => Sep::Tight,
                };
            }
            K::Op("{") | K::Op("}") | K::Op(":") | K::Op("?") | K::Op("#[") => {
                return self.sep_special(cur, p)
            }
            K::Op("-") | K::Op("+") => {
                // binary `a - b` spaced; unary `-x`/`+x` — tight after
                // `(`/`[`/`{`/`!`, spaced otherwise (`= -$x`, `return -1`).
                return if is_operand(&p.token) {
                    Sep::Space
                } else {
                    match &p.token {
                        Token::Op(op) if matches!(*op, "(" | "[" | "{" | "!" | "~" | "@" | "$") => {
                            Sep::Tight
                        }
                        _ => Sep::Space,
                    }
                };
            }
            K::Op("(") => {
                return match &p.token {
                    Token::Ident(w) => {
                        let low = w.to_ascii_lowercase();
                        if CALL_KWS.contains(&low.as_str())
                            || !is_keyword(&low)
                            || matches!(low.as_str(), "static" | "self" | "parent")
                        {
                            // `isset(`, `foo(`, `new static(`, `self(`
                            Sep::Tight
                        } else if low == "class" {
                            // `new class(` — anon class ctor args
                            match prev_sig_idx(toks, i)
                                .and_then(|(k, _)| prev_sig(toks, k))
                                .map(|x| &x.token)
                            {
                                Some(Token::Ident(n)) if n.eq_ignore_ascii_case("new") => {
                                    Sep::Tight
                                }
                                _ => Sep::Space,
                            }
                        } else {
                            Sep::Space // `if (`, `function (`, `use (`
                        }
                    }
                    Token::Op(o) if OPEN_OPS.contains(o) => Sep::Tight,
                    Token::Op(_) => Sep::Space,
                    _ => Sep::Tight, // `$(`, `f()(`...
                };
            }
            K::Op("[") => {
                return match &p.token {
                    Token::Ident(w) if is_keyword(&w.to_ascii_lowercase()) => Sep::Space,
                    Token::Op(o) if BIN_OPS.contains(o) || matches!(*o, "," | ":") => Sep::Space,
                    _ => Sep::Tight,
                };
            }
            K::Op(o) if BIN_OPS.contains(&o) => return Sep::Space,
            K::Op("&") | K::Op("|") => {
                return self.amp_bar_sep(toks, i, p);
            }
            K::Op("!" | "~" | "@" | "++" | "--" | "$" | "...") => {
                // unary / postfix — tight after `(`-likes and operands
                // (`$x++`), spaced after keywords and operators.
                return match &p.token {
                    Token::Op(op)
                        if matches!(
                            *op,
                            "(" | "[" | "{" | "!" | "~" | "@" | "$" | "\\" | "->" | "?->" | "::"
                        ) =>
                    {
                        Sep::Tight
                    }
                    Token::Op(_) => Sep::Space,
                    Token::Ident(w) if is_keyword(&w.to_ascii_lowercase()) => Sep::Space,
                    _ => Sep::Tight,
                };
            }
            _ => {}
        }
        // prev-side rules
        match pk {
            K::Op(";") if &self.src[p.start..p.end] == "?>" => {
                // `?>`+inline html: adjacent stays tight, spaced stays
                // spaced (`?></p>` / `?> <p>`).
                let gap = &self.src[p.end..cur.start];
                if gap.is_empty() {
                    Sep::Tight
                } else {
                    Sep::Space
                }
            }
            K::Op(o) if TIGHT_AFTER.contains(&o) => Sep::Tight,
            _ => Sep::Space,
        }
    }

    /// `&`/`|` space-before: tight for union/intersection types
    /// (`A|B`, `A&B` — type-ish on both sides) and by-ref `&$x` right
    /// after `(`/`[`/`{`; spaced otherwise (`$a & $b`, `int &$x`,
    /// `function &name`).
    fn amp_bar_sep(&self, toks: &[&Lexed], i: usize, p: &Lexed) -> Sep {
        let prev_open = matches!(&p.token, Token::Op(o) if matches!(*o, "(" | "[" | "{" | ";"));
        match toks.get(i + 1).map(|n| &n.token) {
            Some(Token::Variable(_)) => {
                if prev_open {
                    Sep::Tight
                } else {
                    Sep::Space
                }
            }
            Some(n) if typeish(n) => {
                if typeish(&p.token) {
                    Sep::Tight
                } else {
                    Sep::Space
                }
            }
            _ => Sep::Space,
        }
    }

    /// `{`/`}`/`:`/`?`/`#[` leading-space rules (newline rules are in
    /// `emit`; these are same-line fallbacks).
    fn sep_special(&self, cur: &Lexed, p: &Lexed) -> Sep {
        match &cur.token {
            Token::Op("{") => match &p.token {
                Token::Op(")") => Sep::Space,  // `if (x) {`
                Token::Op(":") => Sep::Space,  // `case 1: {`
                Token::Ident(_) => Sep::Space, // `else {`, `class {`, `Trait {`
                _ => Sep::Tight,               // `{$x}`, `Foo\{Bar}`
            },
            Token::Op("}") => Sep::Tight, // inline `}`
            Token::Op("#[") => match &p.token {
                Token::Ident(_) => Sep::Space, // `public #[A]`
                Token::Op(o) if matches!(*o, "," | "=" | "=>") => Sep::Space,
                _ => Sep::Tight,
            },
            Token::Op(":") => {
                // ternary `:` spaced (also after `f()`: `a ? f() : c`);
                // `):` return-type / alt-syntax colon always tight.
                if self.qmarks.get(self.parens.len()).copied().unwrap_or(0) > 0 {
                    return Sep::Space;
                }
                if matches!(&p.token, Token::Op(")")) {
                    return Sep::Tight;
                }
                if self.in_case {
                    return Sep::Tight;
                }
                Sep::Tight
            }
            Token::Op("?") => {
                // nullable `?int` tight after `(`/`[`/`|`/`&`/`?`;
                // ternary `?` (and everything else) spaced.
                if matches!(&p.token, Token::Op(o) if matches!(*o, "(" | "[" | "|" | "&" | "?")) {
                    Sep::Tight
                } else {
                    Sep::Space
                }
            }
            _ => Sep::Tight,
        }
    }

    /// Emit the token text and update all post-state (indent, stacks,
    /// flags). Returns tokens consumed beyond `i` (merges).
    fn emit_tok(
        &mut self,
        i: usize,
        toks: &[&Lexed],
        t: &Lexed,
        brace_kind: Option<BraceKind>,
    ) -> usize {
        let raw = &self.src[t.start..t.end];
        match &t.token {
            Token::Diag(_, _) => {}
            Token::Inline(s) => {
                // pipeline already emitted the wanted newline; inline
                // HTML is never indented — write it verbatim.
                self.out.push_str(s);
                self.bol = s.ends_with('\n');
                self.stmt_open = false;
                self.cont = false; // continuation indent belongs to PHP code
            }
            Token::Echo => {
                self.put("<?=");
                self.stmt_open = true;
            }
            Token::Ident(name) => return self.emit_ident(i, toks, t, name),
            Token::Op(op) => return self.emit_op(i, toks, t, op, raw, brace_kind),
            _ => {
                self.put(raw);
                self.stmt_open = true;
            }
        }
        0
    }

    fn emit_ident(&mut self, i: usize, toks: &[&Lexed], t: &Lexed, name: &str) -> usize {
        let low = name.to_ascii_lowercase();
        if END_KWS.contains(&low.as_str()) {
            self.put(name);
            self.stmt_open = false;
            return 0;
        }
        if low == "else" {
            // `else if` → `elseif` when nothing but ws sits between.
            if let Some(n) = toks.get(i + 1) {
                if let Token::Ident(w) = &n.token {
                    if w.eq_ignore_ascii_case("if") && gap_clean(&self.src[t.end..n.start]) {
                        self.put("elseif");
                        self.stmt_open = true;
                        return 1;
                    }
                }
            }
        }
        if matches!(low.as_str(), "case" | "default")
            && self.in_switch()
            && !matches!(prev_sig(toks, i).map(|p| &p.token), Some(Token::Op("::")))
        {
            self.put(name);
            self.in_case = true;
            self.stmt_open = true;
            return 0;
        }
        self.put(name);
        self.stmt_open = true;
        0
    }

    fn emit_op(
        &mut self,
        i: usize,
        toks: &[&Lexed],
        t: &Lexed,
        op: &str,
        raw: &str,
        brace_kind: Option<BraceKind>,
    ) -> usize {
        match op {
            ";" => {
                if raw == "?>" {
                    // close tag joins the line: `code; ?>` / `<?= $x ?>`.
                    // The next token joins too when adjacent (`?></p>`),
                    // or lands on its own line after a source newline.
                    if !self.bol && !self.out.ends_with(' ') {
                        self.out.push(' ');
                    }
                    self.put("?>");
                    self.stmt_open = true;
                    return 0;
                }
                self.put(";");
                if matches!(self.parens.last(), Some(Nest::For)) {
                    self.stmt_open = true; // `; ` inside `for (;;)`
                } else {
                    self.stmt_open = false;
                    self.hard_nl = true;
                }
                0
            }
            "{" => match brace_kind.unwrap_or(BraceKind::Inline) {
                BraceKind::Inline => {
                    self.parens.push(Nest::InlineBrace);
                    self.qmarks.resize(self.parens.len() + 1, 0);
                    self.put("{");
                    self.stmt_open = true;
                    0
                }
                _ => {
                    let head = self.head_of(toks, i);
                    let empty = matches!(toks.get(i + 1).map(|x| &x.token), Some(Token::Op("}")))
                        && toks
                            .get(i + 1)
                            .map(|n| gap_clean(&self.src[t.end..n.start]))
                            .unwrap_or(false);
                    if empty {
                        // `{}` stays on one line (ws-only gap)
                        self.put("{}");
                        self.last_head = Some(head);
                        self.stmt_open = false;
                        self.hard_nl = true;
                        return 1;
                    }
                    self.put("{");
                    self.blocks.push(head);
                    self.arm_arrow = false;
                    self.indent += 1;
                    self.hard_nl = true;
                    self.after_block_open = true;
                    self.stmt_open = false;
                    0
                }
            },
            "}" => {
                if matches!(self.parens.last(), Some(Nest::InlineBrace)) {
                    self.parens.pop();
                    self.qmarks.truncate(self.parens.len() + 1);
                    self.put("}");
                    self.stmt_open = true;
                    return 0;
                }
                let head = self.blocks.pop().unwrap_or(Head::Other);
                self.put("}");
                self.last_head = Some(head);
                self.stmt_open = false;
                self.hard_nl = true;
                0
            }
            "(" => {
                if let Some((cast, extra)) = self.cast_at(toks, i) {
                    self.put(&cast);
                    self.stmt_open = true;
                    return extra;
                }
                self.parens.push(self.paren_tag(toks, i));
                self.qmarks.resize(self.parens.len() + 1, 0);
                self.put("(");
                self.stmt_open = true;
                0
            }
            ")" => {
                self.last_paren = self.parens.pop();
                self.qmarks.truncate(self.parens.len() + 1);
                self.put(")");
                self.stmt_open = true;
                0
            }
            "]" => {
                let popped = self.parens.pop();
                self.qmarks.truncate(self.parens.len() + 1);
                self.put("]");
                // `#[...]` at statement level ends the line.
                if popped == Some(Nest::Attr) && self.parens.is_empty() {
                    self.hard_nl = true;
                    self.stmt_open = false;
                } else {
                    self.stmt_open = true;
                }
                0
            }
            "[" => {
                self.parens.push(Nest::Bracket);
                self.qmarks.resize(self.parens.len() + 1, 0);
                self.put("[");
                self.stmt_open = true;
                0
            }
            "#[" => {
                self.parens.push(Nest::Attr);
                self.qmarks.resize(self.parens.len() + 1, 0);
                self.put("#[");
                self.stmt_open = true;
                0
            }
            ":" => {
                self.colon(toks, i);
                0
            }
            "?" => self.question(toks, i),
            "," => {
                self.put(",");
                // Arm-separator `,` inside `match {` ends the line
                // (`1 => x,` → next arm); a `,` before `=>` is a key
                // list (`1, 2 =>` stays inline).
                if self.parens.is_empty()
                    && !self.in_case
                    && matches!(self.blocks.last(), Some(Head::Match))
                    && self.arm_arrow
                {
                    self.arm_arrow = false;
                    self.stmt_open = false;
                    self.hard_nl = true;
                } else {
                    self.stmt_open = true;
                }
                0
            }
            "=>" => {
                self.put("=>");
                if self.parens.is_empty() && matches!(self.blocks.last(), Some(Head::Match)) {
                    self.arm_arrow = true;
                }
                self.stmt_open = true;
                0
            }
            "&" | "|" => {
                self.put(op);
                if self.ref_or_union_next(toks, i) {
                    self.tight_next = true;
                }
                self.stmt_open = true;
                0
            }
            "!" | "~" | "@" | "$" | "..." => {
                // always prefix — `!$x`, `~$a`, `@f()`, `${x}`, `...$xs`
                self.put(raw);
                self.tight_next = true;
                self.stmt_open = true;
                0
            }
            "++" | "--" | "-" | "+" => {
                // tight-after only when prefix (`++$x`, `-$a`); postfix
                // `$x++` leaves a normal space before the next token.
                let unary = prev_sig(toks, i)
                    .map(|p| !is_operand(&p.token))
                    .unwrap_or(true);
                self.put(raw);
                if unary {
                    self.tight_next = true;
                }
                self.stmt_open = true;
                0
            }
            _ => {
                self.put(raw);
                self.stmt_open = true;
                0
            }
        }
    }

    /// `&`/`|` — tight after when this is a by-ref (`&$x`, `&name()`)
    /// or a union/intersection type (`A|B`, `A&B`). Binary
    /// `&`/`|` (`$a & $b`) stays spaced.
    fn ref_or_union_next(&self, toks: &[&Lexed], i: usize) -> bool {
        let Some(n) = toks.get(i + 1) else {
            return false;
        };
        let p = prev_sig(toks, i);
        match &n.token {
            // `&$x`/`|$x` tight unless the token before `&`/`|` is a
            // value (`$a & $b`, `f() | $x` are binary).
            Token::Variable(_) => !matches!(
                p.map(|x| &x.token),
                Some(Token::Variable(_))
                    | Some(Token::Int(_))
                    | Some(Token::Float(_))
                    | Some(Token::SimpleString(_))
                    | Some(Token::InterpString(_))
                    | Some(Token::Op(")" | "]"))
            ),
            Token::Ident(_) => {
                // `function &name`/`fn &x` by-ref, or `A|B`/`A&B` type.
                let prev_kw = matches!(p.map(|x| &x.token), Some(Token::Ident(w)) if {
                    matches!(w.to_ascii_lowercase().as_str(), "function" | "fn")
                });
                prev_kw || (p.map(|x| typeish(&x.token)).unwrap_or(false))
            }
            Token::Op("\\") | Token::Op("(") | Token::Op("?") => p
                .map(|x| {
                    matches!(&x.token, Token::Ident(w) if {
                        matches!(w.to_ascii_lowercase().as_str(), "function" | "fn")
                    }) || typeish(&x.token)
                })
                .unwrap_or(false),
            _ => false,
        }
    }

    /// `elseif (...):`/`else:` — reopen the closed arm of the top
    /// Alt block, or open a fresh one.
    fn alt_colon(&mut self) {
        match self.blocks.last_mut() {
            Some(Head::Alt { arm_open }) if !*arm_open => {
                *arm_open = true;
                self.indent += 1;
            }
            _ => {
                self.blocks.push(Head::Alt { arm_open: true });
                self.indent += 1;
            }
        }
        self.hard_nl = true;
    }

    /// `:` — return type / alt-syntax / case / ternary / enum / typed
    /// const / named-arg / goto label. Leading spacing is decided in
    /// `sep_special`; the space *after* comes from the pair table.
    fn colon(&mut self, toks: &[&Lexed], i: usize) {
        let prev = prev_sig(toks, i);
        // pending ternary `?` at this depth → the `:` is its else.
        if self.qmarks.get(self.parens.len()).copied().unwrap_or(0) > 0 {
            let d = self.parens.len();
            self.qmarks[d] -= 1;
            self.put(":");
            self.stmt_open = true;
            return;
        }
        // `)` + `:` → return type or alt-syntax block.
        if matches!(prev.map(|p| &p.token), Some(Token::Op(")"))) {
            match self.last_paren {
                Some(Nest::ElseIf) => {
                    self.put(":");
                    self.alt_colon();
                    self.stmt_open = false;
                }
                Some(Nest::If) | Some(Nest::For) | Some(Nest::Foreach) | Some(Nest::While)
                | Some(Nest::Declare) => {
                    self.put(":");
                    self.blocks.push(Head::Alt { arm_open: true });
                    self.indent += 1;
                    self.hard_nl = true;
                    self.stmt_open = false;
                }
                Some(Nest::Switch) => {
                    self.put(":");
                    self.blocks.push(Head::Switch { case_open: false });
                    self.indent += 1;
                    self.hard_nl = true;
                    self.stmt_open = false;
                }
                _ => {
                    // `function f(): int`, `fn(): T` — return type.
                    self.put(":");
                    self.stmt_open = true;
                }
            }
            return;
        }
        // Ternary `:` inside `case` heads is balanced before the
        // case-close `:` (e.g. `case $a ? 1 : 2:`).
        let d = self.parens.len();
        if self.qmarks.get(d).copied().unwrap_or(0) > 0 {
            self.qmarks[d] -= 1;
            self.put(":");
            self.stmt_open = true;
            return;
        }
        if self.in_case {
            self.put(":");
            self.in_case = false;
            if let Some(Head::Switch { case_open }) = self.blocks.last_mut() {
                *case_open = true;
                self.indent += 1;
            }
            self.hard_nl = true;
            self.stmt_open = false;
            return;
        }
        // `else:` alt-syntax
        if matches!(prev.map(|p| &p.token), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("else"))
        {
            self.put(":");
            self.alt_colon();
            self.stmt_open = false;
            return;
        }
        if matches!(prev.map(|p| &p.token), Some(Token::Ident(_))) {
            self.put(":");
            if !self.parens.is_empty() || self.type_colon_head(toks, i) {
                // named arg `x:` / `enum X: int` / `const X: int`
                self.stmt_open = true;
            } else {
                // goto label
                self.hard_nl = true;
                self.stmt_open = false;
            }
            return;
        }
        self.put(":");
        self.stmt_open = true;
    }

    /// The `:` after an ident that continues a statement — `enum X:` ,
    /// `const FOO: int`, `public int $x` handled elsewhere — check the
    /// token before that ident.
    fn type_colon_head(&self, toks: &[&Lexed], i: usize) -> bool {
        let Some((k, _)) = prev_sig_idx(toks, i) else {
            return false;
        };
        matches!(
            prev_sig(toks, k).map(|x| &x.token),
            Some(Token::Ident(w))
                if matches!(
                    w.to_ascii_lowercase().as_str(),
                    "enum" | "const" | "var" | "public" | "private" | "protected"
                        | "static" | "final" | "readonly" | "abstract"
                )
        )
    }

    /// `?` — ternary vs nullable type marker; `? :` merges to `?:`.
    fn question(&mut self, toks: &[&Lexed], i: usize) -> usize {
        let prev = prev_sig(toks, i);
        let mut ternary = prev.map(|p| is_operand(&p.token)).unwrap_or(false);
        if ternary {
            // `?` after a type-name keyword (`static ?array $x`,
            // `static ?\Closure $f`) is nullable when a type run
            // follows — `$a ? static : b` stays ternary.
            ternary = match prev.map(|p| &p.token) {
                Some(Token::Ident(w)) if TYPE_NAMES.contains(&w.to_ascii_lowercase().as_str()) => {
                    let mut j = i + 1;
                    while toks.get(j).map(|n| typeish(&n.token)).unwrap_or(false) {
                        j += 1;
                    }
                    !matches!(
                        toks.get(j).map(|n| &n.token),
                        Some(Token::Variable(_))
                            | Some(Token::Op("=" | "," | ")" | "[" | "&" | "|"))
                    )
                }
                _ => true,
            };
        }
        if !ternary {
            // nullable `?int` — tight to the type that follows
            self.put("?");
            self.tight_next = true;
            self.stmt_open = true;
            return 0;
        }
        // Elvis `? :` → `?:` (only across clean whitespace)
        if let Some(n) = toks.get(i + 1) {
            if matches!(n.token, Token::Op(":")) && gap_clean(&self.src[toks[i].end..n.start]) {
                self.put("?:");
                self.stmt_open = true;
                return 1;
            }
        }
        self.put("?");
        let d = self.parens.len();
        if self.qmarks.len() <= d {
            self.qmarks.resize(d + 1, 0);
        }
        self.qmarks[d] += 1;
        self.stmt_open = true;
        0
    }

    /// `{` classification — runs before the separator so a Decl brace
    /// can force its own line.
    fn classify_brace(&self, toks: &[&Lexed], i: usize) -> BraceKind {
        let Some(p) = prev_sig(toks, i) else {
            return BraceKind::Block;
        };
        match &p.token {
            Token::Op(")") => match self.last_paren {
                Some(Nest::Function { named: true }) => BraceKind::Decl,
                _ => BraceKind::Block,
            },
            Token::Ident(w) => match w.to_ascii_lowercase().as_str() {
                "else" | "try" | "finally" | "do" => BraceKind::Block,
                _ => match self.head_keyword(toks, i) {
                    Some("interface") | Some("trait") | Some("enum") | Some("function") => {
                        BraceKind::Decl
                    }
                    Some("class") => {
                        if self.anon_class(toks, i) {
                            BraceKind::Block
                        } else {
                            BraceKind::Decl
                        }
                    }
                    // `use Trait { ... }` adaptation, `namespace N {`,
                    // closure / anon-class braces on the same line.
                    Some("use") | Some("namespace") | Some("closure") | Some("newclass") => {
                        BraceKind::Block
                    }
                    _ => BraceKind::Inline,
                },
            },
            // `Foo\{Bar}` use-group — always inline; `label: {` /
            // `case 1: {` — statement block.
            Token::Op("\\") => BraceKind::Inline,
            Token::Op(":") => BraceKind::Block,
            Token::Op(";") | Token::Op("{") | Token::Op("}") => BraceKind::Block,
            _ => BraceKind::Inline,
        }
    }

    /// Scan back over a declaration head (`class X extends Y implements
    /// Z`, `function f(): ?T`, `use A\{B}`) for the introducing
    /// keyword. Matched `(...)` groups are skipped; return types
    /// (`: ?T`, `: A|B`) are skipped.
    fn head_keyword(&self, toks: &[&Lexed], i: usize) -> Option<&'static str> {
        let mut j = i;
        while let Some((k, p)) = prev_sig_idx(toks, j) {
            j = k;
            match &p.token {
                Token::Ident(w) => {
                    let low = w.to_ascii_lowercase();
                    match low.as_str() {
                        "class" => return Some("class"),
                        "interface" => return Some("interface"),
                        "trait" => return Some("trait"),
                        "enum" => return Some("enum"),
                        "use" => return Some("use"),
                        "namespace" => return Some("namespace"),
                        "extends" | "implements" | "readonly" | "abstract" | "final" => {}
                        _ => {
                            // type names (`?array`, `\Ns`, `static`) skip;
                            // other keywords end the head.
                            if !typeish(&p.token) {
                                return None;
                            }
                        }
                    }
                }
                Token::Op(")") => {
                    // jump to the matching `(` and classify its opener
                    let mut depth = 1usize;
                    let mut m = k;
                    while m > 0 && depth > 0 {
                        m -= 1;
                        match &toks[m].token {
                            Token::Op(")") => depth += 1,
                            Token::Op("(") => depth -= 1,
                            _ => {}
                        }
                    }
                    if depth > 0 {
                        return None;
                    }
                    match self.paren_tag(toks, m) {
                        Nest::Function { named: true } => return Some("function"),
                        Nest::Function { named: false } | Nest::Use | Nest::Fn => {
                            return Some("closure")
                        }
                        Nest::New => return Some("newclass"),
                        _ => {
                            j = m;
                            continue;
                        }
                    }
                }
                Token::Op(",")
                | Token::Op("\\")
                | Token::Op(":")
                | Token::Op("?")
                | Token::Op("|")
                | Token::Op("&") => {}
                _ => return None,
            }
        }
        None
    }

    /// The `class` keyword in this head belongs to `new class`.
    fn anon_class(&self, toks: &[&Lexed], i: usize) -> bool {
        let mut j = i;
        while let Some((k, p)) = prev_sig_idx(toks, j) {
            j = k;
            match &p.token {
                Token::Ident(w) if w.eq_ignore_ascii_case("class") => {
                    return matches!(prev_sig(toks, k).map(|x| &x.token), Some(Token::Ident(n)) if n.eq_ignore_ascii_case("new"));
                }
                // stop at statement boundaries — don't scan into
                // earlier code
                Token::Op(";") | Token::Op("{") | Token::Op("}") => return false,
                _ => {}
            }
        }
        false
    }

    /// Head construct for the `{` at toks[i] (for `}`-join rules).
    fn head_of(&self, toks: &[&Lexed], i: usize) -> Head {
        match self.classify_brace(toks, i) {
            BraceKind::Decl => return Head::Decl,
            BraceKind::Inline => return Head::Other,
            _ => {}
        }
        if let Some(p) = prev_sig(toks, i) {
            match &p.token {
                Token::Ident(w) => match w.to_ascii_lowercase().as_str() {
                    "do" => Head::Do,
                    _ => Head::Other,
                },
                Token::Op(")") => match self.last_paren {
                    Some(Nest::Switch) => Head::Switch { case_open: false },
                    Some(Nest::Match) => Head::Match,
                    _ => Head::Other,
                },
                _ => Head::Other,
            }
        } else {
            Head::Other
        }
    }

    /// Tag the `(` at toks[i] by the token before it.
    fn paren_tag(&self, toks: &[&Lexed], i: usize) -> Nest {
        let Some(p) = prev_sig(toks, i) else {
            return Nest::Group;
        };
        let Token::Ident(w) = &p.token else {
            return if is_operand(&p.token) {
                Nest::Call
            } else {
                Nest::Group
            };
        };
        match w.to_ascii_lowercase().as_str() {
            "if" => Nest::If,
            // `elseif (` and merged `else if (` (prev is `else`)
            "elseif" | "else" => Nest::ElseIf,
            "for" => Nest::For,
            "foreach" => Nest::Foreach,
            "while" => Nest::While,
            "switch" => Nest::Switch,
            "match" => Nest::Match,
            "catch" => Nest::Catch,
            "declare" => Nest::Declare,
            "fn" => Nest::Fn,
            "use" => Nest::Use,
            "function" => Nest::Function { named: false },
            "new" | "class" => Nest::New,
            _ => {
                // `name (` — call, or `function name (` decl: scan back
                // over an optional `&` to the token before the name.
                let Some((k, _)) = prev_sig_idx(toks, i) else {
                    return Nest::Call;
                };
                match prev_sig(toks, k).map(|x| &x.token) {
                    Some(Token::Ident(b)) if b.eq_ignore_ascii_case("function") => {
                        Nest::Function { named: true }
                    }
                    Some(Token::Ident(b)) if b.eq_ignore_ascii_case("new") => Nest::New,
                    Some(Token::Op("&")) => {
                        let Some((k2, _)) = prev_sig_idx(toks, k) else {
                            return Nest::Call;
                        };
                        match prev_sig(toks, k2).map(|x| &x.token) {
                            Some(Token::Ident(b)) if b.eq_ignore_ascii_case("function") => {
                                Nest::Function { named: true }
                            }
                            _ => Nest::Call,
                        }
                    }
                    _ => Nest::Call,
                }
            }
        }
    }

    /// `(int)`/`(array)`/... cast at toks[i]: returns the raw cast text
    /// and the number of extra tokens consumed (type + `)`).
    fn cast_at(&self, toks: &[&Lexed], i: usize) -> Option<(String, usize)> {
        let mid = toks.get(i + 1)?;
        let end = toks.get(i + 2)?;
        let Token::Ident(name) = &mid.token else {
            return None;
        };
        if !matches!(&end.token, Token::Op(")")) {
            return None;
        }
        let low = name.to_ascii_lowercase();
        if !CAST_TYPES.contains(&low.as_str()) {
            return None;
        }
        if prev_sig(toks, i)
            .map(|p| is_operand(&p.token))
            .unwrap_or(false)
        {
            return None;
        }
        // gaps must be whitespace-only
        let g1 = &self.src[toks[i].end..mid.start];
        let g2 = &self.src[mid.end..end.start];
        if !gap_clean(g1) || !gap_clean(g2) {
            return None;
        }
        Some((format!("({})", low), 2))
    }

    /// Close an open `case` body indent on the switch at blocks.top.
    fn close_case(&mut self) {
        if let Some(Head::Switch { case_open }) = self.blocks.last_mut() {
            if *case_open {
                *case_open = false;
                if self.indent > 0 {
                    self.indent -= 1;
                }
            }
        }
    }

    fn in_switch(&self) -> bool {
        matches!(self.blocks.last(), Some(Head::Switch { .. }))
    }

    fn newline(&mut self) {
        if !self.bol {
            self.out.push('\n');
            self.bol = true;
        }
    }

    /// At start of a line — `out` empty, ends with `\n`, or only
    /// whitespace since the last `\n`.
    fn at_line_start(&self) -> bool {
        let tail = match self.out.rfind('\n') {
            Some(i) => &self.out[i + 1..],
            None => self.out.as_str(),
        };
        tail.trim_start_matches([' ', '\t']).is_empty()
    }

    /// Emit text, writing the indent first when at line start.
    fn put(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        if self.bol {
            let pad = (self.indent + self.cont as usize) * 4;
            for _ in 0..pad {
                self.out.push(' ');
            }
            self.bol = false;
        }
        self.cont = false;
        self.out.push_str(s);
    }

    /// Emit gap atoms (comments, open tags). Newline counts only feed
    /// `last_nl` for the following `sep`.
    fn emit_atoms(&mut self, atoms: &[Atom]) {
        let mut nl_run = 0usize;
        // trailing ws after a comment → next token on a new line
        let mut last_comment = false;
        for a in atoms {
            match a {
                Atom::Ws(n) => nl_run += n,
                Atom::LineComment(text) => {
                    if self.bol {
                        if nl_run >= 2 && !self.after_block_open {
                            self.out.push('\n'); // keep a blank line above
                        }
                        self.put(text);
                    } else if nl_run == 0 {
                        // trailing `// c` — one space before it, none
                        // right after an open `(`/`[`/`{`.
                        if !matches!(self.out.chars().last(), Some('(' | '[' | '{' | ' ')) {
                            self.out.push(' ');
                        }
                        self.put(text);
                    } else {
                        // own-line comment — keep one blank line above
                        self.newline();
                        if nl_run >= 2 && !self.after_block_open {
                            self.out.push('\n');
                        }
                        self.put(text);
                    }
                    self.newline();
                    nl_run = 0;
                    last_comment = true;
                }
                Atom::BlockComment(text) => {
                    if self.bol {
                        if nl_run >= 2 && !self.after_block_open {
                            self.out.push('\n');
                        }
                        self.put_block(text);
                    } else if nl_run == 0 {
                        if !matches!(self.out.chars().last(), Some('(' | '[' | '{' | ' ')) {
                            self.out.push(' ');
                        }
                        self.put_block(text);
                    } else {
                        self.newline();
                        if nl_run >= 2 && !self.after_block_open {
                            self.out.push('\n');
                        }
                        self.put_block(text);
                    }
                    nl_run = 0;
                    last_comment = true;
                }
                Atom::OpenTag(text) => {
                    if !self.at_line_start() {
                        self.newline();
                    }
                    self.put(text);
                    self.newline();
                    nl_run = 0;
                    last_comment = false;
                }
                Atom::Raw(text) => {
                    self.put(text);
                    last_comment = false;
                }
            }
        }
        if last_comment && nl_run >= 1 {
            self.hard_nl = true;
        }
    }

    /// Emit a `/* */`/`/** */` comment; continuation lines re-indent.
    fn put_block(&mut self, text: &str) {
        let mut lines = text.lines();
        if let Some(first) = lines.next() {
            self.put(first.trim_end());
        }
        for line in lines {
            self.newline();
            let t = line.trim();
            if t.starts_with('*') {
                self.out.push(' ');
            }
            self.put(t);
        }
    }
}

// ---------- free helpers ----------

/// Previous significant (non-Diag) token before toks[i].
fn prev_sig<'t>(toks: &[&'t Lexed], i: usize) -> Option<&'t Lexed> {
    prev_sig_idx(toks, i).map(|x| x.1)
}

fn prev_sig_idx<'t>(toks: &[&'t Lexed], i: usize) -> Option<(usize, &'t Lexed)> {
    let mut j = i;
    while j > 0 {
        j -= 1;
        if !matches!(toks[j].token, Token::Diag(_, _)) {
            return Some((j, toks[j]));
        }
    }
    None
}

/// `true` for tokens that end an operand — tells ternary `?` from
/// nullable `?`, and casts from groups.
fn is_operand(t: &Token) -> bool {
    match t {
        Token::Variable(_)
        | Token::Int(_)
        | Token::Float(_)
        | Token::SimpleString(_)
        | Token::InterpString(_) => true,
        Token::Op(o) => matches!(*o, ")" | "]"),
        Token::Ident(w) => {
            !is_keyword(&w.to_ascii_lowercase())
                || VALUE_KWS.contains(&w.to_ascii_lowercase().as_str())
        }
        _ => false,
    }
}

/// Type-ish token (non-keyword Ident / type-name keyword / `\` / `?`)
/// — union/intersection detection.
fn typeish(t: &Token) -> bool {
    match t {
        Token::Ident(w) => {
            let low = w.to_ascii_lowercase();
            !is_keyword(&low)
                || TYPE_NAMES.contains(&low.as_str())
                || matches!(low.as_str(), "array" | "callable" | "iterable")
        }
        Token::Op(o) => matches!(*o, "\\" | "?" | "(" | ")"),
        _ => false,
    }
}

/// Gap between two tokens is whitespace-only (merge-safe).
fn gap_clean(gap: &str) -> bool {
    gap.chars().all(|c| c.is_whitespace())
}

/// Coarse token class for the spacing table.
#[derive(Clone, Copy)]
enum K {
    Ident,
    Var,
    Int,
    Float,
    Str,
    Inline,
    Op(&'static str),
    Other,
}

fn kind(t: &Lexed) -> K {
    match &t.token {
        Token::Ident(_) => K::Ident,
        Token::Variable(_) => K::Var,
        Token::Int(_) => K::Int,
        Token::Float(_) => K::Float,
        Token::SimpleString(_) | Token::InterpString(_) => K::Str,
        Token::Inline(_) => K::Inline,
        Token::Echo => K::Other,
        Token::Op(o) => K::Op(o),
        _ => K::Other,
    }
}

/// Whitespace runs and trivia found between two token spans.
enum Atom {
    Ws(usize),
    /// `//`/`#` through end of line (newline not included).
    LineComment(String),
    /// `/* */`/`/** */` (may contain newlines).
    BlockComment(String),
    /// `<?php`/`<?` open tag in a gap — emitted verbatim on its own line.
    OpenTag(&'static str),
    /// Anything unexpected is preserved verbatim, never dropped.
    Raw(String),
}

/// Split gap text into atoms; also returns the newline count of the
/// whitespace run after the last comment/tag.
fn gap_atoms(gap: &str) -> (Vec<Atom>, usize) {
    let b = gap.as_bytes();
    let mut atoms = Vec::new();
    let mut i = 0usize;
    let mut ws = 0usize;
    macro_rules! flush_ws {
        () => {
            if ws > 0 {
                atoms.push(Atom::Ws(ws));
                ws = 0;
            }
        };
    }
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\r' => i += 1,
            b'\n' => {
                ws += 1;
                i += 1;
            }
            b'#' | b'/' if line_comment_at(b, i) => {
                flush_ws!();
                let start = i;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                atoms.push(Atom::LineComment(gap[start..i].trim_end().to_string()));
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                flush_ws!();
                let start = i;
                i += 2;
                while i < b.len() {
                    if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                atoms.push(Atom::BlockComment(gap[start..i].to_string()));
            }
            b'<' if gap[i..].starts_with("<?php") => {
                flush_ws!();
                atoms.push(Atom::OpenTag("<?php"));
                i += 5;
            }
            b'<' if gap[i..].starts_with("<?") && !gap[i..].starts_with("<?=") => {
                flush_ws!();
                atoms.push(Atom::OpenTag("<?"));
                i += 2;
            }
            _ => {
                flush_ws!();
                let start = i;
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                atoms.push(Atom::Raw(gap[start..i].to_string()));
            }
        }
    }
    if ws > 0 {
        atoms.push(Atom::Ws(ws));
    }
    let last_nl = atoms
        .iter()
        .rev()
        .take_while(|a| matches!(a, Atom::Ws(_)))
        .map(|a| match a {
            Atom::Ws(n) => *n,
            _ => 0,
        })
        .sum();
    (atoms, last_nl)
}

fn line_comment_at(b: &[u8], i: usize) -> bool {
    match b[i] {
        b'#' => true,
        b'/' => b.get(i + 1) == Some(&b'/'),
        _ => false,
    }
}

/// Unified-context diff (`--- path` / `+++ path` headers, `@@` hunks)
/// between `old` and `new` line sets — for `phpun fmt --diff` and
/// `--check` diagnostics. Line-based LCS, files are small.
pub fn unified_diff(old: &str, new: &str, ctx: usize) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // LCS table — O(n*m); PHP sources formatted by this tool are small.
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    // Walk to an edit script of Keep/Add/Del ops.
    #[derive(Clone, Copy, PartialEq)]
    enum Op {
        Keep,
        Add,
        Del,
    }
    let mut ops: Vec<(Op, &str)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push((Op::Keep, a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push((Op::Del, a[i]));
            i += 1;
        } else {
            ops.push((Op::Add, b[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push((Op::Del, a[i]));
        i += 1;
    }
    while j < m {
        ops.push((Op::Add, b[j]));
        j += 1;
    }
    // Group changed runs with `ctx` context lines into hunks.
    let changed: Vec<usize> = (0..ops.len()).filter(|&k| ops[k].0 != Op::Keep).collect();
    if changed.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    let mut start = 0usize;
    while start < changed.len() {
        let mut end = start;
        // merge changes closer than 2*ctx keep-lines
        while end + 1 < changed.len() && changed[end + 1] - changed[end] <= 2 * ctx + 1 {
            end += 1;
        }
        let lo = changed[start].saturating_sub(ctx);
        let hi = (changed[end] + ctx + 1).min(ops.len());
        let a_start = 1 + ops[..lo].iter().filter(|o| o.0 != Op::Add).count();
        let b_start = 1 + ops[..lo].iter().filter(|o| o.0 != Op::Del).count();
        let a_len = ops[lo..hi].iter().filter(|o| o.0 != Op::Add).count();
        let b_len = ops[lo..hi].iter().filter(|o| o.0 != Op::Del).count();
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            a_start, a_len, b_start, b_len
        ));
        for (op, line) in &ops[lo..hi] {
            let tag = match op {
                Op::Keep => ' ',
                Op::Del => '-',
                Op::Add => '+',
            };
            out.push_str(&format!("{}{}\n", tag, line));
        }
        start = end + 1;
    }
    out
}
