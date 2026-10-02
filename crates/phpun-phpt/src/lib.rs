//! PHPT harness: parse php-src `.phpt` test files and execute them against
//! a PHP binary (reference php or phpun), reporting pass/fail/skip plus a
//! machine-readable JSON report.
//!
//! Semantics mirror php-src's `run-tests.php`:
//! - expected and actual output are `trim()`ed and `\r\n`-normalized
//! - EXPECTF uses the official placeholder → regex mapping
//! - stderr is merged into stdout (2>&1)
//! - `===DONE===` terminates the FILE section

pub mod diff;
pub mod expectf;
pub mod report;
pub mod runner;
pub mod test;

pub use runner::{run_files, RunOptions, Status, TestResult};
pub use test::{parse_file, PhptTest};

use std::process::ExitCode;

/// `phpun phpt` entry point (see crates/phpun/src/main.rs).
pub fn cli(args: Vec<String>) -> ExitCode {
    let mut opts = RunOptions::default();
    let mut paths: Vec<String> = Vec::new();
    let mut json_out: Option<String> = None;
    let mut show_diffs = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--php" => {
                i += 1;
                opts.reference = args.get(i).cloned();
            }
            "--sut" => {
                i += 1;
                opts.binary = args.get(i).cloned().unwrap_or_default();
            }
            "--timeout" => {
                i += 1;
                opts.timeout_secs = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(60);
            }
            "-j" | "--jobs" => {
                i += 1;
                opts.jobs = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(4);
            }
            "--json" => {
                i += 1;
                json_out = args.get(i).cloned();
            }
            "--diff" => show_diffs = true,
            "--keep-going" | "-q" | "--quiet" => {}
            "-h" | "--help" => {
                eprintln!("Usage: phpun phpt <file|dir>... [--php BIN] [--sut BIN]");
                eprintln!("       [--timeout SECS] [-j N] [--json out.json] [--diff]");
                return ExitCode::SUCCESS;
            }
            p => paths.push(p.to_string()),
        }
        i += 1;
    }
    if paths.is_empty() {
        eprintln!("phpun phpt: no test paths given");
        return ExitCode::FAILURE;
    }
    if opts.binary.is_empty() {
        // default SUT: this very executable (the phpun cli)
        opts.binary = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "phpun".into());
    }
    // Canonicalize: tests run with cwd=test dir, so relative paths break.
    if let Ok(c) = std::fs::canonicalize(&opts.binary) {
        opts.binary = c.display().to_string();
    }
    if let Some(r) = &opts.reference {
        if let Ok(c) = std::fs::canonicalize(r) {
            opts.reference = Some(c.display().to_string());
        }
    }
    let files = collect_phpt(&paths);
    let (results, summary) = run_files(&files, &opts);
    report::print(&results, &summary, show_diffs, &opts.binary);
    if let Some(path) = json_out {
        match std::fs::write(&path, report::to_json(&results, &summary)) {
            Ok(_) => eprintln!("wrote {}", path),
            Err(e) => eprintln!("cannot write {}: {}", path, e),
        }
    }
    if summary.failed == 0 && summary.crashed == 0 && summary.timed_out == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn collect_phpt(paths: &[String]) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        let p = std::path::Path::new(p);
        if p.is_dir() {
            visit(p, &mut out);
        } else if p.extension().map(|e| e == "phpt").unwrap_or(false) {
            out.push(p.to_path_buf());
        }
    }
    out.sort();
    out
}

fn visit(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                visit(&p, out);
            } else if p.extension().map(|x| x == "phpt").unwrap_or(false) {
                out.push(p);
            }
        }
    }
}
