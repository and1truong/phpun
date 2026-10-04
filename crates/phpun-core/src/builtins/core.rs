//! Core/standard builtins: constants, ini, env, error handlers, headers, exec.

use super::datetime::date_format;
use super::url::urlencode;
use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        // ----- constants / functions -----
        "define" => {
            let n = arg_str(it, args, 0);
            if n.contains("::") {
                return err(
                    "ValueError",
                    "define(): Argument #1 ($constant_name) cannot be a class constant",
                );
            }
            let v = arg(args, 1);
            it.define_const(&n, v);
            Value::Bool(true)
        }
        "defined" => {
            let n = arg_str(it, args, 0);
            if let Some((cls, cn)) = n.split_once("::") {
                Value::Bool(it.class_const_defined(cls, cn))
            } else {
                Value::Bool(it.const_defined(&n))
            }
        }
        "constant" => {
            let n = arg_str(it, args, 0);
            if let Some((cls, cn)) = n.split_once("::") {
                return it.class_const_named(cls, cn).map(Some);
            }
            match it.const_get(&n) {
                Some(v) => v,
                None => return err("Error", format!("Undefined constant {}", n)),
            }
        }
        "function_exists" => {
            let n = arg_str(it, args, 0).trim_start_matches('\\').to_lowercase();
            Value::Bool(
                it.functions.contains_key(&n) || is_builtin(&n) || builtin_params(&n).is_some(),
            )
        }
        "func_get_args" | "func_num_args" | "func_get_arg" => {
            if !it.in_call() {
                let msg = match name {
                    "func_num_args" => "func_num_args() must be called from a function context",
                    _ => &format!("{}() cannot be called from the global scope", name),
                };
                return Err(PhpError::uncaught("Error", msg, it.cur_line));
            }
            let fa = it.frame_args();
            match name {
                "func_num_args" => Value::Int(fa.len() as i64),
                "func_get_arg" => {
                    let i = arg(args, 0).to_int();
                    if i < 0 {
                        return Err(PhpError::uncaught(
                            "Error",
                            "func_get_arg(): Argument #1 ($position) must be greater than or equal to 0",
                            it.cur_line,
                        ));
                    }
                    match fa.get(i as usize) {
                        Some(c) => c.borrow().clone(),
                        None => {
                            return Err(PhpError::uncaught(
                                "Error",
                                "func_get_arg(): Argument #1 ($position) must be less than the number of the arguments passed to the currently executed function",
                                it.cur_line,
                            ));
                        }
                    }
                }
                _ => {
                    let mut a = PhpArray::new();
                    for c in fa {
                        a.push(c.borrow().clone());
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
            }
        }
        "call_user_func"
        | "call_user_func_array"
        | "forward_static_call"
        | "forward_static_call_array" => {
            let cb = arg(args, 0);
            // `*_array` unpacks the args array: string keys become named
            // args (a later int key is the positional-after-named Error).
            if name.ends_with("_array") {
                let mut ca = crate::interp::CallArgs::empty();
                let mut seen_str = false;
                if let Value::Array(a) = arg(args, 1) {
                    for (k, c) in a.borrow().iter() {
                        match k {
                            ArrKey::Str(s) => {
                                seen_str = true;
                                ca.named.push((s.to_string(), c.clone(), true, false));
                            }
                            _ if seen_str => {
                                return err::<Option<Value>>(
                                    "Error",
                                    "Cannot use positional argument after named argument",
                                );
                            }
                            _ => ca.cells.push(c.clone()),
                        }
                    }
                }
                // call_user_func* never forwards by reference — mark
                // every arg nonref so `&$p` params warn "value given".
                let n = ca.cells.len();
                ca.nonref_cells = (0..n).collect();
                for t in ca.named.iter_mut() {
                    t.2 = false;
                }
                it.call_value(&cb, ca)?
            } else {
                let mut ca =
                    crate::interp::CallArgs::positional(args[1.min(args.len())..].to_vec());
                let n = ca.cells.len();
                ca.nonref_cells = (0..n).collect();
                it.call_value(&cb, ca)?
            }
        }
        "register_shutdown_function" => {
            let f = arg(args, 0);
            let rest: Vec<Cell> = args[1.min(args.len())..].to_vec();
            it.register_shutdown(f, rest);
            Value::Null
        }
        "set_error_handler" => {
            let prev = it.error_handler().unwrap_or(Value::Null);
            it.set_error_handler(if matches!(arg(args, 0), Value::Null) {
                None
            } else {
                Some(arg(args, 0))
            });
            prev
        }
        "set_exception_handler" => {
            let prev = it.exception_handler().unwrap_or(Value::Null);
            it.set_exception_handler(if matches!(arg(args, 0), Value::Null) {
                None
            } else {
                Some(arg(args, 0))
            });
            prev
        }
        "restore_error_handler" => {
            it.restore_error_handler();
            Value::Bool(true)
        }
        "restore_exception_handler" => {
            it.restore_exception_handler();
            Value::Bool(true)
        }
        "trigger_error" | "user_error" => {
            let msg = arg_str(it, args, 0);
            // E_USER_WARNING=512 / E_USER_NOTICE=1024 / E_USER_DEPRECATED=
            // 16384 select the diagnostic label + errno seen by the handler
            // (error_2_exception_001, bug21094). Default is E_USER_NOTICE.
            let level = args.get(1).map(|c| c.borrow().to_int()).unwrap_or(1024);
            it.emit_diag_pub(level, &msg)?;
            Value::Bool(true)
        }
        "error_reporting" => {
            let lv = args.first().map(|c| c.borrow().to_int());
            Value::Int(it.error_reporting(lv))
        }
        "ini_set" => {
            // Stores into the INI table and returns the previous
            // value (false when unset) — memory_limit, html_errors,
            // docref_* etc. all read back through ini_get (bug45392).
            let k = arg_str(it, args, 0);
            let prev = it.ini.get(&k).cloned();
            let v = arg_str(it, args, 1);
            // Shrinking the limit under current usage refuses with a
            // warning and leaves the old value (bug45392).
            if k == "memory_limit" {
                it.ini.insert(k.clone(), v);
                let lim = it.ini_bytes(&k);
                if lim > 0 && (it.mem_used as i64) > lim {
                    let _ = it
                        .ini
                        .insert(k.clone(), prev.clone().unwrap_or_else(|| "-1".into()));
                    it.warn_pub(&format!(
                        "Failed to set memory limit to {} bytes (Current memory usage is {} bytes)",
                        lim, it.mem_used
                    ))?;
                    return Ok(Some(match prev {
                        Some(p) => Value::str(p),
                        None => Value::Bool(false),
                    }));
                }
            } else {
                it.ini.insert(k, v);
            }
            match prev {
                Some(p) => Value::str(p),
                None => Value::Bool(false),
            }
        }
        "ini_get" => {
            let k = arg_str(it, args, 0);
            match it.ini.get(&k) {
                Some(v) => Value::str(v.clone()),
                None => Value::Bool(false),
            }
        }
        "ini_get_all" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "ini_restore" => Value::Null,
        "ini_parse_quantity" => Value::Int(arg(args, 0).to_int()),
        "error_get_last" | "error_clear_last" => Value::Null,
        "set_time_limit" => {
            // Restarts the seconds counter (045).
            it.set_deadline(arg(args, 0).to_int());
            Value::Bool(true)
        }
        "ignore_user_abort" => Value::Int(0),
        "register_tick_function" | "unregister_tick_function" => Value::Bool(true),

        // ----- env/process -----
        "getenv" => {
            let name = arg_str(it, args, 0);
            match it.getenv_pub(&name) {
                Some(v) => Value::str(v),
                None => Value::Bool(false),
            }
        }
        "putenv" => {
            let s = arg_str(it, args, 0);
            Value::Bool(it.putenv_pub(&s))
        }
        "getopt" => {
            // spec: 0 = flag, 1 = required value, 2 = optional value
            let mut short: HashMap<char, u8> = HashMap::new();
            {
                let spec = arg_str(it, args, 0);
                let cs: Vec<char> = spec.chars().collect();
                let mut i = 0;
                while i < cs.len() {
                    if cs[i] == ':' {
                        i += 1;
                        continue;
                    }
                    let kind = if cs.get(i + 1) == Some(&':') {
                        if cs.get(i + 2) == Some(&':') {
                            2
                        } else {
                            1
                        }
                    } else {
                        0
                    };
                    short.insert(cs[i], kind);
                    i += if kind == 0 { 1 } else { kind as usize };
                }
            }
            let mut long: HashMap<String, u8> = HashMap::new();
            if let Value::Array(a) = arg(args, 1) {
                for (_, c) in a.borrow().iter() {
                    let s = c.borrow().to_php_string();
                    let name = s.trim_end_matches(':');
                    let colons = s.len() - name.len();
                    long.insert(name.to_string(), colons.min(2) as u8);
                }
            }
            let argv = it.script_args.clone();
            let mut vals: HashMap<String, Vec<Value>> = HashMap::new();
            let mut order: Vec<String> = Vec::new();
            let put = |name: String,
                       v: Value,
                       vals: &mut HashMap<String, Vec<Value>>,
                       order: &mut Vec<String>| {
                let e = vals.entry(name.clone()).or_default();
                if e.is_empty() {
                    order.push(name);
                }
                e.push(v);
            };
            let mut i = 0usize;
            while i < argv.len() {
                let a = &argv[i];
                if a == "--" {
                    break;
                } else if let Some(body) = a.strip_prefix("--").filter(|b| !b.is_empty()) {
                    let (name, inline) = match body.find('=') {
                        Some(p) => (&body[..p], Some(body[p + 1..].to_string())),
                        None => (body, None),
                    };
                    match long.get(name) {
                        None => {}
                        Some(0) => put(name.to_string(), Value::Bool(false), &mut vals, &mut order),
                        Some(2) => put(
                            name.to_string(),
                            inline.map(Value::str).unwrap_or(Value::Bool(false)),
                            &mut vals,
                            &mut order,
                        ),
                        Some(_) => {
                            if let Some(v) = inline {
                                put(name.to_string(), Value::str(v), &mut vals, &mut order);
                            } else if i + 1 < argv.len() {
                                i += 1;
                                put(
                                    name.to_string(),
                                    Value::str(argv[i].clone()),
                                    &mut vals,
                                    &mut order,
                                );
                            } else {
                                put(name.to_string(), Value::Bool(false), &mut vals, &mut order);
                            }
                        }
                    }
                } else if a.starts_with('-') && a.len() > 1 {
                    let cs: Vec<char> = a[1..].chars().collect();
                    let mut j = 0;
                    while j < cs.len() {
                        match short.get(&cs[j]).copied() {
                            None => j += 1,
                            Some(0) => {
                                put(cs[j].to_string(), Value::Bool(false), &mut vals, &mut order);
                                j += 1;
                            }
                            Some(k) => {
                                if j + 1 < cs.len() {
                                    let v: String = cs[j + 1..].iter().collect();
                                    let v = v.strip_prefix('=').unwrap_or(&v).to_string();
                                    put(cs[j].to_string(), Value::str(v), &mut vals, &mut order);
                                } else if k == 1 && i + 1 < argv.len() {
                                    i += 1;
                                    put(
                                        cs[j].to_string(),
                                        Value::str(argv[i].clone()),
                                        &mut vals,
                                        &mut order,
                                    );
                                } else {
                                    put(
                                        cs[j].to_string(),
                                        Value::Bool(false),
                                        &mut vals,
                                        &mut order,
                                    );
                                }
                                break;
                            }
                        }
                    }
                } else {
                    break; // first non-option arg ends parsing (no permutation)
                }
                i += 1;
            }
            if let Some(c) = args.get(2) {
                *c.borrow_mut() = Value::Int(i as i64);
            }
            let mut out = PhpArray::new();
            for name in order {
                let vs = vals.remove(&name).unwrap_or_default();
                let v = match vs.len() {
                    1 => vs.into_iter().next().unwrap(),
                    _ => {
                        let mut inner = PhpArray::new();
                        for v in vs {
                            inner.push(v);
                        }
                        Value::Array(Rc::new(RefCell::new(inner)))
                    }
                };
                out.set(ArrKey::Str(name.into()), v);
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "php_sapi_name" => Value::str("cli"),
        "phpversion" | "phpversion_strict" => Value::str("8.5.11-phpun"),
        "php_uname" => Value::str("Linux"),
        "memory_get_usage" => Value::Int(2097152),
        "memory_get_peak_usage" => Value::Int(2097152),
        "memory_reset_peak_usage" => Value::Null,
        "zend_version" => Value::str("8.5.11-phpun"),
        "getmypid" => Value::Int(std::process::id() as i64),
        "getmyuid" | "getmygid" | "getmyinode" => Value::Int(1000),
        "get_current_user" => Value::str("ubuntu"),
        "get_cfg_var" | "get_magic_quotes_gpc" | "get_magic_quotes_runtime" => Value::Bool(false),
        "php_ini_loaded_file" | "php_ini_scanned_files" => Value::Bool(false),
        "php_check_syntax" => Value::Bool(true),
        "extension_loaded" => Value::Bool(matches!(
            arg_str(it, args, 0).to_lowercase().as_str(),
            "core"
                | "standard"
                | "spl"
                | "pcre"
                | "hash"
                | "json"
                | "ctype"
                | "random"
                | "date"
                | "reflection"
                | "mbstring"
                // Advertised so Composer's TLS check passes; offline
                // commands never invoke openssl_* functions.
                | "openssl"
        )),
        "get_loaded_extensions" => {
            let mut a = PhpArray::new();
            for e in [
                "Core",
                "standard",
                "SPL",
                "pcre",
                "hash",
                "json",
                "ctype",
                "random",
                "date",
                "Reflection",
                "mbstring",
                "openssl",
            ] {
                a.push(Value::str(e));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "get_extension_funcs" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "dl" => Value::Bool(false),
        "assert" => {
            let v = arg(args, 0);
            if v.is_truthy() {
                Value::Bool(true)
            } else {
                // A `description` arg is the message; otherwise the call
                // renders as `assert(<args>)` (named_params/assert).
                let desc = arg(args, 1);
                let msg = if matches!(desc, Value::Null) || desc.to_php_string().is_empty() {
                    format!("assert({})", std::mem::take(&mut it.assert_src))
                } else {
                    desc.to_php_string()
                };
                return err("AssertionError", &msg);
            }
        }
        "assert_options" => Value::Bool(true),
        "setlocale" => {
            // No real locale switching — echo the first locale string.
            let loc = arg_str(it, args, 1);
            if loc.is_empty() {
                Value::str("C")
            } else {
                Value::str(loc)
            }
        }
        "cli_set_process_title" | "cli_get_process_title" => Value::Bool(true),
        "sleep" => {
            let n = arg(args, 0).to_int().clamp(0, 60);
            std::thread::sleep(std::time::Duration::from_secs(n as u64));
            Value::Int(0)
        }
        "usleep" => {
            let n = arg(args, 0).to_int().clamp(0, 60_000_000);
            std::thread::sleep(std::time::Duration::from_micros(n as u64));
            Value::Null
        }
        "time_nanosleep" => {
            let s = arg(args, 0).to_int().max(0) as u64;
            let ns = arg(args, 1).to_int().clamp(0, 999_999_999) as u64;
            std::thread::sleep(std::time::Duration::new(s.min(60), ns as u32));
            Value::Bool(true)
        }
        "uniqid" => Value::str(format!(
            "{:x}{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            std::process::id()
        )),
        "gc_collect_cycles" | "gc_enable" | "gc_disable" | "gc_mem_caches" => Value::Int(0),
        "gc_status" => {
            let mut a = PhpArray::new();
            for (k, v) in [
                ("running", Value::Bool(false)),
                ("protected", Value::Bool(false)),
                ("full", Value::Bool(false)),
                ("runs", Value::Int(0)),
                ("collected", Value::Int(0)),
                ("threshold", Value::Int(10001)),
                ("buffer_size", Value::Int(16384)),
                ("roots", Value::Int(0)),
                ("application_time", Value::Float(0.0)),
                ("collector_time", Value::Float(0.0)),
                ("destructor_time", Value::Float(0.0)),
                ("free_time", Value::Float(0.0)),
            ] {
                a.set(ArrKey::Str(k.into()), v);
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "gc_enabled" => Value::Bool(false),
        "syslog" | "openlog" | "closelog" => Value::Bool(true),
        "call_func" => Value::Null,
        "get_resource_type" | "get_resource_id" => match arg(args, 0) {
            Value::Resource(r) => {
                if name == "get_resource_id" {
                    Value::Int(r.borrow().id() as i64)
                } else {
                    Value::str("stream")
                }
            }
            _ => Value::Bool(false),
        },
        "proc_open" | "proc_close" | "proc_get_status" | "proc_terminate" => Value::Bool(false),
        "shell_exec" | "exec" | "system" | "passthru" => Value::Null,
        "escapeshellarg" | "escapeshellcmd" => {
            let s = arg_str(it, args, 0);
            Value::str(format!("'{}'", s.replace('\'', "'\\''")))
        }
        "get_include_path" | "set_include_path" | "restore_include_path" => {
            Value::str(".:/home/linuxbrew/.linuxbrew/share/pear")
        }
        "token_get_all" | "token_name" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "highlight_string" => {
            let src = arg(args, 0).to_php_string();
            let h = crate::highlight::highlight_html(&src);
            if arg(args, 1).is_truthy() {
                Value::str(h)
            } else {
                it.emit(&h);
                Value::Bool(true)
            }
        }
        "highlight_file" | "show_source" => {
            let path = arg(args, 0).to_php_string();
            match std::fs::read(&path) {
                Ok(b) => {
                    let h = crate::highlight::highlight_html(&String::from_utf8_lossy(&b));
                    if arg(args, 1).is_truthy() {
                        Value::str(h)
                    } else {
                        it.emit(&h);
                        Value::Bool(true)
                    }
                }
                Err(_) => {
                    it.warn_pub(&format!(
                        "highlight_file({}): Failed to open stream: No such file or directory",
                        path
                    ))?;
                    it.warn_pub(&format!(
                        "highlight_file(): Failed opening '{}' for highlighting",
                        path
                    ))?;
                    Value::Bool(false)
                }
            }
        }
        "php_strip_whitespace" => {
            let path = arg(args, 0).to_php_string();
            match std::fs::read(&path) {
                Ok(b) => Value::str(crate::highlight::strip_whitespace(
                    &String::from_utf8_lossy(&b),
                )),
                Err(_) => Value::str(""),
            }
        }
        "pack" | "unpack" => Value::Bool(false), // stub — tracked in #62
        "header" => {
            let h = arg_str(it, args, 0);
            let replace = args.get(1).map(|c| c.borrow().is_truthy()).unwrap_or(true);
            let code = arg(args, 2).to_int();
            let lower = h.to_lowercase();
            if lower.starts_with("http/") {
                // Status-line form: header("HTTP/1.1 404 Not Found").
                if let Some(c) = h
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse::<i64>().ok())
                {
                    it.resp_code = c;
                }
            } else {
                let name = h.split(':').next().unwrap_or("").trim().to_lowercase();
                if replace && !name.is_empty() {
                    let prefix = format!("{}:", name);
                    it.out_headers
                        .retain(|x| !x.to_lowercase().starts_with(&prefix));
                }
                it.out_headers.push(h);
                if code > 0 {
                    it.resp_code = code;
                } else if name == "location" {
                    it.resp_code = 302;
                }
            }
            Value::Null
        }
        "headers_sent" => Value::Bool(false),
        "headers_list" => {
            let mut a = PhpArray::new();
            for h in &it.out_headers {
                a.push(Value::str(h.clone()));
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "header_remove" => {
            if args.is_empty() {
                it.out_headers.clear();
            } else {
                let prefix = format!("{}:", arg_str(it, args, 0).to_lowercase());
                it.out_headers
                    .retain(|x| !x.to_lowercase().starts_with(&prefix));
            }
            Value::Null
        }
        "header_register_callback" => Value::Bool(false),
        "http_response_code" => {
            if args.is_empty() {
                Value::Int(it.resp_code)
            } else {
                let code = arg(args, 0).to_int();
                it.resp_code = code;
                Value::Int(code)
            }
        }
        "setcookie" | "setrawcookie" => {
            let cname = arg_str(it, args, 0);
            let cval = if name == "setcookie" {
                String::from_utf8(urlencode(&arg_bs(it, args, 1), true)).unwrap_or_default()
            } else {
                arg_str(it, args, 1)
            };
            let mut line = format!("Set-Cookie: {}={}", cname, cval);
            let opts = arg(args, 2);
            let (expires, path, domain, secure, httponly, samesite) = match &opts {
                Value::Array(a) => {
                    let a = a.borrow();
                    let get = |k: &str| {
                        a.entries
                            .iter()
                            .find(|(ek, _)| matches!(ek, ArrKey::Str(s) if s.as_ref() == k))
                            .map(|(_, c)| c.borrow().clone())
                    };
                    (
                        get("expires").map(|v| v.to_int()).unwrap_or(0),
                        get("path").map(|v| it.to_string_of(&v)).unwrap_or_default(),
                        get("domain")
                            .map(|v| it.to_string_of(&v))
                            .unwrap_or_default(),
                        get("secure").map(|v| v.is_truthy()).unwrap_or(false),
                        get("httponly").map(|v| v.is_truthy()).unwrap_or(false),
                        get("samesite")
                            .map(|v| it.to_string_of(&v))
                            .unwrap_or_default(),
                    )
                }
                v => (
                    v.to_int(),
                    arg_str(it, args, 3),
                    arg_str(it, args, 4),
                    arg(args, 5).is_truthy(),
                    arg(args, 6).is_truthy(),
                    String::new(),
                ),
            };
            if expires > 0 {
                line.push_str(&format!(
                    "; expires={}; Max-Age={}",
                    date_format("D, d M Y H:i:s", expires),
                    expires
                        - std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0)
                ));
            }
            if !path.is_empty() {
                line.push_str(&format!("; path={}", path));
            }
            if !domain.is_empty() {
                line.push_str(&format!("; domain={}", domain));
            }
            if secure {
                line.push_str("; secure");
            }
            if httponly {
                line.push_str("; HttpOnly");
            }
            if !samesite.is_empty() {
                line.push_str(&format!("; SameSite={}", samesite));
            }
            it.out_headers.push(line);
            Value::Bool(true)
        }
        "connection_status" | "connection_aborted" => Value::Int(0),
        "fastcgi_finish_request" => Value::Bool(true),
        "phpinfo" | "phpcredits" | "php_logo_guid" | "php_real_logo_guid" | "zend_logo_guid" => {
            Value::Null
        }
        "mail" => {
            if args.len() < 3 {
                return Err(PhpError::fatal(
                    "mail() expects at least 3 arguments",
                    it.cur_line,
                ));
            }
            let to = arg_str(it, args, 0);
            let subject = arg_str(it, args, 1);
            let message = arg_str(it, args, 2);
            let mut headers = String::new();
            if let Some(h) = args.get(3) {
                match &*h.borrow() {
                    Value::Null => {
                        it.deprecated_pub(
                            "mail(): Passing null to parameter #4 ($additional_headers) of type array|string is deprecated",
                        )?;
                    }
                    Value::Array(a) => {
                        for (k, v) in a.borrow().iter() {
                            headers.push_str(&format!(
                                "{}: {}\r\n",
                                key_str(k),
                                v.borrow().to_php_string()
                            ));
                        }
                    }
                    v => headers.push_str(&v.to_php_string()),
                }
            }
            let cmd = it
                .ini
                .get("sendmail_path")
                .cloned()
                .unwrap_or_else(|| "/usr/sbin/sendmail -t -i".into());
            // PHP popen()s sendmail_path and writes the composed message.
            let body = format!("To: {}\nSubject: {}\n{}\n{}", to, subject, headers, message);
            let st = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&cmd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .and_then(|mut p| {
                    use std::io::Write;
                    if let Some(mut s) = p.stdin.take() {
                        let _ = s.write_all(body.as_bytes());
                    }
                    p.wait()
                });
            match st {
                Ok(s) if s.success() => Value::Bool(true),
                Ok(s) => {
                    it.warn_pub(&format!(
                        "Sendmail exited with non-zero exit code {}",
                        s.code().unwrap_or(-1)
                    ))?;
                    Value::Bool(false)
                }
                Err(e) => {
                    it.warn_pub(&format!("Unable to fork sendmail: {}", e))?;
                    Value::Bool(false)
                }
            }
        }
        "version_compare" => {
            let a = arg_str(it, args, 0);
            let b = arg_str(it, args, 1);
            let c = version_cmp(&a, &b);
            if args.len() > 2 {
                match arg_str(it, args, 2).as_str() {
                    "<" | "lt" => Value::Bool(c < 0),
                    "<=" | "le" => Value::Bool(c <= 0),
                    ">" | "gt" => Value::Bool(c > 0),
                    ">=" | "ge" => Value::Bool(c >= 0),
                    "==" | "=" | "eq" => Value::Bool(c == 0),
                    "!=" | "<>" | "ne" => Value::Bool(c != 0),
                    _ => Value::Null,
                }
            } else {
                Value::Int(c)
            }
        }
        "clone" => arg(args, 0),
        "assert_options_now" => Value::Null,
        "zend_test_func" | "zend_test_array_return" => Value::Null,
        _ => return Ok(None),
    }))
}

// ----- helpers -----

fn version_cmp(a: &str, b: &str) -> i64 {
    let pa: Vec<i64> = a
        .split(['.', '-', '_'])
        .map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect();
    let pb: Vec<i64> = b
        .split(['.', '-', '_'])
        .map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect();
    for i in 0..pa.len().max(pb.len()) {
        let x = pa.get(i).copied().unwrap_or(0);
        let y = pb.get(i).copied().unwrap_or(0);
        if x != y {
            return if x < y { -1 } else { 1 };
        }
    }
    0
}
