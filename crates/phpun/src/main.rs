use phpun_core::Interp;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("phpt") => phpun_phpt::cli(args[1..].to_vec()),
        Some("serve") => serve(&args[1..]),
        Some("--version") | Some("-v") => {
            println!("phpun 0.0.1 (php compat target: 8.5)");
            ExitCode::SUCCESS
        }
        Some("--help") | Some("-h") | None => {
            eprintln!("Usage:");
            eprintln!("  phpun <file.php> [args...]   run a PHP script");
            eprintln!("  phpun serve <file.php>       dev HTTP server (default :8000)");
            eprintln!("  phpun phpt <paths> [flags]   run PHPT tests");
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
    for kv in ini {
        if let Some((k, v)) = kv.split_once('=') {
            it.ini.insert(k.trim().to_string(), v.trim().to_string());
        } else {
            it.ini.insert(kv, String::new());
        }
    }
    let res = it.run_source(&src);
    print!("{}", it.out);
    eprint!("{}", it.err_buf);
    ExitCode::from((res.exit_code & 0xff) as u8)
}

/// `phpun serve <file.php> [--host H] [--port N | -p N]`
fn serve(args: &[String]) -> ExitCode {
    let mut file: Option<&str> = None;
    let mut host = "127.0.0.1".to_string();
    let mut port = 8000u16;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                if i + 1 < args.len() {
                    host = args[i + 1].clone();
                }
                i += 2;
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
        eprintln!("usage: phpun serve <file.php> [--host H] [--port N]");
        return ExitCode::FAILURE;
    };
    let abs = std::fs::canonicalize(file)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| file.to_string());
    ExitCode::from(phpun_core::serve::serve(&abs, &host, port) as u8)
}
