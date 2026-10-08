mod install;
mod semver_lite;

use phpun_core::Interp;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("phpt") => phpun_phpt::cli(args[1..].to_vec()),
        Some("install") => install::cli(&args[1..]),
        Some("serve") => serve(&args[1..]),
        Some("test") => run_tests(&args[1..]),
        Some("fmt") => fmt(&args[1..]),
        Some("--version") | Some("-v") => {
            println!("phpun 0.0.1 (php compat target: 8.5)");
            ExitCode::SUCCESS
        }
        Some("--help") | Some("-h") | None => {
            eprintln!("Usage:");
            eprintln!("  phpun <file.php> [args...]   run a PHP script");
            eprintln!("  phpun serve <file.php>       dev HTTP server (default :8000)");
            eprintln!("  phpun phpt <paths> [flags]   run PHPT tests");
            eprintln!(
                "  phpun install [-d DIR]        resolve composer.json deps into vendor/ (#29)"
            );
            eprintln!("  phpun test [path] [flags]    run userland *_test.php / *Test.php files");
            eprintln!("  phpun fmt [-w] [--check] [--diff] <path>...  PSR-12 formatter (#30)");
            ExitCode::SUCCESS
        }
        _ => run_script(&args),
    }
}

/// `phpun [-d k=v]* [-q] [-f] <file.php> [args...]`
///
/// `-d` ini flags and `-q`/`--` separators are accepted so the PHPT harness
/// can invoke phpun with the same argv as reference php.
fn run_script(args: &[String]) -> ExitCode {
    let mut file: Option<&str> = None;
    let mut ini: Vec<String> = Vec::new();
    let mut no_php_ini = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "--" => {
                i += 1;
                if i < args.len() {
                    file = Some(&args[i]);
                }
                break;
            }
            "-d" => {
                if i + 1 < args.len() {
                    ini.push(args[i + 1].clone());
                }
                i += 2;
                continue;
            }
            "-f" | "-q" => {
                i += 1;
                continue;
            }
            "-n" | "--no-php-ini" => {
                // `-n`/`--no-php-ini`: reference php skips ini files —
                // the compiled-in defaults differ (log_errors=0).
                no_php_ini = true;
                i += 1;
                continue;
            }
            s if s.starts_with("-d") => {
                ini.push(s[2..].to_string());
                i += 1;
                continue;
            }
            _ => {
                file = Some(a);
                break;
            }
        }
    }
    let Some(file) = file else {
        eprintln!("phpun: no input file");
        return ExitCode::FAILURE;
    };
    let src = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Could not open input file: {}", file);
            let _ = e;
            return ExitCode::FAILURE;
        }
    };
    // __FILE__/__DIR__ are always absolute in PHP.
    let abs = std::fs::canonicalize(file)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| file.to_string());
    let mut it = Interp::new(&abs);
    if no_php_ini {
        // The binary's compiled-in default is log_errors=0 — the dist
        // php.ini turns it on, so -n leaves the `PHP Fatal error:`
        // stderr copy off entirely (new_oom's merged 2>&1 stream).
        it.ini.insert("log_errors".to_string(), "0".to_string());
    }
    // CLI PHP sets the script-path SERVER vars to the path AS INVOKED
    // (`php console.php` shows "console.php"), unlike __FILE__ which is
    // always canonical.
    for k in ["SCRIPT_FILENAME", "PHP_SELF", "SCRIPT_NAME"] {
        it.set_server_var(k, file);
    }
    // Args after the script path become $argv[1..] like reference php;
    // argv[0] keeps the as-invoked path too.
    it.set_script_args(file, &args[(i + 1).min(args.len())..]);
    // Stream output to the real fds so stderr notices interleave with
    // stdout in PHP's order; `phpun test`/`serve` keep buffered capture.
    // Set before -d application so a refused-limit warning streams
    // ahead of the script's output like zend's does.
    it.live_io = true;
    for kv in ini {
        if let Some((k, v)) = kv.split_once('=') {
            if k.trim() == "memory_limit" {
                // A -d limit under zend's 2M bootstrap floor refuses
                // at startup: 'in Unknown on line 0' warning pair and
                // the previous value stays in effect.
                let prev = it.ini.get("memory_limit").cloned();
                it.ini
                    .insert("memory_limit".to_string(), v.trim().to_string());
                let lim = it.ini_bytes("memory_limit");
                if lim > 0 && lim <= 2097152 {
                    let msg = format!(
                        "Failed to set memory limit to {} bytes (Current memory usage is 2097152 bytes)",
                        lim
                    );
                    eprintln!("PHP Warning:  {} in Unknown on line 0", msg);
                    it.emit(&format!("Warning: {} in Unknown on line 0\n", msg));
                    match prev {
                        Some(p) => {
                            it.ini.insert("memory_limit".to_string(), p);
                        }
                        None => {
                            it.ini.remove("memory_limit");
                        }
                    }
                }
            } else {
                it.ini.insert(k.trim().to_string(), v.trim().to_string());
            }
        } else {
            it.ini.insert(kv, String::new());
        }
    }
    let res = it.run_source(&src);
    let _ = std::io::Write::write_all(&mut std::io::stdout(), &it.out);
    eprint!("{}", it.err_buf);
    ExitCode::from((res.exit_code & 0xff) as u8)
}

/// `phpun serve <file.php> [--host H] [--port N | -p N]`
fn serve(args: &[String]) -> ExitCode {
    let mut file: Option<&str> = None;
    let mut host = "127.0.0.1".to_string();
    let mut port = 8000u16;
    let mut docroot: Option<String> = None;
    let mut workers = 0usize;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--worker" => {
                workers = 1;
                i += 1;
            }
            s if s.starts_with("--workers=") => {
                workers = s[10..].parse().unwrap_or(1);
                i += 1;
            }
            "--workers" => {
                if i + 1 < args.len() {
                    workers = args[i + 1].parse().unwrap_or(1);
                }
                i += 2;
            }
            "--host" => {
                if i + 1 < args.len() {
                    host = args[i + 1].clone();
                }
                i += 2;
            }
            "--docroot" | "-t" => {
                if i + 1 < args.len() {
                    docroot = Some(args[i + 1].clone());
                }
                i += 2;
            }
            s if s.starts_with("--docroot=") => {
                docroot = Some(s[10..].to_string());
                i += 1;
            }
            "--port" | "-p" => {
                if i + 1 < args.len() {
                    port = args[i + 1].parse().unwrap_or(8000);
                }
                i += 2;
            }
            s if s.starts_with("--port=") => {
                port = s[7..].parse().unwrap_or(8000);
                i += 1;
            }
            s if s.starts_with("--host=") => {
                host = s[7..].to_string();
                i += 1;
            }
            a => {
                file = Some(a);
                i += 1;
            }
        }
    }
    let Some(file) = file else {
        eprintln!(
            "usage: phpun serve <file.php> [--host H] [--port N] [--docroot DIR] [--worker[=N]]"
        );
        return ExitCode::FAILURE;
    };
    let abs = std::fs::canonicalize(file)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| file.to_string());
    ExitCode::from(phpun_core::serve::serve(&abs, &host, port, docroot.as_deref(), workers) as u8)
}

/// `phpun test [path] [--bootstrap file] [--filter substr]`
///
/// Discovers `*_test.php` and `*Test.php` files under `path` (a directory,
/// default `.`; a file runs alone) and executes each in a fresh interpreter.
/// A test passes when it exits 0 without an uncaught error — PHPUnit-style
/// files signal failure through `exit(1)` or a thrown exception.
fn run_tests(args: &[String]) -> ExitCode {
    let mut path: Option<String> = None;
    let mut bootstrap: Option<String> = None;
    let mut filter: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bootstrap" => {
                if i + 1 < args.len() {
                    bootstrap = Some(args[i + 1].clone());
                }
                i += 2;
            }
            s if s.starts_with("--bootstrap=") => {
                bootstrap = Some(s[12..].to_string());
                i += 1;
            }
            "--filter" => {
                if i + 1 < args.len() {
                    filter = Some(args[i + 1].clone());
                }
                i += 2;
            }
            s if s.starts_with("--filter=") => {
                filter = Some(s[9..].to_string());
                i += 1;
            }
            "-d" => i += 2, // ini flags accepted, ignored for now
            s if s.starts_with("-d") => i += 1,
            a => {
                path = Some(a.to_string());
                i += 1;
            }
        }
    }
    let root = path.unwrap_or_else(|| ".".to_string());
    let mut files = Vec::new();
    let rp = std::path::Path::new(&root);
    if rp.is_file() {
        files.push(rp.to_path_buf());
    } else {
        collect_tests(rp, &mut files);
        files.sort();
    }
    if let Some(f) = &filter {
        files.retain(|p| p.display().to_string().contains(f.as_str()));
    }
    if files.is_empty() {
        eprintln!("phpun test: no test files found under {}", root);
        return ExitCode::FAILURE;
    }
    println!("phpun test — {} file(s)", files.len());
    let mut pass = 0usize;
    let mut failed: Vec<String> = Vec::new();
    let start = std::time::Instant::now();
    for file in &files {
        let abs = std::fs::canonicalize(file)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| file.display().to_string());
        // Run the test through `require` so __FILE__/__DIR__ bind to the
        // real path and a --bootstrap can be prepended cleanly.
        let mut src = String::new();
        if let Some(b) = &bootstrap {
            let babs = std::fs::canonicalize(b)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| b.clone());
            src.push_str(&format!(
                "require '{}';\n",
                babs.replace('\\', "\\\\").replace('\'', "\\'")
            ));
        }
        src.push_str(&format!(
            "require '{}';",
            abs.replace('\\', "\\\\").replace('\'', "\\'")
        ));
        let t = std::time::Instant::now();
        let mut it = Interp::new(&abs);
        let res = it.run_source(&src);
        let ok = res.exit_code == 0 && res.fatal.is_none();
        let name = file.display().to_string();
        if ok {
            pass += 1;
            println!("PASS {} ({}ms)", name, t.elapsed().as_millis());
        } else {
            failed.push(name.clone());
            println!("FAIL {}", name);
            let out = String::from_utf8_lossy(&it.out);
            let out = out.trim_end();
            if !out.is_empty() {
                for line in out.lines().take(20) {
                    println!("    {}", line);
                }
            }
            let err = it.err_buf.trim_end();
            if !err.is_empty() {
                for line in err.lines().take(10) {
                    println!("    {}", line);
                }
            }
        }
    }
    println!(
        "\n{} passed, {} failed, {} total ({}ms)",
        pass,
        failed.len(),
        files.len(),
        start.elapsed().as_millis()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// `phpun fmt [-w|--write] [--check] [--diff] <path>...` (#30)
///
/// Formats PHP sources PSR-12-style over our own lexer. Directories
/// are walked for `*.php`. Without flags a single file is printed to
/// stdout; `-w` rewrites in place, `--check` exits non-zero when any
/// file differs (for CI), `--diff` shows a unified diff.
fn fmt(args: &[String]) -> ExitCode {
    let mut write = false;
    let mut check = false;
    let mut diff = false;
    let mut paths: Vec<String> = Vec::new();
    for a in args {
        match a.as_str() {
            "-w" | "--write" => write = true,
            "--check" => check = true,
            "--diff" => diff = true,
            _ => paths.push(a.clone()),
        }
    }
    if paths.is_empty() {
        eprintln!("usage: phpun fmt [-w|--write] [--check] [--diff] <path>...");
        return ExitCode::FAILURE;
    }
    let mut files = Vec::new();
    for p in &paths {
        let rp = std::path::Path::new(p);
        if rp.is_dir() {
            collect_php(rp, &mut files);
        } else {
            files.push(rp.to_path_buf());
        }
    }
    files.sort();
    if files.len() > 1 && !write && !check && !diff {
        eprintln!("phpun fmt: multiple files need -w, --check or --diff");
        return ExitCode::FAILURE;
    }
    let mut changed = 0usize;
    let mut failed = false;
    for file in &files {
        let name = file.display().to_string();
        let src = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("phpun fmt: {}: {}", name, e);
                failed = true;
                continue;
            }
        };
        let out = match phpun_core::fmt::format(&src) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("phpun fmt: {}: {}", name, e);
                failed = true;
                continue;
            }
        };
        if !write && !check && !diff {
            // stdout mode — always emit the formatted text.
            print!("{}", out);
            continue;
        }
        if out == src {
            continue;
        }
        changed += 1;
        if diff {
            println!("--- {}\n+++ {}", name, name);
            print!("{}", phpun_core::fmt::unified_diff(&src, &out, 3));
        } else if check {
            println!("{}", name);
        } else if write {
            if let Err(e) = std::fs::write(file, &out) {
                eprintln!("phpun fmt: {}: {}", name, e);
                failed = true;
            } else {
                eprintln!("formatted {}", name);
            }
        } else {
            print!("{}", out);
        }
    }
    if check && changed > 0 {
        eprintln!("phpun fmt: {} file(s) not formatted", changed);
        return ExitCode::FAILURE;
    }
    if failed {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn collect_php(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            let name = e.file_name().to_string_lossy().to_string();
            if name != "vendor" && !name.starts_with('.') {
                collect_php(&p, out);
            }
        } else if p.extension().and_then(|x| x.to_str()) == Some("php") {
            out.push(p);
        }
    }
}

fn collect_tests(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            // Skip vendor and hidden dirs like the common runner default.
            let name = e.file_name().to_string_lossy().to_string();
            if name != "vendor" && !name.starts_with('.') {
                collect_tests(&p, out);
            }
        } else if let Some(stem) = p.file_name().and_then(|n| n.to_str()) {
            if stem.ends_with("_test.php") || stem.ends_with("Test.php") {
                out.push(p);
            }
        }
    }
}
