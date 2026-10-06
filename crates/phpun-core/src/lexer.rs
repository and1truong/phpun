use crate::error::PhpError;

/// A single token. Literal payloads keep their cooked values where cheap
/// (numbers) and raw otherwise (strings are decoded by the parser).
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// Inline HTML outside `<?php ... ?>`; echoed verbatim on output.
    Inline(String),
    /// `$name` — name without the sigil.
    Variable(String),
    /// Bare identifier / keyword text.
    Ident(String),
    Int(i64),
    Float(f64),
    /// Single-quoted string: only \\ and \' escapes.
    SimpleString(String),
    /// Double-quoted / heredoc content as parts for interpolation.
    InterpString(Vec<StringPart>),
    /// `<?=` echo tag — emitted as Token::Echo by the lexer.
    Echo,
    /// Compile-time diagnostic emitted by the scanner (octal overflow,
    /// etc.) — drained by `parse_with` and prepended as `Stmt::Diag`
    /// so it prints before execution like Zend compile warnings.
    Diag(&'static str, String),
    Op(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    /// Literal bytes — escapes decode to raw bytes (PHP strings are
    /// byte arrays; `\xNN` is a byte, not a codepoint).
    Lit(Vec<u8>),
    /// `$name` — the usize is the `$`'s absolute line (diagnostics
    /// while reading the variable site there, Zend's per-op lines).
    Var(String, usize),
    /// `{$expr_source}` — re-lexed lazily by the parser. The usize is
    /// the embedded source's absolute start line: the snippet re-lexes
    /// with snippet-relative (1-based) lines, and diagnostics rebase
    /// onto this so they report file lines.
    Expr(String, usize),
    /// `${expr_source}` — deprecated variable-variable interpolation
    /// (evaluates the expr to a *name*, then reads that variable).
    DollarBraceExpr(String, usize),
}

#[derive(Debug, Clone)]
pub struct Lexed {
    pub token: Token,
    pub line: usize,
    /// For `\` name separators: bit 1 = whitespace on the left in source,
    /// bit 2 = whitespace on the right. Qualified names forbid whitespace
    /// inside them (namespaced_name_whitespace).
    pub ws_adj: u8,
    /// Byte offsets into the lexed source; `usize::MAX` for synthesized
    /// tokens (compile-time diagnostics). `phpun fmt` uses them to
    /// recover raw token text and the trivia between tokens.
    pub start: usize,
    pub end: usize,
}

const KEYWORDS: &[&str] = &[
    "echo",
    "if",
    "else",
    "elseif",
    "while",
    "for",
    "foreach",
    "as",
    "function",
    "return",
    "true",
    "false",
    "null",
    "and",
    "or",
    "xor",
    "break",
    "continue",
    "do",
    "switch",
    "case",
    "default",
    "global",
    "static",
    "const",
    "new",
    "class",
    "extends",
    "implements",
    "interface",
    "trait",
    "use",
    "namespace",
    "try",
    "catch",
    "finally",
    "throw",
    "instanceof",
    "print",
    "isset",
    "unset",
    "empty",
    "list",
    "array",
    "declare",
    "include",
    "include_once",
    "require",
    "require_once",
    "fn",
    "match",
    "enum",
    "readonly",
    "yield",
    "from",
    "clone",
    "public",
    "private",
    "protected",
    "abstract",
    "final",
    "var",
    "goto",
    "die",
    "exit",
    "eval",
    "insteadof",
];

/// Two-mode PHP lexer: outside `<?php`/`<?=`/`<?` everything is inline HTML.
pub fn lex(src: &str) -> Result<Vec<Lexed>, PhpError> {
    lex_with(src, false)
}

/// `lex` with `short_open_tag` — when on, `<?` opens PHP like `<?php`.
pub fn lex_with(src: &str, short_open: bool) -> Result<Vec<Lexed>, PhpError> {
    let (src, shebang) = strip_shebang(src);
    let mut out = Vec::new();
    // The shebang occupies line 1; real numbering starts at line 2.
    let mut line = if shebang { 2 } else { 1 };
    scan_html(src, 0, &mut line, &mut out, short_open)?;
    Ok(out)
}

fn strip_shebang(src: &str) -> (&str, bool) {
    // CLI PHP skips a leading `#!...` shebang line (tests/lang/bug23584).
    match src.strip_prefix("#!") {
        Some(rest) => match rest.find('\n') {
            Some(nl) => (&rest[nl + 1..], true),
            None => ("", true),
        },
        None => (src, false),
    }
}

/// phpun source mode: the file is PHP code from byte 0 — no `<?php` tag
/// required. A leading `<?php` tag opts back into legacy tag mode so
/// mixed/HTML-embedded sources (and the PHPT corpus) keep working; `?>`
/// mid-file still drops to inline output like classic PHP.
pub fn lex_php_source(src: &str, short_open: bool) -> Result<Vec<Lexed>, PhpError> {
    let (body, shebang) = strip_shebang(src);
    if body.len() >= 5 && body[..5].eq_ignore_ascii_case("<?php") && boundary(body, 5) {
        return lex_with(src, short_open);
    }
    let src = body;
    let mut out = Vec::new();
    let mut line = if shebang { 2 } else { 1 };
    let pos = lex_php(src, 0, &mut line, &mut out)?;
    if pos < src.len() {
        scan_html(src, pos, &mut line, &mut out, short_open)?;
    }
    Ok(out)
}

/// Inline-HTML scanning: everything outside `<?php`/`<?=`/`<?` is echoed.
fn scan_html(
    src: &str,
    mut pos: usize,
    line: &mut usize,
    out: &mut Vec<Lexed>,
    short_open: bool,
) -> Result<(), PhpError> {
    let bytes = src.as_bytes();
    while pos < bytes.len() {
        // Inline HTML until an open tag.
        let rest = &src[pos..];
        match rest.find("<?") {
            None => {
                push(
                    out,
                    Token::Inline(rest.to_string()),
                    *line,
                    pos,
                    bytes.len(),
                );
                pos = bytes.len();
            }
            Some(off) => {
                if off > 0 {
                    let html = &rest[..off];
                    *line += html.matches('\n').count();
                    push(out, Token::Inline(html.to_string()), *line, pos, pos + off);
                }
                let tag_at = pos + off;
                let after = &src[tag_at..];
                if after.len() >= 5
                    && after[..5].eq_ignore_ascii_case("<?php")
                    && boundary(after, 5)
                {
                    pos = tag_at + 5;
                    pos += skip_ws_and_newline(&src[pos..], line);
                    pos = lex_php(src, pos, line, out)?;
                } else if after.starts_with("<?=") {
                    pos = tag_at + 3;
                    push(out, Token::Echo, *line, tag_at, tag_at + 3);
                    pos = lex_php(src, pos, line, out)?;
                } else if short_open
                    && (rest[off..].starts_with("<?\n")
                        || rest[off..].starts_with("<?\r")
                        || rest[off..].starts_with("<?\t")
                        || rest[off..].starts_with("<? "))
                {
                    // `<?` with short_open_tag=on opens PHP mode.
                    pos = tag_at + 2;
                    pos += skip_ws_and_newline(&src[pos..], line);
                    pos = lex_php(src, pos, line, out)?;
                } else {
                    push(
                        out,
                        Token::Inline("<?".to_string()),
                        *line,
                        tag_at,
                        tag_at + 2,
                    );
                    pos = tag_at + 2;
                }
            }
        }
    }
    Ok(())
}

fn boundary(s: &str, n: usize) -> bool {
    match s.as_bytes().get(n) {
        None => true,
        Some(&b) => b == b' ' || b == b'\t' || b == b'\n' || b == b'\r',
    }
}

fn skip_ws_and_newline(s: &str, line: &mut usize) -> usize {
    let mut n = 0;
    for &b in s.as_bytes() {
        match b {
            b' ' | b'\t' | b'\r' => n += 1,
            b'\n' => {
                n += 1;
                *line += 1;
                break; // PHP skips at most the first newline after `<?php`
            }
            _ => break,
        }
    }
    n
}

fn push(out: &mut Vec<Lexed>, token: Token, line: usize, start: usize, end: usize) {
    out.push(Lexed {
        token,
        line,
        ws_adj: 0,
        start,
        end,
    });
}

/// Lex PHP code mode starting at `pos`; returns the offset where PHP mode
/// ends (at the `?>` close tag, which is consumed).
fn lex_php(
    src: &str,
    mut pos: usize,
    line: &mut usize,
    out: &mut Vec<Lexed>,
) -> Result<usize, PhpError> {
    let b = src.as_bytes();
    loop {
        let Some(&c) = b.get(pos) else { return Ok(pos) };
        match c {
            b' ' | b'\t' | b'\r' => pos += 1,
            b'\n' => {
                *line += 1;
                pos += 1;
            }
            b'#' if b.get(pos + 1) == Some(&b'[') => {
                // PHP 8 attribute `#[...]` — a real token, not a comment.
                push(out, Token::Op("#["), *line, pos, pos + 2);
                pos += 2;
            }
            b'#' => {
                while matches!(b.get(pos), Some(&x) if x != b'\n') {
                    pos += 1;
                }
            }
            b'/' if b.get(pos + 1) == Some(&b'/') => {
                while matches!(b.get(pos), Some(&x) if x != b'\n') {
                    pos += 1;
                }
            }
            b'/' if b.get(pos + 1) == Some(&b'*') => {
                let start_line = *line;
                pos += 2;
                loop {
                    match b.get(pos) {
                        None => {
                            return Err(PhpError::parse(
                                "syntax error, unexpected end of file, unterminated comment",
                                start_line,
                            ))
                        }
                        Some(&x) if x == b'*' && b.get(pos + 1) == Some(&b'/') => {
                            pos += 2;
                            break;
                        }
                        Some(&x) => {
                            if x == b'\n' {
                                *line += 1;
                            }
                            pos += 1;
                        }
                    }
                }
            }
            b'?' if b.get(pos + 1) == Some(&b'>') => {
                pos += 2;
                // `?>` implies end of statement; a single following newline is
                // swallowed by PHP (it is part of the close tag).
                push(out, Token::Op(";"), *line, pos - 2, pos);
                if b.get(pos) == Some(&b'\n') {
                    pos += 1;
                    *line += 1;
                } else if b.get(pos) == Some(&b'\r') && b.get(pos + 1) == Some(&b'\n') {
                    pos += 2;
                    *line += 1;
                }
                return Ok(pos);
            }
            b'$' => {
                let (name, n) = ident(src, pos + 1);
                if name.is_empty() {
                    push(out, Token::Op("$"), *line, pos, pos + 1);
                    pos += 1;
                } else {
                    push(out, Token::Variable(name), *line, pos, pos + 1 + n);
                    pos += 1 + n;
                }
            }
            b'0'..=b'9' => {
                let (tok, n) = number(src, pos, *line)?;
                out.push(Lexed {
                    ws_adj: 0,
                    token: tok,
                    line: *line,
                    start: pos,
                    end: pos + n,
                });
                pos += n;
            }
            b'.' if matches!(b.get(pos + 1), Some(&x) if x.is_ascii_digit()) => {
                let (tok, n) = number(src, pos, *line)?;
                out.push(Lexed {
                    ws_adj: 0,
                    token: tok,
                    line: *line,
                    start: pos,
                    end: pos + n,
                });
                pos += n;
            }
            b'\'' => {
                let (s, n) = single_string(src, pos, *line)?;
                push(out, Token::SimpleString(s), *line, pos, pos + n);
                *line += s_matches(&src[pos..pos + n]);
                pos += n;
            }
            b'"' => {
                let start = *line;
                let (parts, n) = double_string(src, pos, start, out)?;
                *line += s_matches(&src[pos..pos + n]);
                push(out, Token::InterpString(parts), start, pos, pos + n);
                pos += n;
            }
            b'`' => {
                return Err(PhpError::parse(
                    "syntax error, unexpected '`' (backtick execution not supported)",
                    *line,
                ));
            }
            b'<' if src[pos..].starts_with("<<<") => {
                let start = *line;
                let (tok, n) = heredoc(src, pos, start, out)?;
                *line += s_matches(&src[pos..pos + n]);
                push(out, tok, start, pos, pos + n);
                pos += n;
            }
            _ => {
                if src[pos..].starts_with("b<<<") {
                    // `b` binary-string prefix: accepted and ignored
                    // (heredoc_002, nowdoc_002).
                    let start = *line;
                    let (tok, n) = heredoc(src, pos + 1, start, out)?;
                    *line += s_matches(&src[pos..pos + n + 1]);
                    push(out, tok, start, pos, pos + n + 1);
                    pos += n + 1;
                    continue;
                }
                if c == b'_' || c.is_ascii_alphabetic() || c >= 0x80 {
                    let (name, n) = ident(src, pos);
                    push(out, Token::Ident(name), *line, pos, pos + n);
                    pos += n;
                } else {
                    let (op, n) = operator(src, pos).ok_or_else(|| {
                        // Non-printable bytes report as
                        // `unexpected character 0x7F` (bug71897).
                        let msg = if !(0x20..0x7f).contains(&c) {
                            format!("syntax error, unexpected character 0x{:02X}", c)
                        } else {
                            format!("syntax error, unexpected '{}'", c as char)
                        };
                        PhpError::parse(msg, *line)
                    })?;
                    push(out, Token::Op(op), *line, pos, pos + n);
                    if op == "\\" {
                        let lt = out.last_mut().unwrap();
                        if pos > 0 && b[pos - 1].is_ascii_whitespace() {
                            lt.ws_adj |= 1;
                        }
                        if b.get(pos + n).is_some_and(|c| c.is_ascii_whitespace()) {
                            lt.ws_adj |= 2;
                        }
                    }
                    pos += n;
                }
            }
        }
    }
}

/// Count logical line breaks: `\n`, `\r\n`, and lone `\r` each
/// count once (heredoc bodies in eval'd code use all three —
/// heredoc_nowdoc/bug79934).
fn s_matches(s: &str) -> usize {
    let b = s.as_bytes();
    let mut n = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\n' => n += 1,
            b'\r' => {
                n += 1;
                if b.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    n
}

fn ident(src: &str, pos: usize) -> (String, usize) {
    let b = src.as_bytes();
    let mut n = 0;
    while let Some(&c) = b.get(pos + n) {
        if c == b'_' || c.is_ascii_alphanumeric() || c >= 0x80 {
            n += 1;
        } else {
            break;
        }
    }
    (src[pos..pos + n].to_string(), n)
}

fn number(src: &str, pos: usize, line: usize) -> Result<(Token, usize), PhpError> {
    let b = src.as_bytes();
    let s = &src[pos..];
    // Hex / binary / octal literals.
    // Base-prefixed literals allow `_` separators and overflow to float
    // (tests/lang/integer_literals/*_64bit.phpt).
    fn radix_lit(digits: &str, radix: u32, line: usize) -> Result<Token, PhpError> {
        let clean: String = digits.chars().filter(|c| *c != '_').collect();
        match i64::from_str_radix(&clean, radix) {
            Ok(v) => Ok(Token::Int(v)),
            Err(_) if !clean.is_empty() && clean.chars().all(|c| c.is_digit(radix)) => {
                let v = clean.chars().fold(0.0f64, |a, c| {
                    a * radix as f64 + c.to_digit(radix).unwrap_or(0) as f64
                });
                Ok(Token::Float(v))
            }
            Err(_) => Err(PhpError::parse(
                "syntax error, invalid numeric literal",
                line,
            )),
        }
    }
    if s.starts_with("0x") || s.starts_with("0X") {
        let mut n = 2;
        while matches!(b.get(pos + n), Some(&c) if c.is_ascii_hexdigit() || c == b'_') {
            n += 1;
        }
        return radix_lit(&s[2..n], 16, line).map(|t| (t, n));
    }
    if s.starts_with("0b") || s.starts_with("0B") {
        let mut n = 2;
        while matches!(b.get(pos + n), Some(&c) if c == b'0' || c == b'1' || c == b'_') {
            n += 1;
        }
        return radix_lit(&s[2..n], 2, line).map(|t| (t, n));
    }
    // Legacy octal `0o`/implicit `0...` — PHP 8.1+ also has explicit `0o`.
    if (s.starts_with("0o") || s.starts_with("0O")) && s.len() > 2 {
        let mut n = 2;
        while matches!(b.get(pos + n), Some(&c) if (b'0'..=b'7').contains(&c) || c == b'_') {
            n += 1;
        }
        return radix_lit(&s[2..n], 8, line).map(|t| (t, n));
    }
    let mut n = 0;
    let mut is_float = false;
    while matches!(b.get(pos + n), Some(&c) if c.is_ascii_digit() || c == b'_') {
        n += 1;
    }
    if b.get(pos + n) == Some(&b'.')
        && !matches!(b.get(pos + n + 1), Some(&c) if c.is_ascii_alphabetic() || c == b'_' || c == b'$')
    {
        is_float = true;
        n += 1;
        while matches!(b.get(pos + n), Some(&c) if c.is_ascii_digit() || c == b'_') {
            n += 1;
        }
    }
    if matches!(b.get(pos + n), Some(&c) if c == b'e' || c == b'E') {
        let mut m = n + 1;
        if matches!(b.get(pos + m), Some(&c) if c == b'+' || c == b'-') {
            m += 1;
        }
        if matches!(b.get(pos + m), Some(&c) if c.is_ascii_digit()) {
            is_float = true;
            n = m;
            while matches!(b.get(pos + n), Some(&c) if c.is_ascii_digit()) {
                n += 1;
            }
        }
    }
    if s.starts_with('.') && !is_float {
        is_float = true;
    }
    let text: String = s[..n].chars().filter(|c| *c != '_').collect();
    if is_float {
        let v: f64 = text
            .parse()
            .map_err(|_| PhpError::parse("syntax error, invalid float literal", line))?;
        Ok((Token::Float(v), n))
    } else if text.starts_with('0') && text.len() > 1 && !is_float {
        // Leading-0 decimal literal is an implicit octal — a non-octal
        // digit is PHP's "Invalid numeric literal" (invalid_octal.phpt).
        if !text.chars().all(|c| ('0'..='7').contains(&c)) {
            return Err(PhpError::parse("Invalid numeric literal", line));
        }
        match i64::from_str_radix(&text[1..], 8) {
            Ok(v) => Ok((Token::Int(v), n)),
            Err(_) => {
                let v = text[1..]
                    .chars()
                    .fold(0.0f64, |a, c| a * 8.0 + c.to_digit(8).unwrap_or(0) as f64);
                Ok((Token::Float(v), n))
            }
        }
    } else {
        match text.parse::<i64>() {
            Ok(v) => Ok((Token::Int(v), n)),
            Err(_) => {
                // Integer overflow → float, matching PHP.
                let v: f64 = text
                    .parse()
                    .map_err(|_| PhpError::parse("syntax error, invalid integer literal", line))?;
                Ok((Token::Float(v), n))
            }
        }
    }
}

fn single_string(src: &str, pos: usize, line: usize) -> Result<(String, usize), PhpError> {
    let b = src.as_bytes();
    let mut n = 1;
    let mut s = String::new();
    loop {
        match b.get(pos + n) {
            None => {
                return Err(PhpError::parse("syntax error, unterminated string", line));
            }
            Some(&b'\\') => match b.get(pos + n + 1) {
                Some(&b'\'') => {
                    s.push('\'');
                    n += 2;
                }
                Some(&b'\\') => {
                    s.push('\\');
                    n += 2;
                }
                _ => {
                    s.push('\\');
                    n += 1;
                }
            },
            Some(&b'\'') => return Ok((s, n + 1)),
            Some(&c) => {
                // raw byte — keep UTF-8 correctness by working on chars
                let ch = src[pos + n..].chars().next().unwrap();
                s.push(ch);
                n += ch.len_utf8();
                let _ = c;
            }
        }
    }
}

fn double_string(
    src: &str,
    pos: usize,
    line: usize,
    out: &mut Vec<Lexed>,
) -> Result<(Vec<StringPart>, usize), PhpError> {
    let (parts, n) = interp_scan(src, pos, 1, line, b'"', out)?;
    Ok((parts, n))
}

/// Shared interpolation scanner for `"..."` and heredoc bodies.
/// `end` is the closing byte (`b'"'` for dstrings); `end == 0` means the
/// body runs to end of `src` (heredoc pre-slices its body, so a bare `"`
/// inside is literal text).
fn interp_scan(
    src: &str,
    pos: usize,
    n0: usize,
    line: usize,
    end: u8,
    diags: &mut Vec<Lexed>,
) -> Result<(Vec<StringPart>, usize), PhpError> {
    let b = src.as_bytes();
    let mut n = n0;
    let mut parts: Vec<StringPart> = Vec::new();
    let mut lit: Vec<u8> = Vec::new();
    macro_rules! flush {
        () => {
            if !lit.is_empty() {
                parts.push(StringPart::Lit(std::mem::take(&mut lit)));
            }
        };
    }
    loop {
        match b.get(pos + n) {
            None => {
                if end == 0 {
                    flush!();
                    return Ok((parts, n));
                }
                return Err(PhpError::parse("syntax error, unterminated string", line));
            }
            Some(&c) if c == end => {
                flush!();
                return Ok((parts, n + 1));
            }
            Some(&b'\\') => {
                let e = b.get(pos + n + 1).copied();
                let (ebytes, adv): (Vec<u8>, usize) = match e {
                    Some(b'n') => (b"\n".to_vec(), 2),
                    Some(b't') => (b"\t".to_vec(), 2),
                    Some(b'r') => (b"\r".to_vec(), 2),
                    Some(b'v') => (b"\x0b".to_vec(), 2),
                    Some(b'e') => (b"\x1b".to_vec(), 2),
                    Some(b'f') => (b"\x0c".to_vec(), 2),
                    Some(b'\\') => (b"\\".to_vec(), 2),
                    Some(b'$') => (b"$".to_vec(), 2),
                    Some(b'"') => (b"\"".to_vec(), 2),
                    Some(b'0'..=b'7') => {
                        let mut v = 0u32;
                        let mut k = 1;
                        while k <= 3 {
                            match b.get(pos + n + k) {
                                Some(&d @ b'0'..=b'7') => {
                                    v = v * 8 + (d - b'0') as u32;
                                    k += 1;
                                }
                                _ => break,
                            }
                        }
                        if v > 0o377 {
                            // Zend warns at compile time and wraps the
                            // value to a byte (warning_during_heredoc_*).
                            diags.push(Lexed {
                                token: Token::Diag(
                                    "Warning",
                                    format!(
                                        "Octal escape sequence overflow \\{:o} is greater than \\377",
                                        v
                                    ),
                                ),
                                line: line + s_matches(&src[pos..pos + n]),
                                ws_adj: 0,
                                start: usize::MAX,
                                end: usize::MAX,
                            });
                            (vec![(v & 0xff) as u8], k)
                        } else {
                            // \NNN is a raw byte, not a codepoint.
                            (vec![v as u8], k)
                        }
                    }
                    Some(b'x') => {
                        let mut v = 0u32;
                        let mut k = 2;
                        while k <= 3 {
                            match b.get(pos + n + k) {
                                Some(&d) if d.is_ascii_hexdigit() => {
                                    v = v * 16 + (d as char).to_digit(16).unwrap_or(0);
                                    k += 1;
                                }
                                _ => break,
                            }
                        }
                        if k == 2 {
                            (b"\\".to_vec(), 1)
                        } else {
                            // \xNN is a raw byte, not a codepoint.
                            (vec![v as u8], k)
                        }
                    }
                    Some(b'u') if b.get(pos + n + 2) == Some(&b'{') => {
                        // \u{HEX}: only 1+ hex digits then '}', else PHP's
                        // "Invalid UTF-8 codepoint escape sequence" parse
                        // error (tests/lang/string/unicode_escape_*.phpt).
                        let mut k = 3;
                        let mut v = 0u32;
                        let mut digits = 0usize;
                        let mut closed = false;
                        while let Some(&d) = b.get(pos + n + k) {
                            match d {
                                b'}' if digits > 0 => {
                                    closed = true;
                                    k += 1;
                                    break;
                                }
                                _ if d.is_ascii_hexdigit() => {
                                    v = v
                                        .saturating_mul(16)
                                        .saturating_add((d as char).to_digit(16).unwrap_or(0));
                                    digits += 1;
                                    k += 1;
                                }
                                _ => {
                                    return Err(PhpError::parse(
                                        "Invalid UTF-8 codepoint escape sequence",
                                        line,
                                    ))
                                }
                            }
                        }
                        if !closed {
                            return Err(PhpError::parse(
                                "Invalid UTF-8 codepoint escape sequence",
                                line,
                            ));
                        }
                        if v > 0x10ffff {
                            return Err(PhpError::parse(
                                "Invalid UTF-8 codepoint escape sequence: Codepoint too large",
                                line,
                            ));
                        }
                        // PHP emits CESU-8 for surrogate halves — a
                        // Rust char can't hold them, encode the 3-byte
                        // form by hand (unicode_escape_surrogates.phpt).
                        match char::from_u32(v) {
                            Some(c) => {
                                let mut tmp = [0u8; 4];
                                (c.encode_utf8(&mut tmp).as_bytes().to_vec(), k)
                            }
                            None if (0xD800..=0xDFFF).contains(&v) => (
                                vec![
                                    0xED,
                                    0xA0 + ((v - 0xD800) >> 6) as u8,
                                    0x80 + ((v - 0xD800) & 0x3f) as u8,
                                ],
                                k,
                            ),
                            None => (b"\xef\xbf\xbd".to_vec(), k),
                        }
                    }
                    _ => (b"\\".to_vec(), 1),
                };
                lit.extend_from_slice(&ebytes);
                n += adv;
            }
            Some(&b'{') if b.get(pos + n + 1) == Some(&b'$') => {
                // Complex (curly) interpolation: {$expr}
                let mut k = n + 2;
                let mut depth = 1usize;
                while let Some(&d) = b.get(pos + k) {
                    if d == b'{' {
                        depth += 1;
                    } else if d == b'}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    k += 1;
                }
                if depth != 0 {
                    return Err(PhpError::parse(
                        "syntax error, unterminated '{$' in string",
                        line,
                    ));
                }
                flush!();
                // Compile-time diags inside `{$expr}` (e.g. octal
                // overflow) scan at lex time like Zend.
                let inner = &src[pos + n + 1..pos + k];
                if let Ok(toks) = lex(&format!("<?php {}", inner)) {
                    for t in toks {
                        if let Token::Diag(level, msg) = t.token {
                            diags.push(Lexed {
                                token: Token::Diag(level, msg),
                                line: line + s_matches(&src[pos..pos + n]),
                                ws_adj: 0,
                                start: usize::MAX,
                                end: usize::MAX,
                            });
                        }
                    }
                }
                parts.push(StringPart::Expr(
                    src[pos + n + 1..pos + k].to_string(),
                    line + s_matches(&src[pos..pos + n]),
                ));
                n = k + 1;
            }
            Some(&b'$') => {
                // $var or ${expr}
                if b.get(pos + n + 1) == Some(&b'{') {
                    let mut k = n + 2;
                    let mut depth = 1usize;
                    while let Some(&d) = b.get(pos + k) {
                        if d == b'{' {
                            depth += 1;
                        } else if d == b'}' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        k += 1;
                    }
                    if depth != 0 {
                        return Err(PhpError::parse(
                            "syntax error, unterminated '{$' in string",
                            line,
                        ));
                    }
                    flush!();
                    // Zend reports diagnostics found while scanning the
                    // inner source *before* the `${` deprecation itself
                    // (warning_during_heredoc_scan_ahead): lex it now so
                    // its own diags (octal, nested `${`) emit in order.
                    let inner = &src[pos + n + 2..pos + k];
                    if let Ok(toks) = lex(&format!("<?php {}", inner)) {
                        for t in toks {
                            if let Token::Diag(level, msg) = t.token {
                                diags.push(Lexed {
                                    token: Token::Diag(level, msg),
                                    line: line + s_matches(&src[pos..pos + n]),
                                    ws_adj: 0,
                                    start: usize::MAX,
                                    end: usize::MAX,
                                });
                            }
                        }
                    }
                    diags.push(Lexed {
                        token: Token::Diag(
                            "Deprecated",
                            "Using ${expr} (variable variables) in strings is deprecated, use {${expr}} instead".into(),
                        ),
                        line: line + s_matches(&src[pos..pos + n]),
                        ws_adj: 0,
                        start: usize::MAX,
                        end: usize::MAX,
                    });
                    parts.push(StringPart::DollarBraceExpr(
                        src[pos + n + 2..pos + k].to_string(),
                        line + s_matches(&src[pos..pos + n]),
                    ));
                    n = k + 1;
                } else {
                    let (name, len) = ident(src, pos + n + 1);
                    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
                        lit.push(b'$');
                        n += 1;
                    } else {
                        flush!();
                        // Simple syntax also allows one `->prop` or `[index]`
                        // (more complex forms go through the {$...} branch).
                        let rest = pos + n + 1 + len;
                        if src[rest..].starts_with("->") {
                            let (pn, plen) = ident(src, rest + 2);
                            if !pn.is_empty() {
                                parts.push(StringPart::Expr(
                                    format!("${}->{}", name, pn),
                                    line + s_matches(&src[pos..pos + n]),
                                ));
                                n = rest + 2 + plen - pos;
                            } else {
                                parts.push(StringPart::Var(
                                    name,
                                    line + s_matches(&src[pos..pos + n]),
                                ));
                                n += 1 + len;
                            }
                        } else if src[rest..].starts_with('[') {
                            // Quoted keys are illegal in simple
                            // interpolation — `$arr['x']` is E_PARSE
                            // (bug21820).
                            if matches!(b.get(rest + 1), Some(b'\'') | Some(b'"')) {
                                return Err(PhpError::parse(
                                    "syntax error, unexpected string content \"\", expecting \"-\" or identifier or variable or number",
                                    line,
                                ));
                            }
                            // One-dimensional index (unquoted ident/number/quoted).
                            let mut k = rest + 1;
                            while let Some(&d) = b.get(k) {
                                if d == b']' {
                                    break;
                                }
                                k += 1;
                            }
                            if b.get(k) == Some(&b']') {
                                parts.push(StringPart::Expr(
                                    format!("${}{}", name, &src[rest..=k]),
                                    line + s_matches(&src[pos..pos + n]),
                                ));
                                n = k + 1 - pos;
                            } else {
                                parts.push(StringPart::Var(
                                    name,
                                    line + s_matches(&src[pos..pos + n]),
                                ));
                                n += 1 + len;
                            }
                        } else {
                            parts.push(StringPart::Var(name, line + s_matches(&src[pos..pos + n])));
                            n += 1 + len;
                        }
                    }
                }
            }
            Some(_) => {
                let ch = src[pos + n..].chars().next().unwrap();
                let mut tmp = [0u8; 4];
                lit.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                n += ch.len_utf8();
            }
        }
    }
}

/// Heredoc/nowdoc: `<<<` `ID` / `"ID"` / `'ID'` then lines until a
/// line-start `{ws}{ID}` closer (PHP 7.3+ flexible: the closer's indent
/// is stripped from every body line; mixed tab/space indents are parse
/// errors — Zend/tests/heredoc_nowdoc).
fn heredoc(
    src: &str,
    pos: usize,
    line: usize,
    out: &mut Vec<Lexed>,
) -> Result<(Token, usize), PhpError> {
    let b = src.as_bytes();
    let mut n = 3; // <<<
    while matches!(b.get(pos + n), Some(b' ') | Some(b'\t')) {
        n += 1;
    }
    // Marker: 'ID' = nowdoc, "ID"/ID = heredoc.
    let (marker, nowdoc, mlen) = match b.get(pos + n) {
        Some(&q @ (b'\'' | b'"')) => {
            let (m, l) = ident(src, pos + n + 1);
            if m.is_empty() || b.get(pos + n + 1 + l) != Some(&q) {
                return Err(PhpError::parse("syntax error, unexpected token", line));
            }
            (m, q == b'\'', l + 2)
        }
        _ => {
            let (m, l) = ident(src, pos + n);
            if m.is_empty() {
                return Err(PhpError::parse("syntax error, unexpected token", line));
            }
            (m, false, l)
        }
    };
    n += mlen;
    // The marker line must end in a newline (or EOF → unterminated).
    let eol = match b.get(pos + n) {
        None => 0,
        Some(&b'\r') if b.get(pos + n + 1) == Some(&b'\n') => 2,
        Some(&b'\n') | Some(&b'\r') => 1,
        Some(_) => {
            return Err(PhpError::parse(
                "syntax error, unexpected end of file",
                line + s_matches(&src[pos..pos + n]),
            ))
        }
    };
    if eol == 0 {
        return Err(PhpError::parse(
            "syntax error, unexpected end of file",
            line + s_matches(&src[pos..pos + n]),
        ));
    }
    n += eol;
    let body_start = pos + n;
    let is_label = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    // End-of-line length at byte index `i` (`\r\n`, `\n`, `\r`).
    let eol_at = |i: usize| -> usize {
        match b.get(i) {
            Some(b'\n') => 1,
            Some(b'\r') if b.get(i + 1) == Some(&b'\n') => 2,
            Some(b'\r') => 1,
            _ => 0,
        }
    };
    // Skip a `{$` or `${` interpolation span so a same-named marker
    // inside one can't close the heredoc (flexible-heredoc-complex-*).
    // Mirrors the brace-depth scan `interp_scan` applies.
    let skip_interp = |mut i: usize| -> usize {
        let mut depth = 1usize;
        while depth > 0 {
            match b.get(i) {
                None => break,
                Some(&b'{') => depth += 1,
                Some(&b'}') => depth -= 1,
                _ => {}
            }
            i += 1;
        }
        i
    };
    // Find the closer: at a line start, [ \t]* marker then a
    // non-label char. `{$`/`${` regions are skipped char-wise so a
    // same-named marker inside an interpolation can't close the
    // heredoc (flexible-heredoc-complex-2/4).
    let mut cur = body_start;
    let (closer_line_start, indent_len) = loop {
        if cur >= src.len() {
            // Body non-empty → the expecting-list form; a marker line
            // with nothing after it reports plain `unexpected end of
            // file` (flexible-heredoc-error6 vs error7).
            let eof_line = line + s_matches(&src[pos..cur]);
            if cur == body_start {
                return Err(PhpError::parse(
                    "syntax error, unexpected end of file",
                    eof_line,
                ));
            }
            return Err(PhpError::parse(
                "syntax error, unexpected end of file, expecting variable or heredoc end or \"${\" or \"{$\"",
                eof_line,
            ));
        }
        if !nowdoc && (src[cur..].starts_with("${") || src[cur..].starts_with("{$")) {
            cur = skip_interp(cur + 2);
            continue;
        }
        if cur == body_start || eol_at(cur.wrapping_sub(1)) > 0 && cur > 0 {
            let mut le = cur;
            while le < src.len() && eol_at(le) == 0 {
                le += 1;
            }
            let mut ind_end = cur;
            while matches!(b.get(ind_end), Some(b' ') | Some(b'\t')) && ind_end < le {
                ind_end += 1;
            }
            if le - ind_end >= marker.len() && src[ind_end..].starts_with(&marker) {
                let after = ind_end + marker.len();
                if !matches!(b.get(after), Some(&c) if is_label(c)) {
                    break (cur, ind_end - cur);
                }
            }
        }
        cur += 1;
    };
    let indent = &src[closer_line_start..closer_line_start + indent_len];
    if indent.contains('\t') && indent.contains(' ') {
        return Err(PhpError::parse(
            "Invalid indentation - tabs and spaces cannot be mixed",
            line + s_matches(&src[pos..closer_line_start]),
        ));
    }
    // Dedent + validate every body line against the closer's indent.
    let raw_region = &src[body_start..closer_line_start];
    let mut body = String::with_capacity(raw_region.len());
    let mut idx = 0usize; // body line index (marker line is `line`, body is line+1+idx)
    let mut cur = 0usize;
    let rb = raw_region.as_bytes();
    // The newline that ends the last body line is not part of the value.
    let raw_body = raw_region
        .strip_suffix('\n')
        .map(|x| x.strip_suffix('\r').unwrap_or(x))
        .or_else(|| raw_region.strip_suffix('\r'))
        .unwrap_or(raw_region);
    let rb = &rb[..raw_body.len()];
    while cur <= rb.len() {
        let mut le = cur;
        while le < rb.len() && rb[le] != b'\n' && rb[le] != b'\r' {
            le += 1;
        }
        let l = &raw_body[cur..le];
        if !indent.is_empty() {
            if l.trim_start_matches([' ', '\t']).is_empty() {
                // Whitespace-only lines dedent fully and never error.
            } else if let Some(r) = l.strip_prefix(indent) {
                body.push_str(r);
            } else {
                let lead = l.len() - l.trim_start_matches([' ', '\t']).len();
                let mixed = l[..lead].chars().zip(indent.chars()).any(|(a, c)| a != c);
                let bad_line = line + 1 + idx;
                if mixed {
                    return Err(PhpError::parse(
                        "Invalid indentation - tabs and spaces cannot be mixed",
                        bad_line,
                    ));
                }
                return Err(PhpError::parse(
                    format!(
                        "Invalid body indentation level (expecting an indentation level of at least {})",
                        indent.len()
                    ),
                    bad_line,
                ));
            }
        } else {
            body.push_str(l);
        }
        if le >= rb.len() {
            break;
        }
        let el = match rb[le] {
            b'\r' if rb.get(le + 1) == Some(&b'\n') => 2,
            _ => 1,
        };
        body.push_str(&raw_body[le..le + el]);
        cur = le + el;
        idx += 1;
    }
    let consumed = (closer_line_start + indent_len + marker.len()) - pos;
    if nowdoc {
        Ok((Token::SimpleString(body), consumed))
    } else {
        let mut diags = Vec::new();
        let (parts, _) = interp_scan(&body, 0, 0, line + 1, 0, &mut diags)?;
        out.extend(diags);
        Ok((Token::InterpString(parts), consumed))
    }
}

/// Longest-match operator table.
fn operator(src: &str, pos: usize) -> Option<(&'static str, usize)> {
    const OPS: &[&str] = &[
        "<=>", "===", "!==", "...", "**=", "<<=", ">>=", "??=", "=>", "==", "!=", "<>", "<=", ">=",
        "&&", "||", "++", "--", "+=", "-=", "*=", "/=", ".=", "%=", "&=", "|=", "^=", "<<", ">>",
        "**", "??", "->", "?->", "::", "\\", "(", ")", "[", "]", "{", "}", ";", ",", "?", ":", "+",
        "-", "*", "/", "%", "=", "<", ">", "!", ".", "&", "|", "^", "~", "@",
    ];
    for op in OPS {
        if src[pos..].starts_with(op) {
            return Some((op, op.len()));
        }
    }
    None
}

pub fn is_keyword(name: &str) -> bool {
    KEYWORDS.contains(&name.to_ascii_lowercase().as_str())
}
