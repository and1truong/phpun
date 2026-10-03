use phpun_core::Interp;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("phpt") => phpun_phpt::cli(args[1..].to_vec()),
        Some("--version") | Some("-v") => {
            println!("phpun 0.0.1 (php compat target: 8.5)");
            ExitCode::SUCCESS
        }
        Some("--help") | Some("-h") | None => {
            eprintln!("Usage:");
            eprintln!("  phpun <file.php> [args...]   run a PHP script");
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
    let mut it = Interp::new(file);
    for kv in ini {
        if let Some((k, v)) = kv.split_once('=') {
            it.ini.insert(k.trim().to_string(), v.trim().to_string());
        } else {
            it.ini.insert(kv, String::new());
        }
    }
    let res = it.run_source(&src);
    print!("{}", it.out);
    ExitCode::from((res.exit_code & 0xff) as u8)
}
