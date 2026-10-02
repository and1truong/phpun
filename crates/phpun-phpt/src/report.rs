use crate::runner::{Status, Summary, TestResult};

pub fn print(results: &[TestResult], s: &Summary, show_diffs: bool, binary: &str) {
    println!("phpun PHPT report — binary under test: {}", binary);
    println!("{}", "=".repeat(72));
    for r in results {
        let label = r.status.label();
        let extra = match &r.status {
            Status::Skipped(reason) => format!(" — {}", reason),
            Status::Unsupported(sec) => format!(" (section {} unsupported)", sec),
            Status::Borked(msg) => format!(" — {}", msg),
            Status::Crashed(msg) => {
                let m: String = msg.chars().take(60).collect();
                format!(" — {}", m.trim())
            }
            _ => String::new(),
        };
        match r.status {
            Status::Passed => {}
            _ => println!(
                "{:14} {:50} {}{}",
                label,
                short(&r.path),
                r.test_name,
                extra
            ),
        }
    }
    println!("{}", "=".repeat(72));
    println!("Total:           {}", s.total);
    println!("Passed:          {}", s.passed);
    println!("Failed:          {}", s.failed);
    println!("Expected-fail:   {}", s.xfailed);
    println!("Warned (xpass):  {}", s.xpassed);
    println!("Skipped:         {}", s.skipped);
    println!("Unsupported:     {}", s.unsupported);
    println!("Borked:          {}", s.borked);
    println!("Crashed:         {}", s.crashed);
    println!("Timed out:       {}", s.timed_out);
    println!("Applicable:      {}", s.applicable());
    println!(
        "Compatibility:   {:.1}%  ({}/{})",
        s.compatibility(),
        s.passed,
        s.applicable()
    );
    if show_diffs {
        for r in results.iter().filter(|r| r.diff.is_some()) {
            println!("\n=== {} — {} ===", short(&r.path), r.test_name);
            println!("{}", r.diff.as_ref().unwrap());
        }
    }
}

fn short(p: &std::path::Path) -> String {
    p.file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub fn to_json(results: &[TestResult], s: &Summary) -> String {
    let mut j = String::new();
    j.push_str("{\n  \"summary\": {\n");
    j.push_str(&format!("    \"total\": {},\n", s.total));
    j.push_str(&format!("    \"passed\": {},\n", s.passed));
    j.push_str(&format!("    \"failed\": {},\n", s.failed));
    j.push_str(&format!("    \"xfailed\": {},\n", s.xfailed));
    j.push_str(&format!("    \"xpassed\": {},\n", s.xpassed));
    j.push_str(&format!("    \"skipped\": {},\n", s.skipped));
    j.push_str(&format!("    \"unsupported\": {},\n", s.unsupported));
    j.push_str(&format!("    \"borked\": {},\n", s.borked));
    j.push_str(&format!("    \"crashed\": {},\n", s.crashed));
    j.push_str(&format!("    \"timed_out\": {},\n", s.timed_out));
    j.push_str(&format!("    \"applicable\": {},\n", s.applicable()));
    j.push_str(&format!(
        "    \"compatibility_pct\": {:.2}\n",
        s.compatibility()
    ));
    j.push_str("  },\n  \"results\": [\n");
    for (i, r) in results.iter().enumerate() {
        let status = match &r.status {
            Status::Passed => "pass",
            Status::Failed => "fail",
            Status::XFailed => "xfail",
            Status::XPassed => "xpass",
            Status::Skipped(_) => "skip",
            Status::Unsupported(_) => "unsupported",
            Status::Borked(_) => "borked",
            Status::Crashed(_) => "crash",
            Status::TimedOut => "timeout",
        };
        let reason = match &r.status {
            Status::Skipped(x) | Status::Borked(x) | Status::Crashed(x) => x.clone(),
            Status::Unsupported(x) => x.to_string(),
            _ => String::new(),
        };
        j.push_str(&format!(
            "    {{\"file\": \"{}\", \"name\": \"{}\", \"status\": \"{}\", \"reason\": \"{}\", \"duration_ms\": {}}}{}\n",
            esc(&r.path.display().to_string()),
            esc(&r.test_name),
            status,
            esc(&reason),
            r.duration_ms,
            if i + 1 == results.len() { "" } else { "," }
        ));
    }
    j.push_str("  ]\n}\n");
    j
}
