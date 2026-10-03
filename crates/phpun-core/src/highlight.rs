//! `highlight_string`/`highlight_file`/`php_strip_whitespace` — PHP's
//! syntax highlighter, matching zend_highlight's HTML output: a
//! `<pre><code style="color: #000000">` wrapper, `<span style="color:…">`
//! runs inside PHP tags, bare escaped text for inline HTML.

const BB: &str = "0000BB"; // default: names, vars, numbers, tags
const KW: &str = "007700"; // keywords + punctuation/operators
const ST: &str = "DD0000"; // strings
const CM: &str = "FF8000"; // comments

/// Words zend_highlight renders as T_* (keyword color). Non-reserved
/// words — types, function names, constants, true/false/null — are
/// default color.
const HL_KEYWORDS: &[&str] = &[
    "abstract",
    "and",
    "array",
    "as",
    "break",
    "callable",
    "case",
    "catch",
    "class",
    "clone",
    "const",
    "continue",
    "declare",
    "default",
    "do",
    "echo",
    "else",
    "elseif",
    "empty",
    "enddeclare",
    "endfor",
    "endforeach",
    "endif",
    "endswitch",
    "endwhile",
    "enum",
    "extends",
    "final",
    "finally",
    "fn",
    "for",
    "foreach",
    "function",
    "global",
    "goto",
    "if",
    "implements",
    "include",
    "include_once",
    "instanceof",
    "insteadof",
    "interface",
    "isset",
    "list",
    "match",
    "namespace",
    "new",
    "or",
    "print",
    "private",
    "protected",
    "public",
    "readonly",
    "require",
    "require_once",
    "return",
    "static",
    "switch",
    "throw",
    "trait",
    "try",
    "unset",
    "use",
    "var",
    "while",
    "xor",
    "yield",
];

fn esc(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

struct Hl<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
    out: String,
    cur: &'a str,
    /// inside a php code block
    php: bool,
}

impl<'a> Hl<'a> {
    fn span(&mut self, color: &'a str) {
        if self.php && self.cur != color {
            self.out.push_str("</span><span style=\"color: #");
            self.out.push_str(color);
            self.out.push_str("\">");
            self.cur = color;
        }
    }
    /// raw text outside php tags (no span)
    fn html(&mut self, t: &str) {
        esc(t, &mut self.out);
    }
    fn open_php(&mut self) {
        self.php = true;
        self.out.push_str("<span style=\"color: #");
        self.out.push_str(BB);
        self.out.push_str("\">");
        self.cur = BB;
    }
    fn close_php(&mut self) {
        self.php = false;
        self.out.push_str("</span>");
    }
    fn txt(&mut self, color: &'a str, t: &str) {
        self.span(color);
        esc(t, &mut self.out);
    }

    fn at(&self, t: &str) -> bool {
        self.s[self.i..].starts_with(t)
    }

    /// scan one php code region (after `<?php`/`<?=`) until `?>` or eof
    fn scan_php(&mut self) {
        while self.i < self.b.len() {
            let c = self.b[self.i];
            if self.at("?>") {
                self.txt(BB, "?>");
                self.i += 2;
                self.close_php();
                // a lone newline after ?> is swallowed by the tag
                if self.i < self.b.len() && self.b[self.i] == b'\n' {
                    self.out.push('\n');
                    self.i += 1;
                }
                return;
            }
            if c.is_ascii_whitespace() {
                // whitespace continues the previous span's color
                let st = self.i;
                while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
                    self.i += 1;
                }
                self.txt(self.cur, &self.s[st..self.i]);
                continue;
            }
            if self.at("//") || c == b'#' && !self.at("#[") {
                let st = self.i;
                while self.i < self.b.len() && self.b[self.i] != b'\n' {
                    self.i += 1;
                }
                if self.i < self.b.len() {
                    self.i += 1; // newline stays inside the comment span
                }
                self.txt(CM, &self.s[st..self.i]);
                continue;
            }
            if self.at("/*") {
                let st = self.i;
                self.i += 2;
                while self.i < self.b.len() && !self.at("*/") {
                    self.i += 1;
                }
                self.i = (self.i + 2).min(self.b.len());
                self.txt(CM, &self.s[st..self.i]);
                continue;
            }
            if c == b'\'' {
                self.quoted('\'');
                continue;
            }
            if c == b'"' {
                self.dquoted();
                continue;
            }
            if self.at("<<<") {
                self.heredoc();
                continue;
            }
            if c == b'$' {
                let st = self.i;
                self.i += 1;
                while self.i < self.b.len()
                    && (self.b[self.i].is_ascii_alphanumeric()
                        || self.b[self.i] == b'_'
                        || self.b[self.i] == b'$')
                {
                    self.i += 1;
                }
                self.txt(BB, &self.s[st..self.i]);
                continue;
            }
            if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
                let st = self.i;
                while self.i < self.b.len()
                    && (self.b[self.i].is_ascii_alphanumeric()
                        || self.b[self.i] == b'_'
                        || self.b[self.i] >= 0x80)
                {
                    self.i += 1;
                }
                let w = &self.s[st..self.i];
                let color = if HL_KEYWORDS.contains(&w) { KW } else { BB };
                self.txt(color, w);
                continue;
            }
            if c.is_ascii_digit()
                || (c == b'.' && self.b.get(self.i + 1).is_some_and(|d| d.is_ascii_digit()))
            {
                let st = self.i;
                while self.i < self.b.len()
                    && (self.b[self.i].is_ascii_alphanumeric()
                        || self.b[self.i] == b'.'
                        || self.b[self.i] == b'_')
                {
                    self.i += 1;
                }
                self.txt(BB, &self.s[st..self.i]);
                continue;
            }
            // punctuation / operators
            self.txt(KW, &self.s[self.i..self.i + 1]);
            self.i += 1;
        }
        // unterminated php region
        if self.php {
            self.php = false;
            self.out.push_str("</span>");
        }
    }

    fn quoted(&mut self, q: char) {
        let st = self.i;
        self.i += 1;
        while self.i < self.b.len() {
            if self.b[self.i] == b'\\' {
                self.i += 2;
            } else if self.b[self.i] == q as u8 {
                self.i += 1;
                break;
            } else {
                self.i += 1;
            }
        }
        self.i = self.i.min(self.b.len());
        self.txt(ST, &self.s[st..self.i]);
    }

    fn dquoted(&mut self) {
        // segments: string text DD0000, {$var} braces KW, vars BB
        let mut seg = self.i;
        self.i += 1;
        while self.i < self.b.len() {
            if self.b[self.i] == b'\\' {
                self.i += 2;
                continue;
            }
            if self.b[self.i] == b'"' {
                break;
            }
            if self.at("{$") || self.at("{\\$") {
                if seg < self.i {
                    self.txt(ST, &self.s[seg..self.i]);
                }
                self.txt(KW, "{");
                self.i += 1;
                let mut depth = 1;
                let mut seg2 = self.i;
                while self.i < self.b.len() && depth > 0 {
                    match self.b[self.i] {
                        b'{' => {
                            if seg2 < self.i {
                                self.txt(BB, &self.s[seg2..self.i]);
                            }
                            self.txt(KW, "{");
                            depth += 1;
                            self.i += 1;
                            seg2 = self.i;
                        }
                        b'}' => {
                            if seg2 < self.i {
                                self.txt(BB, &self.s[seg2..self.i]);
                            }
                            depth -= 1;
                            self.txt(KW, "}");
                            self.i += 1;
                            seg2 = self.i;
                        }
                        _ => self.i += 1,
                    }
                }
                if seg2 < self.i {
                    self.txt(BB, &self.s[seg2..self.i]);
                }
                seg = self.i;
                continue;
            }
            if self.b[self.i] == b'$'
                && self
                    .b
                    .get(self.i + 1)
                    .is_some_and(|d| d.is_ascii_alphabetic() || *d == b'_')
            {
                if seg < self.i {
                    self.txt(ST, &self.s[seg..self.i]);
                }
                let st = self.i;
                self.i += 1;
                while self.i < self.b.len()
                    && (self.b[self.i].is_ascii_alphanumeric() || self.b[self.i] == b'_')
                {
                    self.i += 1;
                }
                self.txt(BB, &self.s[st..self.i]);
                self.interp_tail(self.b.len());
                seg = self.i;
                continue;
            }
            self.i += 1;
        }
        if seg < self.i {
            self.txt(ST, &self.s[seg..self.i]);
        }
        if self.i < self.b.len() {
            self.txt(ST, "\"");
            self.i += 1;
        }
    }

    fn heredoc(&mut self) {
        // marker `<<<"LABEL"` + its newline: keyword color
        let st = self.i;
        self.i += 3;
        if self.i < self.b.len() && (self.b[self.i] == b'"' || self.b[self.i] == b'\'') {
            self.i += 1;
        }
        let lstart = self.i;
        while self.i < self.b.len()
            && (self.b[self.i].is_ascii_alphanumeric() || self.b[self.i] == b'_')
        {
            self.i += 1;
        }
        let label = &self.s[lstart..self.i];
        if self.i < self.b.len() && (self.b[self.i] == b'"' || self.b[self.i] == b'\'') {
            self.i += 1;
        }
        // terminator must be at column 0 (Zend highlight rule)
        let mut body_end = self.b.len();
        let mut p = self.i;
        while p < self.b.len() {
            if self.b[p] == b'\n' {
                let mut q = p + 1;
                while q < self.b.len() && (self.b[q] == b' ' || self.b[q] == b'\t') {
                    q += 1;
                }
                if !self.s[q..].starts_with(label) {
                    p += 1;
                    continue;
                }
                let after = q + label.len();
                if after >= self.b.len()
                    || self.b[after] == b';'
                    || self.b[after] == b'\n'
                    || self.b[after] == b'\r'
                {
                    body_end = p + 1;
                    break;
                }
            }
            p += 1;
        }
        // include the marker's newline in the keyword span
        if self.i < self.b.len() && self.b[self.i] == b'\n' {
            self.i += 1;
        }
        self.txt(KW, &self.s[st..self.i]);
        // body: string color with {$..}/$var interpolation like dq
        while self.i < body_end {
            if self.at("{$") || self.at("{\\$") {
                self.txt(KW, "{");
                self.i += 1;
                let mut depth = 1;
                while self.i < body_end && depth > 0 {
                    let c = self.b[self.i];
                    if c == b'{' {
                        self.txt(KW, "{");
                        depth += 1;
                        self.i += 1;
                    } else if c == b'}' {
                        self.txt(KW, "}");
                        depth -= 1;
                        self.i += 1;
                    } else if c == b'$' {
                        let vs = self.i;
                        self.i += 1;
                        while self.i < body_end
                            && (self.b[self.i].is_ascii_alphanumeric() || self.b[self.i] == b'_')
                        {
                            self.i += 1;
                        }
                        self.txt(BB, &self.s[vs..self.i]);
                    } else if c.is_ascii_alphabetic() || c == b'_' {
                        let vs = self.i;
                        while self.i < body_end
                            && (self.b[self.i].is_ascii_alphanumeric() || self.b[self.i] == b'_')
                        {
                            self.i += 1;
                        }
                        self.txt(BB, &self.s[vs..self.i]);
                    } else {
                        let es = self.i;
                        self.i += 1;
                        self.txt(KW, &self.s[es..self.i]);
                    }
                }
                continue;
            }
            if self.b[self.i] == b'$'
                && self
                    .b
                    .get(self.i + 1)
                    .is_some_and(|d| d.is_ascii_alphabetic() || *d == b'_')
            {
                let vs = self.i;
                self.i += 1;
                while self.i < body_end
                    && (self.b[self.i].is_ascii_alphanumeric() || self.b[self.i] == b'_')
                {
                    self.i += 1;
                }
                self.txt(BB, &self.s[vs..self.i]);
                self.interp_tail(body_end);
                continue;
            }
            let es = self.i;
            while self.i < body_end && self.b[self.i] != b'$' && !self.at("{$") {
                self.i += 1;
            }
            self.txt(ST, &self.s[es..self.i]);
        }
        // terminator line (incl. indent) + trailing newline: keyword color
        if self.i < self.b.len() {
            let ts = self.i;
            while self.i < self.b.len() && self.b[self.i] != b'\n' {
                self.i += 1;
            }
            if self.i < self.b.len() && self.b[self.i] == b'\n' {
                self.i += 1;
            }
            self.txt(KW, &self.s[ts..self.i]);
        }
    }

    /// After a `$ident` interpolation, consume PHP's allowed simple
    /// tails — `[expr]` subscripts and `->prop` — as code tokens.
    fn interp_tail(&mut self, limit: usize) {
        loop {
            if self.i < limit && self.at("->") {
                self.txt(KW, "->");
                self.i += 2;
                let st = self.i;
                while self.i < limit
                    && (self.b[self.i].is_ascii_alphanumeric() || self.b[self.i] == b'_')
                {
                    self.i += 1;
                }
                if st < self.i {
                    self.txt(BB, &self.s[st..self.i]);
                }
                continue;
            }
            if self.i < limit && self.b[self.i] == b'[' {
                self.txt(KW, "[");
                self.i += 1;
                // key: number, ident, quoted string, or expr-ish
                while self.i < limit && self.b[self.i] != b']' {
                    let c = self.b[self.i];
                    if c == b'\'' || c == b'"' {
                        self.quoted(c as char);
                    } else if c == b'$' {
                        let vs = self.i;
                        self.i += 1;
                        while self.i < limit
                            && (self.b[self.i].is_ascii_alphanumeric() || self.b[self.i] == b'_')
                        {
                            self.i += 1;
                        }
                        self.txt(BB, &self.s[vs..self.i]);
                    } else if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
                        let vs = self.i;
                        while self.i < limit
                            && (self.b[self.i].is_ascii_alphanumeric() || self.b[self.i] == b'_')
                        {
                            self.i += 1;
                        }
                        self.txt(BB, &self.s[vs..self.i]);
                    } else {
                        self.txt(KW, &self.s[self.i..self.i + 1]);
                        self.i += 1;
                    }
                }
                if self.i < limit && self.b[self.i] == b']' {
                    self.txt(KW, "]");
                    self.i += 1;
                }
                continue;
            }
            break;
        }
    }
}

/// Full document: `<pre><code style="color: #000000">…</code></pre>`.
pub fn highlight_html(src: &str) -> String {
    let mut h = Hl {
        s: src,
        b: src.as_bytes(),
        i: 0,
        out: String::from("<pre><code style=\"color: #000000\">"),
        cur: "",
        php: false,
    };
    while h.i < h.b.len() {
        if h.php {
            h.scan_php();
            continue;
        }
        let rest = &h.s[h.i..];
        match rest.find("<?") {
            None => {
                h.html(rest);
                h.i = h.b.len();
            }
            Some(off) => {
                let tag_at = h.i + off;
                let after = &h.s[tag_at..];
                let is_full = after.len() >= 5
                    && after[..5].eq_ignore_ascii_case("<?php")
                    && (after.len() == 5 || !after.as_bytes()[5].is_ascii_alphanumeric());
                let is_echo = after.starts_with("<?=");
                if off > 0 {
                    h.html(&rest[..off]);
                }
                if is_full {
                    h.i = tag_at;
                    h.open_php();
                    h.txt(BB, "<?php");
                    h.i += 5;
                } else if is_echo {
                    h.i = tag_at;
                    h.open_php();
                    h.txt(BB, "<?=");
                    h.i += 3;
                } else {
                    h.html("<?");
                    h.i = tag_at + 2;
                    continue;
                }
                h.scan_php();
            }
        }
    }
    h.out.push_str("</code></pre>");
    h.out
}

/// `php_strip_whitespace($file)` return value: comments dropped,
/// whitespace runs collapsed to one space; `<?php` keeps its newline.
pub fn strip_whitespace(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut php = false;
    while i < b.len() {
        if !php {
            let rest = &src[i..];
            match rest.find("<?") {
                None => {
                    out.push_str(rest);
                    break;
                }
                Some(off) => {
                    out.push_str(&rest[..off]);
                    let tag = i + off;
                    let after = &src[tag..];
                    if after.len() >= 5
                        && after[..5].eq_ignore_ascii_case("<?php")
                        && (after.len() == 5 || !after.as_bytes()[5].is_ascii_alphanumeric())
                    {
                        out.push_str("<?php\n");
                        i = tag + 5;
                        php = true;
                    } else if after.starts_with("<?=") {
                        out.push_str("<?= ");
                        i = tag + 3;
                        php = true;
                    } else {
                        out.push_str("<?");
                        i = tag + 2;
                    }
                }
            }
            continue;
        }
        let c = b[i];
        if c.is_ascii_whitespace() {
            // collapse any run to one space; a newline directly after
            // <?php was already emitted
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() {
                out.push(' ');
            }
            continue;
        }
        if src[i..].starts_with("?>") {
            out.push_str("?>\n");
            i += 2;
            php = false;
            continue;
        }
        if src[i..].starts_with("//") || c == b'#' && !src[i..].starts_with("#[") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if src[i..].starts_with("/*") {
            i += 2;
            while i < b.len() && !src[i..].starts_with("*/") {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        if c == b'\'' || c == b'"' {
            let st = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                } else if b[i] == c {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            i = i.min(b.len());
            out.push_str(&src[st..i]);
            continue;
        }
        out.push(c as char);
        i += 1;
    }
    out
}
