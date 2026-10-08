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

/// Zend CLI usage text, printed on `-h`/`--help` and after a getopt
/// error (both go to stdout; the error line itself goes to stderr).
const ZEND_USAGE: &str = "\
Usage: php [options] [-f] <file> [--] [args...]
   php [options] -r <code> [--] [args...]
   php [options] [-B <begin_code>] -R <code> [-E <end_code>] [--] [args...]
   php [options] [-B <begin_code>] -F <file> [-E <end_code>] [--] [args...]
   php [options] -S <addr>:<port> [-t docroot] [router]
   php [options] -- [args...]
   php [options] -a

  -a               Run as interactive shell (requires readline extension)
  -c <path>|<file> Look for php.ini file in this directory
  -n               No configuration (ini) files will be used
  -d foo[=bar]     Define INI entry foo with value 'bar'
  -e               Generate extended information for debugger/profiler
  -f <file>        Parse and execute <file>.
  -h               This help
  -i               PHP information
  -l               Syntax check only (lint)
  -m               Show compiled in modules
  -r <code>        Run PHP <code> without using script tags <?..?>
  -B <begin_code>  Run PHP <begin_code> before processing input lines
  -R <code>        Run PHP <code> for every input line
  -F <file>        Parse and execute <file> for every input line
  -E <end_code>    Run PHP <end_code> after processing all input lines
  -H               Hide any passed arguments from external tools.
  -S <addr>:<port> Run with built-in web server.
  -t <docroot>     Specify document root <docroot> for built-in web server.
  -s               Output HTML syntax highlighted source.
  -v               Version number
  -w               Output source with stripped comments and whitespace.

  args...          Arguments passed to script. Use -- args when first argument
                   starts with - or script is read from stdin

  --ini            Show configuration file names
  --ini=diff       Show INI entries that differ from the built-in default

  --rf <name>      Show information about function <name>.
  --rc <name>      Show information about class <name>.
  --re <name>      Show information about extension <name>.
  --rz <name>      Show information about Zend extension <name>.
  --ri <name>      Show configuration for extension <name>.

  --repeat <count> Repeat script execution <count> times.
                   For internal purposes only.

";

fn print_version() {
    println!("phpun 0.0.1 (php compat target: 8.5)");
}

/// Zend getopt error: `Error in argument N, char C: <msg>` on stderr
/// (N is the 0-based argv index counting argv[0]), usage on stdout.
fn opt_err(arg_idx: usize, char_idx: usize, msg: String) -> ExitCode {
    eprintln!("Error in argument {}, char {}: {}", arg_idx, char_idx, msg);
    print!("{}", ZEND_USAGE);
    ExitCode::FAILURE
}

/// A real php option phpun does not implement (-m, -i, -l, -s, -w,
/// -a, -B/-R/-F/-E, -S/-t, --ini, --r*, --repeat). Refusing is more
/// honest than silently ignoring them.
fn opt_unimpl(a: &str) -> ExitCode {
    eprintln!("phpun: option '{}' is not implemented", a);
    ExitCode::FAILURE
}

/// The value of an arg-taking short option: inline when the arg is
/// longer (`-rfoo` binds `foo`), else the next argv. Err = zend's
/// `no argument for option X` getopt error.
fn opt_arg(args: &[String], i: usize) -> Result<(String, usize), ()> {
    let a = &args[i];
    if a.len() > 2 {
        Ok((a[2..].to_string(), 1))
    } else if i + 1 < args.len() {
        Ok((args[i + 1].clone(), 2))
    } else {
        Err(())
    }
}

/// opt_arg at argv `*i` — None on the getopt `no argument for
/// option X` error (caller emits it, naming its own flag).
fn want_opt(args: &[String], i: &mut usize) -> Option<String> {
    match opt_arg(args, *i) {
        Ok((v, n)) => {
            *i += n;
            Some(v)
        }
        Err(()) => None,
    }
}

/// `phpun [-d k=v]* [-n] [-c PATH] [-q] [-f] <file.php> [args...]`
/// `phpun [-d k=v]* [-n] -r <code> [args...]`
///
/// Option parsing follows zend's cli getopt: a single left-to-right
/// pass where `-r`/`-f`/`-d`/`-c` take their value inline or from the
/// next argv, `-r`/`-f` are once-only (`-r` overrides an earlier `-f`;
/// `-f` after `-r` is the "Either execute direct code..." error), and
/// the first bare positional or `--` ends option parsing — everything
/// after lands in $argv. With no file and no `-r` code php reads stdin
/// (labelled "Standard input code", like `php -r`'s "Command line code").
fn run_script(args: &[String]) -> ExitCode {
    let mut file: Option<String> = None;
    let mut code: Option<String> = None;
    let mut ini: Vec<String> = Vec::new();
    let mut script_args: Vec<String> = Vec::new();
    let mut no_ini = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        if a == "--" {
            // End of options: the rest is $argv[1..] (stdin mode when
            // no file/-r was picked).
            i += 1;
            script_args.extend(args[i..].iter().cloned());
            break;
        }
        if !(a.len() > 1 && a.starts_with('-')) {
            // First bare positional — the script file when neither
            // -r nor -f picked a source, else a script arg. Options
            // stop here either way.
            if file.is_none() && code.is_none() {
                file = Some(a);
            } else {
                script_args.push(a);
            }
            i += 1;
            script_args.extend(args[i..].iter().cloned());
            break;
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (long, None),
            };
            match name {
                "help" => {
                    print!("{}", ZEND_USAGE);
                    return ExitCode::SUCCESS;
                }
                "version" => {
                    print_version();
                    return ExitCode::SUCCESS;
                }
                "no-php-ini" => {
                    no_ini = true;
                    i += 1;
                }
                "php-ini" => match inline {
                    Some(_) => i += 1,
                    None if i + 1 < args.len() => i += 2,
                    None => {
                        return opt_err(i + 1, 1, "no argument for option -".to_string());
                    }
                },
                // Real php options phpun doesn't implement yet.
                "modules" | "info" | "phpinfo" | "ini" | "rf" | "rc" | "re" | "rz" | "ri"
                | "repeat" | "process-title" => return opt_unimpl(&a),
                _ => return opt_err(i + 1, 1, "no argument for option -".to_string()),
            }
            continue;
        }
        match a.as_str() {
            // `-n` skips ini loading — zend falls back to compiler
            // defaults (log_errors=0); `-q`/`-e`/`-H` are
            // CGI-era/debugger no-ops.
            "-n" => {
                no_ini = true;
                i += 1;
            }
            "-q" | "-e" | "-H" => i += 1,
            "-h" => {
                print!("{}", ZEND_USAGE);
                return ExitCode::SUCCESS;
            }
            "-v" => {
                print_version();
                return ExitCode::SUCCESS;
            }
            // Real php options phpun doesn't implement yet.
            "-m" | "-i" | "-l" | "-s" | "-w" | "-a" | "-B" | "-R" | "-F" | "-E" | "-S" | "-t" => {
                return opt_unimpl(&a)
            }
            _ if a.starts_with("-r") => {
                let Some(v) = want_opt(args, &mut i) else {
                    return opt_err(i + 1, 2, "no argument for option r".to_string());
                };
                if code.is_some() {
                    println!("You can use -r only once.");
                    return ExitCode::FAILURE;
                }
                code = Some(v);
            }
            _ if a.starts_with("-f") => {
                let Some(v) = want_opt(args, &mut i) else {
                    return opt_err(i + 1, 2, "no argument for option f".to_string());
                };
                if code.is_some() {
                    println!("Either execute direct code, process stdin or use a file.");
                    return ExitCode::FAILURE;
                }
                if file.is_some() {
                    println!("You can use -f only once.");
                    return ExitCode::FAILURE;
                }
                file = Some(v);
            }
            _ if a.starts_with("-d") => {
                let Some(v) = want_opt(args, &mut i) else {
                    return opt_err(i + 1, 2, "no argument for option d".to_string());
                };
                ini.push(v);
            }
            // `-c`/`--php-ini` points at an ini path — ignored.
            _ if a.starts_with("-c") => {
                if want_opt(args, &mut i).is_none() {
                    return opt_err(i + 1, 2, "no argument for option c".to_string());
                }
            }
            _ => {
                return opt_err(i + 1, 2, format!("option not found {}", &a[1..2]));
            }
        }
    }
    // `-r` code wins over an earlier `-f` file; with neither, php
    // reads stdin (pseudo-path "Standard input code").
    let (src, label): (String, &str) = match (&code, &file) {
        (Some(code), f) => {
            // The -f operand is still validated when -r wins — zend
            // reports 'Could not open input file' for a missing one.
            if let Some(f) = f {
                if std::fs::File::open(f).is_err() {
                    eprintln!("Could not open input file: {}", f);
                    return ExitCode::FAILURE;
                }
            }
            (code.clone(), "Command line code")
        }
        (None, Some(file)) => match std::fs::read_to_string(file) {
            Ok(s) => (s, file.as_str()),
            Err(_) => {
                eprintln!("Could not open input file: {}", file);
                return ExitCode::FAILURE;
            }
        },
        (None, None) => (
            std::io::read_to_string(std::io::stdin()).unwrap_or_default(),
            "Standard input code",
        ),
    };
    // __FILE__/__DIR__ are always absolute in PHP; the -r/stdin
    // pseudo-paths stay literal.
    let abs = match (&code, &file) {
        (None, Some(file)) => std::fs::canonicalize(file)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| file.clone()),
        _ => label.to_string(),
    };
    let mut it = Interp::new(&abs);
    // `-n`/`--no-php-ini`: zend falls back to compiler ini defaults —
    // log_errors off, so the 'PHP Fatal error' stderr copy vanishes.
    if no_ini {
        it.ini.insert("log_errors".into(), "0".into());
    }
    // CLI PHP sets the script-path SERVER vars to the path AS INVOKED
    // (`php console.php` shows "console.php"), unlike __FILE__ which is
    // always canonical. `-r`/stdin code sets SCRIPT_FILENAME to "" and
    // PHP_SELF/SCRIPT_NAME to "Standard input code".
    let (svar_file, svar_name) = if code.is_none() && file.is_some() {
        (label, label)
    } else {
        ("", "Standard input code")
    };
    it.set_server_var("SCRIPT_FILENAME", svar_file);
    it.set_server_var("PHP_SELF", svar_name);
    it.set_server_var("SCRIPT_NAME", svar_name);
    // $argv[0] is the as-invoked path (or "Standard input code" for
    // -r/stdin); everything collected after the file/code is $argv[1..].
    it.set_script_args(svar_name, &script_args);
    for kv in ini {
        if let Some((k, v)) = kv.split_once('=') {
            it.ini.insert(k.trim().to_string(), v.trim().to_string());
        } else {
            it.ini.insert(kv, String::new());
        }
    }
    // Stream output to the real fds so stderr notices interleave with
    // stdout in PHP's order; `phpun test`/`serve` keep buffered capture.
    it.live_io = true;
    let res = if code.is_some() {
        // `-r` code is tagless source parsed in-script like eval()'d
        // code — a `<?php` is a syntax error, never an open tag.
        it.run_code(&src)
    } else {
        it.run_source(&src)
    };
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
