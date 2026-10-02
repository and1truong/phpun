use crate::expectf;
use crate::test::PhptTest;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Sections the harness deliberately does not implement yet. Tests using
/// them are classified `unsupported` rather than failed.
const UNSUPPORTED_SECTIONS: &[&str] = &[
    "POST",
    "POST_RAW",
    "GZIP_POST",
    "DEFLATE_POST",
    "PUT",
    "GET",
    "COOKIE",
    "EXPECTHEADERS",
    "HEADERS",
    "STDIN",
    "CGI",
    "PHPDBG",
    "FILE_EXTERNAL",
    "EXPECT_EXTERNAL",
    "EXPECTF_EXTERNAL",
    "EXPECTREGEX_EXTERNAL",
    "PROG",
    "UEXPECT",
    "UEXPECTF",
    "REDIRECTTEST",
    "EXTENSIONS",
    "CAPTURE_STDIO",
];

/// INI defaults mirroring run-tests.php's `$ini_overwrites` (subset that
/// matters for output compatibility).
const DEFAULT_INI: &[&str] = &[
    "error_reporting=E_ALL",
    "display_errors=1",
    "display_startup_errors=1",
    "log_errors=0",
    "html_errors=0",
    "track_errors=0",
    "report_zend_debug=0",
    "docref_root=",
    "docref_ext=.html",
    "error_prepend_string=",
    "error_append_string=",
    "auto_prepend_file=",
    "auto_append_file=",
    "ignore_repeated_errors=0",
    "output_buffering=Off",
    "output_handler=",
    "precision=14",
    "serialize_precision=-1",
    "memory_limit=128M",
    "short_open_tag=0",
    "ignore_repeated_errors=0",
    "zend.assertions=1",
    "zend.exception_ignore_args=0",
    "zend.exception_string_param_max_len=15",
    "date.timezone=UTC",
];

#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Binary under test (phpun or reference php).
    pub binary: String,
    /// Reference PHP used to evaluate SKIPIF. Defaults to `binary`.
    pub reference: Option<String>,
    pub timeout_secs: u64,
    pub jobs: usize,
    /// Run CLEAN section (default true; tests that leave files clean up).
    pub clean: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            binary: String::new(),
            reference: None,
            timeout_secs: 60,
            jobs: 4,
            clean: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Passed,
    Failed,
    /// Expected to fail (--XFAIL--) and did.
    XFailed,
    /// --XFAIL-- but passed anyway (run-tests: WARNED).
    XPassed,
    Skipped(String),
    /// Uses a PHPT feature the harness doesn't implement.
    Unsupported(&'static str),
    /// Malformed .phpt (missing FILE/EXPECT, unknown section).
    Borked(String),
    Crashed(String),
    TimedOut,
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Passed => "PASS",
            Status::Failed => "FAIL",
            Status::XFailed => "XFAIL",
            Status::XPassed => "WARN (xpass)",
            Status::Skipped(_) => "SKIP",
            Status::Unsupported(_) => "UNSUPPORTED",
            Status::Borked(_) => "BORK",
            Status::Crashed(_) => "CRASH",
            Status::TimedOut => "TIMEOUT",
        }
    }
}

#[derive(Debug)]
pub struct TestResult {
    pub path: PathBuf,
    pub test_name: String,
    pub status: Status,
    pub duration_ms: u64,
    /// Populated on failure: trimmed expected text.
    pub expected: Option<String>,
    /// Trimmed actual output.
    pub actual: Option<String>,
    pub diff: Option<String>,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub xfailed: usize,
    pub xpassed: usize,
    pub skipped: usize,
    pub unsupported: usize,
    pub borked: usize,
    pub crashed: usize,
    pub timed_out: usize,
}

impl Summary {
    pub fn applicable(&self) -> usize {
        self.total - self.skipped - self.unsupported - self.borked
    }
    pub fn compatibility(&self) -> f64 {
        if self.applicable() == 0 {
            return 0.0;
        }
        self.passed as f64 / self.applicable() as f64 * 100.0
    }
}

pub fn run_files(files: &[PathBuf], opts: &RunOptions) -> (Vec<TestResult>, Summary) {
    let idx = AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<TestResult>>> =
        files.iter().map(|_| std::sync::Mutex::new(None)).collect();
    let jobs = opts.jobs.max(1).min(files.len().max(1));
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| loop {
                let i = idx.fetch_add(1, Ordering::Relaxed);
                if i >= files.len() {
                    break;
                }
                let r = run_one(&files[i], opts);
                *results[i].lock().unwrap() = Some(r);
            });
        }
    });
    let mut out = Vec::with_capacity(files.len());
    let mut summary = Summary::default();
    for (i, r) in results.into_iter().enumerate() {
        let r = r
            .into_inner()
            .unwrap_or_default()
            .unwrap_or_else(|| TestResult {
                path: files[i].clone(),
                test_name: String::new(),
                status: Status::Crashed("worker panic".into()),
                duration_ms: 0,
                expected: None,
                actual: None,
                diff: None,
            });
        summary.total += 1;
        match r.status {
            Status::Passed => summary.passed += 1,
            Status::Failed => summary.failed += 1,
            Status::XFailed => summary.xfailed += 1,
            Status::XPassed => summary.xpassed += 1,
            Status::Skipped(_) => summary.skipped += 1,
            Status::Unsupported(_) => summary.unsupported += 1,
            Status::Borked(_) => summary.borked += 1,
            Status::Crashed(_) => summary.crashed += 1,
            Status::TimedOut => summary.timed_out += 1,
        }
        out.push(r);
    }
    (out, summary)
}

struct ExecOut {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    signaled: bool,
    timed_out: bool,
}

fn exec(
    binary: &str,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    timeout: Duration,
) -> ExecOut {
    let start = Instant::now();
    let mut cmd = Command::new(binary);
    cmd.args(args).current_dir(cwd).envs(env.iter().cloned());
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ExecOut {
                stdout: String::new(),
                stderr: format!("spawn failed: {}", e),
                code: None,
                signaled: true,
                timed_out: false,
            }
        }
    };
    // Drain pipes on threads so a full pipe never blocks the child.
    use std::io::Read;
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(s) = out_pipe.as_mut() {
            let _ = s.read_to_end(&mut v);
        }
        v
    });
    let err_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(s) = err_pipe.as_mut() {
            let _ = s.read_to_end(&mut v);
        }
        v
    });
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    timed_out = true;
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                return ExecOut {
                    stdout: String::new(),
                    stderr: format!("wait failed: {}", e),
                    code: None,
                    signaled: true,
                    timed_out: false,
                }
            }
        }
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    ExecOut {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        code: status.and_then(|s| s.code()),
        signaled: false,
        timed_out,
    }
}

fn ini_args(test: &PhptTest) -> Vec<String> {
    let mut args = Vec::new();
    for kv in DEFAULT_INI {
        args.push("-d".into());
        args.push(kv.to_string());
    }
    if let Some(ini) = test.get("INI") {
        for line in ini.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            args.push("-d".into());
            args.push(line.to_string());
        }
    }
    args
}

fn env_vars(test: &PhptTest) -> Vec<(String, String)> {
    let mut v = Vec::new();
    if let Some(env) = test.get("ENV") {
        for line in env.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some((k, val)) = line.split_once('=') {
                v.push((k.trim().to_string(), val.to_string()));
            }
        }
    }
    v
}

fn prog_args(test: &PhptTest) -> Vec<String> {
    match test.get("ARGS") {
        Some(a) => shell_split(a),
        None => Vec::new(),
    }
}

fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars = s.chars().peekable();
    let mut quote: Option<char> = None;
    for c in chars {
        match (quote, c) {
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), c2) if c2 == q => quote = None,
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (_, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn write_php(dir: &Path, stem: &str, contents: &str) -> Option<PathBuf> {
    let p = dir.join(format!("{}.php", stem));
    std::fs::File::create(&p)
        .and_then(|mut f| f.write_all(contents.as_bytes()))
        .ok()
        .map(|_| p)
}

pub fn run_one(path: &Path, opts: &RunOptions) -> TestResult {
    let start = Instant::now();
    let test = match crate::test::parse_file(path) {
        Ok(t) => t,
        Err(e) => {
            return done(path, Status::Borked(format!("cannot read: {}", e)), start);
        }
    };
    let name = test.name();
    let mk = |status| TestResult {
        path: path.to_path_buf(),
        test_name: name.clone(),
        status,
        duration_ms: start.elapsed().as_millis() as u64,
        expected: None,
        actual: None,
        diff: None,
    };
    if let Some(b) = &test.borked {
        return TestResult {
            status: Status::Borked(b.clone()),
            ..mk(Status::Borked(String::new()))
        };
    }
    for s in UNSUPPORTED_SECTIONS {
        if test.has(s) {
            return mk(Status::Unsupported(s));
        }
    }
    if !test.has("FILE") {
        return mk(Status::Borked("missing --FILE--".into()));
    }
    let expects = ["EXPECT", "EXPECTF", "EXPECTREGEX"]
        .iter()
        .filter(|s| test.has(s))
        .count();
    if expects != 1 {
        return mk(Status::Borked(
            "needs exactly one of --EXPECT--/--EXPECTF--/--EXPECTREGEX--".into(),
        ));
    }

    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."));
    let stem = path.file_stem().unwrap().to_string_lossy().to_string();

    // SKIPIF — evaluated under the reference binary when available.
    if let Some(skipif) = test.get("SKIPIF") {
        let skip_bin = opts
            .reference
            .clone()
            .unwrap_or_else(|| opts.binary.clone());
        if let Some(sf) = write_php(&dir, &format!("{}.skip", stem), skipif) {
            let mut args = ini_args(&test);
            args.push(sf.display().to_string());
            let o = exec(
                &skip_bin,
                &args,
                &env_vars(&test),
                &dir,
                Duration::from_secs(opts.timeout_secs),
            );
            let _ = std::fs::remove_file(&sf);
            let out = o.stdout.trim().to_lowercase();
            if out.starts_with("skip") {
                let reason = o.stdout.trim().lines().next().unwrap_or("").to_string();
                return mk(Status::Skipped(reason));
            } else if out.starts_with("xfail") {
                // mark by falling through; xfail handled by section anyway
            }
        }
    }

    let php_file = match write_php(&dir, &stem, test.get("FILE").unwrap()) {
        Some(p) => p,
        None => return mk(Status::Borked("cannot write .php file".into())),
    };
    let mut args = ini_args(&test);
    args.push("-f".into());
    args.push(php_file.display().to_string());
    args.extend(prog_args(&test));

    let o = exec(
        &opts.binary,
        &args,
        &env_vars(&test),
        &dir,
        Duration::from_secs(opts.timeout_secs),
    );

    // CLEAN section — run and delete both temp files.
    if let Some(clean) = test.get("CLEAN") {
        if opts.clean {
            if let Some(cf) = write_php(&dir, &format!("{}.clean", stem), clean) {
                let mut cargs = ini_args(&test);
                cargs.push(cf.display().to_string());
                let _ = exec(
                    &opts.binary,
                    &cargs,
                    &env_vars(&test),
                    &dir,
                    Duration::from_secs(opts.timeout_secs),
                );
                let _ = std::fs::remove_file(&cf);
            }
        }
    }
    let _ = std::fs::remove_file(&php_file);

    if o.timed_out {
        return mk(Status::TimedOut);
    }
    if o.signaled {
        return mk(Status::Crashed(o.stderr.clone()));
    }
    if let Some(c) = o.code {
        // Non-zero exit is fine — many tests exit nonzero deliberately.
        // A signal (code 128+n or crash) is reported distinctly.
        if c > 128 && c < 160 {
            let mut r = mk(Status::Crashed(format!("Termsig={}", c - 128)));
            r.actual = Some(normalize(&o.stdout));
            return r;
        }
    } else {
        let mut r = mk(Status::Crashed(o.stderr.clone()));
        r.actual = Some(normalize(&o.stdout));
        return r;
    }

    // Compare: stdout + stderr merged (run-tests uses 2>&1).
    let merged = format!("{}{}", o.stdout, o.stderr);
    let actual = normalize(&merged);
    let (expected, is_regex) = if test.has("EXPECTF") {
        (normalize(test.get("EXPECTF").unwrap()), 1)
    } else if test.has("EXPECTREGEX") {
        (normalize(test.get("EXPECTREGEX").unwrap()), 2)
    } else {
        (normalize(test.get("EXPECT").unwrap()), 0)
    };

    let passed = match is_regex {
        1 => expectf::anchored(&expectf::expectf_to_regex(&expected))
            .map(|re| re.is_match(&actual))
            .unwrap_or(false),
        2 => expectf::anchored(&expected)
            .map(|re| re.is_match(&actual))
            .unwrap_or(false),
        _ => actual == expected,
    };

    let mut r = mk(if passed {
        Status::Passed
    } else {
        Status::Failed
    });
    if test.has("XFAIL") {
        r.status = if passed {
            Status::XPassed
        } else {
            Status::XFailed
        };
    }
    if matches!(r.status, Status::Failed | Status::XPassed) {
        r.expected = Some(expected.clone());
        r.actual = Some(actual.clone());
        r.diff = Some(crate::diff::generate(&expected, &actual, is_regex != 0));
    }
    r
}

fn normalize(s: &str) -> String {
    s.replace("\r\n", "\n").trim().to_string()
}

fn done(path: &Path, status: Status, start: Instant) -> TestResult {
    TestResult {
        path: path.to_path_buf(),
        test_name: String::new(),
        status,
        duration_ms: start.elapsed().as_millis() as u64,
        expected: None,
        actual: None,
        diff: None,
    }
}
