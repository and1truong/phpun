//! Tiny unified-diff for failure reports (keeps the crate dependency-light).

/// Produce a compact `EXPECTED` vs `ACTUAL` diff: for EXPECTF, each expected
/// line is matched per-line as regex (same approach as run-tests.php).
pub fn generate(expected: &str, actual: &str, expected_is_regex: bool) -> String {
    let e_lines: Vec<&str> = expected.split('\n').collect();
    let a_lines: Vec<&str> = actual.split('\n').collect();
    let mut out = String::new();
    let max = e_lines.len().max(a_lines.len());
    let mut first = true;
    for i in 0..max {
        let e = e_lines.get(i).copied();
        let a = a_lines.get(i).copied();
        let same = match (e, a) {
            (Some(e), Some(a)) => {
                if expected_is_regex {
                    crate::expectf::anchored(&crate::expectf::expectf_to_regex(e))
                        .map(|re| re.is_match(a))
                        .unwrap_or(e == a)
                } else {
                    e == a
                }
            }
            _ => false,
        };
        if same {
            continue;
        }
        if first {
            out.push_str("-------- expected\n++++++++ actual\n");
            first = false;
        }
        if let Some(e) = e {
            out.push_str(&format!("{:4}- {}\n", i + 1, e));
        }
        if let Some(a) = a {
            out.push_str(&format!("{:4}+ {}\n", i + 1, a));
        }
    }
    if first {
        out.push_str("(no line diff — outputs differ only in whitespace)\n");
    }
    out
}
