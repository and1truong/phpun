//! EXPECTF placeholder → regex translation, ported from run-tests.php's
//! `expectf_to_regex`.

/// Translate an EXPECTF pattern to a regex matching the whole output.
pub fn expectf_to_regex(wanted: &str) -> String {
    let wanted_re = wanted.replace("\r\n", "\n");

    // Quote everything except %r...%r delimited raw-regex sections.
    let mut temp = String::new();
    let r = "%r";
    let mut start_offset = 0usize;
    let length = wanted_re.len();
    while start_offset < length {
        let (start, end) = match wanted_re[start_offset..].find(r) {
            Some(off) => {
                let start = start_offset + off;
                match wanted_re[start + 2..].find(r) {
                    Some(e) => (start, start + 2 + e),
                    None => (length, length), // unbalanced — ignore
                }
            }
            None => (length, length),
        };
        temp.push_str(&regex::escape(&wanted_re[start_offset..start]));
        if end > start {
            temp.push('(');
            temp.push_str(&wanted_re[start + 2..end]);
            temp.push(')');
        }
        start_offset = end + 2;
    }

    // Placeholder table (must match run-tests.php exactly).
    let mut out = String::with_capacity(temp.len() * 2);
    let bytes = temp.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 1 < bytes.len() {
            let rep: &str = match bytes[i + 1] {
                b'e' => "/",
                b's' => "[^\r\n]+",
                b'S' => "[^\r\n]*",
                b'a' => ".+?",
                b'A' => ".*?",
                b'w' => "\\s*",
                b'i' => "[+-]?\\d+",
                b'd' => "\\d+",
                b'x' => "[0-9a-fA-F]+",
                b'f' => "[+-]?(?:\\d+|(?=\\.\\d))(?:\\.\\d+)?(?:[Ee][+-]?\\d+)?",
                b'c' => ".",
                b'0' => "\\x00",
                _ => "",
            };
            if !rep.is_empty() {
                out.push_str(rep);
                i += 2;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Build an anchored, dotall regex for EXPECTF or EXPECTREGEX.
pub fn anchored(body: &str) -> Option<regex::Regex> {
    regex::RegexBuilder::new(&format!("^{}$", body))
        .dot_matches_new_line(true)
        .build()
        .ok()
}
